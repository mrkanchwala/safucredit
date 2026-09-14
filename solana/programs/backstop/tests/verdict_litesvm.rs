// Stage 2 phase 2: v2 facts verification through the real program, on LiteSVM with the Ed25519
// precompile enabled (litesvm feature "precompiles"; without it signatures are never checked).
//
// This file stays isolated from the vault program on purpose (state set directly, as in v1): it
// tests only the Ed25519/structural layer of `submit_facts`, not a real cross-program liquidation.
// The full cross-program flow — self-dealing, issuer halt, borrower-is-backer, the per-claim cap,
// gate/admission/stream/outflow/expiry — lives in `payout_litesvm.rs` against a real `with_loan()`
// liquidation, because those checks read fields only a genuine `LiquidationRecord` can be trusted
// to carry.
//
// Negative cases first, one happy path last. Every rejection asserts the exact program error, so a
// test cannot pass because the transaction failed for some unrelated reason.

use anchor_lang::{
    prelude::{pubkey, Clock, Pubkey},
    solana_program::instruction::Instruction,
    AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas,
};
use backstop::{
    errors::VerdictError,
    state::{
        BackstopConfig, BorrowerClaims, Claim, ClaimStatus, ACCOUNT_VERSION, BORROWER_CLAIMS_SEED,
        CLAIM_SEED, CONFIG_SEED, REVOKE_SEED,
    },
    verdict::{encode_message, FactsArgs, CLUSTER_DEVNET, CLUSTER_MAINNET, MAX_VERDICT_TTL_SECS},
};
use litesvm::LiteSVM;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use stock_vault::state::{LiquidationRecord, Market, MarketParams, PriceState, RateParams};

const NOW: i64 = 1_800_000_000;
const ED25519: Pubkey = pubkey!("Ed25519SigVerify111111111111111111111111111");
const COMPUTE_BUDGET: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");
const IX_SYSVAR: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");
const SYSTEM: Pubkey = pubkey!("11111111111111111111111111111111");
const USDC: u64 = 1_000_000;
const CAP_BPS: u32 = 500;
// The record's own liquidation price is far off both references — the scenario every happy-path
// and structural test shares: a feed printed ~$100 while the true price was ~$400, before and after.
const RECORD_PRICE: u64 = 10_000_000_000;
const REF_AT_LIQ: u64 = 40_000_000_000;
const REF_AFTER: u64 = 40_100_000_000;

struct Env {
    svm: LiteSVM,
    payer: Keypair,
    oracle: Keypair,
    market: Pubkey,
    borrower: Pubkey,
}

