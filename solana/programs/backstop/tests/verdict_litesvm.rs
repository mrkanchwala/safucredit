// Stage 2 item 1: oracle verdict verification through the real program, on LiteSVM with the Ed25519
// precompile enabled (litesvm feature "precompiles"; without it signatures are never checked).
//
// Negative cases first, one happy path last. Every rejection asserts the exact program error, so a test
// cannot pass because the transaction failed for some unrelated reason.
//
// Not covered here: invocation through CPI (needs a second program); guarded by the stack-height check
// in `verdict::verify_preceding_ed25519` and to be tested when the vault program exists.

use anchor_lang::{
    prelude::{pubkey, Clock, Pubkey},
    solana_program::instruction::Instruction,
    AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas,
};
use backstop::{
    errors::VerdictError,
    state::{BackstopConfig, VerdictAttestation, ACCOUNT_VERSION, ATTESTATION_SEED, CONFIG_SEED},
    verdict::{encode_message, VerdictArgs, CLUSTER_DEVNET, CLUSTER_MAINNET, MAX_VERDICT_TTL_SECS},
};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

const NOW: i64 = 1_800_000_000;
const ED25519: Pubkey = pubkey!("Ed25519SigVerify111111111111111111111111111");
const COMPUTE_BUDGET: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");
const IX_SYSVAR: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");
const SYSTEM: Pubkey = pubkey!("11111111111111111111111111111111");

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    oracle: Keypair,
}

fn setup(cluster_tag: u8) -> Env {
    let mut svm = LiteSVM::new();
    svm.add_program(
        backstop::ID,
        include_bytes!("../../../target/deploy/backstop.so"),
    )
    .unwrap();
    let mut clock: Clock = svm.get_sysvar();
    clock.unix_timestamp = NOW;
    svm.set_sysvar(&clock);

    let payer = Keypair::new();
    svm.airdrop(&payer.pubkey(), 10_000_000_000).unwrap();
    let oracle = Keypair::new();

    // State set directly (skill guidance: set up state, don't replay long setup transactions). The
    // initialize instruction and its upgrade-authority gate are tested in payout_litesvm.rs.
    let (config_pda, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &backstop::ID);
    let config = BackstopConfig {
        version: ACCOUNT_VERSION,
        admin: payer.pubkey(),
        verdict_oracle: oracle.pubkey(),
        cluster_tag,
        bump,
        // Token fields are unused by these tests: nothing here moves USDC. They exist so the
        // fixture matches the live layout after 2c.
        usdc_mint: Pubkey::new_unique(),
        cash: 0,
        total_shares: 0,
        per_claim_cap_bps: 1_000,
        open_claims: 0,
        withdraw_delay_secs: 0,
        reserved: [0; 32],
    };
    let mut data = Vec::new();
    config.try_serialize(&mut data).unwrap();
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        config_pda,
        Account {
            lamports,
            data,
            owner: backstop::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    Env { svm, payer, oracle }
}

fn args() -> VerdictArgs {
    VerdictArgs {
        liquidation_record: Pubkey::new_unique(),
        borrower: Pubkey::new_unique(),
        payout: 150_000_000,
        tier: 1,
        verdict_hash: [9; 32],
        deadline: NOW + 600,
    }
}

fn attestation_pda(record: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[ATTESTATION_SEED, record.as_ref()], &backstop::ID).0
}

