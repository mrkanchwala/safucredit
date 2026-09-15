//! Stage 2 item 2a: `stock_vault` core, exercised through the real compiled program on LiteSVM.
//!
//! Every rejection asserts the exact program error, so a test cannot pass because the transaction
//! failed for some unrelated reason. State that a real deployment would build up over time (prices,
//! debt) is built through real instructions, because the accounting under test *is* that build-up;
//! only `VaultConfig` is ever written directly.

use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_lang::{
    prelude::{Clock, Pubkey},
    solana_program::instruction::Instruction,
    solana_program::program_pack::Pack,
    AccountDeserialize, InstructionData, ToAccountMetas,
};
use litesvm::LiteSVM;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use spl_token_2022_interface::{
    extension::{pausable, scaled_ui_amount, transfer_fee, BaseStateWithExtensions, ExtensionType},
    state::{Account as T22Account, Mint as T22Mint},
    ID as TOKEN_2022,
};
use stock_vault::state::{DEFAULT_FALLBACK_GRACE_SECS, LIQUIDATION_TERMS_RAMP_SECS};
use stock_vault::{
    errors::VaultError,
    state::{
        LiquidationRecord, Market, MarketParams, Position, RateParams, Supplier, VaultConfig,
        COLL_VAULT_SEED, LIQ_RECORD_SEED, MARKET_SEED, POSITION_SEED, SUPPLIER_SEED,
        USDC_VAULT_SEED, VCONFIG_SEED,
    },
};

const NOW: i64 = 1_800_000_000;
const COLL_DECIMALS: u8 = 8;
const USDC_DECIMALS: u8 = 6;
/// One whole xStock share in raw base units.
const ONE_SHARE: u64 = 100_000_000;
/// AAPLx as read on mainnet 2026-09-13: $330.28, 8-decimal price.
const PRICE: u64 = 33_028_000_000;
/// The live AAPLx stored multiplier on the same read.
const MULT: f64 = 1.0026642;
const USDC: u64 = 1_000_000;

const TOKEN_CLASSIC: Pubkey = spl_token_interface::ID;
const SYSTEM: Pubkey = solana_system_interface::program::ID;

/// Market config approved 2026-09-14 (`outputs/2026-09-14_stocklana-market-config-research.md` §2b).
fn params() -> MarketParams {
    MarketParams {
        ltv_bps: 4_000,
        liq_threshold_bps: 5_000,
        min_liq_bonus_bps: 100,
        max_liq_bonus_bps: 500,
        insolvency_ltv_bps: 9_500,
        close_factor_bps: 2_500,
        max_liquidation_debt: 100_000 * USDC,
        deviation_cap_bps: 500,
        activation_pause_secs: 900,
        split_cap_bps: 500,
        split_max_hold_secs: 86_400,
        borrow_max_price_age_secs: 3_600,
        liquidation_max_price_age_open_secs: 90_000,
        max_price_age_closed_secs: 345_600,
        // Kink 90%, 6.0% at the kink (~5.8% at 87% utilization).
        rate: RateParams {
            base_bps: 100,
            slope1_bps: 500,
            slope2_bps: 6_000,
            kink_bps: 9_000,
        },
        borrow_cap: 500_000 * USDC,
        collateral_cap_raw: 2_000 * ONE_SHARE,
        backer_interest_share_bps: 1_500,
    }
}

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    feed: Keypair,
    issuer: Keypair,
    /// Borrower.
    alice: Keypair,
    /// USDC supplier.
    bob: Keypair,
    coll_mint: Pubkey,
    usdc_mint: Pubkey,
    alice_coll: Pubkey,
    alice_usdc: Pubkey,
    bob_usdc: Pubkey,
    now: i64,
}

// ------------------------------------------------------------------ plumbing

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &stock_vault::ID).0
}

fn market_pda(coll_mint: &Pubkey) -> Pubkey {
    pda(&[MARKET_SEED, coll_mint.as_ref()])
}

impl Env {
    fn config(&self) -> Pubkey {
        pda(&[VCONFIG_SEED])
    }
    fn market(&self) -> Pubkey {
        market_pda(&self.coll_mint)
    }
    fn coll_vault(&self) -> Pubkey {
        pda(&[COLL_VAULT_SEED, self.market().as_ref()])
    }
    fn usdc_vault(&self) -> Pubkey {
        pda(&[USDC_VAULT_SEED, self.market().as_ref()])
    }
    fn position(&self, owner: &Pubkey) -> Pubkey {
        pda(&[POSITION_SEED, self.market().as_ref(), owner.as_ref()])
    }
    fn supplier(&self, owner: &Pubkey) -> Pubkey {
        pda(&[SUPPLIER_SEED, self.market().as_ref(), owner.as_ref()])
    }

    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
        let msg = Message::new_with_blockhash(
            ixs,
            Some(&signers[0].pubkey()),
            &self.svm.latest_blockhash(),
        );
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            .map_err(|e| format!("{:?} | logs: {:?}", e.err, e.meta.logs))
    }

    fn ok(&mut self, ixs: &[Instruction], signers: &[&Keypair]) {
        if let Err(e) = self.send(ixs, signers) {
            panic!("transaction should have succeeded: {e}");
        }
    }

    /// Moves the clock forward. Every instruction reads `Clock::get()`, so this is how time passes.
    fn warp(&mut self, secs: i64) {
        self.now += secs;
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = self.now;
        self.svm.set_sysvar(&clock);
        self.svm.expire_blockhash();
    }

    fn market_state(&self) -> Market {
        let acc = self.svm.get_account(&self.market()).expect("market exists");
        Market::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn position_state(&self, owner: &Pubkey) -> Position {
        let acc = self
            .svm
            .get_account(&self.position(owner))
            .expect("position exists");
        Position::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn supplier_state(&self, owner: &Pubkey) -> Supplier {
        let acc = self
            .svm
            .get_account(&self.supplier(owner))
            .expect("supplier exists");
        Supplier::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn config_state(&self) -> VaultConfig {
        let acc = self.svm.get_account(&self.config()).expect("config exists");
        VaultConfig::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn token_balance(&self, account: &Pubkey) -> u64 {
        let acc = self.svm.get_account(account).expect("token account exists");
        // Both token programs keep the base `Account` layout in the first 165 bytes.
        u64::from_le_bytes(acc.data[64..72].try_into().unwrap())
    }
}

fn assert_program_error(result: Result<(), String>, error: VaultError) {
    let code = anchor_lang::error::ERROR_CODE_OFFSET + error as u32;
    let err = result.expect_err("transaction should have failed");
    assert!(
        err.contains(&format!("Custom({code})")),
        "expected Custom({code}) ({error:?}), got: {err}"
    );
}

// ------------------------------------------------------------------ token setup

/// Token-2022 mint with the two extensions a real xStock carries, plus optionally a transfer fee
/// (which the vault must refuse).
fn create_t22_mint(
    svm: &mut LiteSVM,
    payer: &Keypair,
    issuer: &Keypair,
    decimals: u8,
    scaled_ui: bool,
    pausable: bool,
    transfer_fee_bps: Option<u16>,
) -> Keypair {
    let mint = Keypair::new();
    let mut exts = Vec::new();
    if transfer_fee_bps.is_some() {
        exts.push(ExtensionType::TransferFeeConfig);
    }
    if scaled_ui {
        exts.push(ExtensionType::ScaledUiAmount);
    }
    if pausable {
        exts.push(ExtensionType::Pausable);
    }
    let space = ExtensionType::try_calculate_account_len::<T22Mint>(&exts).unwrap();
    let rent = svm.minimum_balance_for_rent_exemption(space);

    let mut ixs = vec![solana_system_interface::instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent,
        space as u64,
        &TOKEN_2022,
    )];
    if let Some(bps) = transfer_fee_bps {
        ixs.push(
            transfer_fee::instruction::initialize_transfer_fee_config(
                &TOKEN_2022,
                &mint.pubkey(),
                Some(&issuer.pubkey()),
                Some(&issuer.pubkey()),
                bps,
                u64::MAX,
            )
            .unwrap(),
        );
    }
    if scaled_ui {
        ixs.push(
            scaled_ui_amount::instruction::initialize(
                &TOKEN_2022,
                &mint.pubkey(),
                Some(issuer.pubkey()),
                MULT,
            )
            .unwrap(),
        );
    }
    if pausable {
        ixs.push(
            pausable::instruction::initialize(&TOKEN_2022, &mint.pubkey(), &issuer.pubkey())
                .unwrap(),
        );
    }
    ixs.push(
        spl_token_2022_interface::instruction::initialize_mint2(
            &TOKEN_2022,
            &mint.pubkey(),
            &issuer.pubkey(),
            // Live AAPLx has a freeze authority; the mock carries one so the guard is testable.
            Some(&issuer.pubkey()),
            decimals,
        )
        .unwrap(),
    );

    let msg = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer, &mint]).unwrap();
    if let Err(e) = svm.send_transaction(tx) {
        panic!(
            "mint creation failed: {:?} | logs: {:?}",
            e.err, e.meta.logs
        );
    }
    mint
}

/// Classic SPL Token mint — what real USDC is.
fn create_classic_mint(
    svm: &mut LiteSVM,
    payer: &Keypair,
    authority: &Pubkey,
    decimals: u8,
) -> Keypair {
    let mint = Keypair::new();
    let space = spl_token_interface::state::Mint::LEN;
    let rent = svm.minimum_balance_for_rent_exemption(space);
    let ixs = [
        solana_system_interface::instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            rent,
            space as u64,
            &TOKEN_CLASSIC,
        ),
        spl_token_interface::instruction::initialize_mint2(
            &TOKEN_CLASSIC,
            &mint.pubkey(),
            authority,
            None,
            decimals,
        )
        .unwrap(),
    ];
    let msg = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer, &mint]).unwrap();
    svm.send_transaction(tx).expect("classic mint creation");
    mint
}

/// A holder's own token account. Token-2022 accounts for a pausable mint need the account-side
/// extension, so the length is computed rather than assumed.
fn create_token_account(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Pubkey,
    owner: &Pubkey,
    token_program: &Pubkey,
) -> Pubkey {
    let account = Keypair::new();
    let (space, init_ix) = if *token_program == TOKEN_2022 {
        let mint_acc = svm.get_account(mint).unwrap();
        let state = spl_token_2022_interface::extension::StateWithExtensions::<T22Mint>::unpack(
            &mint_acc.data,
        )
        .unwrap();
        let mint_exts = state.get_extension_types().unwrap();
        let exts = ExtensionType::get_required_init_account_extensions(&mint_exts);
        (
            ExtensionType::try_calculate_account_len::<T22Account>(&exts).unwrap(),
            spl_token_2022_interface::instruction::initialize_account3(
                &TOKEN_2022,
                &account.pubkey(),
                mint,
                owner,
            )
            .unwrap(),
        )
    } else {
        (
            spl_token_interface::state::Account::LEN,
            spl_token_interface::instruction::initialize_account3(
                &TOKEN_CLASSIC,
                &account.pubkey(),
                mint,
                owner,
            )
            .unwrap(),
        )
    };
    let rent = svm.minimum_balance_for_rent_exemption(space);
    let ixs = [
        solana_system_interface::instruction::create_account(
            &payer.pubkey(),
            &account.pubkey(),
            rent,
            space as u64,
            token_program,
        ),
        init_ix,
    ];
    let msg = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer, &account]).unwrap();
    if let Err(e) = svm.send_transaction(tx) {
        panic!(
            "token account creation failed: {:?} | logs: {:?}",
            e.err, e.meta.logs
        );
    }
    account.pubkey()
}

