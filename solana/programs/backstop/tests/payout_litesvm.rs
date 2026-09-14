//! Stage 2 phase 2: backer capital, v2 wrongful-liquidation claims (facts → gate → admission →
//! cooldown → stream → expiry) and bad-debt cover.
//!
//! The claim tests run the real cross-program path — a genuine liquidation in `stock_vault`
//! produces the `LiquidationRecord` the backstop then reads. Anchor's `Owner` check on that account
//! is the thing that makes a fabricated record impossible, so testing it against an injected struct
//! would prove nothing. The oracle-signed reference prices are chosen deliberately far from the
//! record's own liquidation price so `wrongful_loss` is exact and known ahead of each assertion.

use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_lang::{
    prelude::{Clock, Pubkey},
    solana_program::{instruction::Instruction, program_pack::Pack},
    AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas,
};
use backstop::{
    errors::VerdictError,
    state::{
        Backer, BackstopConfig, BadDebtCover, BorrowerClaims, Claim, ClaimStatus, InterestAbsorbed,
        Inventory, OverrideRequest, RevokedAttestation, BACKER_SEED, BAD_DEBT_SEED,
        BORROWER_CLAIMS_SEED, CLAIM_SEED, CONFIG_SEED, INTEREST_SEED, INVENTORY_SEED,
        INVENTORY_VAULT_SEED, OVERRIDE_SEED, REVOKE_SEED, USDC_VAULT_SEED as B_USDC_VAULT_SEED,
    },
    verdict::{encode_message, FactsArgs, CLUSTER_DEVNET, CLUSTER_MAINNET},
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
const GATE_SECS: i64 = 60 * 86_400;
const COOLDOWN_SECS: i64 = 7 * 86_400;
const STREAM_SECS: i64 = 45 * 86_400;
const CLAIM_WINDOW_SECS: i64 = 30 * 86_400;
const INACTIVITY_EXPIRY_SECS: i64 = 100 * 86_400;

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
        backer_interest_share_bps: 1_500,
    }
}

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    feed: Keypair,
    issuer: Keypair,
    oracle: Keypair,
    co_signer: Keypair,
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
    fn borrower_claims_pda(&self, borrower: &Pubkey) -> Pubkey {
        bpda(&[
            BORROWER_CLAIMS_SEED,
            self.market().as_ref(),
            borrower.as_ref(),
        ])
    }
    fn claim_pda(&self, record: &Pubkey) -> Pubkey {
        bpda(&[CLAIM_SEED, record.as_ref()])
    }
    fn revoked_pda(&self, record: &Pubkey, evidence_hash: &[u8; 32]) -> Pubkey {
        bpda(&[REVOKE_SEED, record.as_ref(), evidence_hash.as_ref()])
    }
    fn override_pda(&self, record: &Pubkey) -> Pubkey {
        bpda(&[OVERRIDE_SEED, record.as_ref()])
    }
    fn override_state(&self, record: &Pubkey) -> OverrideRequest {
        let a = self.svm.get_account(&self.override_pda(record)).unwrap();
        OverrideRequest::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn revoked_state(&self, record: &Pubkey, evidence_hash: &[u8; 32]) -> RevokedAttestation {
        let a = self
            .svm
            .get_account(&self.revoked_pda(record, evidence_hash))
            .unwrap();
        RevokedAttestation::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn inventory(&self, market: &Pubkey) -> Pubkey {
        bpda(&[INVENTORY_SEED, market.as_ref()])
    }
    fn inventory_vault(&self, market: &Pubkey) -> Pubkey {
        bpda(&[INVENTORY_VAULT_SEED, market.as_ref()])
    }
    fn inventory_state(&self, market: &Pubkey) -> Inventory {
        let a = self.svm.get_account(&self.inventory(market)).unwrap();
        Inventory::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn interest_absorbed_pda(&self, market: &Pubkey) -> Pubkey {
        bpda(&[INTEREST_SEED, market.as_ref()])
    }
    fn interest_absorbed_state(&self, market: &Pubkey) -> InterestAbsorbed {
        let a = self
            .svm
            .get_account(&self.interest_absorbed_pda(market))
            .unwrap();
        InterestAbsorbed::try_deserialize(&mut a.data.as_slice()).unwrap()
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

    /// Sends and returns (compute units consumed, serialized legacy transaction size in bytes).
    fn send_measured(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> (u64, usize) {
        let msg = Message::new_with_blockhash(
            ixs,
            Some(&signers[0].pubkey()),
            &self.svm.latest_blockhash(),
        );
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
        // Wire size: short-vec signature count (1 byte below 128) + 64 per signature + message.
        let size = 1 + 64 * tx.signatures.len() + tx.message.serialize().len();
        let meta = self.svm.send_transaction(tx).unwrap_or_else(|e| {
            panic!(
                "measured transaction failed: {:?} | {:?}",
                e.err, e.meta.logs
            )
        });
        (meta.compute_units_consumed, size)
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
    fn claim_state(&self, record: &Pubkey) -> Claim {
        let a = self.svm.get_account(&self.claim_pda(record)).unwrap();
        Claim::try_deserialize(&mut a.data.as_slice()).unwrap()
    }
    fn borrower_claims_state(&self, borrower: &Pubkey) -> BorrowerClaims {
        let a = self
            .svm
            .get_account(&self.borrower_claims_pda(borrower))
            .unwrap();
        BorrowerClaims::try_deserialize(&mut a.data.as_slice()).unwrap()
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

    /// Flushes the TWAP ring of any sample that held across a large time warp. A single sample's
    /// TWAP weight is its held duration — the gap to whatever sample replaced it — so one print
    /// spanning a 60-day warp outweighs thousands of normal 600s prints and pins the TWAP near its
    /// price indefinitely. `walk_price_to`'s small steps then breach the deviation cap against that
    /// stale anchor long before reaching a real crash price. Re-printing the unchanged price
    /// `TWAP_SLOTS` times, evenly spaced, evicts every such sample from the ring before the walk.
    fn reanchor_price(&mut self) {
        const TWAP_SLOTS: usize = 16;
        let price = self.market_state().price.last_price;
        for _ in 0..TWAP_SLOTS {
            self.warp(600);
            self.push_price(price);
        }
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

    fn open_borrower_claims(&mut self, borrower: &Pubkey) {
        let admin = self.admin.insecure_clone();
        let ix = self.b_ix(
            backstop::accounts::OpenBorrowerClaims {
                payer: admin.pubkey(),
                market: self.market(),
                borrower: *borrower,
                borrower_claims: self.borrower_claims_pda(borrower),
                system_program: SYSTEM,
            },
            backstop::instruction::OpenBorrowerClaims {},
        );
        self.ok(&[ix], &[&admin]);
    }

    /// Builds `[ed25519, submit_facts]` for one liquidation record. Opens the borrower's claim
    /// tracker first if it does not exist yet.
    fn submit_facts_ixs(
        &mut self,
        record: &Pubkey,
        borrower: &Pubkey,
        ref_at_liq: u64,
        ref_after: u64,
    ) -> [Instruction; 2] {
        let after_ts = self.now + 3_600;
        self.submit_facts_ixs_after(record, borrower, ref_at_liq, ref_after, after_ts)
    }

    fn submit_facts_ixs_after(
        &mut self,
        record: &Pubkey,
        borrower: &Pubkey,
        ref_at_liq: u64,
        ref_after: u64,
        after_ts: i64,
    ) -> [Instruction; 2] {
        if self
            .svm
            .get_account(&self.borrower_claims_pda(borrower))
            .is_none()
        {
            self.open_borrower_claims(borrower);
        }
        let existing = self.borrower_claims_state(borrower).open;
        let admin = self.admin.insecure_clone();
        let existing_arg = if existing == Pubkey::default() {
            admin.pubkey()
        } else {
            existing
        };
        let args = FactsArgs {
            liquidation_record: *record,
            borrower: *borrower,
            ref_at_liq,
            ref_after,
            after_ts,
            evidence_hash: [7; 32],
            deadline: self.now + 600,
        };
        let msg = encode_message(&backstop::ID, CLUSTER_DEVNET, &args);
        let oracle = self.oracle.insecure_clone();
        let ed = self.ed25519_ix(&oracle, &msg);
        let ix = self.b_ix(
            backstop::accounts::SubmitFacts {
                payer: admin.pubkey(),
                config: self.b_config(),
                market: self.market(),
                liquidation_record: *record,
                borrower: *borrower,
                borrower_claims: self.borrower_claims_pda(borrower),
                existing_claim: existing_arg,
                claim: self.claim_pda(record),
                payout_backer: self.backer(borrower),
                revoked: self.revoked_pda(record, &args.evidence_hash),
                instructions_sysvar: IX_SYSVAR,
                system_program: SYSTEM,
            },
            backstop::instruction::SubmitFacts { args },
        );
        [ed, ix]
    }

    /// Submits facts and expects the transaction to succeed (the claim may still end up `Denied`
    /// on-chain — that is a successful submission, not a failed one).
    fn submit_facts(
        &mut self,
        record: &Pubkey,
        borrower: &Pubkey,
        ref_at_liq: u64,
        ref_after: u64,
    ) {
        let ixs = self.submit_facts_ixs(record, borrower, ref_at_liq, ref_after);
        let admin = self.admin.insecure_clone();
        self.ok(&ixs, &[&admin]);
    }

    fn unlock_claim_ix(&self, record: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::UpdateClaim {
                config: self.b_config(),
                claim: self.claim_pda(record),
            },
            backstop::instruction::UnlockClaim {},
        )
    }
    fn try_release_queued_ix(&self, record: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::UpdateClaim {
                config: self.b_config(),
                claim: self.claim_pda(record),
            },
            backstop::instruction::TryReleaseQueued {},
        )
    }
    fn expire_queued_ix(&self, record: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::UpdateClaim {
                config: self.b_config(),
                claim: self.claim_pda(record),
            },
            backstop::instruction::ExpireQueued {},
        )
    }
    fn expire_stale_ix(&self, record: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::UpdateClaim {
                config: self.b_config(),
                claim: self.claim_pda(record),
            },
            backstop::instruction::ExpireStale {},
        )
    }
    fn claim_stream_ix(&self, record: &Pubkey, payout_usdc: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::ClaimStream {
                config: self.b_config(),
                claim: self.claim_pda(record),
                usdc_mint: self.usdc_mint,
                payout_usdc: *payout_usdc,
                usdc_vault: self.b_vault(),
                usdc_token_program: TOKEN_CLASSIC,
            },
            backstop::instruction::ClaimStream {},
        )
    }
    fn cancel_claim_ix(
        &self,
        admin: &Pubkey,
        record: &Pubkey,
        borrower: &Pubkey,
        reason_code: u16,
    ) -> Instruction {
        self.b_ix(
            backstop::accounts::CancelClaim {
                admin: *admin,
                config: self.b_config(),
                claim: self.claim_pda(record),
                market: self.market(),
                borrower_claims: self.borrower_claims_pda(borrower),
            },
            backstop::instruction::CancelClaim { reason_code },
        )
    }
    fn suspend_claim_ix(&self, admin: &Pubkey, record: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::SuspendClaim {
                admin: *admin,
                config: self.b_config(),
                claim: self.claim_pda(record),
            },
            backstop::instruction::SuspendClaim {},
        )
    }
    fn unsuspend_claim_ix(&self, admin: &Pubkey, record: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::SuspendClaim {
                admin: *admin,
                config: self.b_config(),
                claim: self.claim_pda(record),
            },
            backstop::instruction::UnsuspendClaim {},
        )
    }
    fn revoke_attestation_ix(
        &self,
        admin: &Pubkey,
        record: &Pubkey,
        evidence_hash: [u8; 32],
    ) -> Instruction {
        self.b_ix(
            backstop::accounts::RevokeAttestation {
                admin: *admin,
                config: self.b_config(),
                revoked: self.revoked_pda(record, &evidence_hash),
                system_program: SYSTEM,
            },
            backstop::instruction::RevokeAttestation {
                liquidation_record: *record,
                evidence_hash,
            },
        )
    }
    fn set_claim_timing_ix(&self, admin: &Pubkey, t: [i64; 5]) -> Instruction {
        self.b_ix(
            backstop::accounts::AdminOnly {
                admin: *admin,
                config: self.b_config(),
            },
            backstop::instruction::SetClaimTiming {
                gate_secs: t[0],
                cooldown_secs: t[1],
                stream_secs: t[2],
                inactivity_secs: t[3],
                min_after_wait_secs: t[4],
            },
        )
    }
    fn set_co_signer_ix(&self, admin: &Pubkey, co_signer: Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::AdminOnly {
                admin: *admin,
                config: self.b_config(),
            },
            backstop::instruction::SetCoSigner { co_signer },
        )
    }
    fn open_override_ix(&self, payer: &Pubkey, record: &Pubkey) -> Instruction {
        self.b_ix(
            backstop::accounts::OpenOverride {
                payer: *payer,
                liquidation_record: *record,
                override_request: self.override_pda(record),
                system_program: SYSTEM,
            },
            backstop::instruction::OpenOverride {},
        )
    }
    fn approve_override_ix(
        &self,
        caller: &Pubkey,
        record: &Pubkey,
        borrower: &Pubkey,
        ref_at_liq: u64,
    ) -> Instruction {
        self.b_ix(
            backstop::accounts::ApproveOverride {
                caller: *caller,
                config: self.b_config(),
                market: self.market(),
                liquidation_record: *record,
                claim: self.claim_pda(record),
                override_request: self.override_pda(record),
                payout_backer: self.backer(borrower),
            },
            backstop::instruction::ApproveOverride { ref_at_liq },
        )
    }
    fn open_inventory_ix(&self, payer: &Pubkey) -> Instruction {
        let market = self.market();
        self.b_ix(
            backstop::accounts::OpenInventory {
                payer: *payer,
                market,
                inventory: self.inventory(&market),
                collateral_mint: self.coll_mint,
                config: self.b_config(),
                inventory_vault: self.inventory_vault(&market),
                collateral_token_program: TOKEN_2022,
                system_program: SYSTEM,
            },
            backstop::instruction::OpenInventory {},
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn pool_liquidate_ix(
        &self,
        payer: &Pubkey,
        crank_usdc: &Pubkey,
        seq: u64,
        borrower: &Pubkey,
        repay_amount: u64,
    ) -> Instruction {
        let market = self.market();
        self.b_ix(
            backstop::accounts::PoolLiquidate {
                payer: *payer,
                config: self.b_config(),
                vault_config: vpda(&[VCONFIG_SEED]),
                market,
                position: vpda(&[POSITION_SEED, market.as_ref(), borrower.as_ref()]),
                record: self.record(seq),
                collateral_mint: self.coll_mint,
                usdc_mint: self.usdc_mint,
                pool_usdc_vault: self.b_vault(),
                inventory: self.inventory(&market),
                inventory_vault: self.inventory_vault(&market),
                vault_collateral_vault: vpda(&[COLL_VAULT_SEED, market.as_ref()]),
                vault_usdc_vault: vpda(&[USDC_VAULT_SEED, market.as_ref()]),
                crank_usdc: *crank_usdc,
                collateral_token_program: TOKEN_2022,
                usdc_token_program: TOKEN_CLASSIC,
                system_program: SYSTEM,
                stock_vault_program: stock_vault::ID,
            },
            backstop::instruction::PoolLiquidate { repay_amount },
        )
    }
    fn write_off_inventory_ix(&self, admin: &Pubkey) -> Instruction {
        let market = self.market();
        self.b_ix(
            backstop::accounts::WriteOffInventory {
                admin: *admin,
                config: self.b_config(),
                market,
                collateral_mint: self.coll_mint,
                inventory: self.inventory(&market),
            },
            backstop::instruction::WriteOffInventory {},
        )
    }
    fn open_interest_absorbed_ix(&self, payer: &Pubkey) -> Instruction {
        let market = self.market();
        self.b_ix(
            backstop::accounts::OpenInterestAbsorbed {
                payer: *payer,
                market,
                interest_absorbed: self.interest_absorbed_pda(&market),
                system_program: SYSTEM,
            },
            backstop::instruction::OpenInterestAbsorbed {},
        )
    }
    fn absorb_interest_ix(&self) -> Instruction {
        let market = self.market();
        self.b_ix(
            backstop::accounts::AbsorbInterest {
                config: self.b_config(),
                market,
                interest_absorbed: self.interest_absorbed_pda(&market),
                pool_usdc_vault: self.b_vault(),
                usdc_token_program: TOKEN_CLASSIC,
            },
            backstop::instruction::AbsorbInterest {},
        )
    }
    fn pay_backer_interest_ix(&self) -> Instruction {
        self.v_ix(
            stock_vault::accounts::PayBackerInterest {
                config: vpda(&[VCONFIG_SEED]),
                market: self.market(),
                usdc_mint: self.usdc_mint,
                usdc_vault: vpda(&[USDC_VAULT_SEED, self.market().as_ref()]),
                pool_usdc_vault: self.b_vault(),
                usdc_token_program: TOKEN_CLASSIC,
            },
            stock_vault::instruction::PayBackerInterest {},
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

    let (admin, feed, issuer, oracle, co_signer, alice) = (
        Keypair::new(),
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
        co_signer,
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
                co_signer: self.co_signer.pubkey(),
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
            cluster_tag: CLUSTER_DEVNET,
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

/// A reference price far enough from `record`'s liquidation price that both check (a) and check
/// (b) fire against it, plus the exact `wrongful_loss` it produces.
fn wrongful_ref_and_loss(record: &LiquidationRecord) -> (u64, u64) {
    // The undisturbed price the feed should have shown throughout — `PRICE` itself, well outside a
    // 5% band of any crashed liquidation price used in these tests.
    let ref_price = PRICE;
    let loss = safu_core::loss::wrongful_loss(
        record.seized_raw,
        record.collateral_decimals,
        record.multiplier_fp,
        ref_price,
        record.debt_repaid,
    )
    .unwrap();
    (ref_price, loss)
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
fn a_withdrawal_is_blocked_only_up_to_the_reserved_amount() {
    let mut env = with_loan();
    let (who, acct) = env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    env.submit_facts(&record_key, &env.alice.pubkey(), ref_price, ref_price + 1);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Active, "old loan admits at once");
    assert_eq!(claim.loss, loss);
    assert_eq!(env.config_state().reserved_total, loss);

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
    // Pulling every share would take reserved capital with it — refused.
    assert_program_error(env.send(&[fin], &[&who]), VerdictError::InsufficientBalance);

    // Requesting only the unreserved portion succeeds.
    let cash = env.config_state().cash;
    let available = cash - loss;
    let partial_shares = shares * available as u128 / cash as u128;
    let req2 = env.b_ix(
        backstop::accounts::BackerOnly {
            owner: who.pubkey(),
            backer: env.backer(&who.pubkey()),
        },
        backstop::instruction::RequestWithdraw {
            shares: partial_shares,
        },
    );
    env.ok(&[req2], &[&who]);
    env.warp(86_400);
    let fin2 = env.b_ix(
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
    env.ok(&[fin2], &[&who]);
    assert!(env.config_state().cash >= loss, "the reservation survives");
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
    let mut env = with_loan();
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let (ref_price, _) = wrongful_ref_and_loss(&record);

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
    // Facts signed by the retired key no longer verify.
    let ixs = env.submit_facts_ixs(&record_key, &env.alice.pubkey(), ref_price, ref_price + 1);
    assert_program_error(env.send(&ixs, &[&admin]), VerdictError::WrongVerdictSigner);
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

// ------------------------------------------------------------------ v2 claims: gate, admission, queue

#[test]
fn a_young_loan_claim_is_held_then_released_at_day_sixty() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    // Liquidated right away: the loan is only minutes old.
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);

    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::PendingTime);
    assert_eq!(claim.loss, loss);
    assert_eq!(claim.releasable_at, record.borrow_age_ts + GATE_SECS);
    assert_eq!(
        env.config_state().reserved_total,
        0,
        "held claims reserve nothing"
    );

    let unlock = env.unlock_claim_ix(&record_key);
    let admin = env.admin.insecure_clone();
    assert_program_error(
        env.send(std::slice::from_ref(&unlock), &[&admin]),
        VerdictError::GateNotElapsed,
    );

    env.warp(GATE_SECS + 1);
    env.ok(&[unlock], &[&admin]);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Active);
    assert_eq!(env.config_state().reserved_total, loss);
}

#[test]
fn a_claim_over_the_admission_cap_queues_then_releases_once_it_fits() {
    let mut env = with_loan();
    // 1,000 USDC pool, 10% per-claim cap -> 100 USDC is the most one claim can be admitted for.
    env.back(1_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let alice = env.alice.pubkey();

    // A reference far enough above PRICE that the loss clears 100 USDC.
    let big_ref = PRICE * 5;
    let loss = safu_core::loss::wrongful_loss(
        record.seized_raw,
        record.collateral_decimals,
        record.multiplier_fp,
        big_ref,
        record.debt_repaid,
    )
    .unwrap();
    assert!(
        loss > 100 * USDC,
        "the scenario must actually exceed the cap"
    );
    env.submit_facts(&record_key, &alice, big_ref, big_ref + 1);

    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Queued);
    assert_eq!(claim.loss, loss);
    assert_eq!(
        env.config_state().reserved_total,
        0,
        "queued claims reserve nothing"
    );

    let admin = env.admin.insecure_clone();
    let retry = env.try_release_queued_ix(&record_key);
    env.ok(std::slice::from_ref(&retry), &[&admin]);
    assert_eq!(
        env.claim_state(&record_key).status,
        ClaimStatus::Queued,
        "still short of the cap"
    );

    // Top up so the loss clears both the 10% per-claim cap and the 25% admission-day band, with
    // headroom — a fixed top-up would silently under-shoot if `big_ref` is tuned differently later.
    let cash_now = env.config_state().cash;
    let cash_needed = loss.saturating_mul(11); // 10% cap is the binding constraint; +1x margin
    if cash_needed > cash_now {
        env.back(cash_needed - cash_now);
    }
    env.warp(1); // fresh blockhash: the retry instruction is otherwise byte-identical to the last
    env.ok(&[retry], &[&admin]);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Active);
    assert_eq!(env.config_state().reserved_total, loss);
}

#[test]
fn a_queued_claim_expires_at_the_claim_window() {
    let mut env = with_loan();
    env.back(1_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record_key = env.record(0);
    let big_ref = PRICE * 5;
    env.submit_facts(&record_key, &env.alice.pubkey(), big_ref, big_ref + 1);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Queued);

    let admin = env.admin.insecure_clone();
    let expire = env.expire_queued_ix(&record_key);
    assert_program_error(
        env.send(std::slice::from_ref(&expire), &[&admin]),
        VerdictError::NotYetExpired,
    );

    env.warp(CLAIM_WINDOW_SECS + 1);
    env.ok(&[expire], &[&admin]);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Expired);
    assert_eq!(env.config_state().reserved_total, 0);
}

// ------------------------------------------------------------------ v2 claims: cooldown, stream, outflow, inactivity

#[test]
fn a_claim_streams_linearly_and_completes() {
    let mut env = with_loan();
    env.back(1_000_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Active);

    let admin = env.admin.insecure_clone();
    let stream = env.claim_stream_ix(&record_key, &env.alice_usdc);
    assert_program_error(
        env.send(std::slice::from_ref(&stream), &[&admin]),
        VerdictError::CooldownNotElapsed,
    );

    env.warp(COOLDOWN_SECS + STREAM_SECS / 2);
    let before = env.balance(&env.alice_usdc);
    env.ok(std::slice::from_ref(&stream), &[&admin]);
    let half = env.balance(&env.alice_usdc) - before;
    assert!(
        half > loss * 45 / 100 && half < loss * 55 / 100,
        "roughly half the loss should have vested: {half} of {loss}"
    );
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Active);

    env.warp(STREAM_SECS / 2 + 1);
    env.ok(&[stream], &[&admin]);
    assert_eq!(
        env.balance(&env.alice_usdc) - before,
        loss,
        "fully streamed"
    );
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Completed);
    assert_eq!(claim.streamed, loss);
    assert_eq!(env.config_state().reserved_total, 0);
}

