//! Stage 2 item 2c: backer capital, wrongful-liquidation payouts and bad-debt cover.
//!
//! The payout tests run the real cross-program path — a genuine liquidation in `stock_vault`
//! produces the `LiquidationRecord` the backstop then reads. Anchor's `Owner` check on that account
//! is the thing that makes a fabricated record impossible, so testing it against an injected struct
//! would prove nothing.

use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_lang::{
    prelude::{Clock, Pubkey},
    solana_program::{instruction::Instruction, program_pack::Pack},
    AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas,
};
use backstop::{
    errors::VerdictError,
    state::{
        Backer, BackstopConfig, BadDebtCover, ClaimReceipt, ATTESTATION_SEED, BACKER_SEED,
        BAD_DEBT_SEED, CLAIM_SEED, CONFIG_SEED, USDC_VAULT_SEED as B_USDC_VAULT_SEED,
    },
    verdict::{encode_message, VerdictArgs, CLUSTER_DEVNET},
};
use litesvm::LiteSVM;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use spl_token_2022_interface::{
    extension::{pausable, scaled_ui_amount, BaseStateWithExtensions, ExtensionType},
    state::{Account as T22Account, Mint as T22Mint},
    ID as TOKEN_2022,
};
use stock_vault::state::{
    LiquidationRecord, Market, MarketParams, RateParams, COLL_VAULT_SEED, LIQ_RECORD_SEED,
    MARKET_SEED, POSITION_SEED, SUPPLIER_SEED, USDC_VAULT_SEED, VCONFIG_SEED,
};

const NOW: i64 = 1_800_000_000;
const COLL_DECIMALS: u8 = 8;
const USDC_DECIMALS: u8 = 6;
const ONE_SHARE: u64 = 100_000_000;
const PRICE: u64 = 33_028_000_000;
const MULT: f64 = 1.0026642;
const USDC: u64 = 1_000_000;
const ED25519: Pubkey =
    anchor_lang::prelude::pubkey!("Ed25519SigVerify111111111111111111111111111");
const IX_SYSVAR: Pubkey =
    anchor_lang::prelude::pubkey!("Sysvar1nstructions1111111111111111111111111");
const TOKEN_CLASSIC: Pubkey = spl_token_interface::ID;
const SYSTEM: Pubkey = solana_system_interface::program::ID;
const CAP_BPS: u32 = 1_000;

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
        rate: RateParams {
            base_bps: 100,
            slope1_bps: 500,
            slope2_bps: 6_000,
            kink_bps: 9_000,
        },
        borrow_cap: 500_000 * USDC,
        collateral_cap_raw: 2_000 * ONE_SHARE,
    }
}

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    feed: Keypair,
    issuer: Keypair,
    oracle: Keypair,
    alice: Keypair,
    coll_mint: Pubkey,
    usdc_mint: Pubkey,
    alice_coll: Pubkey,
    alice_usdc: Pubkey,
    now: i64,
}

fn vpda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &stock_vault::ID).0
}
fn bpda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &backstop::ID).0
}

impl Env {
    fn market(&self) -> Pubkey {
        vpda(&[MARKET_SEED, self.coll_mint.as_ref()])
    }
    fn b_config(&self) -> Pubkey {
        bpda(&[CONFIG_SEED])
    }
    fn b_vault(&self) -> Pubkey {
        bpda(&[B_USDC_VAULT_SEED])
    }
    fn backer(&self, o: &Pubkey) -> Pubkey {
        bpda(&[BACKER_SEED, o.as_ref()])
    }
    fn record(&self, seq: u64) -> Pubkey {
        vpda(&[
            LIQ_RECORD_SEED,
            self.market().as_ref(),
            seq.to_le_bytes().as_ref(),
        ])
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

    fn warp(&mut self, secs: i64) {
        self.now += secs;
        let mut c: Clock = self.svm.get_sysvar();
        c.unix_timestamp = self.now;
        self.svm.set_sysvar(&c);
        self.svm.expire_blockhash();
    }

    fn balance(&self, a: &Pubkey) -> u64 {
        let acc = self.svm.get_account(a).expect("token account");
        u64::from_le_bytes(acc.data[64..72].try_into().unwrap())
    }
    fn config_state(&self) -> BackstopConfig {
        let a = self.svm.get_account(&self.b_config()).unwrap();
        BackstopConfig::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn backer_state(&self, o: &Pubkey) -> Backer {
        let a = self.svm.get_account(&self.backer(o)).unwrap();
        Backer::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn market_state(&self) -> Market {
        let a = self.svm.get_account(&self.market()).unwrap();
        Market::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn record_state(&self, seq: u64) -> LiquidationRecord {
        let a = self.svm.get_account(&self.record(seq)).unwrap();
        LiquidationRecord::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn receipt_state(&self, record: &Pubkey) -> ClaimReceipt {
        let a = self
            .svm
            .get_account(&bpda(&[CLAIM_SEED, record.as_ref()]))
            .unwrap();
        ClaimReceipt::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
}

fn assert_program_error(result: Result<(), String>, error: VerdictError) {
    let code = anchor_lang::error::ERROR_CODE_OFFSET + error as u32;
    let err = result.expect_err("transaction should have failed");
    assert!(
        err.contains(&format!("Custom({code})")),
        "expected Custom({code}) ({error:?}), got: {err}"
    );
}

fn assert_vault_error(result: Result<(), String>, error: stock_vault::errors::VaultError) {
    let code = anchor_lang::error::ERROR_CODE_OFFSET + error as u32;
    let err = result.expect_err("transaction should have failed");
    assert!(
        err.contains(&format!("Custom({code})")),
        "expected Custom({code}) ({error:?}), got: {err}"
    );
}

// ------------------------------------------------------------------ token setup

fn t22_mint(svm: &mut LiteSVM, payer: &Keypair, issuer: &Keypair) -> Keypair {
    let mint = Keypair::new();
    let space = ExtensionType::try_calculate_account_len::<T22Mint>(&[
        ExtensionType::ScaledUiAmount,
        ExtensionType::Pausable,
    ])
    .unwrap();
    let rent = svm.minimum_balance_for_rent_exemption(space);
    let ixs = [
        solana_system_interface::instruction::create_account(
            &payer.pubkey(),
            &mint.pubkey(),
            rent,
            space as u64,
            &TOKEN_2022,
        ),
        scaled_ui_amount::instruction::initialize(
            &TOKEN_2022,
            &mint.pubkey(),
            Some(issuer.pubkey()),
            MULT,
        )
        .unwrap(),
        pausable::instruction::initialize(&TOKEN_2022, &mint.pubkey(), &issuer.pubkey()).unwrap(),
        spl_token_2022_interface::instruction::initialize_mint2(
            &TOKEN_2022,
            &mint.pubkey(),
            &issuer.pubkey(),
            Some(&issuer.pubkey()),
            COLL_DECIMALS,
        )
        .unwrap(),
    ];
    let msg = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer, &mint]).unwrap();
    svm.send_transaction(tx).expect("t22 mint");
    mint
}

fn classic_mint(svm: &mut LiteSVM, payer: &Keypair, authority: &Pubkey) -> Keypair {
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
            USDC_DECIMALS,
        )
        .unwrap(),
    ];
    let msg = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer, &mint]).unwrap();
    svm.send_transaction(tx).expect("classic mint");
    mint
}

