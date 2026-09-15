use anchor_lang::prelude::*;

use crate::errors::VaultError;

pub const VCONFIG_SEED: &[u8] = b"vconfig";
pub const MARKET_SEED: &[u8] = b"market";
pub const COLL_VAULT_SEED: &[u8] = b"coll_vault";
pub const USDC_VAULT_SEED: &[u8] = b"usdc_vault";
pub const POSITION_SEED: &[u8] = b"position";
pub const SUPPLIER_SEED: &[u8] = b"supplier";
pub const LIQ_RECORD_SEED: &[u8] = b"liq";

/// Layout version of every account in this program (upgradeability U4).
pub const ACCOUNT_VERSION: u8 = 1;
/// Price samples kept for the TWAP.
pub const TWAP_SLOTS: usize = 16;
/// Every borrower is covered for 100% of a proven wrongful-liquidation loss (founder decision 2026-09-14).
pub const FULL_COVERAGE_BPS: u32 = 10_000;

#[account]
#[derive(InitSpace)]
pub struct VaultConfig {
    pub version: u8,
    pub admin: Pubkey,
    /// Pushes prices through the mock adapter on devnet. Separate key from the verdict oracle.
    pub feed_authority: Pubkey,
    pub paused: bool,
    pub bump: u8,
    /// The backstop pool's liquidator key (its config PDA). It may liquidate at once; anyone else only after
    /// the grace period. Default = no pool registered, so the grace rule applies to every liquidator.
    pub pool_liquidator: Pubkey,
    /// How long a position must have been liquidatable before an outside liquidator may act. 900 s.
    pub fallback_grace_secs: i64,
    /// Which cluster this deployment is on (0 localnet, 1 devnet, 2 mainnet), set once at initialize with no
    /// setter. Selects the time floors (eng review A5): mainnet may never shorten the grace below 5 min.
    pub cluster_tag: u8,
    /// Taken from the old 64 reserved bytes (32 + 8 + 1), so the account size is unchanged (U4).
    pub reserved: [u8; 23],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct RateParams {
    pub base_bps: u32,
    pub slope1_bps: u32,
    pub slope2_bps: u32,
    pub kink_bps: u32,
}

/// Outside liquidators wait this long after a position is first marked liquidatable (eng review A2).
pub const DEFAULT_FALLBACK_GRACE_SECS: i64 = 900;
pub const MIN_FALLBACK_GRACE_SECS: i64 = 60;
pub const MAX_FALLBACK_GRACE_SECS: i64 = 86_400;
/// Eng review A5: on mainnet the pool's head start can never drop below 5 minutes.
pub const MAINNET_MIN_FALLBACK_GRACE_SECS: i64 = 300;

/// Same numbering as the backstop's `verdict::CLUSTER_*` (asserted in the backstop's unit tests).
pub const CLUSTER_LOCALNET: u8 = 0;
pub const CLUSTER_DEVNET: u8 = 1;
pub const CLUSTER_MAINNET: u8 = 2;

pub fn min_fallback_grace_secs(cluster_tag: u8) -> i64 {
    if cluster_tag == CLUSTER_MAINNET {
        MAINNET_MIN_FALLBACK_GRACE_SECS
    } else {
        MIN_FALLBACK_GRACE_SECS
    }
}

/// How long a tightening of the liquidation terms takes to reach existing loans (U3, founder 2026-09-14).
/// Loosening applies at once. A constant, not a parameter: an admin must not be able to set it to zero.
pub const LIQUIDATION_TERMS_RAMP_SECS: i64 = 7 * 86_400;

/// The terms that decide when, and how hard, an existing loan is liquidated. Changes that tighten them
/// ramp in over `LIQUIDATION_TERMS_RAMP_SECS`, so no borrower is liquidated overnight by a number change.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct LiquidationTerms {
    pub liq_threshold_bps: u32,
    pub insolvency_ltv_bps: u32,
    pub close_factor_bps: u32,
    pub min_liq_bonus_bps: u32,
    pub max_liq_bonus_bps: u32,
}