#[test]
fn the_outflow_cap_throttles_a_big_claim_across_days() {
    let mut env = with_loan();
    // 1,000,000 USDC pool: 5% daily outflow band = 50,000 USDC, 10% per-claim cap = 100,000 USDC.
    env.back(1_000_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);

    // Solve for a reference price landing the loss strictly between the two bands (75,000 USDC
    // target), so the outflow cap is deterministically the one that binds — not a maybe.
    // `wrongful_loss` is linear in the reference price: loss(k·PRICE) = k·(loss(PRICE)+debt) − debt.
    let (base_ref, base_loss) = wrongful_ref_and_loss(&record);
    let target_loss = 75_000u128 * USDC as u128;
    let k_num = target_loss + record.debt_repaid as u128;
    let k_den = base_loss as u128 + record.debt_repaid as u128;
    let big_ref = u64::try_from((base_ref as u128 * k_num) / k_den).unwrap();
    let loss = safu_core::loss::wrongful_loss(
        record.seized_raw,
        record.collateral_decimals,
        record.multiplier_fp,
        big_ref,
        record.debt_repaid,
    )
    .unwrap();
    assert!(
        loss > 50_000 * USDC && loss < 100_000 * USDC,
        "loss ({loss}) must sit strictly between the 5% outflow band and the 10% per-claim cap for \
         this test to actually exercise the throttle"
    );

    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, big_ref, big_ref + 1);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Active);

    env.warp(COOLDOWN_SECS + STREAM_SECS); // fully vested at first pull
    let admin = env.admin.insecure_clone();
    let stream = env.claim_stream_ix(&record_key, &env.alice_usdc);
    let before = env.balance(&env.alice_usdc);
    env.ok(std::slice::from_ref(&stream), &[&admin]);
    let first_pull = env.balance(&env.alice_usdc) - before;
    let daily_cap = env
        .config_state()
        .cash
        .max(env.claim_state(&record_key).snapshot_cash)
        / 20; // 5%
    assert!(
        first_pull <= daily_cap + 1,
        "one day cannot exceed the outflow band: pulled {first_pull}, cap {daily_cap}"
    );
    assert!(
        first_pull < loss,
        "the band must actually have throttled this claim: pulled {first_pull} of {loss}"
    );
    assert_eq!(
        env.claim_state(&record_key).status,
        ClaimStatus::Active,
        "a throttled claim is not done in one call"
    );

    // A second pull the same day gets nothing more.
    env.warp(1); // fresh blockhash: otherwise byte-identical to the last, already-processed tx
    assert_program_error(
        env.send(std::slice::from_ref(&stream), &[&admin]),
        VerdictError::NothingToPay,
    );

    // Next day, the rest becomes available and the claim completes.
    env.warp(86_400);
    env.ok(&[stream], &[&admin]);
    assert_eq!(env.balance(&env.alice_usdc) - before, loss);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Completed);
}