fn token_account(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Pubkey,
    owner: &Pubkey,
    program: &Pubkey,
) -> Pubkey {
    let account = Keypair::new();
    let (space, init) = if *program == TOKEN_2022 {
        let m = svm.get_account(mint).unwrap();
        let st =
            spl_token_2022_interface::extension::StateWithExtensions::<T22Mint>::unpack(&m.data)
                .unwrap();
        let exts =
            ExtensionType::get_required_init_account_extensions(&st.get_extension_types().unwrap());
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
            program,
        ),
        init,
    ];
    let msg = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &svm.latest_blockhash());
    let tx =
        VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[payer, &account]).unwrap();
    svm.send_transaction(tx).expect("token account");
    account.pubkey()
}

fn mint_to(
    svm: &mut LiteSVM,
    auth: &Keypair,
    mint: &Pubkey,
    to: &Pubkey,
    amount: u64,
    prog: &Pubkey,
) {
    let ix = if *prog == TOKEN_2022 {
        spl_token_2022_interface::instruction::mint_to(
            &TOKEN_2022,
            mint,
            to,
            &auth.pubkey(),
            &[],
            amount,
        )
        .unwrap()
    } else {
        spl_token_interface::instruction::mint_to(
            &TOKEN_CLASSIC,
            mint,
            to,
            &auth.pubkey(),
            &[],
            amount,
        )
        .unwrap()
    };
    let msg = Message::new_with_blockhash(&[ix], Some(&auth.pubkey()), &svm.latest_blockhash());
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[auth]).unwrap();
    svm.send_transaction(tx).expect("mint_to");
}

// ------------------------------------------------------------------ instruction builders

impl Env {
    fn v_ix<A: ToAccountMetas, D: InstructionData>(&self, accounts: A, data: D) -> Instruction {
        Instruction {
            program_id: stock_vault::ID,
            accounts: accounts.to_account_metas(None),
            data: data.data(),
        }
    }
    fn b_ix<A: ToAccountMetas, D: InstructionData>(&self, accounts: A, data: D) -> Instruction {
        Instruction {
            program_id: backstop::ID,
            accounts: accounts.to_account_metas(None),
            data: data.data(),
        }
    }

    fn push_price(&mut self, price: u64) {
        let feed = self.feed.insecure_clone();
        let ix = self.v_ix(
            stock_vault::accounts::PushPrice {
                feed_authority: feed.pubkey(),
                config: vpda(&[VCONFIG_SEED]),
                market: self.market(),
            },
            stock_vault::instruction::PushPrice {
                price,
                market_open: true,
                last_close: PRICE,
            },
        );
        self.ok(&[ix], &[&feed]);
    }

    /// Steps the feed down in increments small enough to clear the deviation cap against the
    /// trailing TWAP — a crash cannot be staged as a single print.
    fn walk_price_to(&mut self, target: u64) {
        for _ in 0..400 {
            let current = self.market_state().price.last_price;
            if current <= target {
                return;
            }
            let next = (current * 996 / 1_000).max(target);
            self.warp(600);
            self.push_price(next);
            assert!(!self.market_state().price.flagged, "walk step flagged");
        }
        panic!("price walk never reached {target}");
    }

    fn new_funded(&mut self, usdc: u64) -> (Keypair, Pubkey) {
        let who = Keypair::new();
        self.svm.airdrop(&who.pubkey(), 100_000_000_000).unwrap();
        let m = self.usdc_mint;
        let acct = token_account(&mut self.svm, &who, &m, &who.pubkey(), &TOKEN_CLASSIC);
        if usdc > 0 {
            let issuer = self.issuer.insecure_clone();
            mint_to(&mut self.svm, &issuer, &m, &acct, usdc, &TOKEN_CLASSIC);
        }
        (who, acct)
    }

    /// Opens a backer account and deposits.
    fn back(&mut self, amount: u64) -> (Keypair, Pubkey) {
        let (who, acct) = self.new_funded(amount);
        let open = self.b_ix(
            backstop::accounts::OpenBacker {
                owner: who.pubkey(),
                config: self.b_config(),
                backer: self.backer(&who.pubkey()),
                system_program: SYSTEM,
            },
            backstop::instruction::OpenBacker {},
        );
        self.ok(&[open], &[&who]);
        if amount > 0 {
            let dep = self.b_ix(
                backstop::accounts::Deposit {
                    owner: who.pubkey(),
                    config: self.b_config(),
                    backer: self.backer(&who.pubkey()),
                    usdc_mint: self.usdc_mint,
                    owner_usdc: acct,
                    usdc_vault: self.b_vault(),
                    usdc_token_program: TOKEN_CLASSIC,
                },
                backstop::instruction::Deposit { amount },
            );
            self.ok(&[dep], &[&who]);
        }
        (who, acct)
    }