fn mint_to(
    svm: &mut LiteSVM,
    authority: &Keypair,
    mint: &Pubkey,
    to: &Pubkey,
    amount: u64,
    token_program: &Pubkey,
) {
    let ix = if *token_program == TOKEN_2022 {
        spl_token_2022_interface::instruction::mint_to(
            &TOKEN_2022,
            mint,
            to,
            &authority.pubkey(),
            &[],
            amount,
        )
        .unwrap()
    } else {
        spl_token_interface::instruction::mint_to(
            &TOKEN_CLASSIC,
            mint,
            to,
            &authority.pubkey(),
            &[],
            amount,
        )
        .unwrap()
    };
    let msg =
        Message::new_with_blockhash(&[ix], Some(&authority.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[authority]).unwrap();
    svm.send_transaction(tx).expect("mint_to");
}

// ------------------------------------------------------------------ instructions

fn init_config_ix(admin: &Pubkey, feed: &Pubkey) -> Instruction {
    init_config_ix_with(admin, feed, programdata(&stock_vault::ID))
}

fn init_config_ix_with(admin: &Pubkey, feed: &Pubkey, program_data: Pubkey) -> Instruction {
    init_config_ix_cluster(
        admin,
        feed,
        program_data,
        stock_vault::state::CLUSTER_DEVNET,
    )
}

fn init_config_ix_cluster(
    admin: &Pubkey,
    feed: &Pubkey,
    program_data: Pubkey,
    cluster_tag: u8,
) -> Instruction {
    Instruction {
        program_id: stock_vault::ID,
        accounts: stock_vault::accounts::InitializeConfig {
            admin: *admin,
            program: stock_vault::ID,
            program_data,
            config: pda(&[VCONFIG_SEED]),
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: stock_vault::instruction::InitializeConfig {
            feed_authority: *feed,
            cluster_tag,
        }
        .data(),
    }
}

fn create_market_ix(
    admin: &Pubkey,
    coll_mint: &Pubkey,
    usdc_mint: &Pubkey,
    coll_program: &Pubkey,
    usdc_program: &Pubkey,
    params: MarketParams,
) -> Instruction {
    let market = market_pda(coll_mint);
    Instruction {
        program_id: stock_vault::ID,
        accounts: stock_vault::accounts::CreateMarket {
            admin: *admin,
            config: pda(&[VCONFIG_SEED]),
            collateral_mint: *coll_mint,
            usdc_mint: *usdc_mint,
            market,
            collateral_vault: pda(&[COLL_VAULT_SEED, market.as_ref()]),
            usdc_vault: pda(&[USDC_VAULT_SEED, market.as_ref()]),
            collateral_token_program: *coll_program,
            usdc_token_program: *usdc_program,
            system_program: SYSTEM,
        }
        .to_account_metas(None),
        data: stock_vault::instruction::CreateMarket { params }.data(),
    }
}

// ------------------------------------------------------------------ environment

/// SVM with the program loaded (upgrade authority = admin) and both mints created. Config NOT initialized.
fn base_uninitialized() -> Env {
    let mut svm = LiteSVM::new();
    svm.add_program(
        stock_vault::ID,
        include_bytes!("../../../target/deploy/stock_vault.so"),
    )
    .unwrap();
    let mut clock: Clock = svm.get_sysvar();
    clock.unix_timestamp = NOW;
    svm.set_sysvar(&clock);

    let admin = Keypair::new();
    let feed = Keypair::new();
    let issuer = Keypair::new();
    let alice = Keypair::new();
    let bob = Keypair::new();
    set_upgrade_authority(&mut svm, &stock_vault::ID, Some(&admin.pubkey()));
    for k in [&admin, &feed, &issuer, &alice, &bob] {
        svm.airdrop(&k.pubkey(), 100_000_000_000).unwrap();
    }

    let coll_mint = create_t22_mint(&mut svm, &admin, &issuer, COLL_DECIMALS, true, true, None);
    let usdc_mint = create_classic_mint(&mut svm, &admin, &issuer.pubkey(), USDC_DECIMALS);

    Env {
        svm,
        admin,
        feed,
        issuer,
        alice,
        bob,
        coll_mint: coll_mint.pubkey(),
        usdc_mint: usdc_mint.pubkey(),
        alice_coll: Pubkey::default(),
        alice_usdc: Pubkey::default(),
        bob_usdc: Pubkey::default(),
        now: NOW,
    }
}

/// SVM with the program loaded, both mints created, and the config initialized. No market yet.
fn base() -> Env {
    let mut env = base_uninitialized();
    let admin_key = env.admin.pubkey();
    let feed_key = env.feed.pubkey();
    let admin = env.admin.insecure_clone();
    env.ok(&[init_config_ix(&admin_key, &feed_key)], &[&admin]);
    env
}

/// ProgramData account of an upgradeable program.
fn programdata(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::ID).0
}

/// LiteSVM loads programs upgradeable with no upgrade authority. Set (or clear) it, as a real deploy would.
fn set_upgrade_authority(svm: &mut LiteSVM, program_id: &Pubkey, authority: Option<&Pubkey>) {
    let address = programdata(program_id);
    let mut account = svm.get_account(&address).expect("program data account");
    // bincode UpgradeableLoaderState::ProgramData: u32 tag (3) · u64 slot · Option<Pubkey> (1 + 32).
    assert_eq!(
        &account.data[..4],
        &3u32.to_le_bytes(),
        "not a ProgramData account"
    );
    match authority {
        Some(key) => {
            account.data[12] = 1;
            account.data[13..45].copy_from_slice(key.as_ref());
        }
        None => account.data[12..45].fill(0),
    }
    svm.set_account(address, account).unwrap();
}

// ------------------------------------------------------------------ T1 probe

/// T1 (eng review 2026-09-14, carried into the 2a job memory): does Anchor's `init` with
/// `token::mint = <Token-2022 mint carrying ScaledUiAmount + Pausable>` allocate the account-side
/// extension space the mint requires? Everything else in this file depends on the answer.
#[test]
fn create_market_initializes_a_token2022_collateral_vault() {
    let mut env = base();
    let admin = env.admin.insecure_clone();
    let (admin_key, coll, usdc) = (admin.pubkey(), env.coll_mint, env.usdc_mint);
    env.ok(
        &[create_market_ix(
            &admin_key,
            &coll,
            &usdc,
            &TOKEN_2022,
            &TOKEN_CLASSIC,
            params(),
        )],
        &[&admin],
    );

    let m = env.market_state();
    assert_eq!(m.collateral_mint, coll);
    assert_eq!(m.usdc_mint, usdc);
    assert_eq!(m.collateral_decimals, COLL_DECIMALS);
    assert_eq!(m.usdc_decimals, USDC_DECIMALS);
    assert_eq!(m.borrow_index, safu_core::lending::INDEX_SCALE);
    assert_eq!(m.cash, 0);
    assert_eq!(m.last_accrual_ts, NOW);
    assert_eq!(m.params, params());

    // The vault must be a usable Token-2022 account for a pausable mint, owned by the market PDA.
    let vault = env.svm.get_account(&env.coll_vault()).unwrap();
    assert_eq!(vault.owner, TOKEN_2022);
    let state =
        spl_token_2022_interface::extension::StateWithExtensions::<T22Account>::unpack(&vault.data)
            .expect("collateral vault unpacks as a Token-2022 account with its extensions");
    assert_eq!(state.base.owner, env.market());
    assert_eq!(state.base.mint, coll);

    let usdc_vault = env.svm.get_account(&env.usdc_vault()).unwrap();
    assert_eq!(usdc_vault.owner, TOKEN_CLASSIC);
}

// ------------------------------------------------------------------ remaining instructions

type Metas = Vec<anchor_lang::solana_program::instruction::AccountMeta>;
/// One named way of breaking a single `MarketParams` bound.
type ParamMutation = (&'static str, fn(&mut MarketParams));

fn admin_only_metas(admin: &Pubkey) -> Metas {
    stock_vault::accounts::AdminOnly {
        admin: *admin,
        config: pda(&[VCONFIG_SEED]),
    }
    .to_account_metas(None)
}

fn set_paused_ix(admin: &Pubkey, paused: bool) -> Instruction {
    Instruction {
        program_id: stock_vault::ID,
        accounts: admin_only_metas(admin),
        data: stock_vault::instruction::SetPaused { paused }.data(),
    }
}

fn set_feed_authority_ix(admin: &Pubkey, feed_authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: stock_vault::ID,
        accounts: admin_only_metas(admin),
        data: stock_vault::instruction::SetFeedAuthority {
            feed_authority: *feed_authority,
        }
        .data(),
    }
}

fn set_admin_ix(admin: &Pubkey, new_admin: &Pubkey) -> Instruction {
    Instruction {
        program_id: stock_vault::ID,
        accounts: admin_only_metas(admin),
        data: stock_vault::instruction::SetAdmin { admin: *new_admin }.data(),
    }
}

fn set_pool_liquidator_ix(admin: &Pubkey, pool: &Pubkey) -> Instruction {
    Instruction {
        program_id: stock_vault::ID,
        accounts: admin_only_metas(admin),
        data: stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: *pool,
        }
        .data(),
    }
}

fn set_fallback_grace_ix(admin: &Pubkey, secs: i64) -> Instruction {
    Instruction {
        program_id: stock_vault::ID,
        accounts: admin_only_metas(admin),
        data: stock_vault::instruction::SetFallbackGrace { secs }.data(),
    }
}

fn update_params_ix(admin: &Pubkey, coll_mint: &Pubkey, params: MarketParams) -> Instruction {
    Instruction {
        program_id: stock_vault::ID,
        accounts: stock_vault::accounts::UpdateMarket {
            admin: *admin,
            config: pda(&[VCONFIG_SEED]),
            market: market_pda(coll_mint),
        }
        .to_account_metas(None),
        data: stock_vault::instruction::UpdateMarketParams { params }.data(),
    }
}

impl Env {
    fn push_price_ix(
        &self,
        feed: &Pubkey,
        price: u64,
        market_open: bool,
        last_close: u64,
    ) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::PushPrice {
                feed_authority: *feed,
                config: self.config(),
                market: self.market(),
                collateral_mint: self.coll_mint,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::PushPrice {
                price,
                market_open,
                last_close,
            }
            .data(),
        }
    }

    fn open_supplier_ix(&self, owner: &Pubkey) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::OpenSupplier {
                owner: *owner,
                market: self.market(),
                supplier: self.supplier(owner),
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::OpenSupplier {}.data(),
        }
    }

    fn open_position_ix(&self, owner: &Pubkey) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::OpenPosition {
                owner: *owner,
                market: self.market(),
                position: self.position(owner),
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::OpenPosition {}.data(),
        }
    }

    fn supply_ix(&self, owner: &Pubkey, owner_usdc: &Pubkey, amount: u64) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::Supply {
                owner: *owner,
                config: self.config(),
                market: self.market(),
                supplier: self.supplier(owner),
                usdc_mint: self.usdc_mint,
                owner_usdc: *owner_usdc,
                usdc_vault: self.usdc_vault(),
                usdc_token_program: TOKEN_CLASSIC,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::Supply { amount }.data(),
        }
    }

    fn withdraw_supply_ix(&self, owner: &Pubkey, owner_usdc: &Pubkey, shares: u128) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::WithdrawSupply {
                owner: *owner,
                config: self.config(),
                market: self.market(),
                supplier: self.supplier(owner),
                usdc_mint: self.usdc_mint,
                owner_usdc: *owner_usdc,
                usdc_vault: self.usdc_vault(),
                usdc_token_program: TOKEN_CLASSIC,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::WithdrawSupply { shares }.data(),
        }
    }

    fn deposit_collateral_ix(
        &self,
        owner: &Pubkey,
        owner_coll: &Pubkey,
        amount: u64,
    ) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::DepositCollateral {
                owner: *owner,
                market: self.market(),
                position: self.position(owner),
                collateral_mint: self.coll_mint,
                owner_collateral: *owner_coll,
                collateral_vault: self.coll_vault(),
                collateral_token_program: TOKEN_2022,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::DepositCollateral { amount }.data(),
        }
    }

    fn withdraw_collateral_ix(
        &self,
        owner: &Pubkey,
        owner_coll: &Pubkey,
        amount: u64,
    ) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::WithdrawCollateral {
                owner: *owner,
                config: self.config(),
                market: self.market(),
                position: self.position(owner),
                collateral_mint: self.coll_mint,
                owner_collateral: *owner_coll,
                collateral_vault: self.coll_vault(),
                collateral_token_program: TOKEN_2022,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::WithdrawCollateral { amount }.data(),
        }
    }

    fn borrow_ix(&self, owner: &Pubkey, owner_usdc: &Pubkey, amount: u64) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::Borrow {
                owner: *owner,
                config: self.config(),
                market: self.market(),
                position: self.position(owner),
                collateral_mint: self.coll_mint,
                usdc_mint: self.usdc_mint,
                owner_usdc: *owner_usdc,
                usdc_vault: self.usdc_vault(),
                usdc_token_program: TOKEN_CLASSIC,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::Borrow { amount }.data(),
        }
    }

    fn repay_ix(
        &self,
        payer: &Pubkey,
        payer_usdc: &Pubkey,
        borrower: &Pubkey,
        amount: u64,
    ) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::Repay {
                payer: *payer,
                market: self.market(),
                position: self.position(borrower),
                usdc_mint: self.usdc_mint,
                payer_usdc: *payer_usdc,
                usdc_vault: self.usdc_vault(),
                usdc_token_program: TOKEN_CLASSIC,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::Repay { amount }.data(),
        }
    }

    /// Multiplier in fixed point exactly as the program computes it — never a hand-copied constant,
    /// because `(f64 × 1e12).floor()` is not always the decimal you would write down.
    fn acknowledge_ix(&self, authority: &Pubkey) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::AcknowledgeMultiplier {
                authority: *authority,
                config: self.config(),
                market: self.market(),
                collateral_mint: self.coll_mint,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::AcknowledgeMultiplier {}.data(),
        }
    }

    fn mult_fp(&self) -> u128 {
        let acc = self.svm.get_account(&self.coll_mint).unwrap();
        let state =
            spl_token_2022_interface::extension::StateWithExtensions::<T22Mint>::unpack(&acc.data)
                .unwrap();
        let cfg = state
            .get_extension::<scaled_ui_amount::ScaledUiAmountConfig>()
            .unwrap();
        let m = f64::from(cfg.multiplier);
        (m * safu_core::MULT_SCALE as f64).floor() as u128
    }

    fn borrow_limit(&self, raw: u64, price: u64) -> u64 {
        let value =
            safu_core::collateral::collateral_value(raw, COLL_DECIMALS, self.mult_fp(), price)
                .unwrap();
        safu_core::lending::max_borrow(value, params().ltv_bps).unwrap()
    }

    fn schedule_multiplier(&mut self, new_multiplier: f64, effective_ts: i64) {
        let issuer = self.issuer.insecure_clone();
        let ix = scaled_ui_amount::instruction::update_multiplier(
            &TOKEN_2022,
            &self.coll_mint,
            &issuer.pubkey(),
            &[],
            new_multiplier,
            effective_ts,
        )
        .unwrap();
        self.ok(&[ix], &[&issuer]);
    }

    fn pause_mint(&mut self) {
        let issuer = self.issuer.insecure_clone();
        let ix = pausable::instruction::pause(&TOKEN_2022, &self.coll_mint, &issuer.pubkey(), &[])
            .unwrap();
        self.ok(&[ix], &[&issuer]);
    }

    /// A fresh USDC holder with an open supplier account.
    fn new_supplier(&mut self, usdc: u64) -> (Keypair, Pubkey) {
        let who = Keypair::new();
        self.svm.airdrop(&who.pubkey(), 100_000_000_000).unwrap();
        let acct = create_token_account(
            &mut self.svm,
            &who,
            &self.usdc_mint.clone(),
            &who.pubkey(),
            &TOKEN_CLASSIC,
        );
        let issuer = self.issuer.insecure_clone();
        let usdc_mint = self.usdc_mint;
        mint_to(
            &mut self.svm,
            &issuer,
            &usdc_mint,
            &acct,
            usdc,
            &TOKEN_CLASSIC,
        );
        let ix = self.open_supplier_ix(&who.pubkey());
        self.ok(&[ix], &[&who]);
        (who, acct)
    }
}