#[test]
fn an_uncollected_claim_expires_after_the_inactivity_window() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    assert_eq!(env.config_state().reserved_total, loss);

    let admin = env.admin.insecure_clone();
    let expire = env.expire_stale_ix(&record_key);
    assert_program_error(
        env.send(std::slice::from_ref(&expire), &[&admin]),
        VerdictError::NotYetStale,
    );

    env.warp(INACTIVITY_EXPIRY_SECS + 1);
    env.ok(&[expire], &[&admin]);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Expired);
    assert_eq!(
        env.config_state().reserved_total,
        0,
        "the unpaid remainder returns to the backstop"
    );
}

// ------------------------------------------------------------------ v2 claims: structural refusals

#[test]
fn a_second_submission_for_the_same_liquidation_is_impossible() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);

    env.warp(60);
    // The claim account already exists, so `init` refuses — replaying cannot open a second claim.
    let ixs = env.submit_facts_ixs(&record_key, &alice, ref_price, ref_price + 1);
    let admin = env.admin.insecure_clone();
    assert!(
        env.send(&ixs, &[&admin]).is_err(),
        "a replayed submission must not succeed"
    );
}

#[test]
fn a_self_dealt_liquidation_is_never_covered() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    // Self-liquidation is legitimate deleveraging, so the vault allows it. It is simply not a
    // wrongful liquidation, and the refusal belongs here (spec 13c).
    let alice = env.alice.insecure_clone();
    liquidate(&mut env, Some(&alice), 75);

    let record = env.record_state(0);
    let record_key = env.record(0);
    assert_eq!(record.liquidator, alice.pubkey());
    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let ixs = env.submit_facts_ixs(&record_key, &alice.pubkey(), ref_price, ref_price + 1);
    let admin = env.admin.insecure_clone();
    assert_program_error(
        env.send(&ixs, &[&admin]),
        VerdictError::SelfDealtLiquidation,
    );
}