fn set_state<T: AccountSerialize>(svm: &mut LiteSVM, key: Pubkey, owner: Pubkey, state: &T) {
    let mut data = Vec::new();
    state.try_serialize(&mut data).unwrap();
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        key,
        Account {
            lamports,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn market_params() -> MarketParams {
    MarketParams {
        ltv_bps: 4_000,
        liq_threshold_bps: 5_000,
        min_liq_bonus_bps: 100,
        max_liq_bonus_bps: 500,
        insolvency_ltv_bps: 9_500,
        close_factor_bps: 2_500,
        max_liquidation_debt: 100_000 * USDC,
        deviation_cap_bps: CAP_BPS,
        activation_pause_secs: 900,
        split_cap_bps: 500,
        split_max_hold_secs: 86_400,
        borrow_max_price_age_secs: 3_600,
        liquidation_max_price_age_open_secs: 90_000,
        max_price_age_closed_secs: 345_600,
        rate: RateParams {
            base_bps: 100,
            slope1_bps: 500,
            slope2_bps: 6_000,
            kink_bps: 9_000,
        },
        borrow_cap: 500_000 * USDC,
        collateral_cap_raw: 2_000 * 100_000_000,
        backer_interest_share_bps: 1_500,
    }
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
    let borrower = Pubkey::new_unique();

    // State set directly (skill guidance: set up state, don't replay long setup transactions). The
    // initialize instruction and its upgrade-authority gate are tested in payout_litesvm.rs.
    let (config_pda, bump) = Pubkey::find_program_address(&[CONFIG_SEED], &backstop::ID);
    let config = BackstopConfig {
        version: ACCOUNT_VERSION,
        admin: payer.pubkey(),
        verdict_oracle: oracle.pubkey(),
        co_signer: Pubkey::new_unique(),
        cluster_tag,
        bump,
        // Token fields are unused by these tests: nothing here moves USDC. `cash` is set generous
        // so the admission cap never binds — that is exercised separately in payout_litesvm.rs.
        usdc_mint: Pubkey::new_unique(),
        cash: 1_000_000 * USDC,
        total_shares: (1_000_000 * USDC) as u128,
        per_claim_cap_bps: 1_000,
        reserved_total: 0,
        admission_day: 0,
        admitted_today: 0,
        withdraw_delay_secs: 0,
        inventory_cost_total: 0,
        per_liq_cap_bps: 1_000,
        daily_liq_cap_bps: 2_500,
        liq_day: 0,
        liq_spent_today: 0,
        resale_discount_bps: 200,
        resale_floor_secs: 4 * 86_400,
        fee_share_bps: 1_000,
        min_pool_repay: 10 * USDC,
        gate_secs: 60 * 86_400,
        cooldown_secs: 7 * 86_400,
        stream_secs: 45 * 86_400,
        inactivity_secs: 100 * 86_400,
        min_after_wait_secs: 3_600,
        reserved: [0; 16],
    };
    set_state(&mut svm, config_pda, backstop::ID, &config);

    let market_key = Pubkey::new_unique();
    let market = Market {
        version: 1,
        bump: 0,
        collateral_mint: Pubkey::new_unique(),
        usdc_mint: Pubkey::new_unique(),
        collateral_decimals: 8,
        usdc_decimals: 6,
        params: market_params(),
        price: PriceState::default(),
        cash: 0,
        total_supply_shares: 0,
        total_borrow_shares: 0,
        borrow_index: safu_core::MULT_SCALE,
        last_accrual_ts: NOW,
        total_collateral_raw: 0,
        bad_debt: 0,
        bad_debt_cumulative: 0,
        issuer_halt: false,
        liq_seq: 0,
        observed_multiplier_fp: safu_core::MULT_SCALE,
        ramp_from: market_params().liquidation_terms(),
        ramp_start_ts: 0,
        backer_interest_owed: 0,
        backer_interest_cumulative: 0,
        backer_interest_paid_cumulative: 0,
        issuer_loss_cumulative: 0,
        reserved: [0; 4],
    };
    set_state(&mut svm, market_key, stock_vault::ID, &market);

    let (bc_pda, bc_bump) = Pubkey::find_program_address(
        &[BORROWER_CLAIMS_SEED, market_key.as_ref(), borrower.as_ref()],
        &backstop::ID,
    );
    let bc = BorrowerClaims {
        version: ACCOUNT_VERSION,
        bump: bc_bump,
        market: market_key,
        borrower,
        open: Pubkey::default(),
        penalty_since: 0,
        penalty_until: 0,
        reserved: [0; 32],
    };
    set_state(&mut svm, bc_pda, backstop::ID, &bc);

    Env {
        svm,
        payer,
        oracle,
        market: market_key,
        borrower,
    }
}

/// Loan old enough to skip the 60-day gate, liquidated with a price far from both references, for a
/// clean `Active` admission on the happy path (and a clean structural-rejection surface otherwise).
fn record(env: &Env, seq: u64) -> LiquidationRecord {
    LiquidationRecord {
        version: 1,
        bump: 0,
        market: env.market,
        seq,
        borrower: env.borrower,
        liquidator: Pubkey::new_unique(),
        seized_raw: 100_000_000,
        debt_repaid: 250 * USDC,
        multiplier_fp: safu_core::MULT_SCALE,
        price_fp: RECORD_PRICE,
        collateral_decimals: 8,
        bonus_bps: 300,
        ltv_bps: 9_000,
        coverage_bps: 10_000,
        borrow_age_ts: NOW - 90 * 86_400,
        payout: env.borrower,
        bad_debt: 0,
        ts: NOW,
        slot: 0,
        issuer_halt: false,
        reserved: [0; 32],
    }
}

/// Deterministic stand-in for a vault-issued `LiquidationRecord` address — fabricated directly (no
/// vault program loaded), so any unique key stands in for it. Tests key off the
/// `args.liquidation_record` field matching the account passed, never off the address shape.
fn record_pda(seq: u64) -> Pubkey {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seq.to_le_bytes());
    bytes[8] = 0xAA;
    Pubkey::new_from_array(bytes)
}

fn args(record_key: Pubkey, borrower: Pubkey) -> FactsArgs {
    FactsArgs {
        liquidation_record: record_key,
        borrower,
        ref_at_liq: REF_AT_LIQ,
        ref_after: REF_AFTER,
        after_ts: NOW + 3_600,
        evidence_hash: [9; 32],
        deadline: NOW + 600,
    }
}

fn claim_pda(record_key: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[CLAIM_SEED, record_key.as_ref()], &backstop::ID).0
}