/// Market created, token accounts funded, supplier and position accounts open. **No price pushed.**
fn with_market() -> Env {
    let mut env = base();
    let admin = env.admin.insecure_clone();
    let (admin_key, coll, usdc) = (admin.pubkey(), env.coll_mint, env.usdc_mint);
    env.ok(
        &[create_market_ix(
            &admin_key,
            &coll,
            &usdc,
            &TOKEN_2022,
            &TOKEN_CLASSIC,
            params(),
        )],
        &[&admin],
    );

    let (alice, bob, issuer) = (
        env.alice.insecure_clone(),
        env.bob.insecure_clone(),
        env.issuer.insecure_clone(),
    );
    env.alice_coll =
        create_token_account(&mut env.svm, &alice, &coll, &alice.pubkey(), &TOKEN_2022);
    env.alice_usdc =
        create_token_account(&mut env.svm, &alice, &usdc, &alice.pubkey(), &TOKEN_CLASSIC);
    env.bob_usdc = create_token_account(&mut env.svm, &bob, &usdc, &bob.pubkey(), &TOKEN_CLASSIC);

    let (alice_coll, alice_usdc, bob_usdc) = (env.alice_coll, env.alice_usdc, env.bob_usdc);
    mint_to(
        &mut env.svm,
        &issuer,
        &coll,
        &alice_coll,
        100 * ONE_SHARE,
        &TOKEN_2022,
    );
    // Enough to repay principal plus any interest the tests accrue.
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc,
        &alice_usdc,
        50_000 * USDC,
        &TOKEN_CLASSIC,
    );
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc,
        &bob_usdc,
        1_000_000 * USDC,
        &TOKEN_CLASSIC,
    );

    let (open_sup, open_pos) = (
        env.open_supplier_ix(&bob.pubkey()),
        env.open_position_ix(&alice.pubkey()),
    );
    env.ok(&[open_sup], &[&bob]);
    env.ok(&[open_pos], &[&alice]);
    env
}

/// `with_market`, plus Bob's USDC supplied, Alice's collateral deposited, and one accepted price.
fn full() -> Env {
    let mut env = with_market();
    let (alice, bob, feed) = (
        env.alice.insecure_clone(),
        env.bob.insecure_clone(),
        env.feed.insecure_clone(),
    );
    let (supply, deposit, push) = (
        env.supply_ix(&bob.pubkey(), &env.bob_usdc, 500_000 * USDC),
        env.deposit_collateral_ix(&alice.pubkey(), &env.alice_coll, 10 * ONE_SHARE),
        env.push_price_ix(&feed.pubkey(), PRICE, true, PRICE),
    );
    env.ok(&[supply], &[&bob]);
    env.ok(&[deposit], &[&alice]);
    env.ok(&[push], &[&feed]);
    env
}

// ------------------------------------------------------------------ config and admin

#[test]
fn initialize_config_records_the_admin_and_the_feed_key() {
    let env = base();
    let c = env.config_state();
    assert_eq!(c.admin, env.admin.pubkey());
    assert_eq!(c.feed_authority, env.feed.pubkey());
    assert!(!c.paused);
}

#[test]
fn only_the_upgrade_authority_can_initialize_the_config() {
    let mut env = base_uninitialized();
    let feed = env.feed.pubkey();
    // Someone watching the deploy calls initialize first: refused, and nothing is created.
    let stranger = env.alice.insecure_clone();
    assert_program_error(
        env.send(&[init_config_ix(&stranger.pubkey(), &feed)], &[&stranger]),
        VaultError::NotUpgradeAuthority,
    );
    assert!(env.svm.get_account(&pda(&[VCONFIG_SEED])).is_none());
    // The upgrade authority can, and becomes admin.
    let admin = env.admin.insecure_clone();
    env.ok(&[init_config_ix(&admin.pubkey(), &feed)], &[&admin]);
    assert_eq!(env.config_state().admin, admin.pubkey());
}

#[test]
fn initialize_refuses_the_upgrade_authority_of_a_different_program() {
    // The attacker deploys a program they control and presents its ProgramData as ours.
    let mut env = base_uninitialized();
    let attacker = env.alice.insecure_clone();
    let theirs = Pubkey::new_unique();
    env.svm
        .add_program(
            theirs,
            include_bytes!("../../../target/deploy/stock_vault.so"),
        )
        .unwrap();
    set_upgrade_authority(&mut env.svm, &theirs, Some(&attacker.pubkey()));
    let ix = init_config_ix_with(&attacker.pubkey(), &env.feed.pubkey(), programdata(&theirs));
    assert_program_error(
        env.send(&[ix], &[&attacker]),
        VaultError::NotUpgradeAuthority,
    );
}

#[test]
fn a_program_with_no_upgrade_authority_can_never_be_initialized() {
    let mut env = base_uninitialized();
    set_upgrade_authority(&mut env.svm, &stock_vault::ID, None);
    let admin = env.admin.insecure_clone();
    let ix = init_config_ix(&admin.pubkey(), &env.feed.pubkey());
    assert_program_error(env.send(&[ix], &[&admin]), VaultError::NotUpgradeAuthority);
}

#[test]
fn a_non_admin_cannot_pause_or_rotate_the_feed_key() {
    let mut env = base();
    let intruder = env.alice.insecure_clone();
    let key = intruder.pubkey();
    assert_program_error(
        env.send(&[set_paused_ix(&key, true)], &[&intruder]),
        VaultError::Unauthorized,
    );
    assert_program_error(
        env.send(&[set_feed_authority_ix(&key, &key)], &[&intruder]),
        VaultError::Unauthorized,
    );
    assert!(!env.config_state().paused);
}

#[test]
fn rotating_the_admin_hands_over_control_and_retires_the_old_key() {
    let mut env = base();
    let old = env.admin.insecure_clone();
    let new = Keypair::new();
    env.svm.airdrop(&new.pubkey(), 1_000_000_000).unwrap();

    env.ok(&[set_admin_ix(&old.pubkey(), &new.pubkey())], &[&old]);
    assert_eq!(env.config_state().admin, new.pubkey());

    assert_program_error(
        env.send(&[set_paused_ix(&old.pubkey(), true)], &[&old]),
        VaultError::Unauthorized,
    );
    env.ok(&[set_paused_ix(&new.pubkey(), true)], &[&new]);
    assert!(env.config_state().paused);
}

#[test]
fn a_non_admin_cannot_rotate_the_admin_and_the_default_key_is_refused() {
    let mut env = base();
    let intruder = env.alice.insecure_clone();
    assert_program_error(
        env.send(
            &[set_admin_ix(&intruder.pubkey(), &intruder.pubkey())],
            &[&intruder],
        ),
        VaultError::Unauthorized,
    );
    let admin = env.admin.insecure_clone();
    assert_program_error(
        env.send(
            &[set_admin_ix(&admin.pubkey(), &Pubkey::default())],
            &[&admin],
        ),
        VaultError::InvalidAdmin,
    );
    assert_eq!(env.config_state().admin, admin.pubkey());
}

#[test]
fn rotating_the_feed_key_retires_the_old_one() {
    let mut env = full();
    let (admin, old_feed) = (env.admin.insecure_clone(), env.feed.insecure_clone());
    let new_feed = Keypair::new();
    env.svm.airdrop(&new_feed.pubkey(), 1_000_000_000).unwrap();

    let ix = set_feed_authority_ix(&admin.pubkey(), &new_feed.pubkey());
    env.ok(&[ix], &[&admin]);
    assert_eq!(env.config_state().feed_authority, new_feed.pubkey());

    env.warp(60);
    let stale = env.push_price_ix(&old_feed.pubkey(), PRICE, true, PRICE);
    assert_program_error(env.send(&[stale], &[&old_feed]), VaultError::Unauthorized);

    let fresh = env.push_price_ix(&new_feed.pubkey(), PRICE, true, PRICE);
    env.ok(&[fresh], &[&new_feed]);
    assert_eq!(env.market_state().price.last_update, env.now);
}

// ------------------------------------------------------------------ create_market

#[test]
fn a_non_admin_cannot_create_a_market() {
    let mut env = base();
    let intruder = env.alice.insecure_clone();
    let (coll, usdc) = (env.coll_mint, env.usdc_mint);
    assert_program_error(
        env.send(
            &[create_market_ix(
                &intruder.pubkey(),
                &coll,
                &usdc,
                &TOKEN_2022,
                &TOKEN_CLASSIC,
                params(),
            )],
            &[&intruder],
        ),
        VaultError::Unauthorized,
    );
}

#[test]
fn market_params_outside_their_hard_bounds_are_refused() {
    let mut env = base();
    let admin = env.admin.insecure_clone();
    let (admin_key, coll, usdc) = (admin.pubkey(), env.coll_mint, env.usdc_mint);

    // Each mutation breaks exactly one bound, so a pass cannot come from an unrelated failure.
    let mutations: [ParamMutation; 8] = [
        ("liquidation line at or below LTV", |p| {
            p.liq_threshold_bps = p.ltv_bps
        }),
        ("LTV above the 80% ceiling", |p| p.ltv_bps = 8_001),
        ("insolvency line below the liquidation line", |p| {
            p.insolvency_ltv_bps = p.liq_threshold_bps - 1
        }),
        ("zero close factor", |p| p.close_factor_bps = 0),
        ("deviation cap below the 50 bps floor", |p| {
            p.deviation_cap_bps = 49
        }),
        ("liquidation price age below the borrow age", |p| {
            p.liquidation_max_price_age_open_secs = p.borrow_max_price_age_secs - 1
        }),
        ("kink at 100% utilization", |p| p.rate.kink_bps = 10_000),
        ("zero borrow cap", |p| p.borrow_cap = 0),
    ];
    for (label, mutate) in mutations {
        let mut bad = params();
        mutate(&mut bad);
        let result = env.send(
            &[create_market_ix(
                &admin_key,
                &coll,
                &usdc,
                &TOKEN_2022,
                &TOKEN_CLASSIC,
                bad,
            )],
            &[&admin],
        );
        assert!(
            result.is_err(),
            "{label} should have been refused by MarketParams::validate"
        );
        assert_program_error(result, VaultError::InvalidMarketParams);
    }

    // The unmutated set is accepted, proving the loop above failed on the bound and nothing else.
    env.ok(
        &[create_market_ix(
            &admin_key,
            &coll,
            &usdc,
            &TOKEN_2022,
            &TOKEN_CLASSIC,
            params(),
        )],
        &[&admin],
    );
}

#[test]
fn a_fee_bearing_collateral_mint_is_refused() {
    let mut env = base();
    let (admin, issuer) = (env.admin.insecure_clone(), env.issuer.insecure_clone());
    let fee_mint = create_t22_mint(
        &mut env.svm,
        &admin,
        &issuer,
        COLL_DECIMALS,
        true,
        true,
        Some(100),
    );
    let usdc = env.usdc_mint;
    assert_program_error(
        env.send(
            &[create_market_ix(
                &admin.pubkey(),
                &fee_mint.pubkey(),
                &usdc,
                &TOKEN_2022,
                &TOKEN_CLASSIC,
                params(),
            )],
            &[&admin],
        ),
        VaultError::UnsupportedMintExtension,
    );
}

#[test]
fn a_mint_without_a_multiplier_schedule_is_refused() {
    let mut env = base();
    let (admin, issuer) = (env.admin.insecure_clone(), env.issuer.insecure_clone());
    let plain = create_t22_mint(
        &mut env.svm,
        &admin,
        &issuer,
        COLL_DECIMALS,
        false,
        true,
        None,
    );
    let usdc = env.usdc_mint;
    assert_program_error(
        env.send(
            &[create_market_ix(
                &admin.pubkey(),
                &plain.pubkey(),
                &usdc,
                &TOKEN_2022,
                &TOKEN_CLASSIC,
                params(),
            )],
            &[&admin],
        ),
        VaultError::UnsupportedCollateralMint,
    );
}

#[test]
fn updating_params_accrues_first_and_still_enforces_the_bounds() {
    let mut env = full();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 500 * USDC);
    env.ok(&[borrow], &[&alice]);

    env.warp(30 * 86_400);
    let before = env.market_state().borrow_index;
    let mut tighter = params();
    tighter.ltv_bps = 3_000;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, tighter);
    env.ok(&[ix], &[&admin]);

    let m = env.market_state();
    assert!(
        m.borrow_index > before,
        "interest to the change must be booked at the old rate"
    );
    assert_eq!(m.last_accrual_ts, env.now);
    assert_eq!(m.params.ltv_bps, 3_000);

    env.warp(60);
    let mut bad = params();
    bad.ltv_bps = 0;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, bad);
    assert_program_error(env.send(&[ix], &[&admin]), VaultError::InvalidMarketParams);
}

#[test]
fn loosening_applies_at_once_and_tightening_starts_from_the_live_terms() {
    let mut env = full();
    let admin = env.admin.insecure_clone();

    // Loosen the liquidation line from 50% to 55%: in force immediately.
    let mut looser = params();
    looser.liq_threshold_bps = 5_500;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, looser);
    env.ok(&[ix], &[&admin]);
    let m = env.market_state();
    assert_eq!(m.ramp_from.liq_threshold_bps, 5_500);
    assert_eq!(m.ramp_start_ts, env.now);

    // Tighten to 45%: the ramp starts from the live 55%.
    env.warp(60);
    let mut tighter = params();
    tighter.liq_threshold_bps = 4_500;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, tighter);
    env.ok(&[ix], &[&admin]);
    let m = env.market_state();
    assert_eq!(m.ramp_from.liq_threshold_bps, 5_500);
    assert_eq!(m.params.liq_threshold_bps, 4_500);

    // Half way through, tighten again to 42%: the new ramp continues from the live 50%, never back
    // up at 55%, never jumping down.
    env.warp(LIQUIDATION_TERMS_RAMP_SECS / 2);
    let mut tighter_again = params();
    tighter_again.liq_threshold_bps = 4_200;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, tighter_again);
    env.ok(&[ix], &[&admin]);
    let m = env.market_state();
    assert_eq!(m.ramp_from.liq_threshold_bps, 5_000);
    assert_eq!(m.ramp_start_ts, env.now);
}

// ------------------------------------------------------------------ prices

#[test]
fn only_the_feed_authority_can_push_a_price() {
    let mut env = full();
    let intruder = env.admin.insecure_clone();
    env.warp(60);
    let ix = env.push_price_ix(&intruder.pubkey(), PRICE, true, PRICE);
    assert_program_error(env.send(&[ix], &[&intruder]), VaultError::Unauthorized);
}

#[test]
fn a_zero_price_or_a_non_advancing_clock_is_refused() {
    let mut env = full();
    let feed = env.feed.insecure_clone();
    env.warp(60);
    let zero = env.push_price_ix(&feed.pubkey(), 0, true, PRICE);
    assert_program_error(env.send(&[zero], &[&feed]), VaultError::InvalidPrice);

    let zero_close = env.push_price_ix(&feed.pubkey(), PRICE, true, 0);
    assert_program_error(env.send(&[zero_close], &[&feed]), VaultError::InvalidPrice);

    // The clock has not moved since `full()`'s push plus this warp; pushing twice in the same second
    // must be refused, or a stale reading could overwrite a newer one.
    let ok = env.push_price_ix(&feed.pubkey(), PRICE, true, PRICE);
    env.ok(&[ok], &[&feed]);
    let same_second = env.push_price_ix(&feed.pubkey(), PRICE + 1, true, PRICE);
    assert_program_error(
        env.send(&[same_second], &[&feed]),
        VaultError::StalePriceUpdate,
    );
}