#[test]
fn a_borrower_who_backs_the_pool_cannot_be_paid_from_it() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let alice_key = env.alice.pubkey();
    let alice = env.alice.insecure_clone();

    // A4, applied to the payout: Alice backs the very pool that would pay her.
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

    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let ixs = env.submit_facts_ixs(&record_key, &alice_key, ref_price, ref_price + 1);
    let admin = env.admin.insecure_clone();
    assert_program_error(env.send(&ixs, &[&admin]), VerdictError::BorrowerIsABacker);
}

#[test]
fn a_liquidation_taken_under_an_issuer_halt_is_never_covered() {
    // Spec 14a. The record's flag cannot be set through a live flow today, because `liquidate`
    // refuses while the market is halted — so the record is edited directly to exercise the
    // backstop's own refusal, which is defence in depth rather than dead weight.
    let mut env = with_loan();
    env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
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

    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    let ixs = env.submit_facts_ixs(&record_key, &alice, ref_price, ref_price + 1);
    let admin = env.admin.insecure_clone();
    assert_program_error(
        env.send(&ixs, &[&admin]),
        VerdictError::IssuerHaltedLiquidation,
    );
}

#[test]
fn the_facts_must_match_the_record_supplied() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 60);
    env.warp(60);
    liquidate(&mut env, None, 55);

    let (first, second) = (env.record(0), env.record(1));
    let alice = env.alice.pubkey();
    let record0 = env.record_state(0);
    let (ref_price, _) = wrongful_ref_and_loss(&record0);

    // Facts signed for the first liquidation, but the `liquidation_record` account swapped for the
    // second — `args.liquidation_record` still says `first`, so the cross-check must catch it.
    let mut ixs = env.submit_facts_ixs(&first, &alice, ref_price, ref_price + 1);
    for meta in ixs[1].accounts.iter_mut() {
        if meta.pubkey == first {
            meta.pubkey = second;
        }
    }
    let admin = env.admin.insecure_clone();
    assert_program_error(env.send(&ixs, &[&admin]), VerdictError::RecordMismatch);
}

#[test]
fn a_borrower_cannot_open_a_second_claim_while_one_is_unresolved() {
    let mut env = with_loan();
    env.back(100_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 60);
    let record = env.record_state(0);
    let record_key = env.record(0);
    let alice = env.alice.pubkey();
    // Not actually wrong: this submission is denied, but denial is a terminal status, so it frees
    // the slot for a later real claim.
    env.submit_facts(&record_key, &alice, record.price_fp, record.price_fp + 1);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Denied);

    env.warp(60);
    liquidate(&mut env, None, 40);
    let second_key = env.record(1);
    let record1 = env.record_state(1);
    let (ref_price, _) = wrongful_ref_and_loss(&record1);
    // Slot is free (the first is terminal) — the second submission is accepted.
    env.submit_facts(&second_key, &alice, ref_price, ref_price + 1);
    assert_eq!(
        env.claim_state(&second_key).status,
        ClaimStatus::Active,
        "the second claim admits normally"
    );
    assert_eq!(
        env.borrower_claims_state(&alice).open,
        env.claim_pda(&second_key)
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

// ------------------------------------------------------------------ phase 3: pool-as-liquidator
//
// These are the only tests in the suite that exercise pool_liquidate/buy_inventory through a real
// CPI into stock_vault::liquidate -- everything else about the CPI's account wiring is checked by
// the compiler (Anchor's generated stock_vault::cpi::accounts::Liquidate), but only a real LiteSVM
// run proves the signer seeds, the account list order, and the vault's own guards actually agree.

#[test]
fn pool_liquidate_seizes_collateral_into_inventory_and_pays_a_crank_fee() {
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let market = env.market();
    let alice = env.alice.pubkey();

    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: env.b_config(),
        },
    );
    env.ok(&[register], &[&admin]);

    env.back(100_000 * USDC);
    let open_inv = env.open_inventory_ix(&admin.pubkey());
    env.ok(&[open_inv], &[&admin]);

    env.walk_price_to(PRICE * 75 / 100);

    let (crank, crank_usdc) = env.new_funded(0);
    let seq = env.market_state().liq_seq;
    let cash_before = env.config_state().cash;
    let ix = env.pool_liquidate_ix(&crank.pubkey(), &crank_usdc, seq, &alice, 50_000 * USDC);
    env.ok(&[ix], &[&crank]);

    let inv = env.inventory_state(&market);
    assert!(inv.raw > 0, "the pool should have seized some collateral");
    assert!(inv.cost_total > 0);
    assert_eq!(inv.last_acquired_at, env.now);

    let record_data = env.svm.get_account(&env.record(seq)).unwrap().data;
    let record = LiquidationRecord::try_deserialize(&mut record_data.as_slice()).unwrap();
    assert_eq!(record.liquidator, env.b_config());
    assert_eq!(record.seized_raw, inv.raw);
    assert_eq!(record.debt_repaid, inv.cost_total);

    let config = env.config_state();
    assert_eq!(config.inventory_cost_total, inv.cost_total);
    assert_eq!(config.liq_spent_today, record.debt_repaid);

    let crank_fee = env.balance(&crank_usdc);
    assert!(crank_fee > 0, "the crank must be paid a slice of the bonus");
    assert_eq!(
        cash_before - config.cash,
        record.debt_repaid + crank_fee,
        "cash must fall by exactly the repay plus the fee -- nothing unaccounted for"
    );

    // Deposits and withdrawals pause while the pool holds inventory (D4).
    let (another, another_usdc) = env.new_funded(100 * USDC);
    let open = env.b_ix(
        backstop::accounts::OpenBacker {
            owner: another.pubkey(),
            config: env.b_config(),
            backer: env.backer(&another.pubkey()),
            system_program: SYSTEM,
        },
        backstop::instruction::OpenBacker {},
    );
    env.ok(&[open], &[&another]);
    let dep = env.b_ix(
        backstop::accounts::Deposit {
            owner: another.pubkey(),
            config: env.b_config(),
            backer: env.backer(&another.pubkey()),
            usdc_mint: env.usdc_mint,
            owner_usdc: another_usdc,
            usdc_vault: env.b_vault(),
            usdc_token_program: TOKEN_CLASSIC,
        },
        backstop::instruction::Deposit { amount: 10 * USDC },
    );
    assert_program_error(
        env.send(&[dep], &[&another]),
        VerdictError::PausedForInventory,
    );
}

#[test]
fn pool_liquidate_repay_is_bounded_by_the_per_liquidation_cap() {
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let alice = env.alice.pubkey();

    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: env.b_config(),
        },
    );
    env.ok(&[register], &[&admin]);
    // Small pool: 10% of 10_000 USDC = 1_000 USDC per-liquidation cap, well under the 50_000 USDC
    // the position could otherwise be liquidated for.
    env.back(10_000 * USDC);
    let open_inv = env.open_inventory_ix(&admin.pubkey());
    env.ok(&[open_inv], &[&admin]);
    env.walk_price_to(PRICE * 75 / 100);

    let (crank, crank_usdc) = env.new_funded(0);
    let seq = env.market_state().liq_seq;
    let ix = env.pool_liquidate_ix(&crank.pubkey(), &crank_usdc, seq, &alice, 50_000 * USDC);
    env.ok(&[ix], &[&crank]);

    let record_data = env.svm.get_account(&env.record(seq)).unwrap().data;
    let record = LiquidationRecord::try_deserialize(&mut record_data.as_slice()).unwrap();
    assert!(
        record.debt_repaid <= 1_000 * USDC,
        "must be capped at 10% of the pool's cash, got {}",
        record.debt_repaid
    );
}

#[test]
fn pool_liquidate_below_the_minimum_is_refused() {
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let alice = env.alice.pubkey();

    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: env.b_config(),
        },
    );
    env.ok(&[register], &[&admin]);
    env.back(100_000 * USDC);
    let open_inv = env.open_inventory_ix(&admin.pubkey());
    env.ok(&[open_inv], &[&admin]);
    env.walk_price_to(PRICE * 75 / 100);

    let (crank, crank_usdc) = env.new_funded(0);
    let seq = env.market_state().liq_seq;
    // Below DEFAULT_MIN_POOL_REPAY (10 USDC).
    let ix = env.pool_liquidate_ix(&crank.pubkey(), &crank_usdc, seq, &alice, USDC);
    assert_program_error(env.send(&[ix], &[&crank]), VerdictError::BelowMinPoolRepay);
}