    fn ed25519_ix(&self, signer: &Keypair, message: &[u8]) -> Instruction {
        let signature = signer.sign_message(message);
        let (sig_off, pk_off, msg_off) = (16u16, 80u16, 112u16);
        let mut data = vec![1u8, 0u8];
        for v in [sig_off, 0, pk_off, 0, msg_off, message.len() as u16, 0] {
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

    /// Signs and records a verdict for a liquidation record.
    fn attest(&mut self, record: &Pubkey, borrower: &Pubkey, payout: u64) {
        let args = VerdictArgs {
            liquidation_record: *record,
            borrower: *borrower,
            payout,
            tier: 1,
            verdict_hash: [7; 32],
            deadline: self.now + 600,
        };
        let msg = encode_message(&backstop::ID, CLUSTER_DEVNET, &args);
        let oracle = self.oracle.insecure_clone();
        let admin = self.admin.insecure_clone();
        let ed = self.ed25519_ix(&oracle, &msg);
        let ix = self.b_ix(
            backstop::accounts::AttestVerdict {
                payer: admin.pubkey(),
                config: self.b_config(),
                attestation: bpda(&[ATTESTATION_SEED, record.as_ref()]),
                instructions_sysvar: IX_SYSVAR,
                system_program: SYSTEM,
            },
            backstop::instruction::AttestVerdict { args },
        );
        self.ok(&[ed, ix], &[&admin]);
    }

    fn pay_ix(
        &self,
        payer: &Pubkey,
        record: &Pubkey,
        borrower: &Pubkey,
        borrower_usdc: &Pubkey,
    ) -> Instruction {
        self.b_ix(
            backstop::accounts::PayWrongfulLiquidation {
                payer: *payer,
                config: self.b_config(),
                attestation: bpda(&[ATTESTATION_SEED, record.as_ref()]),
                liquidation_record: *record,
                receipt: bpda(&[CLAIM_SEED, record.as_ref()]),
                borrower: *borrower,
                borrower_usdc: *borrower_usdc,
                borrower_backer: self.backer(borrower),
                usdc_mint: self.usdc_mint,
                usdc_vault: self.b_vault(),
                usdc_token_program: TOKEN_CLASSIC,
                system_program: SYSTEM,
            },
            backstop::instruction::PayWrongfulLiquidation {},
        )
    }
}

// ------------------------------------------------------------------ environments

/// Both programs loaded (upgrade authority = admin for each) and mints created. Backstop NOT initialized.
fn base_uninitialized() -> Env {
    let mut svm = LiteSVM::new();
    svm.add_program(
        stock_vault::ID,
        include_bytes!("../../../target/deploy/stock_vault.so"),
    )
    .unwrap();
    svm.add_program(
        backstop::ID,
        include_bytes!("../../../target/deploy/backstop.so"),
    )
    .unwrap();
    let mut clock: Clock = svm.get_sysvar();
    clock.unix_timestamp = NOW;
    svm.set_sysvar(&clock);

    let (admin, feed, issuer, oracle, alice) = (
        Keypair::new(),
        Keypair::new(),
        Keypair::new(),
        Keypair::new(),
        Keypair::new(),
    );
    set_upgrade_authority(&mut svm, &stock_vault::ID, Some(&admin.pubkey()));
    set_upgrade_authority(&mut svm, &backstop::ID, Some(&admin.pubkey()));
    for k in [&admin, &feed, &issuer, &alice] {
        svm.airdrop(&k.pubkey(), 1_000_000_000_000).unwrap();
    }
    let coll = t22_mint(&mut svm, &admin, &issuer);
    let usdc = classic_mint(&mut svm, &admin, &issuer.pubkey());

    Env {
        svm,
        admin,
        feed,
        issuer,
        oracle,
        alice,
        coll_mint: coll.pubkey(),
        usdc_mint: usdc.pubkey(),
        alice_coll: Pubkey::default(),
        alice_usdc: Pubkey::default(),
        now: NOW,
    }
}

/// Both programs loaded, mints created, backstop initialized. No market yet.
fn base() -> Env {
    let mut env = base_uninitialized();
    let admin = env.admin.insecure_clone();
    let init = env.init_backstop_ix(&admin.pubkey(), programdata(&backstop::ID));
    env.ok(&[init], &[&admin]);
    env
}

impl Env {
    fn init_backstop_ix(&self, admin: &Pubkey, program_data: Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::InitializeBackstop {
                admin: *admin,
                program: backstop::ID,
                program_data,
                config: self.b_config(),
                usdc_mint: self.usdc_mint,
                usdc_vault: self.b_vault(),
                usdc_token_program: TOKEN_CLASSIC,
                system_program: SYSTEM,
            },
            backstop::instruction::InitializeBackstop {
                verdict_oracle: self.oracle.pubkey(),
                cluster_tag: CLUSTER_DEVNET,
                per_claim_cap_bps: CAP_BPS,
                withdraw_delay_secs: 86_400,
            },
        )
    }
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

/// `base`, plus a lending market with Alice borrowed to the limit against 10 shares.
fn with_loan() -> Env {
    let mut env = base();
    let (admin, alice, issuer, feed) = (
        env.admin.insecure_clone(),
        env.alice.insecure_clone(),
        env.issuer.insecure_clone(),
        env.feed.insecure_clone(),
    );
    let (coll, usdc) = (env.coll_mint, env.usdc_mint);

    let vcfg = env.v_ix(
        stock_vault::accounts::InitializeConfig {
            admin: admin.pubkey(),
            program: stock_vault::ID,
            program_data: programdata(&stock_vault::ID),
            config: vpda(&[VCONFIG_SEED]),
            system_program: SYSTEM,
        },
        stock_vault::instruction::InitializeConfig {
            feed_authority: feed.pubkey(),
        },
    );
    env.ok(&[vcfg], &[&admin]);

    let market = env.market();
    let create = env.v_ix(
        stock_vault::accounts::CreateMarket {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
            collateral_mint: coll,
            usdc_mint: usdc,
            market,
            collateral_vault: vpda(&[COLL_VAULT_SEED, market.as_ref()]),
            usdc_vault: vpda(&[USDC_VAULT_SEED, market.as_ref()]),
            collateral_token_program: TOKEN_2022,
            usdc_token_program: TOKEN_CLASSIC,
            system_program: SYSTEM,
        },
        stock_vault::instruction::CreateMarket { params: params() },
    );
    env.ok(&[create], &[&admin]);

    env.alice_coll = token_account(&mut env.svm, &alice, &coll, &alice.pubkey(), &TOKEN_2022);
    env.alice_usdc = token_account(&mut env.svm, &alice, &usdc, &alice.pubkey(), &TOKEN_CLASSIC);
    let (ac, au) = (env.alice_coll, env.alice_usdc);
    mint_to(
        &mut env.svm,
        &issuer,
        &coll,
        &ac,
        100 * ONE_SHARE,
        &TOKEN_2022,
    );

    // A supplier funds the market.
    let (bob, bob_usdc) = env.new_funded(100_000 * USDC);
    let open_sup = env.v_ix(
        stock_vault::accounts::OpenSupplier {
            owner: bob.pubkey(),
            market,
            supplier: vpda(&[SUPPLIER_SEED, market.as_ref(), bob.pubkey().as_ref()]),
            system_program: SYSTEM,
        },
        stock_vault::instruction::OpenSupplier {},
    );
    env.ok(&[open_sup], &[&bob]);
    let supply = env.v_ix(
        stock_vault::accounts::Supply {
            owner: bob.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
            market,
            supplier: vpda(&[SUPPLIER_SEED, market.as_ref(), bob.pubkey().as_ref()]),
            usdc_mint: usdc,
            owner_usdc: bob_usdc,
            usdc_vault: vpda(&[USDC_VAULT_SEED, market.as_ref()]),
            usdc_token_program: TOKEN_CLASSIC,
        },
        stock_vault::instruction::Supply {
            amount: 50_000 * USDC,
        },
    );
    env.ok(&[supply], &[&bob]);

    let open_pos = env.v_ix(
        stock_vault::accounts::OpenPosition {
            owner: alice.pubkey(),
            market,
            position: vpda(&[POSITION_SEED, market.as_ref(), alice.pubkey().as_ref()]),
            system_program: SYSTEM,
        },
        stock_vault::instruction::OpenPosition {},
    );
    env.ok(&[open_pos], &[&alice]);
    let deposit = env.v_ix(
        stock_vault::accounts::DepositCollateral {
            owner: alice.pubkey(),
            market,
            position: vpda(&[POSITION_SEED, market.as_ref(), alice.pubkey().as_ref()]),
            collateral_mint: coll,
            owner_collateral: ac,
            collateral_vault: vpda(&[COLL_VAULT_SEED, market.as_ref()]),
            collateral_token_program: TOKEN_2022,
        },
        stock_vault::instruction::DepositCollateral {
            amount: 10 * ONE_SHARE,
        },
    );
    env.ok(&[deposit], &[&alice]);
    env.push_price(PRICE);

    let mult_fp = (MULT * safu_core::MULT_SCALE as f64).floor() as u128;
    let value =
        safu_core::collateral::collateral_value(10 * ONE_SHARE, COLL_DECIMALS, mult_fp, PRICE)
            .unwrap();
    let limit = safu_core::lending::max_borrow(value, params().ltv_bps).unwrap();
    let borrow = env.v_ix(
        stock_vault::accounts::Borrow {
            owner: alice.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
            market,
            position: vpda(&[POSITION_SEED, market.as_ref(), alice.pubkey().as_ref()]),
            collateral_mint: coll,
            usdc_mint: usdc,
            owner_usdc: au,
            usdc_vault: vpda(&[USDC_VAULT_SEED, market.as_ref()]),
            usdc_token_program: TOKEN_CLASSIC,
        },
        stock_vault::instruction::Borrow { amount: limit },
    );
    env.ok(&[borrow], &[&alice]);
    env
}

/// Drives a real liquidation of Alice's position and returns the liquidator's keypair.
fn liquidate(env: &mut Env, liquidator: Option<&Keypair>, price_fraction: u64) -> Keypair {
    env.walk_price_to(PRICE * price_fraction / 100);
    let (liq, liq_usdc) = match liquidator {
        Some(k) => {
            let m = env.usdc_mint;
            let acct = token_account(&mut env.svm, k, &m, &k.pubkey(), &TOKEN_CLASSIC);
            let issuer = env.issuer.insecure_clone();
            mint_to(
                &mut env.svm,
                &issuer,
                &m,
                &acct,
                50_000 * USDC,
                &TOKEN_CLASSIC,
            );
            (k.insecure_clone(), acct)
        }
        None => {
            let (k, a) = env.new_funded(50_000 * USDC);
            (k, a)
        }
    };
    let m = env.usdc_mint;
    let liq_coll = token_account(
        &mut env.svm,
        &liq,
        &env.coll_mint.clone(),
        &liq.pubkey(),
        &TOKEN_2022,
    );
    let _ = m;
    let market = env.market();
    let seq = env.market_state().liq_seq;
    let alice = env.alice.pubkey();
    // Registered as the pool, so it may act at once; the grace rule is tested in the vault's own suite.
    let admin = env.admin.insecure_clone();
    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: liq.pubkey(),
        },
    );
    env.ok(&[register], &[&admin]);
    let ix = env.v_ix(
        stock_vault::accounts::Liquidate {
            payer: liq.pubkey(),
            liquidator: liq.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
            market,
            position: vpda(&[POSITION_SEED, market.as_ref(), alice.as_ref()]),
            record: env.record(seq),
            collateral_mint: env.coll_mint,
            usdc_mint: env.usdc_mint,
            liquidator_usdc: liq_usdc,
            liquidator_collateral: liq_coll,
            collateral_vault: vpda(&[COLL_VAULT_SEED, market.as_ref()]),
            usdc_vault: vpda(&[USDC_VAULT_SEED, market.as_ref()]),
            collateral_token_program: TOKEN_2022,
            usdc_token_program: TOKEN_CLASSIC,
            system_program: SYSTEM,
        },
        stock_vault::instruction::Liquidate {
            repay_amount: 50_000 * USDC,
        },
    );
    env.ok(&[ix], &[&liq]);
    liq
}