/// Admin-settable within hard bounds (U2). Values and the evidence behind each:
/// `outputs/2026-09-14_stocklana-market-config-research.md` §2b (research-ops).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub struct MarketParams {
    /// Borrowing limit. AAPLx: 4_000 (matched to the market leader).
    pub ltv_bps: u32,
    /// Liquidation line. AAPLx: 5_000.
    pub liq_threshold_bps: u32,
    /// Dynamic liquidation bonus: max(min, LTV − threshold), capped at max and at 100% − LTV. 100 / 500.
    pub min_liq_bonus_bps: u32,
    pub max_liq_bonus_bps: u32,
    /// Above this LTV the whole debt may be liquidated at once. 9_500.
    pub insolvency_ltv_bps: u32,
    /// Share of debt repayable per liquidation below the insolvency line. 2_500.
    pub close_factor_bps: u32,
    /// USDC base units per liquidation, sized from measured collateral liquidity ($100K).
    pub max_liquidation_debt: u64,
    /// Single-update spike cap vs TWAP. 500 (US LULD Tier 1 band).
    pub deviation_cap_bps: u32,
    /// Issuer-recommended pause around every multiplier activation. 900 s.
    pub activation_pause_secs: i64,
    /// A multiplier change above this is a split / reverse split. 500.
    pub split_cap_bps: u32,
    /// Longest a split can hold risk actions while waiting for a market-open price. 86_400 s.
    pub split_max_hold_secs: i64,
    /// Borrow / withdraw need a price at most this old while the market is open. 3_600 s.
    pub borrow_max_price_age_secs: i64,
    /// Liquidation price age limit while open (feed's own 0.5% / 24h guarantee + 1h). 90_000 s.
    pub liquidation_max_price_age_open_secs: i64,
    /// Any action while the underlying market is closed (longest US closure ≈ 89.5h). 345_600 s.
    pub max_price_age_closed_secs: i64,
    pub rate: RateParams,
    /// Total USDC that may be lent (backstop sizing rule, 10:1). Base units.
    pub borrow_cap: u64,
    /// Total raw collateral accepted.
    pub collateral_cap_raw: u64,
    /// Share of accrued borrower interest credited to backers, bounded <= 5_000 (founder-locked at
    /// 1_500 = 15%; eng review addendum 2026-09-14). A change applies to interest accrued after it.
    pub backer_interest_share_bps: u32,
}

impl MarketParams {
    pub fn liquidation_terms(&self) -> LiquidationTerms {
        LiquidationTerms {
            liq_threshold_bps: self.liq_threshold_bps,
            insolvency_ltv_bps: self.insolvency_ltv_bps,
            close_factor_bps: self.close_factor_bps,
            min_liq_bonus_bps: self.min_liq_bonus_bps,
            max_liq_bonus_bps: self.max_liq_bonus_bps,
        }
    }

    /// Hard bounds in code, so an admin mistake cannot configure something unsafe.
    pub fn validate(&self) -> Result<()> {
        let ok = self.ltv_bps > 0
            && self.ltv_bps <= 8_000
            && self.liq_threshold_bps > self.ltv_bps
            && self.liq_threshold_bps <= 9_000
            && self.min_liq_bonus_bps <= self.max_liq_bonus_bps
            && self.max_liq_bonus_bps <= 2_000
            && self.insolvency_ltv_bps > self.liq_threshold_bps
            && self.insolvency_ltv_bps <= 10_000
            && self.close_factor_bps > 0
            && self.close_factor_bps <= 10_000
            && self.max_liquidation_debt > 0
            && self.deviation_cap_bps >= 50
            && self.deviation_cap_bps <= 2_000
            && (0..=3_600).contains(&self.activation_pause_secs)
            && self.split_cap_bps > 0
            && self.split_cap_bps <= 10_000
            && (0..=7 * 86_400).contains(&self.split_max_hold_secs)
            && (60..=86_400).contains(&self.borrow_max_price_age_secs)
            && (self.borrow_max_price_age_secs..=2 * 86_400)
                .contains(&self.liquidation_max_price_age_open_secs)
            && (3_600..=7 * 86_400).contains(&self.max_price_age_closed_secs)
            && self.rate.kink_bps > 0
            && self.rate.kink_bps < 10_000
            && self.rate.base_bps <= 5_000
            && self.rate.slope1_bps <= 20_000
            && self.rate.slope2_bps <= 100_000
            && self.borrow_cap > 0
            && self.collateral_cap_raw > 0
            && self.backer_interest_share_bps <= 5_000;
        require!(ok, VaultError::InvalidMarketParams);
        Ok(())
    }
}