#[test]
fn a_spike_beyond_the_cap_is_flagged_and_kept_out_of_the_twap() {
    let mut env = full();
    let feed = env.feed.insecure_clone();
    let before = env.market_state().price;

    env.warp(60);
    // +6% against a 500 bps cap.
    let spike = PRICE + PRICE * 6 / 100;
    let ix = env.push_price_ix(&feed.pubkey(), spike, true, PRICE);
    env.ok(&[ix], &[&feed]);

    let p = env.market_state().price;
    assert!(p.flagged, "a spike must raise the flag");
    assert_eq!(p.flagged_price, spike);
    assert_eq!(p.last_price, before.last_price, "the spike is not recorded");
    assert_eq!(p.count, before.count, "and never enters the TWAP");
    assert_eq!(p.last_update, before.last_update);
}

#[test]
fn a_second_update_confirming_the_move_is_accepted() {
    let mut env = full();
    let feed = env.feed.insecure_clone();
    let spike = PRICE + PRICE * 6 / 100;

    env.warp(60);
    let ix = env.push_price_ix(&feed.pubkey(), spike, true, PRICE);
    env.ok(&[ix], &[&feed]);
    assert!(env.market_state().price.flagged);

    // A real crash or rally holds. The confirming print sits within the cap of the flagged one.
    env.warp(60);
    let confirm = spike + spike / 1_000;
    let ix = env.push_price_ix(&feed.pubkey(), confirm, true, PRICE);
    env.ok(&[ix], &[&feed]);

    let p = env.market_state().price;
    assert!(!p.flagged, "confirmation clears the flag");
    assert_eq!(p.last_price, confirm);
    assert_eq!(p.count, 2);
}

#[test]
fn a_flagged_price_blocks_borrowing_until_it_clears() {
    let mut env = full();
    let (feed, alice) = (env.feed.insecure_clone(), env.alice.insecure_clone());
    env.warp(60);
    let spike = env.push_price_ix(&feed.pubkey(), PRICE * 2, true, PRICE);
    env.ok(&[spike], &[&feed]);

    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::PriceUnavailable);

    env.warp(60);
    let good = env.push_price_ix(&feed.pubkey(), PRICE, true, PRICE);
    env.ok(&[good], &[&feed]);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
}

#[test]
fn a_price_older_than_the_borrow_age_limit_blocks_borrowing() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    // Borrowing tolerates an hour; liquidation tolerates 25. One second past the borrow limit.
    env.warp(params().borrow_max_price_age_secs + 1);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::PriceUnavailable);
}

// ------------------------------------------------------------------ supply side

#[test]
fn the_first_supply_mints_shares_one_for_one() {
    let mut env = with_market();
    let bob = env.bob.insecure_clone();
    let ix = env.supply_ix(&bob.pubkey(), &env.bob_usdc, 1_000 * USDC);
    env.ok(&[ix], &[&bob]);

    let m = env.market_state();
    assert_eq!(m.cash, 1_000 * USDC);
    assert_eq!(m.total_supply_shares, (1_000 * USDC) as u128);
    assert_eq!(
        env.supplier_state(&bob.pubkey()).shares,
        (1_000 * USDC) as u128
    );
    assert_eq!(env.token_balance(&env.usdc_vault()), 1_000 * USDC);
}

#[test]
fn a_donation_into_the_vault_does_not_move_the_share_price() {
    let mut env = with_market();
    let (bob, issuer) = (env.bob.insecure_clone(), env.issuer.insecure_clone());
    let ix = env.supply_ix(&bob.pubkey(), &env.bob_usdc, 1_000 * USDC);
    env.ok(&[ix], &[&bob]);

    // Cash is tracked internally, so tokens pushed straight at the vault are not supplier value.
    let (usdc_mint, usdc_vault) = (env.usdc_mint, env.usdc_vault());
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc_mint,
        &usdc_vault,
        500 * USDC,
        &TOKEN_CLASSIC,
    );
    assert_eq!(env.token_balance(&usdc_vault), 1_500 * USDC);
    assert_eq!(env.market_state().cash, 1_000 * USDC);

    env.warp(60);
    let (carol, carol_usdc) = env.new_supplier(1_000 * USDC);
    let ix = env.supply_ix(&carol.pubkey(), &carol_usdc, 1_000 * USDC);
    env.ok(&[ix], &[&carol]);

    assert_eq!(
        env.supplier_state(&carol.pubkey()).shares,
        env.supplier_state(&bob.pubkey()).shares,
        "the same deposit must buy the same shares after a donation"
    );
}

#[test]
fn supply_refuses_zero_and_is_blocked_while_paused() {
    let mut env = with_market();
    let (bob, admin) = (env.bob.insecure_clone(), env.admin.insecure_clone());
    let zero = env.supply_ix(&bob.pubkey(), &env.bob_usdc, 0);
    assert_program_error(env.send(&[zero], &[&bob]), VaultError::ZeroAmount);

    env.ok(&[set_paused_ix(&admin.pubkey(), true)], &[&admin]);
    env.warp(60);
    let ix = env.supply_ix(&bob.pubkey(), &env.bob_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&bob]), VaultError::Paused);
}

#[test]
fn a_withdrawal_cannot_take_cash_that_is_lent_out() {
    let mut env = full();
    let (alice, bob) = (env.alice.insecure_clone(), env.bob.insecure_clone());
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 1_000 * USDC);
    env.ok(&[borrow], &[&alice]);

    env.warp(60);
    let all = env.supplier_state(&bob.pubkey()).shares;
    let ix = env.withdraw_supply_ix(&bob.pubkey(), &env.bob_usdc, all);
    assert_program_error(env.send(&[ix], &[&bob]), VaultError::InsufficientCash);

    // What is still in the market comes out.
    let part = all / 2;
    let ix = env.withdraw_supply_ix(&bob.pubkey(), &env.bob_usdc, part);
    env.ok(&[ix], &[&bob]);
    assert_eq!(env.supplier_state(&bob.pubkey()).shares, all - part);
}

// ------------------------------------------------------------------ backers' interest share (eng review addendum)

#[test]
fn backers_earn_their_locked_share_of_accrued_interest() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let admin = env.admin.insecure_clone();
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 1_000 * USDC);
    env.ok(&[borrow], &[&alice]);

    let before = stock_vault::logic::total_borrows(&env.market_state()).unwrap();
    env.warp(365 * 86_400);
    // update_market_params calls accrue() before anything else, with no other side effect when the
    // params supplied are unchanged -- the cleanest way to force accrual without also borrowing/repaying.
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, params());
    env.ok(&[ix], &[&admin]);

    let market = env.market_state();
    let after = stock_vault::logic::total_borrows(&market).unwrap();
    let interest = after - before;
    assert!(interest > 0, "a year of accrual must produce real interest");

    let expected_share = (interest as u128 * 1_500 / 10_000) as u64; // params().backer_interest_share_bps
    assert_eq!(market.backer_interest_owed, expected_share);
    assert_eq!(market.backer_interest_cumulative, expected_share);
    assert_eq!(market.backer_interest_paid_cumulative, 0);
}

#[test]
fn total_assets_excludes_owed_backer_interest() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let admin = env.admin.insecure_clone();
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 1_000 * USDC);
    env.ok(&[borrow], &[&alice]);
    env.warp(365 * 86_400);
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, params());
    env.ok(&[ix], &[&admin]);

    let market = env.market_state();
    assert!(market.backer_interest_owed > 0);
    let gross = market.cash + stock_vault::logic::total_borrows(&market).unwrap();
    assert_eq!(
        stock_vault::logic::total_assets(&market).unwrap(),
        gross - market.backer_interest_owed
    );
}

#[test]
fn backer_interest_share_bps_over_the_bound_is_refused() {
    let mut env = with_market();
    let admin = env.admin.insecure_clone();
    let mut bad = params();
    bad.backer_interest_share_bps = 5_001; // bound is <= 5_000
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, bad);
    assert_program_error(env.send(&[ix], &[&admin]), VaultError::InvalidMarketParams);
}

#[test]
fn suppliers_earn_the_interest_borrowers_pay() {
    let mut env = full();
    let (alice, bob) = (env.alice.insecure_clone(), env.bob.insecure_clone());
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 1_000 * USDC);
    env.ok(&[borrow], &[&alice]);

    env.warp(365 * 86_400);
    let repay_all = env.repay_ix(&alice.pubkey(), &env.alice_usdc, &alice.pubkey(), u64::MAX);
    env.ok(&[repay_all], &[&alice]);

    env.warp(60);
    let shares = env.supplier_state(&bob.pubkey()).shares;
    let before = env.token_balance(&env.bob_usdc);
    let ix = env.withdraw_supply_ix(&bob.pubkey(), &env.bob_usdc, shares);
    env.ok(&[ix], &[&bob]);
    let paid = env.token_balance(&env.bob_usdc) - before;
    assert!(
        paid > 500_000 * USDC,
        "a year of interest must leave the supplier ahead: got {paid}"
    );
}

// ------------------------------------------------------------------ collateral

#[test]
fn the_collateral_cap_is_enforced() {
    let mut env = with_market();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let mut small = params();
    small.collateral_cap_raw = 5 * ONE_SHARE;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, small);
    env.ok(&[ix], &[&admin]);

    let ok = env.deposit_collateral_ix(&alice.pubkey(), &env.alice_coll, 5 * ONE_SHARE);
    env.ok(&[ok], &[&alice]);
    env.warp(60);
    let over = env.deposit_collateral_ix(&alice.pubkey(), &env.alice_coll, 1);
    assert_program_error(env.send(&[over], &[&alice]), VaultError::CapReached);
}

#[test]
fn adding_collateral_is_allowed_while_paused() {
    let mut env = full();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    env.ok(&[set_paused_ix(&admin.pubkey(), true)], &[&admin]);
    env.warp(60);

    // Topping up only reduces risk, so a pause must not trap a borrower near liquidation.
    let ix = env.deposit_collateral_ix(&alice.pubkey(), &env.alice_coll, ONE_SHARE);
    env.ok(&[ix], &[&alice]);
    assert_eq!(
        env.position_state(&alice.pubkey()).raw_collateral,
        11 * ONE_SHARE
    );

    let out = env.withdraw_collateral_ix(&alice.pubkey(), &env.alice_coll, ONE_SHARE);
    assert_program_error(env.send(&[out], &[&alice]), VaultError::Paused);
}

#[test]
fn collateral_comes_back_out_with_no_debt_and_no_price() {
    let mut env = with_market();
    let alice = env.alice.insecure_clone();
    let deposit = env.deposit_collateral_ix(&alice.pubkey(), &env.alice_coll, 4 * ONE_SHARE);
    env.ok(&[deposit], &[&alice]);
    assert_eq!(
        env.market_state().price.count,
        0,
        "no price has been pushed"
    );

    env.warp(60);
    let ix = env.withdraw_collateral_ix(&alice.pubkey(), &env.alice_coll, 4 * ONE_SHARE);
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.position_state(&alice.pubkey()).raw_collateral, 0);
    assert_eq!(env.market_state().total_collateral_raw, 0);
}

#[test]
fn collateral_that_still_backs_a_loan_cannot_be_withdrawn() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let limit = env.borrow_limit(10 * ONE_SHARE, PRICE);
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, limit / 2);
    env.ok(&[borrow], &[&alice]);

    env.warp(60);
    // Half the collateral backs exactly half the limit; taking more than half breaches it.
    let too_much = env.withdraw_collateral_ix(&alice.pubkey(), &env.alice_coll, 6 * ONE_SHARE);
    assert_program_error(env.send(&[too_much], &[&alice]), VaultError::ExceedsLtv);

    let fine = env.withdraw_collateral_ix(&alice.pubkey(), &env.alice_coll, 4 * ONE_SHARE);
    env.ok(&[fine], &[&alice]);
}

// ------------------------------------------------------------------ borrow and repay

#[test]
fn borrowing_stops_exactly_at_the_loan_to_value_limit() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let limit = env.borrow_limit(10 * ONE_SHARE, PRICE);

    let over = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, limit + 1);
    assert_program_error(env.send(&[over], &[&alice]), VaultError::ExceedsLtv);

    let exact = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, limit);
    env.ok(&[exact], &[&alice]);
    assert_eq!(env.token_balance(&env.alice_usdc), 50_000 * USDC + limit);
    assert_eq!(env.market_state().cash, 500_000 * USDC - limit);
}

#[test]
fn the_first_draw_snapshots_full_coverage() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    assert_eq!(env.position_state(&alice.pubkey()).coverage_bps, 0);

    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
    assert_eq!(
        env.position_state(&alice.pubkey()).coverage_bps,
        stock_vault::state::FULL_COVERAGE_BPS
    );
}

#[test]
fn the_market_wide_borrow_cap_is_enforced() {
    let mut env = full();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let mut capped = params();
    capped.borrow_cap = 100 * USDC;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, capped);
    env.ok(&[ix], &[&admin]);

    env.warp(60);
    let over = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 101 * USDC);
    assert_program_error(env.send(&[over], &[&alice]), VaultError::CapReached);
    let under = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[under], &[&alice]);
}

#[test]
fn borrowing_is_blocked_while_paused() {
    let mut env = full();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    env.ok(&[set_paused_ix(&admin.pubkey(), true)], &[&admin]);
    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::Paused);
}

#[test]
fn an_issuer_pause_on_the_mint_blocks_borrowing() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    env.pause_mint();
    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::IssuerHalt);
}

#[test]
fn the_activation_window_pauses_borrowing_either_side_of_the_timestamp() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let pause = params().activation_pause_secs;

    // A routine dividend step, scheduled just inside the window.
    let activation = env.now + pause - 60;
    env.schedule_multiplier(MULT * 1.0006, activation);
    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::CorporateActionHold);

    // Past the far edge of the window (the window is ±`pause` around the activation, not around
    // now), and a 6 bps dividend step is not a split, so nothing holds any more.
    env.warp(2 * pause);
    assert!(env.now > activation + pause);
    let feed = env.feed.insecure_clone();
    let fresh = env.push_price_ix(&feed.pubkey(), PRICE, true, PRICE);
    env.ok(&[fresh], &[&feed]);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
}