// ------------------------------------------------------------------ backer capital

#[test]
fn the_first_deposit_mints_shares_one_for_one() {
    let mut env = base();
    let (who, _) = env.back(1_000 * USDC);
    let c = env.config_state();
    assert_eq!(c.cash, 1_000 * USDC);
    assert_eq!(c.total_shares, (1_000 * USDC) as u128);
    assert_eq!(
        env.backer_state(&who.pubkey()).shares,
        (1_000 * USDC) as u128
    );
    assert_eq!(env.balance(&env.b_vault()), 1_000 * USDC);
}

#[test]
fn a_donation_into_the_backstop_does_not_move_the_share_price() {
    let mut env = base();
    let (first, _) = env.back(1_000 * USDC);

    // Capital is tracked internally, so tokens pushed straight at the vault are not backer value.
    let (issuer, usdc, vault) = (env.issuer.insecure_clone(), env.usdc_mint, env.b_vault());
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc,
        &vault,
        500 * USDC,
        &TOKEN_CLASSIC,
    );
    assert_eq!(env.balance(&vault), 1_500 * USDC);
    assert_eq!(env.config_state().cash, 1_000 * USDC);

    env.warp(60);
    let (second, _) = env.back(1_000 * USDC);
    assert_eq!(
        env.backer_state(&second.pubkey()).shares,
        env.backer_state(&first.pubkey()).shares,
        "the same deposit must buy the same shares after a donation"
    );
}

