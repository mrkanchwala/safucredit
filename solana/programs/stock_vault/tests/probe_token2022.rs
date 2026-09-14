// T1 probe (eng review 2026-09-14): does LiteSVM's bundled Token-2022 support the two extensions the
// vault depends on — Scaled UI Amount (xStocks multiplier schedule) and Pausable (issuer pause)?
// If this fails, the fallback is loading a current spl_token_2022.so dumped from devnet.

use anchor_lang::prelude::Pubkey;
use litesvm::LiteSVM;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use spl_token_2022_interface::{
    extension::{
        pausable::{self, PausableConfig},
        scaled_ui_amount::{self, ScaledUiAmountConfig},
        BaseStateWithExtensions, ExtensionType, StateWithExtensions,
    },
    instruction::initialize_mint2,
    state::Mint,
    ID as TOKEN_2022,
};

fn send(svm: &mut LiteSVM, ixs: &[anchor_lang::solana_program::instruction::Instruction], signers: &[&Keypair]) {
    let msg = Message::new_with_blockhash(ixs, Some(&signers[0].pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
    if let Err(e) = svm.send_transaction(tx) {
        panic!("tx failed: {:?}\nlogs: {:#?}", e.err, e.meta.logs);
    }
}

fn read_config(svm: &LiteSVM, mint: &Pubkey) -> (f64, i64, f64, bool) {
    let acc = svm.get_account(mint).expect("mint exists");
    let state = StateWithExtensions::<Mint>::unpack(&acc.data).expect("unpack mint");
    let s = state.get_extension::<ScaledUiAmountConfig>().expect("scaled ui amount ext");
    let p = state.get_extension::<PausableConfig>().expect("pausable ext");
    (
        f64::from_le_bytes(s.multiplier.0),
        i64::from(s.new_multiplier_effective_timestamp),
        f64::from_le_bytes(s.new_multiplier.0),
        bool::from(p.paused),
    )
}

#[test]
fn litesvm_token2022_supports_scaled_ui_amount_and_pausable() {
    let mut svm = LiteSVM::new();
    let issuer = Keypair::new();
    let mint = Keypair::new();
    svm.airdrop(&issuer.pubkey(), 10_000_000_000).unwrap();

    let space = ExtensionType::try_calculate_account_len::<Mint>(&[
        ExtensionType::ScaledUiAmount,
        ExtensionType::Pausable,
    ])
    .unwrap();
    let rent = svm.minimum_balance_for_rent_exemption(space);

    // Live AAPLx carried multiplier 1.0026642 (read 2026-09-13); use it as the starting value.
    send(
        &mut svm,
        &[
            solana_system_interface::instruction::create_account(
                &issuer.pubkey(),
                &mint.pubkey(),
                rent,
                space as u64,
                &TOKEN_2022,
            ),
            scaled_ui_amount::instruction::initialize(&TOKEN_2022, &mint.pubkey(), Some(issuer.pubkey()), 1.0026642)
                .unwrap(),
            pausable::instruction::initialize(&TOKEN_2022, &mint.pubkey(), &issuer.pubkey()).unwrap(),
            initialize_mint2(&TOKEN_2022, &mint.pubkey(), &issuer.pubkey(), Some(&issuer.pubkey()), 8).unwrap(),
        ],
        &[&issuer, &mint],
    );

    let (m, _, _, paused) = read_config(&svm, &mint.pubkey());
    assert_eq!(m, 1.0026642);
    assert!(!paused);

    // Schedule a 4-for-1 split far in the future: stored multiplier must stay, new one must be queued.
    let split_ts: i64 = 4_102_444_800; // 2100-01-01
    send(
        &mut svm,
        &[scaled_ui_amount::instruction::update_multiplier(
            &TOKEN_2022,
            &mint.pubkey(),
            &issuer.pubkey(),
            &[],
            4.0106568,
            split_ts,
        )
        .unwrap()],
        &[&issuer],
    );
    let (m, ts, new_m, _) = read_config(&svm, &mint.pubkey());
    assert_eq!(m, 1.0026642, "current multiplier unchanged before the effective timestamp");
    assert_eq!(ts, split_ts);
    assert_eq!(new_m, 4.0106568);

    // Issuer pause must be visible to a reader.
    send(
        &mut svm,
        &[pausable::instruction::pause(&TOKEN_2022, &mint.pubkey(), &issuer.pubkey(), &[]).unwrap()],
        &[&issuer],
    );
    let (_, _, _, paused) = read_config(&svm, &mint.pubkey());
    assert!(paused, "pause flag readable after pause");
}