#[test]
fn a_split_holds_borrowing_until_the_feed_reprices() {
    let mut env = full();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    let priced_at = env.now;

    // 4-for-1 announced now, effective after the last price print. It has to be announced ahead of
    // the timestamp: a back-dated update leaves no schedule for the hold to see.
    env.schedule_multiplier(MULT * 4.0, priced_at + 1_000);
    // Past the ±15 min window, activation passed, and the feed has not printed since.
    env.warp(2_000);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::CorporateActionHold);

    // The first market-open print after the activation ends the hold. It is quoted per post-split
    // token, a quarter of the old price (before 2026-09-15 this test pushed the pre-split PRICE, which
    // only passed because the price history was never re-quoted for the split).
    env.warp(60);
    let repriced = env.push_price_ix(&feed.pubkey(), PRICE / 4, true, PRICE / 4);
    env.ok(&[repriced], &[&feed]);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
}

#[test]
fn a_split_hold_does_not_block_a_market_that_is_still_closed() {
    let mut env = full();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    let priced_at = env.now;
    env.schedule_multiplier(MULT * 4.0, priced_at + 1_000);
    env.warp(2_000);

    // An out-of-hours print is not a reprice: the hold must survive it.
    env.warp(60);
    let closed = env.push_price_ix(&feed.pubkey(), PRICE / 4, false, PRICE / 4);
    env.ok(&[closed], &[&feed]);
    assert!(
        !env.market_state().price.flagged,
        "accepted, so the hold below is the closed market's doing"
    );
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::CorporateActionHold);
}

#[test]
fn interest_accrues_and_a_full_repayment_clears_the_position() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let principal = 1_000 * USDC;
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, principal);
    env.ok(&[ix], &[&alice]);
    let shares = env.position_state(&alice.pubkey()).debt_shares;
    assert!(shares > 0);

    env.warp(365 * 86_400);
    let owed = safu_core::lending::debt_for_shares(shares, {
        // Accrual is lazy, so read the index the next instruction will produce by touching the market.
        let admin = env.admin.insecure_clone();
        let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, params());
        env.ok(&[ix], &[&admin]);
        env.market_state().borrow_index
    })
    .unwrap();
    assert!(
        owed > principal,
        "a year at the kinked curve must cost something: {owed} vs {principal}"
    );

    env.warp(60);
    let before = env.token_balance(&env.alice_usdc);
    let repay = env.repay_ix(&alice.pubkey(), &env.alice_usdc, &alice.pubkey(), u64::MAX);
    env.ok(&[repay], &[&alice]);

    let p = env.position_state(&alice.pubkey());
    assert_eq!(p.debt_shares, 0, "full repayment leaves no dust behind");
    assert_eq!(env.market_state().total_borrow_shares, 0);
    let paid = before - env.token_balance(&env.alice_usdc);
    assert!(paid >= owed, "paid {paid}, owed at least {owed}");
}

#[test]
fn a_partial_repayment_burns_only_what_it_pays_for() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 1_000 * USDC);
    env.ok(&[ix], &[&alice]);
    let shares = env.position_state(&alice.pubkey()).debt_shares;

    env.warp(30 * 86_400);
    let repay = env.repay_ix(
        &alice.pubkey(),
        &env.alice_usdc,
        &alice.pubkey(),
        400 * USDC,
    );
    env.ok(&[repay], &[&alice]);

    let left = env.position_state(&alice.pubkey()).debt_shares;
    assert!(left > 0 && left < shares);
    let index = env.market_state().borrow_index;
    let remaining = safu_core::lending::debt_for_shares(left, index).unwrap();
    let original = safu_core::lending::debt_for_shares(shares, index).unwrap();
    // Rounding is allowed to favour the protocol, never the borrower.
    assert!(
        remaining >= original - 400 * USDC,
        "a repayment must never cancel more debt than it pays"
    );
}

#[test]
fn anyone_may_repay_someone_elses_loan() {
    let mut env = full();
    let (alice, bob) = (env.alice.insecure_clone(), env.bob.insecure_clone());
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 1_000 * USDC);
    env.ok(&[ix], &[&alice]);

    env.warp(60);
    let bob_before = env.token_balance(&env.bob_usdc);
    // Bob pays, Alice's position is credited: the rescue path a liquidation-averse lender needs.
    let repay = env.repay_ix(&bob.pubkey(), &env.bob_usdc, &alice.pubkey(), u64::MAX);
    env.ok(&[repay], &[&bob]);

    assert_eq!(env.position_state(&alice.pubkey()).debt_shares, 0);
    assert!(env.token_balance(&env.bob_usdc) < bob_before);
}

#[test]
fn repaying_a_loan_that_does_not_exist_is_refused() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let zero = env.repay_ix(&alice.pubkey(), &env.alice_usdc, &alice.pubkey(), 0);
    assert_program_error(env.send(&[zero], &[&alice]), VaultError::ZeroAmount);

    env.warp(60);
    let none = env.repay_ix(
        &alice.pubkey(),
        &env.alice_usdc,
        &alice.pubkey(),
        100 * USDC,
    );
    assert_program_error(
        env.send(&[none], &[&alice]),
        VaultError::InsufficientBalance,
    );
}

#[test]
fn a_borrower_cannot_drive_another_borrowers_position() {
    let mut env = full();
    let (alice, bob) = (env.alice.insecure_clone(), env.bob.insecure_clone());
    // Bob signs, but the instruction names Alice's position PDA: the `has_one = owner` seed check
    // is what stops a borrow being drawn against collateral that is not the signer's.
    let ix = Instruction {
        program_id: stock_vault::ID,
        accounts: stock_vault::accounts::Borrow {
            owner: bob.pubkey(),
            config: env.config(),
            market: env.market(),
            position: env.position(&alice.pubkey()),
            collateral_mint: env.coll_mint,
            usdc_mint: env.usdc_mint,
            owner_usdc: env.bob_usdc,
            usdc_vault: env.usdc_vault(),
            usdc_token_program: TOKEN_CLASSIC,
        }
        .to_account_metas(None),
        data: stock_vault::instruction::Borrow { amount: 100 * USDC }.data(),
    };
    assert!(
        env.send(&[ix], &[&bob]).is_err(),
        "a position may only be borrowed against by its own owner"
    );
}

/// Behaviour of Token-2022 that the hold logic depends on, pinned here because it is not obvious:
/// a back-dated `update_multiplier` is applied at once — the new value lands in BOTH fields and no
/// pending schedule remains. So `corporate_action_hold` can only ever see a schedule that was set
/// with a future timestamp; a split announced after the fact is invisible to it.
#[test]
fn a_backdated_multiplier_update_leaves_no_pending_schedule() {
    let mut env = full();
    let before = env.mult_fp();
    let past = env.now - 1_000;
    env.schedule_multiplier(MULT * 4.0, past);

    let after = env.mult_fp();
    assert!(
        after > before * 3,
        "a back-dated update must move the stored multiplier immediately: {before} -> {after}"
    );

    let acc = env.svm.get_account(&env.coll_mint).unwrap();
    let state =
        spl_token_2022_interface::extension::StateWithExtensions::<T22Mint>::unpack(&acc.data)
            .unwrap();
    let cfg = state
        .get_extension::<scaled_ui_amount::ScaledUiAmountConfig>()
        .unwrap();
    assert_eq!(
        f64::from(cfg.multiplier),
        f64::from(cfg.new_multiplier),
        "no schedule is left pending after a back-dated update"
    );
}

// --------------------------------------------- unannounced (back-dated) multiplier changes
//
// The hole these close: Token-2022 collapses a back-dated `update_multiplier` into both fields, so
// the schedule-based hold sees nothing and the collateral value jumps by the split ratio with no
// pause at all. The mint's activation timestamp cannot be used instead — the issuer chooses it.

#[test]
fn an_unannounced_split_holds_borrowing() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let baseline = env.market_state().observed_multiplier_fp;
    assert!(baseline > 0, "create_market seeds the baseline");

    // 4-for-1 applied the moment it takes effect: no schedule is ever visible.
    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);
    assert!(env.mult_fp() > baseline * 3);

    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::CorporateActionHold);
}

#[test]
fn an_unannounced_change_dated_to_the_epoch_still_holds() {
    // The case that rules out counting from the mint's own timestamp: an issuer can back-date far
    // enough that every window — the ±15 min pause and the 24 h split hold — is long expired.
    let mut env = full();
    let alice = env.alice.insecure_clone();
    env.schedule_multiplier(MULT * 4.0, 0);

    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::CorporateActionHold);
}

#[test]
fn the_unannounced_hold_does_not_expire_with_time() {
    let mut env = full();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);

    // Well past both the activation pause and the 24 h split hold that bound the scheduled path.
    env.warp(30 * 86_400);
    let fresh = env.push_price_ix(&feed.pubkey(), PRICE, true, PRICE);
    env.ok(&[fresh], &[&feed]);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::CorporateActionHold);
}

#[test]
fn an_unannounced_split_also_blocks_collateral_withdrawal() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 500 * USDC);
    env.ok(&[borrow], &[&alice]);

    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);
    env.warp(60);
    let ix = env.withdraw_collateral_ix(&alice.pubkey(), &env.alice_coll, ONE_SHARE);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::CorporateActionHold);
}

#[test]
fn acknowledging_an_unannounced_split_resumes_borrowing() {
    let mut env = full();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);
    env.warp(60);
    let blocked = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(
        env.send(&[blocked], &[&alice]),
        VaultError::CorporateActionHold,
    );

    let ack = env.acknowledge_ix(&feed.pubkey());
    env.ok(&[ack], &[&feed]);
    assert_eq!(
        env.market_state().observed_multiplier_fp,
        env.mult_fp(),
        "the baseline moves to the value the market now acts on"
    );

    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
}

#[test]
fn the_admin_can_also_acknowledge_but_a_borrower_cannot() {
    let mut env = full();
    let (alice, admin) = (env.alice.insecure_clone(), env.admin.insecure_clone());
    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);
    env.warp(60);

    let theirs = env.acknowledge_ix(&alice.pubkey());
    assert_program_error(env.send(&[theirs], &[&alice]), VaultError::Unauthorized);

    let ours = env.acknowledge_ix(&admin.pubkey());
    env.ok(&[ours], &[&admin]);
}

#[test]
fn acknowledging_against_a_flagged_price_is_refused() {
    let mut env = full();
    let feed = env.feed.insecure_clone();
    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);

    // A flagged feed is the wrong moment to attest that price and multiplier are back in step.
    env.warp(60);
    let spike = env.push_price_ix(&feed.pubkey(), PRICE * 2, true, PRICE);
    env.ok(&[spike], &[&feed]);
    assert!(env.market_state().price.flagged);

    let ack = env.acknowledge_ix(&feed.pubkey());
    assert_program_error(env.send(&[ack], &[&feed]), VaultError::PriceUnavailable);
}

#[test]
fn an_unannounced_dividend_step_does_not_halt_the_market() {
    // Routine dividend steps land unannounced all the time — the live AAPLx mint carried a ~6 bps
    // step. Holding the whole market for a move the price deviation cap already tolerates would be
    // a liveness bug, so only changes beyond `split_cap_bps` halt it.
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let baseline = env.market_state().observed_multiplier_fp;
    env.schedule_multiplier(MULT * 1.0006, env.now - 1_000);

    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
    let after = env.market_state().observed_multiplier_fp;
    assert!(
        after != baseline && after == env.mult_fp(),
        "a sub-threshold change moves the baseline instead of halting"
    );
}

// ============================================================ 4a: loan age and payback address

impl Env {
    fn set_payout_ix(&self, signer: &Pubkey, borrower: &Pubkey, payout: &Pubkey) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::SetPayoutAddress {
                owner: *signer,
                config: self.config(),
                position: self.position(borrower),
            }
            .to_account_metas(None),
            data: stock_vault::instruction::SetPayoutAddress { payout: *payout }.data(),
        }
    }

    /// Warps and pushes an unchanged price, so a borrow after a long gap has a fresh price to use.
    fn warp_with_fresh_price(&mut self, secs: i64) {
        let feed = self.feed.insecure_clone();
        self.warp(secs);
        let ix = self.push_price_ix(&feed.pubkey(), PRICE, true, PRICE);
        self.ok(&[ix], &[&feed]);
    }
}

#[test]
fn loan_age_starts_at_the_first_borrow_and_a_big_top_up_makes_the_loan_young() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    assert_eq!(env.position_state(&alice.pubkey()).borrow_age_ts, 0);

    let t0 = env.now;
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.position_state(&alice.pubkey()).borrow_age_ts, t0);

    // Fifty days later she borrows nine times as much: the loan now reads about five days old, not fifty.
    env.warp_with_fresh_price(50 * 86_400);
    let t1 = env.now;
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 900 * USDC);
    env.ok(&[ix], &[&alice]);
    let age = env.position_state(&alice.pubkey()).borrow_age_ts;
    let expected = t1 - 5 * 86_400; // (100·t0 + 900·t1) / 1000
    assert!(
        (age - expected).abs() <= 3_600,
        "age {age}, expected about {expected} (interest on the first $100 moves it by minutes)"
    );
}

#[test]
fn a_small_top_up_barely_moves_the_loan_age() {
    let mut env = full();
    let alice = env.alice.insecure_clone();
    let t0 = env.now;
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 1_000 * USDC);
    env.ok(&[ix], &[&alice]);

    env.warp_with_fresh_price(50 * 86_400);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 10 * USDC);
    env.ok(&[ix], &[&alice]);
    let age = env.position_state(&alice.pubkey()).borrow_age_ts;
    // $10 on top of ~$1,000 moves a fifty-day-old loan by under twelve hours.
    assert!(age > t0 && age - t0 < 12 * 3_600, "moved {}s", age - t0);
}