#[test]
fn a_withdrawal_waits_for_the_delay() {
    let mut env = base();
    let (who, acct) = env.back(1_000 * USDC);
    let shares = env.backer_state(&who.pubkey()).shares;

    let req = env.b_ix(
        backstop::accounts::BackerOnly {
            owner: who.pubkey(),
            backer: env.backer(&who.pubkey()),
        },
        backstop::instruction::RequestWithdraw { shares },
    );
    env.ok(&[req], &[&who]);

    let fin = env.b_ix(
        backstop::accounts::FinalizeWithdraw {
            owner: who.pubkey(),
            config: env.b_config(),
            backer: env.backer(&who.pubkey()),
            usdc_mint: env.usdc_mint,
            owner_usdc: acct,
            usdc_vault: env.b_vault(),
            usdc_token_program: TOKEN_CLASSIC,
        },
        backstop::instruction::FinalizeWithdraw {},
    );
    assert_program_error(
        env.send(std::slice::from_ref(&fin), &[&who]),
        VerdictError::WithdrawalNotReady,
    );

    env.warp(86_400);
    env.ok(&[fin], &[&who]);
    assert_eq!(env.balance(&acct), 1_000 * USDC);
    assert_eq!(env.config_state().cash, 0);
    assert_eq!(env.backer_state(&who.pubkey()).shares, 0);
}

#[test]
fn a_withdrawal_is_blocked_while_a_claim_is_outstanding() {
    let mut env = base();
    let (who, acct) = env.back(1_000 * USDC);
    let shares = env.backer_state(&who.pubkey()).shares;
    let req = env.b_ix(
        backstop::accounts::BackerOnly {
            owner: who.pubkey(),
            backer: env.backer(&who.pubkey()),
        },
        backstop::instruction::RequestWithdraw { shares },
    );
    env.ok(&[req], &[&who]);
    env.warp(86_400);

    // An attested verdict is a liability the remaining backers would otherwise be left holding.
    let record = Pubkey::new_unique();
    let borrower = Pubkey::new_unique();
    env.attest(&record, &borrower, 100 * USDC);
    assert_eq!(env.config_state().open_claims, 1);

    let fin = env.b_ix(
        backstop::accounts::FinalizeWithdraw {
            owner: who.pubkey(),
            config: env.b_config(),
            backer: env.backer(&who.pubkey()),
            usdc_mint: env.usdc_mint,
            owner_usdc: acct,
            usdc_vault: env.b_vault(),
            usdc_token_program: TOKEN_CLASSIC,
        },
        backstop::instruction::FinalizeWithdraw {},
    );
    assert_program_error(env.send(&[fin], &[&who]), VerdictError::ClaimsOutstanding);
}

#[test]
fn a_backer_cannot_queue_more_shares_than_they_hold() {
    let mut env = base();
    let (who, _) = env.back(1_000 * USDC);
    let shares = env.backer_state(&who.pubkey()).shares;
    let req = env.b_ix(
        backstop::accounts::BackerOnly {
            owner: who.pubkey(),
            backer: env.backer(&who.pubkey()),
        },
        backstop::instruction::RequestWithdraw { shares: shares + 1 },
    );
    assert_program_error(env.send(&[req], &[&who]), VerdictError::InsufficientBalance);
}

// ------------------------------------------------------------------ admin knobs

/// `set_admin` existed since 2c without a test of its own.
#[test]
fn rotating_the_backstop_admin_hands_over_control_and_retires_the_old_key() {
    let mut env = base();
    let old = env.admin.insecure_clone();
    let new = Keypair::new();
    env.svm.airdrop(&new.pubkey(), 1_000_000_000).unwrap();
    let set_admin = |env: &Env, signer: &Pubkey, to: Pubkey| {
        env.b_ix(
            backstop::accounts::AdminOnly {
                admin: *signer,
                config: env.b_config(),
            },
            backstop::instruction::SetAdmin { admin: to },
        )
    };
    let knobs = |env: &Env, signer: &Pubkey| {
        env.b_ix(
            backstop::accounts::AdminOnly {
                admin: *signer,
                config: env.b_config(),
            },
            backstop::instruction::SetBackstopParams {
                per_claim_cap_bps: 2_000,
                withdraw_delay_secs: 3_600,
            },
        )
    };

    let intruder = env.alice.insecure_clone();
    let ix = set_admin(&env, &intruder.pubkey(), intruder.pubkey());
    assert_program_error(env.send(&[ix], &[&intruder]), VerdictError::Unauthorized);
    let ix = set_admin(&env, &old.pubkey(), Pubkey::default());
    assert_program_error(env.send(&[ix], &[&old]), VerdictError::InvalidParams);

    let ix = set_admin(&env, &old.pubkey(), new.pubkey());
    env.ok(&[ix], &[&old]);
    let ix = knobs(&env, &old.pubkey());
    assert_program_error(env.send(&[ix], &[&old]), VerdictError::Unauthorized);
    let ix = knobs(&env, &new.pubkey());
    env.ok(&[ix], &[&new]);

    let data = env.svm.get_account(&env.b_config()).unwrap().data;
    let config = BackstopConfig::try_deserialize(&mut &data[..]).unwrap();
    assert_eq!(config.admin, new.pubkey());
    assert_eq!(config.per_claim_cap_bps, 2_000);
}

#[test]
fn only_the_admin_can_rotate_the_oracle_or_move_the_knobs() {
    let mut env = base();
    let intruder = env.alice.insecure_clone();
    let rotate = env.b_ix(
        backstop::accounts::AdminOnly {
            admin: intruder.pubkey(),
            config: env.b_config(),
        },
        backstop::instruction::SetVerdictOracle {
            verdict_oracle: intruder.pubkey(),
        },
    );
    assert_program_error(
        env.send(&[rotate], &[&intruder]),
        VerdictError::Unauthorized,
    );

    let knobs = env.b_ix(
        backstop::accounts::AdminOnly {
            admin: intruder.pubkey(),
            config: env.b_config(),
        },
        backstop::instruction::SetBackstopParams {
            per_claim_cap_bps: 5_000,
            withdraw_delay_secs: 0,
        },
    );
    assert_program_error(env.send(&[knobs], &[&intruder]), VerdictError::Unauthorized);
}