#[derive(
    AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, InitSpace,
)]
pub struct PriceState {
    /// Ring buffer, USD per whole share with 8 decimals.
    pub prices: [u64; TWAP_SLOTS],
    pub timestamps: [i64; TWAP_SLOTS],
    /// Next write position.
    pub head: u8,
    pub count: u8,
    pub last_price: u64,
    pub last_update: i64,
    pub last_close: u64,
    /// Whether the underlying equity market was open at the last accepted update.
    pub market_open: bool,
    /// Set when an update broke the spike cap. Blocks borrowing and liquidation until cleared.
    pub flagged: bool,
    /// The rejected price, so a second consecutive update confirming it can be accepted.
    pub flagged_price: u64,
}

#[account]
#[derive(InitSpace)]
pub struct Market {
    pub version: u8,
    pub bump: u8,
    pub collateral_mint: Pubkey,
    pub usdc_mint: Pubkey,
    pub collateral_decimals: u8,
    pub usdc_decimals: u8,
    pub params: MarketParams,
    pub price: PriceState,
    /// USDC held, tracked internally. Never inferred from the vault balance (donation attacks).
    pub cash: u64,
    pub total_supply_shares: u128,
    pub total_borrow_shares: u128,
    /// Debt index, 1.0 == safu_core::lending::INDEX_SCALE.
    pub borrow_index: u128,
    pub last_accrual_ts: i64,
    /// Sum of all positions' raw collateral, for reconciliation against the vault (review D4).
    pub total_collateral_raw: u64,
    /// Bad debt still uncovered. Falls when the backstop reimburses it.
    pub bad_debt: u64,
    /// Every write-off this market has ever taken, monotonic. The backstop compares its own running
    /// total against this to work out what it still owes, which makes reimbursement idempotent with
    /// no epoch counter to keep in step.
    pub bad_debt_cumulative: u64,
    pub issuer_halt: bool,
    pub liq_seq: u64,
    /// Effective multiplier this market last acted on, in `MULT_SCALE` fixed point. The baseline the
    /// unannounced-change guard compares the live value against. Seeded by `create_market`; 0 means
    /// "not yet observed", which the guard treats as no change rather than as a change from zero.
    pub observed_multiplier_fp: u128,
    /// Where the current liquidation-terms ramp started from, and when (U3). The target is `params`.
    /// Taken from the old 40 reserved bytes (20 + 8), so the account size is unchanged (U4).
    pub ramp_from: LiquidationTerms,
    pub ramp_start_ts: i64,
    /// Interest credited to backers but not yet paid out (eng review addendum). Excluded from
    /// `total_assets`, so lenders' share price reflects only their 85% (2c pattern, in reverse).
    pub backer_interest_owed: u64,
    /// Monotonic total ever credited, mirrored by `InterestAbsorbed.total_absorbed` on the backstop
    /// side so repeat `absorb_interest` calls settle only the remainder.
    pub backer_interest_cumulative: u64,
    /// Monotonic total ever transferred to the pool via `pay_backer_interest`.
    pub backer_interest_paid_cumulative: u64,
    /// Debt written off on positions whose collateral the issuer seized (`write_down_collateral`). Kept apart
    /// from `bad_debt_cumulative` on purpose: the backstop reimburses only that counter, and issuer actions are
    /// never covered (spec 14a) -- lenders carry this loss, as they carry it on every market holding this token.
    /// Taken from the old 12 reserved bytes, so the account size is unchanged (U4).
    pub issuer_loss_cumulative: u64,
    /// Multiplier the stored price history (`price`) is quoted in, `MULT_SCALE` fixed point. Every price is per
    /// whole token, and a split changes what one token is worth, so when the live multiplier moves away from this
    /// value the history is re-quoted into it first (`logic::sync_price_units`). Kept apart from
    /// `observed_multiplier_fp` on purpose: the price history must follow an unannounced change at once, while
    /// the market's baseline waits for `acknowledge_multiplier`. Added before any deployment, so it grows the
    /// account instead of taking reserved bytes (4 were not enough).
    pub price_units_fp: u128,
    pub reserved: [u8; 4],
}