#[test]
fn partial_repayment_keeps_the_age_and_full_repayment_resets_it() {
    let mut env = full();
    let (alice, issuer) = (env.alice.insecure_clone(), env.issuer.insecure_clone());
    let t0 = env.now;
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
    let (usdc, alice_usdc) = (env.usdc_mint, env.alice_usdc);
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc,
        &alice_usdc,
        10 * USDC,
        &TOKEN_CLASSIC,
    );

    env.warp(86_400);
    let ix = env.repay_ix(&alice.pubkey(), &alice_usdc, &alice.pubkey(), 10 * USDC);
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.position_state(&alice.pubkey()).borrow_age_ts, t0);

    env.warp(60);
    let ix = env.repay_ix(&alice.pubkey(), &alice_usdc, &alice.pubkey(), 200 * USDC);
    env.ok(&[ix], &[&alice]);
    let p = env.position_state(&alice.pubkey());
    assert_eq!((p.debt_shares, p.borrow_age_ts), (0, 0));

    // The next borrow is a new loan.
    env.warp_with_fresh_price(3_600);
    let t2 = env.now;
    let ix = env.borrow_ix(&alice.pubkey(), &alice_usdc, 50 * USDC);
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.position_state(&alice.pubkey()).borrow_age_ts, t2);
}

#[test]
fn only_the_owner_sets_the_payback_address_and_never_to_a_vault_key() {
    let mut env = full();
    let (alice, bob, admin, feed) = (
        env.alice.insecure_clone(),
        env.bob.insecure_clone(),
        env.admin.insecure_clone(),
        env.feed.insecure_clone(),
    );
    assert_eq!(env.position_state(&alice.pubkey()).payout, alice.pubkey());

    let ix = env.set_payout_ix(&bob.pubkey(), &alice.pubkey(), &bob.pubkey());
    assert_program_error(env.send(&[ix], &[&bob]), VaultError::Unauthorized);

    for (i, bad) in [Pubkey::default(), admin.pubkey(), feed.pubkey()]
        .iter()
        .enumerate()
    {
        env.warp(1 + i as i64);
        let ix = env.set_payout_ix(&alice.pubkey(), &alice.pubkey(), bad);
        assert_program_error(env.send(&[ix], &[&alice]), VaultError::InvalidPayoutAddress);
    }

    let cold = Pubkey::new_unique();
    let ix = env.set_payout_ix(&alice.pubkey(), &alice.pubkey(), &cold);
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.position_state(&alice.pubkey()).payout, cold);
}

// ============================================================ 2b: liquidation

impl Env {
    fn record_pda(&self, seq: u64) -> Pubkey {
        pda(&[
            LIQ_RECORD_SEED,
            self.market().as_ref(),
            seq.to_le_bytes().as_ref(),
        ])
    }

    fn record_state(&self, seq: u64) -> LiquidationRecord {
        let acc = self
            .svm
            .get_account(&self.record_pda(seq))
            .expect("record exists");
        LiquidationRecord::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn liquidate_ix(
        &self,
        liquidator: &Pubkey,
        liq_usdc: &Pubkey,
        liq_coll: &Pubkey,
        borrower: &Pubkey,
        repay: u64,
    ) -> Instruction {
        self.liquidate_ix_paid_by(liquidator, liquidator, liq_usdc, liq_coll, borrower, repay)
    }

    fn liquidate_ix_paid_by(
        &self,
        payer: &Pubkey,
        liquidator: &Pubkey,
        liq_usdc: &Pubkey,
        liq_coll: &Pubkey,
        borrower: &Pubkey,
        repay: u64,
    ) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::Liquidate {
                payer: *payer,
                liquidator: *liquidator,
                config: self.config(),
                market: self.market(),
                position: self.position(borrower),
                record: self.record_pda(self.market_state().liq_seq),
                collateral_mint: self.coll_mint,
                usdc_mint: self.usdc_mint,
                liquidator_usdc: *liq_usdc,
                liquidator_collateral: *liq_coll,
                collateral_vault: self.coll_vault(),
                usdc_vault: self.usdc_vault(),
                collateral_token_program: TOKEN_2022,
                usdc_token_program: TOKEN_CLASSIC,
                system_program: SYSTEM,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::Liquidate {
                repay_amount: repay,
            }
            .data(),
        }
    }

    fn write_down_ix(&self, admin: &Pubkey, owner: &Pubkey, amount: u64) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::WriteDownCollateral {
                admin: *admin,
                config: pda(&[VCONFIG_SEED]),
                market: self.market(),
                position: self.position(owner),
                collateral_vault: self.coll_vault(),
            }
            .to_account_metas(None),
            data: stock_vault::instruction::WriteDownCollateral { amount }.data(),
        }
    }

    fn sync_issuer_ix(&self) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::SyncIssuerState {
                market: self.market(),
                collateral_mint: self.coll_mint,
                collateral_vault: self.coll_vault(),
            }
            .to_account_metas(None),
            data: stock_vault::instruction::SyncIssuerState {}.data(),
        }
    }

    /// A funded liquidator registered as the pool, so it may act at once (as the backstop pool will).
    fn new_liquidator(&mut self, usdc: u64) -> (Keypair, Pubkey, Pubkey) {
        let (who, u, c) = self.new_outside_liquidator(usdc);
        let admin = self.admin.insecure_clone();
        self.ok(
            &[set_pool_liquidator_ix(&admin.pubkey(), &who.pubkey())],
            &[&admin],
        );
        (who, u, c)
    }

    /// A funded liquidator with both token accounts open, NOT the pool: it waits for the grace period.
    fn new_outside_liquidator(&mut self, usdc: u64) -> (Keypair, Pubkey, Pubkey) {
        let who = Keypair::new();
        self.svm.airdrop(&who.pubkey(), 100_000_000_000).unwrap();
        let (usdc_mint, coll_mint) = (self.usdc_mint, self.coll_mint);
        let u = create_token_account(
            &mut self.svm,
            &who,
            &usdc_mint,
            &who.pubkey(),
            &TOKEN_CLASSIC,
        );
        let c = create_token_account(&mut self.svm, &who, &coll_mint, &who.pubkey(), &TOKEN_2022);
        let issuer = self.issuer.insecure_clone();
        mint_to(&mut self.svm, &issuer, &usdc_mint, &u, usdc, &TOKEN_CLASSIC);
        (who, u, c)
    }

    /// Walks the feed down to `target` the way a real 5-minute feed would, in steps small enough to
    /// stay inside the deviation cap against the trailing TWAP. A price crash cannot be staged as
    /// one print: the spike guard would flag it, which is exactly the behaviour tested elsewhere.
    fn walk_price_to(&mut self, target: u64) {
        let feed = self.feed.insecure_clone();
        for _ in 0..400 {
            let current = self.market_state().price.last_price;
            if current <= target {
                return;
            }
            let next = (current * 996 / 1_000).max(target);
            self.warp(600);
            let ix = self.push_price_ix(&feed.pubkey(), next, true, PRICE);
            self.ok(&[ix], &[&feed]);
            assert!(
                !self.market_state().price.flagged,
                "walk step to {next} was flagged; steps must stay inside the deviation cap"
            );
        }
        panic!("price walk never reached {target}");
    }

    fn freeze_vault(&mut self) {
        let issuer = self.issuer.insecure_clone();
        let ix = spl_token_2022_interface::instruction::freeze_account(
            &TOKEN_2022,
            &self.coll_vault(),
            &self.coll_mint,
            &issuer.pubkey(),
            &[],
        )
        .unwrap();
        self.ok(&[ix], &[&issuer]);
    }

    /// Simulates the mint's permanent delegate burning straight out of the vault, which is what D4
    /// exists to catch. Written directly because the mock mint has no delegate configured.
    fn shrink_vault_balance(&mut self, by: u64) {
        let key = self.coll_vault();
        let mut acc = self.svm.get_account(&key).unwrap();
        let amount = u64::from_le_bytes(acc.data[64..72].try_into().unwrap());
        acc.data[64..72].copy_from_slice(&(amount - by).to_le_bytes());
        self.svm.set_account(key, acc).unwrap();
    }
}

/// A market with a borrower at the loan-to-value limit and a thin supply, so the position can be
/// walked underwater by the feed alone.
fn lending_env() -> Env {
    let mut env = with_market();
    let (alice, bob, feed) = (
        env.alice.insecure_clone(),
        env.bob.insecure_clone(),
        env.feed.insecure_clone(),
    );
    let (supply, deposit, push) = (
        env.supply_ix(&bob.pubkey(), &env.bob_usdc, 50_000 * USDC),
        env.deposit_collateral_ix(&alice.pubkey(), &env.alice_coll, 10 * ONE_SHARE),
        env.push_price_ix(&feed.pubkey(), PRICE, true, PRICE),
    );
    env.ok(&[supply], &[&bob]);
    env.ok(&[deposit], &[&alice]);
    env.ok(&[push], &[&feed]);

    let limit = env.borrow_limit(10 * ONE_SHARE, PRICE);
    let borrow = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, limit);
    env.ok(&[borrow], &[&alice]);
    env
}

#[test]
fn a_healthy_position_cannot_be_liquidated() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 100 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::NotLiquidatable);
}

#[test]
fn exactly_at_the_threshold_is_not_liquidatable_but_one_step_past_it_is() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);

    // LTV starts at the 40% borrow limit; the line is 50%, so value must fall by a fifth.
    env.walk_price_to(PRICE * 81 / 100);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 100 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::NotLiquidatable);

    env.walk_price_to(PRICE * 75 / 100);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 100 * USDC);
    env.ok(&[ix], &[&liq]);
}

/// U3 (founder 2026-09-14): lowering the liquidation line must not liquidate an existing loan
/// overnight. It reaches the loan linearly over seven days.
#[test]
fn tightening_the_liquidation_line_reaches_an_existing_loan_only_over_seven_days() {
    let mut env = lending_env();
    let (admin, alice, feed) = (
        env.admin.insecure_clone(),
        env.alice.insecure_clone(),
        env.feed.insecure_clone(),
    );
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    let low = PRICE * 87 / 100;
    let push_low = |env: &mut Env, secs: i64| {
        env.warp(secs);
        let ix = env.push_price_ix(&feed.pubkey(), low, true, PRICE);
        env.ok(&[ix], &[&feed]);
    };

    // Borrowed at the 40% limit; a 13% fall puts the loan at about 46%, below today's 50% line.
    env.walk_price_to(low);
    for _ in 0..8 {
        push_low(&mut env, 3_600);
    }
    // Each attempt repays a different amount: an identical failed transaction resent under the same
    // blockhash is rejected as a duplicate before the program ever runs.
    let attempt = |env: &mut Env, n: u64| {
        let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), (100 + n) * USDC);
        env.send(&[ix], &[&liq])
    };
    assert_program_error(attempt(&mut env, 0), VaultError::NotLiquidatable);

    // The admin lowers the line to 42%, below where the loan now sits.
    let mut tighter = params();
    tighter.liq_threshold_bps = 4_200;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, tighter);
    env.ok(&[ix], &[&admin]);

    // Immediately: still the old line.
    assert_program_error(attempt(&mut env, 1), VaultError::NotLiquidatable);

    // Two days in, the line is about 47.7%: still above the loan.
    push_low(&mut env, 2 * 86_400);
    assert_program_error(attempt(&mut env, 2), VaultError::NotLiquidatable);

    // Seven days in, the new 42% line is fully in force and the loan can be liquidated.
    push_low(&mut env, 5 * 86_400);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 100 * USDC);
    env.ok(&[ix], &[&liq]);
}

#[test]
fn the_liquidation_record_freezes_the_loan_age_and_payback_address() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    let cold = Pubkey::new_unique();
    let ix = env.set_payout_ix(&alice.pubkey(), &alice.pubkey(), &cold);
    env.ok(&[ix], &[&alice]);
    let age = env.position_state(&alice.pubkey()).borrow_age_ts;
    assert!(age > 0);

    env.walk_price_to(PRICE * 75 / 100);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 100 * USDC);
    env.ok(&[ix], &[&liq]);
    let r = env.record_state(0);
    assert_eq!((r.payout, r.borrow_age_ts), (cold, age));

    // A later change, or someone holding Alice's key, cannot redirect what is already owed.
    let other = Pubkey::new_unique();
    let ix = env.set_payout_ix(&alice.pubkey(), &alice.pubkey(), &other);
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.position_state(&alice.pubkey()).payout, other);
    assert_eq!(env.record_state(0).payout, cold);
}

#[test]
fn an_underwater_position_is_seized_at_the_dynamic_bonus_and_recorded() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);

    let before_debt = {
        let m = env.market_state();
        safu_core::lending::debt_for_shares(
            env.position_state(&alice.pubkey()).debt_shares,
            m.borrow_index,
        )
        .unwrap()
    };
    let usdc_before = env.token_balance(&lu);
    let coll_before = env.token_balance(&lc);
    let cash_before = env.market_state().cash;

    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    env.ok(&[ix], &[&liq]);

    let r = env.record_state(0);
    assert_eq!(r.seq, 0);
    assert_eq!(r.borrower, alice.pubkey());
    assert_eq!(r.liquidator, liq.pubkey());
    assert_eq!(r.bad_debt, 0, "collateral still covers this one");
    assert_eq!(r.coverage_bps, stock_vault::state::FULL_COVERAGE_BPS);
    assert_eq!(r.collateral_decimals, COLL_DECIMALS);
    assert!(r.price_fp > 0 && r.multiplier_fp > 0);
    assert!(!r.issuer_halt);

    // Dynamic bonus: past the 50% line but not by much, so it sits inside the 100..500 bps band.
    assert!(
        r.bonus_bps >= 100 && r.bonus_bps <= 500,
        "bonus {} outside the configured band",
        r.bonus_bps
    );
    assert!(
        r.ltv_bps > 5_000,
        "ltv {} should be past the line",
        r.ltv_bps
    );

    // The close factor caps this liquidation at a quarter of the debt. Derived from the state the
    // instruction itself produced: `liquidate` accrues before it prices anything, so a debt read
    // taken beforehand is already stale.
    let m = env.market_state();
    let remaining = safu_core::lending::debt_for_shares(
        env.position_state(&alice.pubkey()).debt_shares,
        m.borrow_index,
    )
    .unwrap();
    let debt_at_seizure = remaining + r.debt_repaid;
    assert!(
        debt_at_seizure >= before_debt,
        "accrual only ever adds to the debt"
    );
    let expected = debt_at_seizure / 4;
    assert!(
        r.debt_repaid.abs_diff(expected) <= debt_at_seizure / 1_000,
        "repaid {} should be the 25% close factor of {debt_at_seizure} (≈{expected})",
        r.debt_repaid
    );

    // The liquidator paid USDC and received collateral; the market booked the cash.
    assert_eq!(usdc_before - env.token_balance(&lu), r.debt_repaid);
    assert_eq!(env.token_balance(&lc) - coll_before, r.seized_raw);
    assert_eq!(env.market_state().cash - cash_before, r.debt_repaid);
    assert_eq!(
        env.position_state(&alice.pubkey()).raw_collateral,
        10 * ONE_SHARE - r.seized_raw
    );
    assert_eq!(env.market_state().liq_seq, 1);

    // The seizure is worth what was repaid plus the bonus, and never more.
    let seized_value = safu_core::collateral::collateral_value(
        r.seized_raw,
        COLL_DECIMALS,
        r.multiplier_fp,
        r.price_fp,
    )
    .unwrap();
    let with_bonus = safu_core::apply_bps(r.debt_repaid, 10_000 + r.bonus_bps).unwrap();
    assert!(
        seized_value <= with_bonus,
        "seized {seized_value} exceeds repay+bonus {with_bonus}"
    );
}