#[test]
fn rotating_the_oracle_retires_the_old_key() {
    // The verdict key is a throwaway Ed25519 key; a leak has to be recoverable without a redeploy.
    let mut env = base();
    let admin = env.admin.insecure_clone();
    let new_oracle = Keypair::new();
    let rotate = env.b_ix(
        backstop::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: env.b_config(),
        },
        backstop::instruction::SetVerdictOracle {
            verdict_oracle: new_oracle.pubkey(),
        },
    );
    env.ok(&[rotate], &[&admin]);
    assert_eq!(env.config_state().verdict_oracle, new_oracle.pubkey());

    env.warp(60);
    // A verdict signed by the retired key no longer verifies.
    let record = Pubkey::new_unique();
    let args = VerdictArgs {
        liquidation_record: record,
        borrower: Pubkey::new_unique(),
        payout: 10 * USDC,
        tier: 1,
        verdict_hash: [1; 32],
        deadline: env.now + 600,
    };
    let msg = encode_message(&backstop::ID, CLUSTER_DEVNET, &args);
    let old_oracle = env.oracle.insecure_clone();
    let ed = env.ed25519_ix(&old_oracle, &msg);
    let ix = env.b_ix(
        backstop::accounts::AttestVerdict {
            payer: admin.pubkey(),
            config: env.b_config(),
            attestation: bpda(&[ATTESTATION_SEED, record.as_ref()]),
            instructions_sysvar: IX_SYSVAR,
            system_program: SYSTEM,
        },
        backstop::instruction::AttestVerdict { args },
    );
    assert_program_error(
        env.send(&[ed, ix], &[&admin]),
        VerdictError::WrongVerdictSigner,
    );
}

#[test]
fn backstop_knobs_outside_their_hard_bounds_are_refused() {
    let mut env = base();
    let admin = env.admin.insecure_clone();
    for (cap, delay) in [
        (0u32, 0i64),
        (5_001, 0),
        (10_001, 0),
        (1_000, -1),
        (1_000, 31 * 86_400),
    ] {
        let ix = env.b_ix(
            backstop::accounts::AdminOnly {
                admin: admin.pubkey(),
                config: env.b_config(),
            },
            backstop::instruction::SetBackstopParams {
                per_claim_cap_bps: cap,
                withdraw_delay_secs: delay,
            },
        );
        assert_program_error(env.send(&[ix], &[&admin]), VerdictError::InvalidParams);
        env.warp(1);
    }
    let ok = env.b_ix(
        backstop::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: env.b_config(),
        },
        backstop::instruction::SetBackstopParams {
            per_claim_cap_bps: 2_000,
            withdraw_delay_secs: 3_600,
        },
    );
    env.ok(&[ok], &[&admin]);
    let c = env.config_state();
    assert_eq!(c.per_claim_cap_bps, 2_000);
    assert_eq!(c.withdraw_delay_secs, 3_600);
}

// ------------------------------------------------------------------ wrongful-liquidation payout

#[test]
fn a_wrongful_liquidation_is_paid_and_receipted() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    liquidate(&mut env, None, 75);

    let record_key = env.record(0);
    let record = env.record_state(0);
    let alice = env.alice.pubkey();
    assert_eq!(record.borrower, alice);

    // The verdict engine prices the loss off an independent reference; 500 USDC stands in here.
    let payout = 500 * USDC;
    env.warp(60);
    env.attest(&record_key, &alice, payout);
    assert_eq!(env.config_state().open_claims, 1);

    let before = env.balance(&env.alice_usdc);
    let cash_before = env.config_state().cash;
    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let ix = env.pay_ix(&payer_key, &record_key, &alice, &alice_usdc);
    env.ok(&[ix], &[&payer]);

    assert_eq!(env.balance(&alice_usdc) - before, payout, "paid 1:1");
    let r = env.receipt_state(&record_key);
    assert_eq!(r.attested, payout);
    assert_eq!(r.paid, payout);
    assert_eq!(r.borrower, alice);
    assert_eq!(r.liquidation_record, record_key);
    let c = env.config_state();
    assert_eq!(
        cash_before - c.cash,
        payout,
        "the payout comes out of backer capital"
    );
    assert_eq!(c.open_claims, 0, "the claim is settled");
}

#[test]
fn a_second_payout_for_the_same_liquidation_is_impossible() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    liquidate(&mut env, None, 75);
    let (record_key, alice) = (env.record(0), env.alice.pubkey());
    env.warp(60);
    env.attest(&record_key, &alice, 500 * USDC);

    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let ix = env.pay_ix(&payer_key, &record_key, &alice, &alice_usdc);
    env.ok(std::slice::from_ref(&ix), &[&payer]);

    env.warp(60);
    // The receipt account already exists, so `init` refuses — replaying pays nothing twice.
    assert!(
        env.send(&[ix], &[&payer]).is_err(),
        "a replayed payout must not succeed"
    );
}

#[test]
fn a_self_dealt_liquidation_is_never_covered() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    // Self-liquidation is legitimate deleveraging, so the vault allows it. It is simply not a
    // wrongful liquidation, and the refusal belongs here (spec 13c).
    let alice = env.alice.insecure_clone();
    liquidate(&mut env, Some(&alice), 75);

    let (record_key, alice_key) = (env.record(0), alice.pubkey());
    assert_eq!(env.record_state(0).liquidator, alice_key);
    env.warp(60);
    env.attest(&record_key, &alice_key, 500 * USDC);

    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let ix = env.pay_ix(&payer_key, &record_key, &alice_key, &alice_usdc);
    assert_program_error(
        env.send(&[ix], &[&payer]),
        VerdictError::SelfDealtLiquidation,
    );
}

#[test]
fn a_borrower_who_backs_the_pool_cannot_be_paid_from_it() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    liquidate(&mut env, None, 75);
    let (record_key, alice_key) = (env.record(0), env.alice.pubkey());
    let alice = env.alice.insecure_clone();

    // A4: Alice backs the very pool that would pay her.
    let open = env.b_ix(
        backstop::accounts::OpenBacker {
            owner: alice_key,
            config: env.b_config(),
            backer: env.backer(&alice_key),
            system_program: SYSTEM,
        },
        backstop::instruction::OpenBacker {},
    );
    env.ok(&[open], &[&alice]);
    let dep = env.b_ix(
        backstop::accounts::Deposit {
            owner: alice_key,
            config: env.b_config(),
            backer: env.backer(&alice_key),
            usdc_mint: env.usdc_mint,
            owner_usdc: env.alice_usdc,
            usdc_vault: env.b_vault(),
            usdc_token_program: TOKEN_CLASSIC,
        },
        backstop::instruction::Deposit { amount: 100 * USDC },
    );
    env.ok(&[dep], &[&alice]);

    env.warp(60);
    env.attest(&record_key, &alice_key, 500 * USDC);
    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let ix = env.pay_ix(&payer_key, &record_key, &alice_key, &alice_usdc);
    assert_program_error(env.send(&[ix], &[&payer]), VerdictError::BorrowerIsABacker);
}