#[test]
fn buy_inventory_after_a_pool_liquidation_resells_at_the_current_price() {
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let market = env.market();
    let alice = env.alice.pubkey();

    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: env.b_config(),
        },
    );
    env.ok(&[register], &[&admin]);
    env.back(100_000 * USDC);
    let open_inv = env.open_inventory_ix(&admin.pubkey());
    env.ok(&[open_inv], &[&admin]);
    env.walk_price_to(PRICE * 75 / 100);

    let (crank, crank_usdc) = env.new_funded(0);
    let seq = env.market_state().liq_seq;
    let ix = env.pool_liquidate_ix(&crank.pubkey(), &crank_usdc, seq, &alice, 50_000 * USDC);
    env.ok(&[ix], &[&crank]);
    let inv_before = env.inventory_state(&market);

    // Well past the resale floor (4 days default), so the discount off the healthy price applies
    // rather than the cost floor. A fresh price keeps `risk_price`'s age guard happy after the warp.
    env.warp(5 * 86_400);
    env.push_price(PRICE * 75 / 100);

    let (buyer, buyer_usdc) = env.new_funded(1_000_000 * USDC);
    let buyer_coll = token_account(
        &mut env.svm,
        &buyer,
        &env.coll_mint.clone(),
        &buyer.pubkey(),
        &TOKEN_2022,
    );
    let cash_before = env.config_state().cash;
    let ix = env.b_ix(
        backstop::accounts::BuyInventory {
            buyer: buyer.pubkey(),
            config: env.b_config(),
            market,
            inventory: env.inventory(&market),
            collateral_mint: env.coll_mint,
            usdc_mint: env.usdc_mint,
            inventory_vault: env.inventory_vault(&market),
            pool_usdc_vault: env.b_vault(),
            buyer_usdc,
            buyer_collateral: buyer_coll,
            collateral_token_program: TOKEN_2022,
            usdc_token_program: TOKEN_CLASSIC,
        },
        backstop::instruction::BuyInventory {
            raw: inv_before.raw,
            max_price_per_share: u64::MAX,
        },
    );
    env.ok(&[ix], &[&buyer]);

    let inv_after = env.inventory_state(&market);
    assert_eq!(inv_after.raw, 0);
    assert_eq!(
        inv_after.cost_total, 0,
        "dust-safe close-out on a full sale"
    );
    assert_eq!(
        env.balance(&buyer_coll),
        inv_before.raw,
        "the buyer must receive exactly what was bought"
    );
    let config = env.config_state();
    assert_eq!(
        config.inventory_cost_total, 0,
        "pause signal must fully clear"
    );
    assert!(
        config.cash > cash_before,
        "resale proceeds must land back in cash"
    );

    // The pause is lifted: a deposit now succeeds.
    env.back(USDC);
}

#[test]
fn write_off_inventory_is_refused_when_the_market_is_not_frozen_or_halted() {
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let open_inv = env.open_inventory_ix(&admin.pubkey());
    env.ok(&[open_inv], &[&admin]);
    let ix = env.write_off_inventory_ix(&admin.pubkey());
    assert_program_error(env.send(&[ix], &[&admin]), VerdictError::NotFrozenOrHalted);
}

#[test]
fn absorb_interest_credits_cash_and_a_donation_is_never_counted() {
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let market = env.market();

    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: env.b_config(),
        },
    );
    env.ok(&[register], &[&admin]);
    env.back(1_000 * USDC);
    let open_ia = env.open_interest_absorbed_ix(&admin.pubkey());
    env.ok(&[open_ia], &[&admin]);

    // Force real interest to accrue -- update_market_params calls accrue() first, with no other
    // side effect when the params supplied are unchanged.
    env.warp(365 * 86_400);
    let upd = env.v_ix(
        stock_vault::accounts::UpdateMarket {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
            market,
        },
        stock_vault::instruction::UpdateMarketParams { params: params() },
    );
    env.ok(&[upd], &[&admin]);
    let owed = env.market_state().backer_interest_owed;
    assert!(
        owed > 0,
        "a year of accrual on a real loan must produce owed interest"
    );

    let pay = env.pay_backer_interest_ix();
    env.ok(&[pay], &[&admin]);
    assert_eq!(env.market_state().backer_interest_owed, 0);

    // A raw donation straight into the pool's USDC vault -- must never be counted as recognised
    // interest (2c donation invariant, in reverse).
    let issuer = env.issuer.insecure_clone();
    let (usdc, vault) = (env.usdc_mint, env.b_vault());
    mint_to(
        &mut env.svm,
        &issuer,
        &usdc,
        &vault,
        5_000 * USDC,
        &TOKEN_CLASSIC,
    );

    let cash_before = env.config_state().cash;
    let absorb = env.absorb_interest_ix();
    env.ok(&[absorb], &[&admin]);
    let credited = env.config_state().cash - cash_before;
    assert_eq!(
        credited, owed,
        "must credit exactly the paid interest, not the donation too"
    );
    assert_eq!(env.interest_absorbed_state(&market).total_absorbed, owed);

    // A repeat call settles nothing further -- the remainder is zero. A fresh blockhash is needed
    // so this transaction (otherwise byte-identical to the one above) isn't just deduplicated.
    env.warp(1);
    let ix2 = env.absorb_interest_ix();
    assert_program_error(env.send(&[ix2], &[&admin]), VerdictError::ZeroAmount);
}

// ------------------------------------------------------------------ phase 4: roles + overrides

/// Liquidated loan old enough to skip the gate, pool backed with 1M USDC. Returns (record key,
/// record state).
fn liquidated_for_claims() -> (Env, Pubkey, LiquidationRecord) {
    let mut env = with_loan();
    env.back(1_000_000 * USDC);
    env.warp(GATE_SECS + 86_400);
    env.reanchor_price();
    liquidate(&mut env, None, 75);
    let record = env.record_state(0);
    let record_key = env.record(0);
    (env, record_key, record)
}

fn funded_co_signer(env: &mut Env) -> Keypair {
    let co = env.co_signer.insecure_clone();
    env.svm.airdrop(&co.pubkey(), 1_000_000_000).unwrap();
    co
}

fn overwrite_borrower_claims(env: &mut Env, borrower: &Pubkey, bc: &BorrowerClaims) {
    let pda = env.borrower_claims_pda(borrower);
    let mut account = env.svm.get_account(&pda).unwrap();
    let mut data = Vec::new();
    bc.try_serialize(&mut data).unwrap();
    account.data = data;
    env.svm.set_account(pda, account).unwrap();
}

#[test]
fn initialize_refuses_overlapping_roles() {
    let mut env = base_uninitialized();
    let admin = env.admin.insecure_clone();
    let init_with = |env: &Env, oracle: Pubkey, co_signer: Pubkey| {
        env.b_ix(
            backstop::accounts::InitializeBackstop {
                admin: admin.pubkey(),
                program: backstop::ID,
                program_data: programdata(&backstop::ID),
                config: env.b_config(),
                usdc_mint: env.usdc_mint,
                usdc_vault: env.b_vault(),
                usdc_token_program: TOKEN_CLASSIC,
                system_program: SYSTEM,
            },
            backstop::instruction::InitializeBackstop {
                verdict_oracle: oracle,
                co_signer,
                cluster_tag: CLUSTER_DEVNET,
                per_claim_cap_bps: CAP_BPS,
                withdraw_delay_secs: 86_400,
            },
        )
    };
    let (oracle, co) = (env.oracle.pubkey(), env.co_signer.pubkey());
    for (o, c) in [
        (admin.pubkey(), co),
        (oracle, admin.pubkey()),
        (oracle, oracle),
    ] {
        let ix = init_with(&env, o, c);
        assert_program_error(env.send(&[ix], &[&admin]), VerdictError::RoleCollision);
    }
    let ix = init_with(&env, oracle, co);
    env.ok(&[ix], &[&admin]);
    let config = env.config_state();
    assert_eq!(config.co_signer, co);
}

#[test]
fn every_role_setter_refuses_a_collision_and_the_co_signer_rotates() {
    let mut env = base();
    let admin = env.admin.insecure_clone();
    let (oracle, co) = (env.oracle.pubkey(), env.co_signer.pubkey());

    for to in [admin.pubkey(), oracle] {
        let ix = env.set_co_signer_ix(&admin.pubkey(), to);
        assert_program_error(env.send(&[ix], &[&admin]), VerdictError::RoleCollision);
    }
    let rotate_oracle = env.b_ix(
        backstop::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: env.b_config(),
        },
        backstop::instruction::SetVerdictOracle { verdict_oracle: co },
    );
    assert_program_error(
        env.send(&[rotate_oracle], &[&admin]),
        VerdictError::RoleCollision,
    );
    let rotate_admin = env.b_ix(
        backstop::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: env.b_config(),
        },
        backstop::instruction::SetAdmin { admin: co },
    );
    assert_program_error(
        env.send(&[rotate_admin], &[&admin]),
        VerdictError::RoleCollision,
    );

    let intruder = env.alice.insecure_clone();
    let ix = env.set_co_signer_ix(&intruder.pubkey(), intruder.pubkey());
    assert_program_error(env.send(&[ix], &[&intruder]), VerdictError::Unauthorized);

    let fresh = Pubkey::new_unique();
    let ix = env.set_co_signer_ix(&admin.pubkey(), fresh);
    env.ok(&[ix], &[&admin]);
    assert_eq!(env.config_state().co_signer, fresh);
}

#[test]
fn a_payout_address_that_is_the_co_signer_is_refused() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let admin = env.admin.insecure_clone();
    let alice = env.alice.pubkey();
    let ix = env.set_co_signer_ix(&admin.pubkey(), alice);
    env.ok(&[ix], &[&admin]);
    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let ixs = env.submit_facts_ixs(&record_key, &alice, ref_price, ref_price + 1);
    assert_program_error(env.send(&ixs, &[&admin]), VerdictError::PrivilegedPayout);
}