fn attest_ix(payer: &Pubkey, args: &VerdictArgs) -> Instruction {
    Instruction {
        program_id: backstop::ID,
        accounts: backstop::accounts::AttestVerdict {
            payer: *payer,
            config: Pubkey::find_program_address(&[CONFIG_SEED], &backstop::ID).0,
            attestation: attestation_pda(&args.liquidation_record),
            instructions_sysvar: IX_SYSVAR,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: backstop::instruction::AttestVerdict { args: args.clone() }.data(),
    }
}

/// One-signature Ed25519 precompile instruction. `indexes` = [signature, pubkey, message] instruction
/// indexes; for a self-contained instruction they equal its own position in the transaction.
fn ed25519_ix(signer: &Keypair, message: &[u8], indexes: [u16; 3]) -> Instruction {
    let signature = signer.sign_message(message);
    let (sig_off, pk_off, msg_off) = (16u16, 80u16, 112u16);
    let mut data = vec![1u8, 0u8];
    for v in [
        sig_off,
        indexes[0],
        pk_off,
        indexes[1],
        msg_off,
        message.len() as u16,
        indexes[2],
    ] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    data.extend_from_slice(signature.as_ref());
    data.extend_from_slice(signer.pubkey().as_ref());
    data.extend_from_slice(message);
    Instruction {
        program_id: ED25519,
        accounts: vec![],
        data,
    }
}

fn oracle_message(args: &VerdictArgs) -> Vec<u8> {
    encode_message(&backstop::ID, CLUSTER_DEVNET, args)
}

fn send(env: &mut Env, ixs: &[Instruction]) -> Result<(), String> {
    let msg =
        Message::new_with_blockhash(ixs, Some(&env.payer.pubkey()), &env.svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[&env.payer]).unwrap();
    env.svm
        .send_transaction(tx)
        .map(|_| ())
        .map_err(|e| format!("{:?} | logs: {:?}", e.err, e.meta.logs))
}

fn assert_program_error(result: Result<(), String>, error: VerdictError) {
    let code = anchor_lang::error::ERROR_CODE_OFFSET + error as u32;
    let err = result.expect_err("transaction should have failed");
    assert!(
        err.contains(&format!("Custom({code})")),
        "expected Custom({code}), got: {err}"
    );
}

fn assert_no_attestation(env: &Env, args: &VerdictArgs) {
    let acc = env
        .svm
        .get_account(&attestation_pda(&args.liquidation_record));
    assert!(
        acc.is_none_or(|a| a.data.is_empty()),
        "attestation must not exist after a rejection"
    );
}

// ---------------------------------------------------------------- negative

#[test]
fn runtime_rejects_a_corrupted_signature() {
    // Proves the precompile is live: if this passed, every other signature test would be meaningless.
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let mut ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    ed.data[16] ^= 0xFF;
    let payer = env.payer.pubkey();
    let err =
        send(&mut env, &[ed, attest_ix(&payer, &a)]).expect_err("corrupted signature must fail");
    assert!(
        err.contains("InstructionError(0"),
        "precompile (ix 0) should reject: {err}"
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn missing_ed25519_instruction_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    assert_program_error(
        send(&mut env, &[attest_ix(&payer, &a)]),
        VerdictError::MissingEd25519Instruction,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn ed25519_not_immediately_before_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    let mut cu = vec![2u8];
    cu.extend_from_slice(&200_000u32.to_le_bytes());
    let budget = Instruction {
        program_id: COMPUTE_BUDGET,
        accounts: vec![],
        data: cu,
    };
    assert_program_error(
        send(&mut env, &[ed, budget, attest_ix(&payer, &a)]),
        VerdictError::MissingEd25519Instruction,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn verdict_signed_by_another_key_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let impostor = Keypair::new();
    let ed = ed25519_ix(&impostor, &oracle_message(&a), [0, 0, 0]);
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &a)]),
        VerdictError::WrongVerdictSigner,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn raised_payout_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let signed = args();
    let mut submitted = signed.clone();
    submitted.payout += 1;
    let payer = env.payer.pubkey();
    let ed = ed25519_ix(&env.oracle, &oracle_message(&signed), [0, 0, 0]);
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &submitted)]),
        VerdictError::VerdictMessageMismatch,
    );
    assert_no_attestation(&env, &submitted);
}