#[test]
fn past_the_insolvency_line_the_whole_debt_can_go_in_one_liquidation() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    // 40% LTV at the start; a 58% fall in value puts it past the 95% insolvency line.
    env.walk_price_to(PRICE * 40 / 100);

    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    env.ok(&[ix], &[&liq]);

    let r = env.record_state(0);
    assert!(
        r.ltv_bps > 9_500,
        "ltv {} should be past the insolvency line",
        r.ltv_bps
    );
    assert_eq!(
        env.position_state(&alice.pubkey()).debt_shares,
        0,
        "the whole debt goes in one liquidation past the insolvency line"
    );
}

#[test]
fn the_chunk_cap_bounds_a_single_liquidation() {
    let mut env = lending_env();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let mut capped = params();
    capped.max_liquidation_debt = 10 * USDC;
    let ix = update_params_ix(&admin.pubkey(), &env.coll_mint, capped);
    env.ok(&[ix], &[&admin]);

    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 40 / 100);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    env.ok(&[ix], &[&liq]);

    assert_eq!(
        env.record_state(0).debt_repaid,
        10 * USDC,
        "no single liquidation may exceed the measured-liquidity chunk cap"
    );
}

#[test]
fn bad_debt_is_written_off_when_the_collateral_runs_out() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(50_000 * USDC);
    // Value below the debt: seizing everything still leaves the loan short.
    env.walk_price_to(PRICE * 25 / 100);

    let assets_before = {
        let m = env.market_state();
        m.cash + safu_core::lending::debt_for_shares(m.total_borrow_shares, m.borrow_index).unwrap()
    };

    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 50_000 * USDC);
    env.ok(&[ix], &[&liq]);

    let r = env.record_state(0);
    let m = env.market_state();
    let p = env.position_state(&alice.pubkey());
    assert_eq!(p.raw_collateral, 0, "all collateral seized");
    assert_eq!(
        p.debt_shares, 0,
        "the remainder is written off, not left accruing"
    );
    assert!(r.bad_debt > 0, "a shortfall must be recorded");
    assert_eq!(m.bad_debt, r.bad_debt);

    // The write-off lands on suppliers once, through total_borrow_shares -- not twice.
    let assets_after = m.cash
        + safu_core::lending::debt_for_shares(m.total_borrow_shares, m.borrow_index).unwrap();
    let loss = assets_before - assets_after;
    assert!(
        loss <= r.bad_debt + 1,
        "suppliers lost {loss} against a {} write-off -- double counted?",
        r.bad_debt
    );
}

#[test]
fn each_liquidation_gets_its_own_immutable_record() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    // Far enough past the line that a second liquidation is still due after the first one repairs
    // some of the ratio -- 70% would leave it at ~50.4%, on the boundary.
    env.walk_price_to(PRICE * 60 / 100);

    let first = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    env.ok(&[first], &[&liq]);
    env.warp(60);
    let second = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    env.ok(&[second], &[&liq]);

    assert_eq!(env.market_state().liq_seq, 2);
    let (r0, r1) = (env.record_state(0), env.record_state(1));
    assert_eq!(r0.seq, 0);
    assert_eq!(r1.seq, 1);
    assert!(
        r1.debt_repaid < r0.debt_repaid,
        "the close factor shrinks with the remaining debt"
    );
}

// ---------------------------------------------- liquidation guards

#[test]
fn liquidation_is_blocked_while_paused() {
    let mut env = lending_env();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);
    env.ok(&[set_paused_ix(&admin.pubkey(), true)], &[&admin]);

    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::Paused);
}

#[test]
fn liquidation_is_blocked_by_an_issuer_pause() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);
    env.pause_mint();

    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::IssuerHalt);
}

#[test]
fn liquidation_is_blocked_by_a_frozen_vault_account() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);
    env.freeze_vault();

    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(
        env.send(&[ix], &[&liq]),
        VaultError::CollateralAccountFrozen,
    );
}

#[test]
fn a_vault_short_of_its_recorded_collateral_refuses_to_liquidate() {
    // D4: the permanent delegate can burn out of the vault. Seizing on the strength of a number
    // that no longer matches the tokens would take collateral from whoever is still in the pool.
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);
    env.shrink_vault_balance(ONE_SHARE);

    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::ReconciliationFailed);
}

#[test]
fn liquidation_is_blocked_by_a_flagged_price() {
    let mut env = lending_env();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);

    env.warp(600);
    let last = env.market_state().price.last_price;
    let spike = env.push_price_ix(&feed.pubkey(), last / 2, true, PRICE);
    env.ok(&[spike], &[&feed]);
    assert!(env.market_state().price.flagged);

    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::PriceUnavailable);
}

#[test]
fn liquidation_is_blocked_by_an_unannounced_multiplier_change() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);

    // The 2a guard must cover the liquidation path too: a wrongful liquidation during an
    // unannounced split is the exact scenario this product is named after.
    env.schedule_multiplier(MULT * 0.25, env.now - 1_000);
    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::CorporateActionHold);
}

#[test]
fn liquidation_is_blocked_inside_a_scheduled_split_hold() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);

    env.schedule_multiplier(MULT * 4.0, env.now + 1_000);
    env.warp(2_000);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::CorporateActionHold);
}

// --------------------------------------------- split-adjusted price history
//
// The TWAP ring stores prices per whole token. A split changes what one token is worth by the split
// ratio, so every sample recorded before it is quoted in the wrong units afterwards. Left as is, a
// reverse split undervalues collateral by the ratio (wrongful liquidation) and a forward split
// overvalues it (a genuine liquidation stalls). The vault re-quotes the stored history in the live
// multiplier, the way equity data vendors back-adjust price history for splits.

#[test]
fn a_reverse_split_never_liquidates_a_healthy_loan() {
    let mut env = lending_env();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);

    // 1-for-4: each token now carries a quarter of the units, so its quoted price is four times higher.
    env.schedule_multiplier(MULT * 0.25, env.now + 1_000);
    env.warp(2_000);
    for _ in 0..2 {
        let print = env.push_price_ix(&feed.pubkey(), PRICE * 4, true, PRICE * 4);
        env.ok(&[print], &[&feed]);
        env.warp(60);
    }

    // Nothing about the loan changed: same shares, same dollar value. It must not be seizable.
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert_program_error(env.send(&[ix], &[&liq]), VaultError::NotLiquidatable);
    assert_eq!(env.market_state().liq_seq, 0);
}

#[test]
fn a_forward_split_does_not_delay_a_genuine_liquidation() {
    let mut env = lending_env();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);
    let low = env.market_state().price.last_price;

    env.schedule_multiplier(MULT * 4.0, env.now + 1_000);
    env.warp(2_000);
    let print = env.push_price_ix(&feed.pubkey(), low / 4, true, low / 4);
    env.ok(&[print], &[&feed]);
    assert!(
        !env.market_state().price.flagged,
        "a price correctly re-quoted for the split is not a spike"
    );

    // One ordinary print ends the hold, and the loan is exactly as underwater as before the split.
    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    env.ok(&[ix], &[&liq]);
    assert_eq!(env.market_state().liq_seq, 1);
}

#[test]
fn a_stale_pre_split_price_after_a_reverse_split_is_never_confirmed() {
    // The mirror image, and the dangerous direction: after a 1-for-4 reverse split a lagging feed still
    // quotes the old, four-times-lower price. Confirmed, it would value every loan at a quarter.
    let mut env = lending_env();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.schedule_multiplier(MULT * 0.25, env.now + 1_000);
    env.warp(2_000);
    for _ in 0..3 {
        let stale = env.push_price_ix(&feed.pubkey(), PRICE, true, PRICE);
        env.ok(&[stale], &[&feed]);
        assert_ne!(
            env.market_state().price.last_price,
            PRICE,
            "a repeated stale quote must never be confirmed into the history"
        );
        env.warp(60);
    }
    assert!(env.market_state().price.flagged);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    assert!(env.send(&[ix], &[&liq]).is_err());
    assert_eq!(env.market_state().liq_seq, 0);
}

#[test]
fn a_genuine_crash_on_split_day_still_confirms() {
    // The guard above must not lock out a real move: a 30% fall right after a 4-for-1 split is not the
    // split ratio, so the second consecutive print confirms it as usual.
    let mut env = full();
    let feed = env.feed.insecure_clone();
    env.schedule_multiplier(MULT * 4.0, env.now + 1_000);
    env.warp(2_000);
    let crashed = PRICE / 4 * 70 / 100;
    for _ in 0..2 {
        let print = env.push_price_ix(&feed.pubkey(), crashed, true, PRICE / 4);
        env.ok(&[print], &[&feed]);
        env.warp(60);
    }
    let st = env.market_state().price;
    assert!(!st.flagged);
    assert_eq!(st.last_price, crashed);
}

#[test]
fn stored_prices_are_requoted_in_the_new_multiplier() {
    let mut env = full();
    let feed = env.feed.insecure_clone();
    let before = env.market_state().price;
    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);
    env.warp(60);
    let ack = env.acknowledge_ix(&feed.pubkey());
    env.ok(&[ack], &[&feed]);

    // Every stored figure is re-quoted so price × multiplier, the collateral value, is unchanged.
    let after = env.market_state().price;
    for i in 0..before.count as usize {
        let expected = before.prices[i] / 4;
        assert!(
            after.prices[i].abs_diff(expected) <= 1,
            "sample {i}: {} re-quoted to {}, expected {expected}",
            before.prices[i],
            after.prices[i]
        );
        assert_eq!(after.timestamps[i], before.timestamps[i]);
    }
    assert!(after.last_price.abs_diff(before.last_price / 4) <= 1);
    assert!(after.last_close.abs_diff(before.last_close / 4) <= 1);
}

#[test]
fn prices_pushed_during_an_unannounced_split_are_quoted_in_the_new_units() {
    // The feed keeps publishing while the fail-closed hold waits for a human acknowledgement. Those
    // prints are already in post-split terms and must land in the history as such.
    let mut env = full();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    env.schedule_multiplier(MULT * 4.0, env.now - 1_000);
    env.warp(60);
    let print = env.push_price_ix(&feed.pubkey(), PRICE / 4, true, PRICE / 4);
    env.ok(&[print], &[&feed]);
    assert!(!env.market_state().price.flagged);

    // The price history being right does not lift the hold: that still takes an acknowledgement.
    let blocked = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    assert_program_error(
        env.send(&[blocked], &[&alice]),
        VaultError::CorporateActionHold,
    );
    let ack = env.acknowledge_ix(&feed.pubkey());
    env.ok(&[ack], &[&feed]);
    env.warp(60);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
}

#[test]
fn an_early_post_split_price_is_never_confirmed_into_the_history() {
    // The desync the split demo stages: the feed publishes the post-split price before the
    // multiplier activates. A move of exactly the split ratio is a price in the wrong units, not a
    // crash, so repeating it must not confirm it.
    let mut env = full();
    let (alice, feed) = (env.alice.insecure_clone(), env.feed.insecure_clone());
    let activation = env.now + 1_000;
    env.schedule_multiplier(MULT * 4.0, activation);

    env.warp(900);
    for _ in 0..2 {
        let early = env.push_price_ix(&feed.pubkey(), PRICE / 4, true, PRICE / 4);
        env.ok(&[early], &[&feed]);
        env.warp(30);
    }
    let st = env.market_state().price;
    assert!(st.flagged, "the early print stays flagged");
    assert_eq!(st.last_price, PRICE, "and never enters the price history");

    // Past the far edge of the window the same price, now correct, is accepted at once.
    env.warp(activation + params().activation_pause_secs + 60 - env.now);
    let repriced = env.push_price_ix(&feed.pubkey(), PRICE / 4, true, PRICE / 4);
    env.ok(&[repriced], &[&feed]);
    let st = env.market_state().price;
    assert!(!st.flagged);
    assert_eq!(st.last_price, PRICE / 4);
    let ix = env.borrow_ix(&alice.pubkey(), &env.alice_usdc, 100 * USDC);
    env.ok(&[ix], &[&alice]);
}

#[test]
fn sync_issuer_state_reflects_the_issuer_controlled_facts() {
    let mut env = lending_env();
    env.warp(60);
    env.ok(&[env.sync_issuer_ix()], &[&env.admin.insecure_clone()]);
    assert!(!env.market_state().issuer_halt, "nothing wrong yet");

    env.pause_mint();
    env.warp(60);
    env.ok(&[env.sync_issuer_ix()], &[&env.admin.insecure_clone()]);
    assert!(
        env.market_state().issuer_halt,
        "an issuer pause raises the halt"
    );
}

#[test]
fn sync_issuer_state_catches_a_vault_shortfall_and_is_permissionless() {
    let mut env = lending_env();
    env.shrink_vault_balance(ONE_SHARE);
    // Anyone may call it: it only records facts already visible on-chain.
    let (stranger, _, _) = env.new_liquidator(0);
    env.warp(60);
    let ix = env.sync_issuer_ix();
    env.ok(&[ix], &[&stranger]);
    assert!(env.market_state().issuer_halt);
}

// ============================================================ phase 1: pool priority and the fallback grace period