#[test]
fn cancelling_before_any_payout_releases_the_reservation_without_a_penalty() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    assert_eq!(env.config_state().reserved_total, loss);

    let intruder = env.alice.insecure_clone();
    let ix = env.cancel_claim_ix(&intruder.pubkey(), &record_key, &alice, 42);
    assert_program_error(env.send(&[ix], &[&intruder]), VerdictError::Unauthorized);

    let admin = env.admin.insecure_clone();
    let ix = env.cancel_claim_ix(&admin.pubkey(), &record_key, &alice, 42);
    env.ok(&[ix], &[&admin]);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Cancelled);
    assert_eq!(claim.deny_reason, 42, "the public reason code is recorded");
    assert_eq!(env.config_state().reserved_total, 0);
    let bc = env.borrower_claims_state(&alice);
    assert_eq!(
        (bc.penalty_since, bc.penalty_until),
        (0, 0),
        "nothing had streamed"
    );

    env.warp(1);
    let ix = env.cancel_claim_ix(&admin.pubkey(), &record_key, &alice, 42);
    assert_program_error(
        env.send(&[ix], &[&admin]),
        VerdictError::ClaimNotCancellable,
    );
}

#[test]
fn cancelling_after_a_payout_started_applies_the_365_day_penalty() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);

    let admin = env.admin.insecure_clone();
    env.warp(COOLDOWN_SECS + STREAM_SECS / 2);
    let stream = env.claim_stream_ix(&record_key, &env.alice_usdc);
    env.ok(&[stream], &[&admin]);
    let streamed = env.claim_state(&record_key).streamed;
    assert!(streamed > 0 && streamed < loss);

    let ix = env.cancel_claim_ix(&admin.pubkey(), &record_key, &alice, 7);
    env.ok(&[ix], &[&admin]);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Cancelled);
    assert_eq!(
        env.config_state().reserved_total,
        0,
        "only the unstreamed remainder was still reserved, and it is released"
    );
    let bc = env.borrower_claims_state(&alice);
    assert_eq!(bc.penalty_since, env.now);
    assert_eq!(bc.penalty_until, env.now + 365 * 86_400);
}

#[test]
fn a_loan_inside_the_penalty_window_is_denied_with_a_reason_code() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let alice = env.alice.pubkey();
    env.open_borrower_claims(&alice);
    let mut bc = env.borrower_claims_state(&alice);
    bc.penalty_since = record.borrow_age_ts - 1;
    bc.penalty_until = record.borrow_age_ts + 365 * 86_400;
    overwrite_borrower_claims(&mut env, &alice, &bc);

    let (ref_price, _) = wrongful_ref_and_loss(&record);
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Denied);
    assert_eq!(
        claim.deny_reason,
        backstop::state::deny_reason::PENALTY_ACTIVE
    );
    assert_eq!(env.config_state().reserved_total, 0);
}

#[test]
fn a_loan_that_predates_the_penalty_is_still_covered() {
    // Regression guard for the lower bound: `borrow_age_ts < penalty_until` alone would catch
    // every older loan too, since `penalty_until` sits in the future by construction.
    let (mut env, record_key, record) = liquidated_for_claims();
    let alice = env.alice.pubkey();
    env.open_borrower_claims(&alice);
    let mut bc = env.borrower_claims_state(&alice);
    bc.penalty_since = record.borrow_age_ts + 1;
    bc.penalty_until = record.borrow_age_ts + 365 * 86_400;
    overwrite_borrower_claims(&mut env, &alice, &bc);

    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Active);
    assert_eq!(claim.loss, loss);
}

#[test]
fn a_suspended_claim_is_frozen_and_its_expiry_clock_does_not_run() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    let admin = env.admin.insecure_clone();

    let intruder = env.alice.insecure_clone();
    let ix = env.suspend_claim_ix(&intruder.pubkey(), &record_key);
    assert_program_error(env.send(&[ix], &[&intruder]), VerdictError::Unauthorized);
    let ix = env.unsuspend_claim_ix(&admin.pubkey(), &record_key);
    assert_program_error(env.send(&[ix], &[&admin]), VerdictError::NotSuspended);

    let ix = env.suspend_claim_ix(&admin.pubkey(), &record_key);
    env.ok(&[ix], &[&admin]);
    env.warp(1);
    let ix = env.suspend_claim_ix(&admin.pubkey(), &record_key);
    assert_program_error(env.send(&[ix], &[&admin]), VerdictError::AlreadySuspended);

    env.warp(COOLDOWN_SECS + INACTIVITY_EXPIRY_SECS);
    let stream = env.claim_stream_ix(&record_key, &env.alice_usdc);
    assert_program_error(
        env.send(std::slice::from_ref(&stream), &[&admin]),
        VerdictError::ClaimSuspended,
    );
    let expire = env.expire_stale_ix(&record_key);
    assert_program_error(
        env.send(std::slice::from_ref(&expire), &[&admin]),
        VerdictError::ClaimSuspended,
    );

    let ix = env.unsuspend_claim_ix(&admin.pubkey(), &record_key);
    env.ok(&[ix], &[&admin]);
    let claim = env.claim_state(&record_key);
    assert!(!claim.suspended);
    assert!(claim.suspended_secs >= COOLDOWN_SECS + INACTIVITY_EXPIRY_SECS);

    // Without the discount this would already be stale: more than 100 days passed since admission.
    env.warp(1);
    assert_program_error(
        env.send(std::slice::from_ref(&expire), &[&admin]),
        VerdictError::NotYetStale,
    );
    env.warp(INACTIVITY_EXPIRY_SECS);
    env.ok(&[expire], &[&admin]);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Expired);
}

#[test]
fn a_revoked_attestation_can_never_be_submitted() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    let admin = env.admin.insecure_clone();

    let intruder = env.alice.insecure_clone();
    let ix = env.revoke_attestation_ix(&intruder.pubkey(), &record_key, [7; 32]);
    assert_program_error(env.send(&[ix], &[&intruder]), VerdictError::Unauthorized);

    let ix = env.revoke_attestation_ix(&admin.pubkey(), &record_key, [7; 32]);
    env.ok(&[ix], &[&admin]);
    let revoked = env.revoked_state(&record_key, &[7; 32]);
    assert_eq!(revoked.liquidation_record, record_key);
    assert_eq!(revoked.evidence_hash, [7; 32]);

    let ixs = env.submit_facts_ixs(&record_key, &alice, ref_price, ref_price + 1);
    assert_program_error(env.send(&ixs, &[&admin]), VerdictError::AttestationRevoked);
    assert!(env.svm.get_account(&env.claim_pda(&record_key)).is_none());
}

#[test]
fn revoking_a_different_evidence_hash_leaves_this_attestation_valid() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    let admin = env.admin.insecure_clone();
    let ix = env.revoke_attestation_ix(&admin.pubkey(), &record_key, [8; 32]);
    env.ok(&[ix], &[&admin]);
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Active);
}

#[test]
fn a_two_of_two_override_pays_a_denied_claim_only_on_matching_approvals() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let alice = env.alice.pubkey();
    // The oracle's reference agrees with the price used, so check (a) denies.
    env.submit_facts(&record_key, &alice, record.price_fp, record.price_fp);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Denied);

    let admin = env.admin.insecure_clone();
    let co = funded_co_signer(&mut env);
    let (ref_price, loss) = wrongful_ref_and_loss(&record);

    let ix = env.open_override_ix(&admin.pubkey(), &record_key);
    env.ok(&[ix], &[&admin]);

    let intruder = env.alice.insecure_clone();
    let ix = env.approve_override_ix(&intruder.pubkey(), &record_key, &alice, ref_price);
    assert_program_error(
        env.send(&[ix], &[&intruder]),
        VerdictError::CallerNotAdminOrCoSigner,
    );

    let ix = env.approve_override_ix(&admin.pubkey(), &record_key, &alice, ref_price);
    env.ok(&[ix], &[&admin]);
    assert_eq!(
        env.claim_state(&record_key).status,
        ClaimStatus::Denied,
        "one approval alone never executes"
    );

    let ix = env.approve_override_ix(&co.pubkey(), &record_key, &alice, ref_price + 1);
    assert_program_error(
        env.send(&[ix], &[&co]),
        VerdictError::OverrideParamsMismatch,
    );

    let ix = env.approve_override_ix(&co.pubkey(), &record_key, &alice, ref_price);
    env.ok(&[ix], &[&co]);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Active);
    assert_eq!(
        claim.loss, loss,
        "loss still comes from the on-chain formula"
    );
    assert_eq!(claim.deny_reason, backstop::state::deny_reason::NONE);
    assert_eq!(env.config_state().reserved_total, loss);
    assert!(env.override_state(&record_key).executed);

    env.warp(1);
    let ix = env.approve_override_ix(&co.pubkey(), &record_key, &alice, ref_price);
    assert_program_error(
        env.send(&[ix], &[&co]),
        VerdictError::OverrideAlreadyExecuted,
    );
}