#[account]
#[derive(InitSpace)]
pub struct Position {
    pub version: u8,
    pub bump: u8,
    pub market: Pubkey,
    pub owner: Pubkey,
    pub raw_collateral: u64,
    pub debt_shares: u128,
    /// Coverage in force when this loan was opened (U3: later changes never shrink an open loan's cover).
    pub coverage_bps: u32,
    /// Debt-weighted borrow time (unix seconds), 0 while there is no debt. Each borrow pulls it toward
    /// now in proportion to its size; full repayment resets it. The backstop's 60-day gate reads it, so
    /// a tiny early loan topped up after a price breaks still counts as young.
    pub borrow_age_ts: i64,
    /// Where a wrongful-liquidation payback goes. Defaults to the owner, who may change it. A liquidation
    /// record freezes it, so nobody can redirect a payback after the fact.
    pub payout: Pubkey,
    /// When this position was first seen liquidatable by `mark_liquidatable`, and most recently. 0 = not
    /// marked. Outside liquidators wait on these; the pool does not.
    pub liquidatable_first_seen: i64,
    pub liquidatable_last_seen: i64,
    /// Set by `write_down_collateral` once any of this position's collateral is attributed to an issuer seizure.
    /// Permanent. Every later liquidation of this position records `issuer_halt = true`, so the backstop refuses
    /// a payback claim on it, and any debt it leaves behind is issuer loss, never reimbursable bad debt.
    pub issuer_seized: bool,
    /// Raw collateral ever written down on this position.
    pub seized_raw_total: u64,
    /// Taken from the old 32 reserved bytes (8 + 8 + 1 + 8).
    pub reserved: [u8; 7],
}

#[account]
#[derive(InitSpace)]
pub struct Supplier {
    pub version: u8,
    pub bump: u8,
    pub market: Pubkey,
    pub owner: Pubkey,
    pub shares: u128,
    pub reserved: [u8; 32],
}

/// Immutable snapshot of one liquidation.
///
/// The verdict engine decides wrongfulness off-chain, so it must be able to re-derive the decision
/// from what the contract actually used — not from whatever the feed says later. Every input that
/// fed the seizure is recorded here, including `issuer_halt` at the time, because a liquidation
/// taken while the issuer had intervened is excluded from cover (spec 14a).
#[account]
#[derive(InitSpace)]
pub struct LiquidationRecord {
    pub version: u8,
    pub bump: u8,
    pub market: Pubkey,
    pub seq: u64,
    pub borrower: Pubkey,
    pub liquidator: Pubkey,
    /// Raw collateral handed to the liquidator.
    pub seized_raw: u64,
    /// USDC the liquidator repaid on the borrower's behalf.
    pub debt_repaid: u64,
    /// Effective multiplier used for the valuation, in `MULT_SCALE` fixed point.
    pub multiplier_fp: u128,
    /// TWAP the seizure was priced at, 8 decimals.
    pub price_fp: u64,
    pub collateral_decimals: u8,
    pub bonus_bps: u32,
    /// Position LTV at the moment of seizure.
    pub ltv_bps: u32,
    /// Coverage in force for this borrower when the loan was opened (U3).
    pub coverage_bps: u32,
    /// The loan's debt-weighted borrow time at the moment of liquidation (for the 60-day gate).
    pub borrow_age_ts: i64,
    /// Payback address at the moment of liquidation. Frozen here; later changes cannot redirect it.
    pub payout: Pubkey,
    /// Debt written off because the collateral ran out, 0 in the normal case.
    pub bad_debt: u64,
    pub ts: i64,
    pub slot: u64,
    pub issuer_halt: bool,
    pub reserved: [u8; 32],
}