impl Env {
    fn mark_ix(&self, borrower: &Pubkey) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: stock_vault::accounts::MarkLiquidatable {
                config: self.config(),
                market: self.market(),
                position: self.position(borrower),
                collateral_mint: self.coll_mint,
                collateral_vault: self.coll_vault(),
                collateral_token_program: TOKEN_2022,
            }
            .to_account_metas(None),
            data: stock_vault::instruction::MarkLiquidatable {}.data(),
        }
    }

    fn marks(&self, borrower: &Pubkey) -> (i64, i64) {
        let p = self.position_state(borrower);
        (p.liquidatable_first_seen, p.liquidatable_last_seen)
    }
}

#[test]
fn only_the_admin_registers_the_pool_and_sets_the_grace_within_bounds() {
    let mut env = base();
    let (admin, feed, intruder) = (
        env.admin.insecure_clone(),
        env.feed.insecure_clone(),
        env.alice.insecure_clone(),
    );
    let pool = Pubkey::new_unique();
    assert_eq!(
        env.config_state().fallback_grace_secs,
        DEFAULT_FALLBACK_GRACE_SECS
    );
    assert_eq!(env.config_state().pool_liquidator, Pubkey::default());

    assert_program_error(
        env.send(
            &[set_pool_liquidator_ix(&intruder.pubkey(), &pool)],
            &[&intruder],
        ),
        VaultError::Unauthorized,
    );
    for bad in [admin.pubkey(), feed.pubkey()] {
        assert_program_error(
            env.send(&[set_pool_liquidator_ix(&admin.pubkey(), &bad)], &[&admin]),
            VaultError::InvalidPoolLiquidator,
        );
    }
    env.ok(&[set_pool_liquidator_ix(&admin.pubkey(), &pool)], &[&admin]);
    assert_eq!(env.config_state().pool_liquidator, pool);
    env.ok(
        &[set_pool_liquidator_ix(&admin.pubkey(), &Pubkey::default())],
        &[&admin],
    );
    assert_eq!(env.config_state().pool_liquidator, Pubkey::default());

    for bad in [59, 86_401] {
        assert_program_error(
            env.send(&[set_fallback_grace_ix(&admin.pubkey(), bad)], &[&admin]),
            VaultError::InvalidFallbackGrace,
        );
    }
    env.ok(&[set_fallback_grace_ix(&admin.pubkey(), 600)], &[&admin]);
    assert_eq!(env.config_state().fallback_grace_secs, 600);
}

#[test]
fn mainnet_never_lets_the_fallback_grace_drop_below_five_minutes() {
    let mut env = base_uninitialized();
    let admin = env.admin.insecure_clone();
    let feed = env.feed.pubkey();
    assert_program_error(
        env.send(
            &[init_config_ix_cluster(
                &admin.pubkey(),
                &feed,
                programdata(&stock_vault::ID),
                3,
            )],
            &[&admin],
        ),
        VaultError::InvalidClusterTag,
    );
    env.ok(
        &[init_config_ix_cluster(
            &admin.pubkey(),
            &feed,
            programdata(&stock_vault::ID),
            stock_vault::state::CLUSTER_MAINNET,
        )],
        &[&admin],
    );
    assert_eq!(
        env.config_state().cluster_tag,
        stock_vault::state::CLUSTER_MAINNET
    );
    assert_program_error(
        env.send(&[set_fallback_grace_ix(&admin.pubkey(), 299)], &[&admin]),
        VaultError::InvalidFallbackGrace,
    );
    env.ok(&[set_fallback_grace_ix(&admin.pubkey(), 300)], &[&admin]);
    assert_eq!(env.config_state().fallback_grace_secs, 300);
}

#[test]
fn the_pool_liquidates_at_once_while_an_outsider_is_refused() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (outsider, ou, oc) = env.new_outside_liquidator(10_000 * USDC);
    let (pool, pu, pc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);

    let ix = env.liquidate_ix(&outsider.pubkey(), &ou, &oc, &alice.pubkey(), 100 * USDC);
    assert_program_error(env.send(&[ix], &[&outsider]), VaultError::PoolPriority);
    let ix = env.liquidate_ix(&pool.pubkey(), &pu, &pc, &alice.pubkey(), 100 * USDC);
    env.ok(&[ix], &[&pool]);
    assert_eq!(env.record_state(0).liquidator, pool.pubkey());
}

#[test]
fn an_outsider_may_liquidate_once_the_position_has_been_marked_for_the_grace_period() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (outsider, ou, oc) = env.new_outside_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);
    let g = DEFAULT_FALLBACK_GRACE_SECS;

    let ix = env.liquidate_ix(&outsider.pubkey(), &ou, &oc, &alice.pubkey(), 100 * USDC);
    assert_program_error(env.send(&[ix], &[&outsider]), VaultError::PoolPriority);

    let ix = env.mark_ix(&alice.pubkey());
    env.ok(&[ix], &[&outsider]);
    env.warp(g - 1);
    let ix = env.liquidate_ix(&outsider.pubkey(), &ou, &oc, &alice.pubkey(), 101 * USDC);
    assert_program_error(env.send(&[ix], &[&outsider]), VaultError::PoolPriority);
    env.warp(1);
    let ix = env.liquidate_ix(&outsider.pubkey(), &ou, &oc, &alice.pubkey(), 102 * USDC);
    env.ok(&[ix], &[&outsider]);
    assert_eq!(env.record_state(0).liquidator, outsider.pubkey());
}

#[test]
fn a_stale_mark_restarts_the_grace_clock() {
    let mut env = lending_env();
    let alice = env.alice.insecure_clone();
    let (outsider, ou, oc) = env.new_outside_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 75 / 100);
    let g = DEFAULT_FALLBACK_GRACE_SECS;

    let ix = env.mark_ix(&alice.pubkey());
    env.ok(&[ix], &[&outsider]);
    // Nobody re-marks for longer than the grace period: the old mark no longer counts.
    env.warp(2 * g + 1);
    let ix = env.liquidate_ix(&outsider.pubkey(), &ou, &oc, &alice.pubkey(), 100 * USDC);
    assert_program_error(env.send(&[ix], &[&outsider]), VaultError::PoolPriority);

    // Re-marking after that long restarts the clock rather than continuing it.
    let ix = env.mark_ix(&alice.pubkey());
    env.ok(&[ix], &[&outsider]);
    assert_eq!(env.marks(&alice.pubkey()), (env.now, env.now));
    env.warp(g);
    let ix = env.liquidate_ix(&outsider.pubkey(), &ou, &oc, &alice.pubkey(), 101 * USDC);
    env.ok(&[ix], &[&outsider]);
}

#[test]
fn marking_counts_only_while_a_liquidation_could_actually_happen() {
    let mut env = lending_env();
    let (alice, admin) = (env.alice.insecure_clone(), env.admin.insecure_clone());

    // At the borrow limit the position is healthy: nothing is recorded.
    let ix = env.mark_ix(&alice.pubkey());
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.marks(&alice.pubkey()), (0, 0));

    env.walk_price_to(PRICE * 75 / 100);
    let ix = env.mark_ix(&alice.pubkey());
    env.ok(&[ix], &[&alice]);
    let first = env.now;
    assert_eq!(env.marks(&alice.pubkey()), (first, first));

    // A re-mark within the window keeps the first sighting.
    env.warp(60);
    let ix = env.mark_ix(&alice.pubkey());
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.marks(&alice.pubkey()), (first, env.now));

    // While the vault is paused no liquidation can happen, so the clock is cleared, not left running.
    env.ok(&[set_paused_ix(&admin.pubkey(), true)], &[&admin]);
    env.warp(60);
    let ix = env.mark_ix(&alice.pubkey());
    env.ok(&[ix], &[&alice]);
    assert_eq!(env.marks(&alice.pubkey()), (0, 0));
}

/// Eng review A1: the backstop pool's key is a PDA that cannot pay rent, so the record's rent comes from a
/// separate payer. Proven here with a liquidator that holds no SOL at all.
#[test]
fn a_separate_payer_covers_the_record_rent_for_a_liquidator_with_no_sol() {
    let mut env = lending_env();
    let (alice, bob, admin, issuer) = (
        env.alice.insecure_clone(),
        env.bob.insecure_clone(),
        env.admin.insecure_clone(),
        env.issuer.insecure_clone(),
    );
    let broke = Keypair::new();
    let (usdc, coll) = (env.usdc_mint, env.coll_mint);
    let bu = create_token_account(&mut env.svm, &admin, &usdc, &broke.pubkey(), &TOKEN_CLASSIC);
    let bc = create_token_account(&mut env.svm, &admin, &coll, &broke.pubkey(), &TOKEN_2022);
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc,
        &bu,
        10_000 * USDC,
        &TOKEN_CLASSIC,
    );
    env.ok(
        &[set_pool_liquidator_ix(&admin.pubkey(), &broke.pubkey())],
        &[&admin],
    );
    env.walk_price_to(PRICE * 75 / 100);

    let ix = env.liquidate_ix_paid_by(
        &bob.pubkey(),
        &broke.pubkey(),
        &bu,
        &bc,
        &alice.pubkey(),
        100 * USDC,
    );
    env.ok(&[ix], &[&bob, &broke]);
    assert_eq!(env.svm.get_balance(&broke.pubkey()).unwrap_or(0), 0);
    assert_eq!(env.record_state(0).liquidator, broke.pubkey());
}

// ------------------------------------------------------------------ issuer seizure write-down

#[test]
fn an_issuer_seizure_is_written_down_and_the_market_recovers() {
    let mut env = lending_env();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let before = env.position_state(&alice.pubkey());
    let total_before = env.market_state().total_collateral_raw;

    // Nothing is missing yet: there is nothing to write down.
    let ix = env.write_down_ix(&admin.pubkey(), &alice.pubkey(), 1);
    assert_program_error(env.send(&[ix], &[&admin]), VaultError::WriteDownTooLarge);

    env.shrink_vault_balance(ONE_SHARE);
    env.ok(&[env.sync_issuer_ix()], &[&admin]);
    assert!(
        env.market_state().issuer_halt,
        "a short vault halts the market"
    );

    let ix = env.write_down_ix(&alice.pubkey(), &alice.pubkey(), ONE_SHARE);
    assert_program_error(env.send(&[ix], &[&alice]), VaultError::Unauthorized);
    env.warp(1);
    let ix = env.write_down_ix(&admin.pubkey(), &alice.pubkey(), ONE_SHARE + 1);
    assert_program_error(env.send(&[ix], &[&admin]), VaultError::WriteDownTooLarge);

    let ix = env.write_down_ix(&admin.pubkey(), &alice.pubkey(), ONE_SHARE);
    env.ok(&[ix], &[&admin]);
    let after = env.position_state(&alice.pubkey());
    assert_eq!(after.raw_collateral, before.raw_collateral - ONE_SHARE);
    assert!(after.issuer_seized);
    assert_eq!(after.seized_raw_total, ONE_SHARE);
    assert_eq!(
        after.debt_shares, before.debt_shares,
        "debt stays while collateral remains"
    );
    assert_eq!(
        env.market_state().total_collateral_raw,
        total_before - ONE_SHARE
    );

    env.warp(1);
    let ix = env.write_down_ix(&admin.pubkey(), &alice.pubkey(), 1);
    assert_program_error(env.send(&[ix], &[&admin]), VaultError::WriteDownTooLarge);

    env.warp(1);
    env.ok(&[env.sync_issuer_ix()], &[&admin]);
    assert!(
        !env.market_state().issuer_halt,
        "books match again: the halt lifts"
    );

    // A seized position can still be liquidated, but its record is marked so no payback claim can use it.
    let (liq, lu, lc) = env.new_liquidator(10_000 * USDC);
    env.walk_price_to(PRICE * 70 / 100);
    let seq = env.market_state().liq_seq;
    env.warp(60);
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 10_000 * USDC);
    env.ok(&[ix], &[&liq]);
    assert!(env.record_state(seq).issuer_halt);
}

#[test]
fn writing_down_all_collateral_books_the_debt_as_issuer_loss_not_bad_debt() {
    let mut env = lending_env();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let raw = env.position_state(&alice.pubkey()).raw_collateral;
    let borrow_shares_before = env.market_state().total_borrow_shares;
    assert!(env.position_state(&alice.pubkey()).debt_shares > 0);

    env.shrink_vault_balance(raw);
    let ix = env.write_down_ix(&admin.pubkey(), &alice.pubkey(), raw);
    env.ok(&[ix], &[&admin]);

    let p = env.position_state(&alice.pubkey());
    let m = env.market_state();
    assert_eq!(p.raw_collateral, 0);
    assert_eq!(p.debt_shares, 0);
    assert!(
        m.issuer_loss_cumulative > 0,
        "the unpayable debt is booked as issuer loss"
    );
    assert_eq!(
        m.bad_debt, 0,
        "and never as bad debt the backstop would reimburse"
    );
    assert_eq!(m.bad_debt_cumulative, 0);
    assert!(m.total_borrow_shares < borrow_shares_before);
}

#[test]
fn a_seized_position_liquidated_into_shortfall_is_issuer_loss_not_bad_debt() {
    let mut env = lending_env();
    let (admin, alice) = (env.admin.insecure_clone(), env.alice.insecure_clone());
    let raw = env.position_state(&alice.pubkey()).raw_collateral;
    // The issuer takes all but one share; what remains cannot cover the loan.
    env.shrink_vault_balance(raw - ONE_SHARE);
    let ix = env.write_down_ix(&admin.pubkey(), &alice.pubkey(), raw - ONE_SHARE);
    env.ok(&[ix], &[&admin]);
    env.ok(&[env.sync_issuer_ix()], &[&admin]);
    assert!(!env.market_state().issuer_halt);

    let (liq, lu, lc) = env.new_liquidator(50_000 * USDC);
    env.warp(60);
    let seq = env.market_state().liq_seq;
    let ix = env.liquidate_ix(&liq.pubkey(), &lu, &lc, &alice.pubkey(), 50_000 * USDC);
    env.ok(&[ix], &[&liq]);

    let record = env.record_state(seq);
    assert!(
        record.bad_debt > 0,
        "the remaining collateral could not cover the loan"
    );
    assert!(record.issuer_halt);
    let m = env.market_state();
    assert_eq!(m.issuer_loss_cumulative, record.bad_debt);
    assert_eq!(m.bad_debt, 0);
    assert_eq!(
        m.bad_debt_cumulative, 0,
        "the backstop must never see this as reimbursable"
    );
}