fn submit_ix(env: &Env, record_key: Pubkey, a: &FactsArgs) -> Instruction {
    let (bc_pda, _) = Pubkey::find_program_address(
        &[
            BORROWER_CLAIMS_SEED,
            env.market.as_ref(),
            env.borrower.as_ref(),
        ],
        &backstop::ID,
    );
    Instruction {
        program_id: backstop::ID,
        accounts: backstop::accounts::SubmitFacts {
            payer: env.payer.pubkey(),
            config: Pubkey::find_program_address(&[CONFIG_SEED], &backstop::ID).0,
            market: env.market,
            liquidation_record: record_key,
            borrower: env.borrower,
            borrower_claims: bc_pda,
            // No claim occupies the slot yet in every one of these tests, so any real account is a
            // harmless placeholder — it is never read on that branch.
            existing_claim: env.payer.pubkey(),
            claim: claim_pda(&record_key),
            payout_backer: Pubkey::find_program_address(
                &[backstop::state::BACKER_SEED, env.borrower.as_ref()],
                &backstop::ID,
            )
            .0,
            revoked: Pubkey::find_program_address(
                &[REVOKE_SEED, record_key.as_ref(), a.evidence_hash.as_ref()],
                &backstop::ID,
            )
            .0,
            instructions_sysvar: IX_SYSVAR,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: backstop::instruction::SubmitFacts { args: a.clone() }.data(),
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

fn oracle_message(a: &FactsArgs) -> Vec<u8> {
    encode_message(&backstop::ID, CLUSTER_DEVNET, a)
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

fn assert_no_claim(env: &Env, record_key: &Pubkey) {
    let acc = env.svm.get_account(&claim_pda(record_key));
    assert!(
        acc.is_none_or(|a| a.data.is_empty()),
        "claim must not exist after a structural rejection"
    );
}

// ---------------------------------------------------------------- negative (structural)

#[test]
fn runtime_rejects_a_corrupted_signature() {
    // Proves the precompile is live: if this passed, every other signature test would be meaningless.
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let mut ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    ed.data[16] ^= 0xFF;
    let ix = submit_ix(&env, record_pda(0), &a);
    let err = send(&mut env, &[ed, ix]).expect_err("corrupted signature must fail");
    assert!(
        err.contains("InstructionError(0"),
        "precompile (ix 0) should reject: {err}"
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn missing_ed25519_instruction_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(
        send(&mut env, &[ix]),
        VerdictError::MissingEd25519Instruction,
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn ed25519_not_immediately_before_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    let mut cu = vec![2u8];
    cu.extend_from_slice(&200_000u32.to_le_bytes());
    let budget = Instruction {
        program_id: COMPUTE_BUDGET,
        accounts: vec![],
        data: cu,
    };
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(
        send(&mut env, &[ed, budget, ix]),
        VerdictError::MissingEd25519Instruction,
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn facts_signed_by_another_key_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let impostor = Keypair::new();
    let ed = ed25519_ix(&impostor, &oracle_message(&a), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(send(&mut env, &[ed, ix]), VerdictError::WrongVerdictSigner);
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn an_altered_reference_price_after_signing_is_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let signed = args(record_pda(0), env.borrower);
    let mut submitted = signed.clone();
    submitted.ref_at_liq += 1;
    let ed = ed25519_ix(&env.oracle, &oracle_message(&signed), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &submitted);
    assert_program_error(
        send(&mut env, &[ed, ix]),
        VerdictError::VerdictMessageMismatch,
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn signature_for_another_program_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let foreign = encode_message(&Pubkey::new_unique(), CLUSTER_DEVNET, &a);
    let ed = ed25519_ix(&env.oracle, &foreign, [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(
        send(&mut env, &[ed, ix]),
        VerdictError::VerdictMessageMismatch,
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn signature_for_another_cluster_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let mainnet = encode_message(&backstop::ID, CLUSTER_MAINNET, &a);
    let ed = ed25519_ix(&env.oracle, &mainnet, [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(
        send(&mut env, &[ed, ix]),
        VerdictError::VerdictMessageMismatch,
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn expired_facts_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let mut a = args(record_pda(0), env.borrower);
    a.deadline = NOW - 1;
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(send(&mut env, &[ed, ix]), VerdictError::VerdictExpired);
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn deadline_beyond_ttl_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let mut a = args(record_pda(0), env.borrower);
    a.deadline = NOW + MAX_VERDICT_TTL_SECS + 1;
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(
        send(&mut env, &[ed, ix]),
        VerdictError::VerdictDeadlineTooFar,
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn after_ts_outside_the_wait_window_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    // Too soon: the move can't be judged to have "held" or not after only a minute.
    let mut too_soon = args(record_pda(0), env.borrower);
    too_soon.after_ts = NOW + 60;
    let ed = ed25519_ix(&env.oracle, &oracle_message(&too_soon), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &too_soon);
    assert_program_error(send(&mut env, &[ed, ix]), VerdictError::InvalidAfterWindow);
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn offsets_pointing_into_another_instruction_rejected() {
    // ix0 holds a genuinely valid oracle signature over the right message. ix1 is a second precompile
    // instruction that the runtime also accepts, because its offsets read ix0's bytes. Our check must refuse
    // ix1 because its data does not live inside ix1.
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let message = oracle_message(&a);
    let holder = ed25519_ix(&env.oracle, &message, [0, 0, 0]);
    let mut data = vec![1u8, 0u8];
    for v in [16u16, 0, 80, 0, 112, message.len() as u16, 0] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let borrower_ix = Instruction {
        program_id: ED25519,
        accounts: vec![],
        data,
    };
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(
        send(&mut env, &[holder, borrower_ix, ix]),
        VerdictError::OffsetsOutsideEd25519Instruction,
    );
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn two_signatures_in_one_instruction_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
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
    let ix = submit_ix(&env, record_pda(0), &a);
    assert_program_error(send(&mut env, &[ed, ix]), VerdictError::WrongSignatureCount);
    assert_no_claim(&env, &record_pda(0));
}

#[test]
fn second_submission_for_same_liquidation_rejected() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    send(&mut env, &[ed.clone(), ix.clone()]).expect("first submission succeeds");

    env.svm.expire_blockhash();
    // `existing_claim` slot is now occupied by a terminal-or-not claim — either way `init` on the
    // claim PDA itself is what actually blocks the replay, before the slot logic is even reached.
    let second = send(&mut env, &[ed, ix]);
    assert!(second.is_err(), "replay must fail");

    let acc = env.svm.get_account(&claim_pda(&record_pda(0))).unwrap();
    let stored = Claim::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert_eq!(stored.liquidation_record, record_pda(0));
}

// ---------------------------------------------------------------- positive

#[test]
fn valid_facts_admit_the_claim() {
    let mut env = setup(CLUSTER_DEVNET);
    let r = record(&env, 0);
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    send(&mut env, &[ed, ix]).expect("valid facts must be recorded");

    let acc = env.svm.get_account(&claim_pda(&record_pda(0))).unwrap();
    assert_eq!(acc.owner, backstop::ID);
    let stored = Claim::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert_eq!(stored.version, ACCOUNT_VERSION);
    assert_eq!(stored.liquidation_record, record_pda(0));
    assert_eq!(stored.borrower, env.borrower);
    assert_eq!(stored.payout, env.borrower);
    assert_eq!(
        stored.status,
        ClaimStatus::Active,
        "loan is old enough to skip the gate"
    );
    assert_eq!(stored.deny_reason, backstop::state::deny_reason::NONE);
    assert_eq!(
        stored.loss,
        150 * USDC,
        "1 share @ $400 ref minus $250 debt repaid"
    );
    assert_eq!(stored.submitted_at, NOW);
    assert_eq!(stored.evidence_hash, a.evidence_hash);

    let bc_acc = env
        .svm
        .get_account(
            &Pubkey::find_program_address(
                &[
                    BORROWER_CLAIMS_SEED,
                    env.market.as_ref(),
                    env.borrower.as_ref(),
                ],
                &backstop::ID,
            )
            .0,
        )
        .unwrap();
    let bc = BorrowerClaims::try_deserialize(&mut bc_acc.data.as_slice()).unwrap();
    assert_eq!(bc.open, claim_pda(&record_pda(0)));
}

#[test]
fn a_price_that_was_not_actually_wrong_is_denied_on_chain() {
    let mut env = setup(CLUSTER_DEVNET);
    let mut r = record(&env, 0);
    // The liquidation price matches the reference: nothing to claim.
    r.price_fp = REF_AT_LIQ;
    set_state(&mut env.svm, record_pda(0), stock_vault::ID, &r);
    let a = args(record_pda(0), env.borrower);
    let ed = ed25519_ix(&env.oracle, &oracle_message(&a), [0, 0, 0]);
    let ix = submit_ix(&env, record_pda(0), &a);
    send(&mut env, &[ed, ix]).expect("a denial is a successful, on-chain-recorded transaction");

    let acc = env.svm.get_account(&claim_pda(&record_pda(0))).unwrap();
    let stored = Claim::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert_eq!(stored.status, ClaimStatus::Denied);
    assert_eq!(
        stored.deny_reason,
        backstop::state::deny_reason::PRICE_NOT_WRONG
    );
    assert_eq!(stored.loss, 0);
}