#[test]
fn signature_for_another_program_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let foreign = encode_message(&Pubkey::new_unique(), CLUSTER_DEVNET, &a);
    let ed = ed25519_ix(&env.oracle, &foreign, [0, 0, 0]);
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &a)]),
        VerdictError::VerdictMessageMismatch,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn signature_for_another_cluster_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let mainnet = encode_message(&backstop::ID, CLUSTER_MAINNET, &a);
    let ed = ed25519_ix(&env.oracle, &mainnet, [0, 0, 0]);
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &a)]),
        VerdictError::VerdictMessageMismatch,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn expired_verdict_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let mut a = args();
    a.deadline = NOW - 1;
    let payer = env.payer.pubkey();
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &a)]),
        VerdictError::VerdictExpired,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn deadline_beyond_ttl_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let mut a = args();
    a.deadline = NOW + MAX_VERDICT_TTL_SECS + 1;
    let payer = env.payer.pubkey();
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &a)]),
        VerdictError::VerdictDeadlineTooFar,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn offsets_pointing_into_another_instruction_rejected() {
    // ix0 holds a genuinely valid oracle signature over the right message. ix1 is a second precompile
    // instruction that the runtime also accepts, because its offsets read ix0's bytes. Our check must refuse
    // ix1 because its data does not live inside ix1.
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let message = oracle_message(&a);
    let holder = ed25519_ix(&env.oracle, &message, [0, 0, 0]);
    let mut data = vec![1u8, 0u8];
    for v in [16u16, 0, 80, 0, 112, message.len() as u16, 0] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let borrower = Instruction {
        program_id: ED25519,
        accounts: vec![],
        data,
    };
    assert_program_error(
        send(&mut env, &[holder, borrower, attest_ix(&payer, &a)]),
        VerdictError::OffsetsOutsideEd25519Instruction,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn two_signatures_in_one_instruction_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let message = oracle_message(&a);
    let sig = env.oracle.sign_message(&message);
    let len = message.len() as u16;
    let (s1, p1, m1) = (30u16, 94u16, 126u16);
    let (s2, p2, m2) = (m1 + len, m1 + len + 64, m1 + len + 96);
    let mut data = vec![2u8, 0u8];
    for v in [s1, 0, p1, 0, m1, len, 0, s2, 0, p2, 0, m2, len, 0] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    for _ in 0..2 {
        data.extend_from_slice(sig.as_ref());
        data.extend_from_slice(env.oracle.pubkey().as_ref());
        data.extend_from_slice(&message);
    }
    let ed = Instruction {
        program_id: ED25519,
        accounts: vec![],
        data,
    };
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &a)]),
        VerdictError::WrongSignatureCount,
    );
    assert_no_attestation(&env, &a);
}

#[test]
fn zero_payout_and_bad_tier_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let payer = env.payer.pubkey();

    let mut zero = args();
    zero.payout = 0;
    let ed = ed25519_ix(&env.oracle, &oracle_message(&zero), [0, 0, 0]);
    assert_program_error(
        send(&mut env, &[ed, attest_ix(&payer, &zero)]),
        VerdictError::ZeroPayout,
    );

    for tier in [0u8, 4, 255] {
        let mut t = args();
        t.tier = tier;
        let ed = ed25519_ix(&env.oracle, &oracle_message(&t), [0, 0, 0]);
        assert_program_error(
            send(&mut env, &[ed, attest_ix(&payer, &t)]),
            VerdictError::InvalidTier,
        );
        assert_no_attestation(&env, &t);
    }
}

#[test]
fn second_attestation_for_same_liquidation_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    send(&mut env, &[ed.clone(), attest_ix(&payer, &a)]).expect("first attestation succeeds");

    env.svm.expire_blockhash();
    let mut again = a.clone();
    again.payout = a.payout; // identical verdict, fresh transaction
    let second = send(&mut env, &[ed, attest_ix(&payer, &again)]);
    assert!(second.is_err(), "replay must fail");

    let acc = env
        .svm
        .get_account(&attestation_pda(&a.liquidation_record))
        .unwrap();
    let stored = VerdictAttestation::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert_eq!(stored.payout, a.payout);
}

// ---------------------------------------------------------------- positive

#[test]
fn valid_verdict_is_attested() {
    let mut env = setup(CLUSTER_DEVNET);
    let a = args();
    let payer = env.payer.pubkey();
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    send(&mut env, &[ed, attest_ix(&payer, &a)]).expect("valid verdict must be attested");

    let acc = env
        .svm
        .get_account(&attestation_pda(&a.liquidation_record))
        .unwrap();
    assert_eq!(acc.owner, backstop::ID);
    let stored = VerdictAttestation::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert_eq!(stored.version, ACCOUNT_VERSION);
    assert_eq!(stored.liquidation_record, a.liquidation_record);
    assert_eq!(stored.borrower, a.borrower);
    assert_eq!(stored.payout, a.payout);
    assert_eq!(stored.tier, a.tier);
    assert_eq!(stored.verdict_hash, a.verdict_hash);
    assert_eq!(stored.attested_at, NOW);
}