#[test]
fn a_liquidation_taken_under_an_issuer_halt_is_never_covered() {
    // Spec 14a. The record's flag cannot be set through a live flow today, because `liquidate`
    // refuses while the market is halted — so the record is edited directly to exercise the
    // backstop's own refusal, which is defence in depth rather than dead weight.
    let mut env = with_loan();
    env.back(100_000 * USDC);
    liquidate(&mut env, None, 75);

    let record_key = env.record(0);
    let mut acc = env.svm.get_account(&record_key).unwrap();
    let mut record = LiquidationRecord::try_deserialize(&mut acc.data.as_slice()).unwrap();
    assert!(!record.issuer_halt, "a live flow cannot produce this");
    record.issuer_halt = true;
    let mut data = Vec::new();
    record.try_serialize(&mut data).unwrap();
    acc.data = data;
    env.svm.set_account(record_key, acc).unwrap();

    let alice = env.alice.pubkey();
    env.warp(60);
    env.attest(&record_key, &alice, 500 * USDC);
    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let ix = env.pay_ix(&payer_key, &record_key, &alice, &alice_usdc);
    assert_program_error(
        env.send(&[ix], &[&payer]),
        VerdictError::IssuerHaltedLiquidation,
    );
}

#[test]
fn the_attestation_must_match_the_record_supplied() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    liquidate(&mut env, None, 60);
    env.warp(60);
    liquidate(&mut env, None, 55);

    let (first, second) = (env.record(0), env.record(1));
    let alice = env.alice.pubkey();
    env.warp(60);
    env.attest(&first, &alice, 500 * USDC);

    // Attestation for the first liquidation, record account for the second.
    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let mut ix = env.pay_ix(&payer_key, &first, &alice, &alice_usdc);
    let (old_receipt, new_receipt) = (
        bpda(&[CLAIM_SEED, first.as_ref()]),
        bpda(&[CLAIM_SEED, second.as_ref()]),
    );
    for meta in ix.accounts.iter_mut() {
        if meta.pubkey == first {
            meta.pubkey = second;
        } else if meta.pubkey == old_receipt {
            // The receipt is seeded by the record, so it has to move with it -- otherwise Anchor's
            // seeds check fires first and the program's own cross-check is never exercised.
            meta.pubkey = new_receipt;
        }
    }
    assert_program_error(env.send(&[ix], &[&payer]), VerdictError::RecordMismatch);
}

#[test]
fn the_per_claim_cap_bounds_a_payout_and_the_shortfall_is_recorded() {
    let mut env = with_loan();
    // 1,000 USDC of backer capital, 10% per-claim cap -> 100 USDC is the most one claim can take.
    env.back(1_000 * USDC);
    liquidate(&mut env, None, 75);
    let (record_key, alice) = (env.record(0), env.alice.pubkey());
    env.warp(60);
    env.attest(&record_key, &alice, 500 * USDC);

    let before = env.balance(&env.alice_usdc);
    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let ix = env.pay_ix(&payer_key, &record_key, &alice, &alice_usdc);
    env.ok(&[ix], &[&payer]);

    let r = env.receipt_state(&record_key);
    assert_eq!(r.paid, 100 * USDC, "capped at 10% of backer capital");
    assert_eq!(r.attested, 500 * USDC, "what was owed stays on the record");
    assert!(
        r.attested > r.paid,
        "the shortfall is visible, not silently dropped"
    );
    assert_eq!(env.balance(&alice_usdc) - before, 100 * USDC);
}

#[test]
fn a_payout_lowers_the_share_price_for_backers() {
    let mut env = with_loan();
    let (who, _) = env.back(100_000 * USDC);
    let shares = env.backer_state(&who.pubkey()).shares;
    liquidate(&mut env, None, 75);
    let (record_key, alice) = (env.record(0), env.alice.pubkey());
    env.warp(60);
    env.attest(&record_key, &alice, 500 * USDC);
    let payer = env.admin.insecure_clone();
    let (alice_usdc, payer_key) = (env.alice_usdc, payer.pubkey());
    let ix = env.pay_ix(&payer_key, &record_key, &alice, &alice_usdc);
    env.ok(&[ix], &[&payer]);

    // Backers carry the cost of the cover they sold; that is the whole point of the pool.
    let c = env.config_state();
    let value = (shares * c.cash as u128 / c.total_shares) as u64;
    assert!(
        (99_500 * USDC..100_000 * USDC).contains(&value),
        "backer value {value} should have fallen by the payout"
    );
}

// ------------------------------------------------------------------ bad-debt cover

fn open_cover(env: &mut Env) {
    let payer = env.admin.insecure_clone();
    let ix = env.b_ix(
        backstop::accounts::OpenBadDebtCover {
            payer: payer.pubkey(),
            market: env.market(),
            cover: bpda(&[BAD_DEBT_SEED, env.market().as_ref()]),
            system_program: SYSTEM,
        },
        backstop::instruction::OpenBadDebtCover {},
    );
    env.ok(&[ix], &[&payer]);
}

fn cover_ix(env: &Env) -> Instruction {
    let market = env.market();
    env.b_ix(
        backstop::accounts::CoverBadDebt {
            config: env.b_config(),
            market,
            cover: bpda(&[BAD_DEBT_SEED, market.as_ref()]),
            market_usdc_vault: vpda(&[USDC_VAULT_SEED, market.as_ref()]),
            usdc_mint: env.usdc_mint,
            usdc_vault: env.b_vault(),
            usdc_token_program: TOKEN_CLASSIC,
        },
        backstop::instruction::CoverBadDebt {},
    )
}

fn absorb_ix(env: &Env) -> Instruction {
    let market = env.market();
    env.v_ix(
        stock_vault::accounts::AbsorbBadDebtCover {
            market,
            usdc_vault: vpda(&[USDC_VAULT_SEED, market.as_ref()]),
        },
        stock_vault::instruction::AbsorbBadDebtCover {},
    )
}

fn cover_state(env: &Env) -> BadDebtCover {
    let a = env
        .svm
        .get_account(&bpda(&[BAD_DEBT_SEED, env.market().as_ref()]))
        .unwrap();
    BadDebtCover::try_deserialize(&mut a.data.as_slice()).unwrap()
}