#[test]
fn an_approval_from_a_rotated_out_admin_no_longer_counts() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, record.price_fp, record.price_fp);
    let old_admin = env.admin.insecure_clone();
    let co = funded_co_signer(&mut env);
    let (ref_price, _) = wrongful_ref_and_loss(&record);

    let ix = env.open_override_ix(&old_admin.pubkey(), &record_key);
    env.ok(&[ix], &[&old_admin]);
    let ix = env.approve_override_ix(&old_admin.pubkey(), &record_key, &alice, ref_price);
    env.ok(&[ix], &[&old_admin]);

    let new_admin = Keypair::new();
    env.svm.airdrop(&new_admin.pubkey(), 1_000_000_000).unwrap();
    let rotate = env.b_ix(
        backstop::accounts::AdminOnly {
            admin: old_admin.pubkey(),
            config: env.b_config(),
        },
        backstop::instruction::SetAdmin {
            admin: new_admin.pubkey(),
        },
    );
    env.ok(&[rotate], &[&old_admin]);

    let ix = env.approve_override_ix(&co.pubkey(), &record_key, &alice, ref_price);
    env.ok(&[ix], &[&co]);
    assert_eq!(
        env.claim_state(&record_key).status,
        ClaimStatus::Denied,
        "the stale admin approval must not complete the pair"
    );

    env.warp(1);
    let ix = env.approve_override_ix(&old_admin.pubkey(), &record_key, &alice, ref_price);
    assert_program_error(
        env.send(&[ix], &[&old_admin]),
        VerdictError::CallerNotAdminOrCoSigner,
    );
    let ix = env.approve_override_ix(&new_admin.pubkey(), &record_key, &alice, ref_price);
    env.ok(&[ix], &[&new_admin]);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Active);
}

#[test]
fn overriding_a_streaming_claim_carries_what_was_paid_and_never_overpays() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let alice = env.alice.pubkey();
    let (ref1, loss1) = wrongful_ref_and_loss(&record);
    env.submit_facts(&record_key, &alice, ref1, ref1 + 1);
    let admin = env.admin.insecure_clone();

    let start = env.balance(&env.alice_usdc);
    env.warp(COOLDOWN_SECS + STREAM_SECS / 2);
    let stream = env.claim_stream_ix(&record_key, &env.alice_usdc);
    env.ok(std::slice::from_ref(&stream), &[&admin]);
    let s1 = env.balance(&env.alice_usdc) - start;
    assert!(s1 > 0 && s1 < loss1);

    let ref2 = ref1 * 2;
    let loss2 = safu_core::loss::wrongful_loss(
        record.seized_raw,
        record.collateral_decimals,
        record.multiplier_fp,
        ref2,
        record.debt_repaid,
    )
    .unwrap();
    assert!(loss2 > loss1);

    let co = funded_co_signer(&mut env);
    let ix = env.open_override_ix(&admin.pubkey(), &record_key);
    env.ok(&[ix], &[&admin]);
    let ix = env.approve_override_ix(&admin.pubkey(), &record_key, &alice, ref2);
    env.ok(&[ix], &[&admin]);
    let ix = env.approve_override_ix(&co.pubkey(), &record_key, &alice, ref2);
    env.ok(&[ix], &[&co]);

    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Active);
    assert_eq!(claim.loss, loss2);
    assert_eq!(
        claim.streamed, s1,
        "what already streamed is carried forward"
    );
    assert_eq!(env.config_state().reserved_total, loss2 - s1);

    // Still goes through a fresh cooldown.
    env.warp(60);
    assert_program_error(
        env.send(std::slice::from_ref(&stream), &[&admin]),
        VerdictError::CooldownNotElapsed,
    );

    // Halfway through the new stream, half of the corrected loss is available — not half minus
    // what was already paid, which is what re-streaming from zero would give.
    env.warp(COOLDOWN_SECS - 60 + STREAM_SECS / 2);
    let before = env.balance(&env.alice_usdc);
    env.ok(std::slice::from_ref(&stream), &[&admin]);
    let got = env.balance(&env.alice_usdc) - before;
    let half = loss2 / 2;
    assert!(
        got.abs_diff(half) <= loss2 / 100 + 2,
        "expected ~{half} halfway through the corrected stream, got {got}"
    );

    env.warp(STREAM_SECS);
    env.ok(std::slice::from_ref(&stream), &[&admin]);
    assert_eq!(
        env.balance(&env.alice_usdc) - start,
        loss2,
        "total paid equals the corrected loss exactly"
    );
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Completed);
    assert_eq!(env.config_state().reserved_total, 0);

    env.warp(1);
    let ix = env.approve_override_ix(&admin.pubkey(), &record_key, &alice, ref2);
    assert_program_error(
        env.send(&[ix], &[&admin]),
        VerdictError::ClaimAlreadyCompleted,
    );
}

// ------------------------------------------------------------------ phase 5: time bounds + measurement

const DAY: i64 = 86_400;
/// gate, cooldown, stream, inactivity, min_after_wait -- the LOCKED spec values.
const DEFAULT_TIMING: [i64; 5] = [60 * DAY, 7 * DAY, 45 * DAY, 100 * DAY, 3_600];
const MAINNET_FLOORS: [i64; 5] = [30 * DAY, 3 * DAY, 14 * DAY, 30 * DAY, 3_600];

fn mainnet_backstop() -> Env {
    let mut env = base_uninitialized();
    let admin = env.admin.insecure_clone();
    let init = |env: &Env, cluster_tag: u8| {
        env.b_ix(
            backstop::accounts::InitializeBackstop {
                admin: admin.pubkey(),
                program: backstop::ID,
                program_data: programdata(&backstop::ID),
                config: env.b_config(),
                usdc_mint: env.usdc_mint,
                usdc_vault: env.b_vault(),
                usdc_token_program: TOKEN_CLASSIC,
                system_program: SYSTEM,
            },
            backstop::instruction::InitializeBackstop {
                verdict_oracle: env.oracle.pubkey(),
                co_signer: env.co_signer.pubkey(),
                cluster_tag,
                per_claim_cap_bps: CAP_BPS,
                withdraw_delay_secs: 86_400,
            },
        )
    };
    let bad = init(&env, 3);
    assert_program_error(env.send(&[bad], &[&admin]), VerdictError::InvalidParams);
    let ix = init(&env, CLUSTER_MAINNET);
    env.ok(&[ix], &[&admin]);
    env
}

#[test]
fn a_fresh_backstop_starts_on_the_locked_spec_timings() {
    let env = base();
    let c = env.config_state();
    assert_eq!(
        [
            c.gate_secs,
            c.cooldown_secs,
            c.stream_secs,
            c.inactivity_secs,
            c.min_after_wait_secs
        ],
        DEFAULT_TIMING
    );
}

#[test]
fn devnet_timings_may_go_to_seconds_but_stay_inside_their_bounds() {
    let mut env = base();
    let admin = env.admin.insecure_clone();
    let refused: [[i64; 5]; 7] = [
        [0, 1, 1, 2, 1],                     // zero gate
        [1, 1, 0, 2, 1],                     // zero stream
        [DEFAULT_TIMING[0] + 1, 1, 1, 2, 1], // gate slower than the spec
        [1, DEFAULT_TIMING[1] + 1, 1, 8 * DAY, 1],
        [1, 1, 1, 366 * DAY, 1], // inactivity past one year
        [1, 1, 1, 2, 3_601],     // minimum wait slower than the spec
        [1, 5, 1, 5, 1],         // inactivity no longer than the cooldown
    ];
    for (i, t) in refused.iter().enumerate() {
        env.warp(1);
        let ix = env.set_claim_timing_ix(&admin.pubkey(), *t);
        let result = env.send(&[ix], &[&admin]);
        assert!(result.is_err(), "case {i} should be refused: {t:?}");
        assert_program_error(result, VerdictError::InvalidParams);
    }

    let intruder = env.alice.insecure_clone();
    let ix = env.set_claim_timing_ix(&intruder.pubkey(), [1, 1, 1, 2, 1]);
    assert_program_error(env.send(&[ix], &[&intruder]), VerdictError::Unauthorized);

    let ix = env.set_claim_timing_ix(&admin.pubkey(), [1, 1, 1, 2, 1]);
    env.ok(&[ix], &[&admin]);
    let c = env.config_state();
    assert_eq!(
        [
            c.gate_secs,
            c.cooldown_secs,
            c.stream_secs,
            c.inactivity_secs,
            c.min_after_wait_secs
        ],
        [1, 1, 1, 2, 1]
    );
}

#[test]
fn mainnet_timings_can_never_drop_below_their_floors() {
    let mut env = mainnet_backstop();
    let admin = env.admin.insecure_clone();
    for field in 0..5 {
        let mut t = MAINNET_FLOORS;
        t[field] -= 1;
        env.warp(1);
        let ix = env.set_claim_timing_ix(&admin.pubkey(), t);
        assert_program_error(env.send(&[ix], &[&admin]), VerdictError::InvalidParams);
    }
    let ix = env.set_claim_timing_ix(&admin.pubkey(), MAINNET_FLOORS);
    env.ok(&[ix], &[&admin]);
    assert_eq!(env.config_state().gate_secs, MAINNET_FLOORS[0]);

    let resale = |env: &Env, floor: i64| {
        env.b_ix(
            backstop::accounts::AdminOnly {
                admin: admin.pubkey(),
                config: env.b_config(),
            },
            backstop::instruction::SetPoolLiquidationParams {
                per_liq_cap_bps: 1_000,
                daily_liq_cap_bps: 2_500,
                resale_discount_bps: 200,
                resale_floor_secs: floor,
                fee_share_bps: 1_000,
                min_pool_repay: 10 * USDC,
            },
        )
    };
    let ix = resale(&env, 2 * DAY - 1);
    assert_program_error(env.send(&[ix], &[&admin]), VerdictError::InvalidParams);
    let ix = resale(&env, 2 * DAY);
    env.ok(&[ix], &[&admin]);
    assert_eq!(env.config_state().resale_floor_secs, 2 * DAY);
}

#[test]
fn a_timing_change_never_reaches_a_claim_already_admitted() {
    let (mut env, record_key, record) = liquidated_for_claims();
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    env.submit_facts(&record_key, &alice, ref_price, ref_price + 1);
    let before = env.claim_state(&record_key);
    assert_eq!(before.status, ClaimStatus::Active);

    let admin = env.admin.insecure_clone();
    let ix = env.set_claim_timing_ix(&admin.pubkey(), [1, 1, 1, 2, 1]);
    env.ok(&[ix], &[&admin]);

    let after = env.claim_state(&record_key);
    assert_eq!(after.cooldown_end, before.cooldown_end);
    assert_eq!(after.stream_end - after.cooldown_end, DEFAULT_TIMING[2]);
    assert_eq!(after.inactivity_secs, DEFAULT_TIMING[3]);

    // Halfway through its own 45-day stream it has vested half -- not everything, as a 1 s stream would.
    env.warp(COOLDOWN_SECS + STREAM_SECS / 2);
    let start = env.balance(&env.alice_usdc);
    let stream = env.claim_stream_ix(&record_key, &env.alice_usdc);
    env.ok(&[stream], &[&admin]);
    let got = env.balance(&env.alice_usdc) - start;
    assert!(
        got.abs_diff(loss / 2) <= loss / 100 + 2,
        "expected ~half of {loss}, got {got}"
    );

    // And its 100-day inactivity window still holds, not the new 2 s one.
    env.warp(3);
    let expire = env.expire_stale_ix(&record_key);
    assert_program_error(env.send(&[expire], &[&admin]), VerdictError::NotYetStale);
}

#[test]
fn a_whole_claim_runs_on_short_devnet_timings() {
    let mut env = with_loan();
    env.back(1_000_000 * USDC);
    liquidate(&mut env, None, 75); // the price walk ages the loan ~12 hours
    let record = env.record_state(0);
    let record_key = env.record(0);
    let (ref_price, loss) = wrongful_ref_and_loss(&record);
    let alice = env.alice.pubkey();
    let admin = env.admin.insecure_clone();

    // Under the default 1-hour minimum wait, a reference taken 2 minutes after the liquidation is refused.
    let quick_after = record.ts + 120;
    let ixs =
        env.submit_facts_ixs_after(&record_key, &alice, ref_price, ref_price + 1, quick_after);
    assert_program_error(env.send(&ixs, &[&admin]), VerdictError::InvalidAfterWindow);

    // gate 1 day, cooldown 60 s, stream 5 min, inactivity 1 h, minimum wait 60 s.
    let ix = env.set_claim_timing_ix(&admin.pubkey(), [DAY, 60, 300, 3_600, 60]);
    env.ok(&[ix], &[&admin]);
    env.warp(1);
    let ixs =
        env.submit_facts_ixs_after(&record_key, &alice, ref_price, ref_price + 1, quick_after);
    env.ok(&ixs, &[&admin]);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::PendingTime);
    assert_eq!(claim.releasable_at, record.borrow_age_ts + DAY);

    env.warp(claim.releasable_at - env.now);
    let unlock = env.unlock_claim_ix(&record_key);
    env.ok(&[unlock], &[&admin]);
    let claim = env.claim_state(&record_key);
    assert_eq!(claim.status, ClaimStatus::Active);
    assert_eq!(claim.cooldown_end, env.now + 60);
    assert_eq!(claim.stream_end, env.now + 360);

    let start = env.balance(&env.alice_usdc);
    env.warp(360);
    let stream = env.claim_stream_ix(&record_key, &env.alice_usdc);
    env.ok(&[stream], &[&admin]);
    assert_eq!(env.balance(&env.alice_usdc) - start, loss);
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Completed);
}

/// T3 (eng review): measure, don't assume. Legacy transactions, no lookup tables, default client
/// setup. Limits: 1,400,000 compute units per transaction, 1,232 bytes per packet.
#[test]
fn heaviest_transactions_fit_solana_compute_and_size_limits() {
    const MAX_CU: u64 = 1_400_000;
    const MAX_TX_BYTES: usize = 1_232;
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let alice = env.alice.pubkey();
    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: env.b_config(),
        },
    );
    env.ok(&[register], &[&admin]);
    env.back(1_000_000 * USDC);
    let open_inv = env.open_inventory_ix(&admin.pubkey());
    env.ok(&[open_inv], &[&admin]);
    env.walk_price_to(PRICE * 75 / 100);

    let mut rows: Vec<(&str, u64, usize)> = Vec::new();

    let (crank, crank_usdc) = env.new_funded(0);
    let seq = env.market_state().liq_seq;
    let ix = env.pool_liquidate_ix(&crank.pubkey(), &crank_usdc, seq, &alice, 50_000 * USDC);
    let (cu, size) = env.send_measured(&[ix], &[&crank]);
    rows.push(("pool_liquidate (CPI into liquidate)", cu, size));

    // Short gate so submission takes the heavier admit-straight-to-Active path.
    let ix = env.set_claim_timing_ix(&admin.pubkey(), [1, 60, 300, 3_600, 3_600]);
    env.ok(&[ix], &[&admin]);
    env.warp(10);
    let record_key = env.record(seq);
    let record = env.record_state(seq);
    let (ref_price, _) = wrongful_ref_and_loss(&record);
    let ixs = env.submit_facts_ixs(&record_key, &alice, ref_price, ref_price + 1);
    let (cu, size) = env.send_measured(&ixs, &[&admin]);
    rows.push(("ed25519 + submit_facts (admits)", cu, size));
    assert_eq!(env.claim_state(&record_key).status, ClaimStatus::Active);

    let co = funded_co_signer(&mut env);
    let ix = env.open_override_ix(&admin.pubkey(), &record_key);
    env.ok(&[ix], &[&admin]);
    let ix = env.approve_override_ix(&admin.pubkey(), &record_key, &alice, ref_price * 2);
    let (cu, size) = env.send_measured(&[ix], &[&admin]);
    rows.push(("approve_override (first approval)", cu, size));
    let ix = env.approve_override_ix(&co.pubkey(), &record_key, &alice, ref_price * 2);
    let (cu, size) = env.send_measured(&[ix], &[&co]);
    rows.push(("approve_override (executes)", cu, size));

    env.warp(60 + 150);
    let ix = env.claim_stream_ix(&record_key, &env.alice_usdc);
    let (cu, size) = env.send_measured(&[ix], &[&admin]);
    rows.push(("claim_stream", cu, size));

    for (name, cu, size) in &rows {
        println!("T3 | {name:<38} | {cu:>7} CU | {size:>5} bytes");
        assert!(*cu < MAX_CU, "{name}: {cu} CU exceeds {MAX_CU}");
        assert!(
            *size <= MAX_TX_BYTES,
            "{name}: {size} bytes exceeds {MAX_TX_BYTES}"
        );
    }
}

#[test]
fn write_off_inventory_clears_a_halted_markets_inventory_and_lifts_the_pause() {
    let mut env = with_loan();
    let admin = env.admin.insecure_clone();
    let market = env.market();
    let alice = env.alice.pubkey();
    let register = env.v_ix(
        stock_vault::accounts::AdminOnly {
            admin: admin.pubkey(),
            config: vpda(&[VCONFIG_SEED]),
        },
        stock_vault::instruction::SetPoolLiquidator {
            pool_liquidator: env.b_config(),
        },
    );
    env.ok(&[register], &[&admin]);
    env.back(100_000 * USDC);
    let open_inv = env.open_inventory_ix(&admin.pubkey());
    env.ok(&[open_inv], &[&admin]);
    env.walk_price_to(PRICE * 75 / 100);
    let (crank, crank_usdc) = env.new_funded(0);
    let seq = env.market_state().liq_seq;
    let ix = env.pool_liquidate_ix(&crank.pubkey(), &crank_usdc, seq, &alice, 50_000 * USDC);
    env.ok(&[ix], &[&crank]);
    let cost = env.inventory_state(&market).cost_total;
    assert!(cost > 0 && env.config_state().inventory_cost_total == cost);

    // The issuer burns out of the market's collateral vault; syncing halts the market.
    let coll_vault = vpda(&[COLL_VAULT_SEED, market.as_ref()]);
    let mut acc = env.svm.get_account(&coll_vault).unwrap();
    let amount = u64::from_le_bytes(acc.data[64..72].try_into().unwrap());
    acc.data[64..72].copy_from_slice(&(amount - ONE_SHARE).to_le_bytes());
    env.svm.set_account(coll_vault, acc).unwrap();
    let sync = env.v_ix(
        stock_vault::accounts::SyncIssuerState {
            market,
            collateral_mint: env.coll_mint,
            collateral_vault: coll_vault,
        },
        stock_vault::instruction::SyncIssuerState {},
    );
    env.ok(&[sync], &[&admin]);
    assert!(env.market_state().issuer_halt);

    let intruder = env.alice.insecure_clone();
    let ix = env.write_off_inventory_ix(&intruder.pubkey());
    assert_program_error(env.send(&[ix], &[&intruder]), VerdictError::Unauthorized);

    let ix = env.write_off_inventory_ix(&admin.pubkey());
    env.ok(&[ix], &[&admin]);
    assert_eq!(env.inventory_state(&market).cost_total, 0);
    assert_eq!(env.config_state().inventory_cost_total, 0);

    // The deposit/withdraw pause is lifted: a new backer can deposit again.
    env.back(USDC);

    env.warp(1);
    let ix = env.write_off_inventory_ix(&admin.pubkey());
    assert_program_error(env.send(&[ix], &[&admin]), VerdictError::ZeroAmount);
}