#[test]
fn bad_debt_is_reimbursed_and_the_market_absorbs_it() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    // Collateral worth less than the debt: seizing everything still leaves the loan short.
    liquidate(&mut env, None, 25);
    let bad = env.market_state().bad_debt;
    assert!(bad > 0, "the liquidation must have written debt off");
    assert_eq!(env.market_state().bad_debt_cumulative, bad);

    open_cover(&mut env);
    env.warp(60);
    let cash_before = env.config_state().cash;
    let ix = cover_ix(&env);
    env.ok(&[ix], &[&env.admin.insecure_clone()]);
    assert_eq!(cash_before - env.config_state().cash, bad);
    assert_eq!(cover_state(&env).total_paid, bad);

    // The market only recognises it through its own instruction, bounded by its bad debt.
    let market_cash_before = env.market_state().cash;
    env.warp(60);
    let ix = absorb_ix(&env);
    env.ok(&[ix], &[&env.admin.insecure_clone()]);
    let m = env.market_state();
    assert_eq!(m.cash - market_cash_before, bad, "suppliers are made whole");
    assert_eq!(m.bad_debt, 0);
    assert_eq!(m.bad_debt_cumulative, bad, "the history is not rewritten");
}

#[test]
fn repeat_cover_settles_only_the_remainder() {
    let mut env = with_loan();
    // Backer capital far smaller than the write-off, so one pass cannot settle it.
    env.back(100 * USDC);
    liquidate(&mut env, None, 25);
    let bad = env.market_state().bad_debt;
    assert!(bad > 100 * USDC, "the shortfall should exceed the pool");

    open_cover(&mut env);
    env.warp(60);
    env.ok(&[cover_ix(&env)], &[&env.admin.insecure_clone()]);
    // Capped at the per-call share of capital, not the whole balance.
    assert_eq!(cover_state(&env).total_paid, 10 * USDC);
    assert_eq!(env.config_state().cash, 90 * USDC);

    // Top the pool up; only the remainder moves, never the whole amount again.
    env.back(1_000_000 * USDC);
    env.warp(60);
    env.ok(&[cover_ix(&env)], &[&env.admin.insecure_clone()]);
    assert_eq!(
        cover_state(&env).total_paid,
        bad,
        "settled exactly once in total"
    );

    env.warp(60);
    assert_program_error(
        env.send(&[cover_ix(&env)], &[&env.admin.insecure_clone()]),
        VerdictError::NoBadDebt,
    );
}

#[test]
fn covering_bad_debt_can_never_drain_the_pool_to_nothing() {
    // The bug this pins: an uncapped `cover_bad_debt` took `min(owed, cash)`, so a large enough
    // write-off emptied the pool exactly while shares were still outstanding. The share price is
    // then 0/n -- undefined -- and no one can ever deposit again. The backstop was brickable.
    let mut env = with_loan();
    env.back(100 * USDC);
    liquidate(&mut env, None, 25);
    let bad = env.market_state().bad_debt;
    assert!(
        bad > 100 * USDC,
        "the write-off ({bad}) must exceed the 100 USDC pool, or nothing is being drained"
    );
    open_cover(&mut env);

    let admin = env.admin.insecure_clone();
    for _ in 0..40 {
        env.warp(60);
        if env.send(&[cover_ix(&env)], &[&admin]).is_err() {
            break;
        }
    }
    let c = env.config_state();
    assert!(c.total_shares > 0, "shares are still outstanding");
    assert!(c.cash > 0, "cash must never reach zero while shares exist");

    // And the pool is still usable: a fresh backer can price their deposit.
    env.warp(60);
    let (fresh, _) = env.back(1_000 * USDC);
    assert!(env.backer_state(&fresh.pubkey()).shares > 0);
}

#[test]
fn the_market_cannot_absorb_more_than_its_bad_debt() {
    let mut env = with_loan();
    env.back(1_000_000 * USDC);
    liquidate(&mut env, None, 25);
    let bad = env.market_state().bad_debt;
    open_cover(&mut env);
    env.warp(60);
    env.ok(&[cover_ix(&env)], &[&env.admin.insecure_clone()]);

    // Someone also donates to the market's vault. Absorption is bounded by the bad debt, so the
    // surplus stays unaccounted and cannot inflate the supply share price.
    let (issuer, usdc) = (env.issuer.insecure_clone(), env.usdc_mint);
    let market_vault = vpda(&[USDC_VAULT_SEED, env.market().as_ref()]);
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc,
        &market_vault,
        5_000 * USDC,
        &TOKEN_CLASSIC,
    );

    let before = env.market_state().cash;
    env.warp(60);
    env.ok(&[absorb_ix(&env)], &[&env.admin.insecure_clone()]);
    let m = env.market_state();
    assert_eq!(m.cash - before, bad, "only the write-off is recognised");
    assert_eq!(m.bad_debt, 0);

    // A second attempt finds nothing to recognise, donation or not.
    env.warp(60);
    assert_vault_error(
        env.send(&[absorb_ix(&env)], &[&env.admin.insecure_clone()]),
        stock_vault::errors::VaultError::ZeroAmount,
    );
}

// ------------------------------------------------------------------ initialization authority

#[test]
fn only_the_upgrade_authority_can_initialize_the_backstop() {
    let mut env = base_uninitialized();
    // Someone watching the deploy calls initialize first: refused, and nothing is created.
    let stranger = env.alice.insecure_clone();
    let ix = env.init_backstop_ix(&stranger.pubkey(), programdata(&backstop::ID));
    assert_program_error(
        env.send(&[ix], &[&stranger]),
        VerdictError::NotUpgradeAuthority,
    );
    assert!(env.svm.get_account(&env.b_config()).is_none());
    // The upgrade authority can, and becomes admin.
    let admin = env.admin.insecure_clone();
    let ix = env.init_backstop_ix(&admin.pubkey(), programdata(&backstop::ID));
    env.ok(&[ix], &[&admin]);
    let data = env.svm.get_account(&env.b_config()).unwrap().data;
    let config = BackstopConfig::try_deserialize(&mut &data[..]).unwrap();
    assert_eq!(config.admin, admin.pubkey());
}

#[test]
fn backstop_initialize_refuses_the_upgrade_authority_of_a_different_program() {
    // The attacker deploys a program they control and presents its ProgramData as ours.
    let mut env = base_uninitialized();
    let attacker = env.alice.insecure_clone();
    let theirs = Pubkey::new_unique();
    env.svm
        .add_program(theirs, include_bytes!("../../../target/deploy/backstop.so"))
        .unwrap();
    set_upgrade_authority(&mut env.svm, &theirs, Some(&attacker.pubkey()));
    let ix = env.init_backstop_ix(&attacker.pubkey(), programdata(&theirs));
    assert_program_error(
        env.send(&[ix], &[&attacker]),
        VerdictError::NotUpgradeAuthority,
    );
}
