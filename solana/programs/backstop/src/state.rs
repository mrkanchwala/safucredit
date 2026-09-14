use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"bconfig";
pub const BACKER_SEED: &[u8] = b"backer";
pub const CLAIM_SEED: &[u8] = b"claim";
pub const BORROWER_CLAIMS_SEED: &[u8] = b"bclaims";
pub const BAD_DEBT_SEED: &[u8] = b"bd";
pub const USDC_VAULT_SEED: &[u8] = b"busdc";

/// Current layout version of every account in this program (upgradeability U4).
pub const ACCOUNT_VERSION: u8 = 1;
/// Hard ceiling on the per-call share of backer capital any single payout may take. Strictly below
/// 100% on purpose: it is what keeps `cash` from reaching zero while shares are still outstanding,
/// which would leave the share price undefined and block every future deposit.
pub const MAX_PER_CLAIM_CAP_BPS: u32 = 5_000;

/// Loan younger than this at liquidation gets its claim held, not denied (LOCKED verdict spec).
pub const GATE_SECS: i64 = 60 * 86_400;
/// A wrongful-liquidation claim must be filed within this many seconds of the liquidation.
pub const CLAIM_WINDOW_SECS: i64 = 30 * 86_400;
/// Delay between a claim being admitted and its stream starting.
pub const COOLDOWN_SECS: i64 = 7 * 86_400;
/// Linear payout window once streaming starts.
pub const STREAM_SECS: i64 = 45 * 86_400;
/// An admitted claim with no pull for this long returns its unpaid remainder to the backstop.
pub const INACTIVITY_EXPIRY_SECS: i64 = 100 * 86_400;
/// Check (b)'s "reference after" must land in this window relative to the liquidation.
pub const MIN_AFTER_WAIT_SECS: i64 = 3_600;
pub const MAX_AFTER_WAIT_SECS: i64 = 4 * 86_400;
/// Day-bucket length for the admission and outflow counters.
pub const DAY_SECS: i64 = 86_400;

/// Utilisation bands (bps of `reserved_total` over `cash`) that set the admission and outflow caps.
pub const UTIL_LOW_BPS: u128 = 2_000;
pub const UTIL_MID_BPS: u128 = 5_000;
/// Daily admission cap, as a share of cash, by utilisation band (LOCKED verdict spec: 25/10/3%).
pub const ADMISSION_CAP_LOW_BPS: u32 = 2_500;
pub const ADMISSION_CAP_MID_BPS: u32 = 1_000;
pub const ADMISSION_CAP_HIGH_BPS: u32 = 300;
/// Daily outflow cap per claim, as a share of `max(cash, snapshot)`, by utilisation band (5/3/1%).
pub const OUTFLOW_CAP_LOW_BPS: u32 = 500;
pub const OUTFLOW_CAP_MID_BPS: u32 = 300;
pub const OUTFLOW_CAP_HIGH_BPS: u32 = 100;

/// On-chain deny reason codes (LOCKED verdict spec: "denials recorded on-chain with a reason
/// code"). `NONE` on every non-denied claim.
pub mod deny_reason {
    pub const NONE: u16 = 0;
    /// Check (a): the price used to liquidate was not actually off the reference.
    pub const PRICE_NOT_WRONG: u16 = 1;
    /// Check (b): the reference price recovered — the move held, so it was not fake.
    pub const MOVE_HELD: u16 = 2;
    /// Loss computed to zero once debt repaid is netted out.
    pub const ZERO_LOSS: u16 = 3;
}

#[account]
#[derive(InitSpace)]
pub struct BackstopConfig {
    pub version: u8,
    pub admin: Pubkey,
    /// Throwaway per-chain Ed25519 key. Never the production KMS key.
    pub verdict_oracle: Pubkey,
    /// Bound into every signed verdict so a devnet signature can never verify on mainnet.
    pub cluster_tag: u8,
    pub bump: u8,
    pub usdc_mint: Pubkey,
    /// Backer capital actually paid in, tracked internally. Never read from the token account, so a
    /// raw transfer into the vault cannot move the share price (same rule as the lending market).
    pub cash: u64,
    pub total_shares: u128,
    /// Ceiling on a single claim's admission, as a share of cash checked at admission time. A loss
    /// above it never shrinks — it queues until cash grows or utilisation falls enough to admit it.
    pub per_claim_cap_bps: u32,
    /// Sum of `loss − streamed` across every `Active` claim. Backers may withdraw `cash −
    /// reserved_total`; a queued or held claim reserves nothing until it is actually admitted.
    pub reserved_total: u64,
    /// Day bucket (`now / DAY_SECS`) the admission counter below is tracking.
    pub admission_day: i64,
    /// Cumulative loss admitted so far within `admission_day`.
    pub admitted_today: u64,
    /// Delay between requesting an exit and taking the money.
    pub withdraw_delay_secs: i64,
    /// Room for later fields without a migration (U4).
    pub reserved: [u8; 32],
}

/// One backer's stake in the pool.
#[account]
#[derive(InitSpace)]
pub struct Backer {
    pub version: u8,
    pub bump: u8,
    pub owner: Pubkey,
    pub shares: u128,
    /// Shares queued for exit, and when the queue started. Zero when no exit is pending.
    pub withdraw_shares: u128,
    pub withdraw_requested_at: i64,
    pub reserved: [u8; 32],
}

/// Running total of bad debt the backstop has reimbursed one market for. Makes `cover_bad_debt`
/// idempotent against the market's own monotonic counter, with no epochs to coordinate.
#[account]
#[derive(InitSpace)]
pub struct BadDebtCover {
    pub version: u8,
    pub bump: u8,
    pub market: Pubkey,
    pub total_paid: u64,
    pub reserved: [u8; 32],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum ClaimStatus {
    /// A check failed or loss computed to zero. Terminal; see `deny_reason`.
    Denied,
    /// Loan was younger than `GATE_SECS` at liquidation; held at full value.
    PendingTime,
    /// Passed every check but did not fit the admission cap or solvency at the time; re-checkable.
    Queued,
    /// Admitted: reserved, cooling down or streaming.
    Active,
    /// Fully streamed.
    Completed,
    /// Timed out — either queued past the claim window, or admitted but uncollected for
    /// `INACTIVITY_EXPIRY_SECS`. Terminal.
    Expired,
    /// Reserved for a future admin override (phase 4). Unused today. Terminal.
    Cancelled,
}

impl ClaimStatus {
    /// A terminal claim frees its slot in `BorrowerClaims`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ClaimStatus::Denied
                | ClaimStatus::Completed
                | ClaimStatus::Expired
                | ClaimStatus::Cancelled
        )
    }
}

/// One per liquidation. `init` is the idempotency: a second facts submission for the same
/// liquidation cannot create this twice, so a replayed transaction cannot open a second claim.
#[account]
#[derive(InitSpace)]
pub struct Claim {
    pub version: u8,
    pub bump: u8,
    pub market: Pubkey,
    pub borrower: Pubkey,
    pub liquidation_record: Pubkey,
    /// Frozen at submission from the liquidation record; the eventual payout destination.
    pub payout: Pubkey,
    pub status: ClaimStatus,
    pub deny_reason: u16,
    /// `safu_core::wrongful_loss`, computed once at submission. Zero on a denied claim.
    pub loss: u64,
    /// Cumulative amount actually paid out via `claim_stream`.
    pub streamed: u64,
    pub submitted_at: i64,
    /// When this liquidation happened (`LiquidationRecord.ts`), copied here so later instructions
    /// never need to re-fetch the vault's record.
    pub liquidated_at: i64,
    /// `PendingTime` only: when the 60-day gate clears and `unlock_claim` may be called.
    pub releasable_at: i64,
    /// `Active` only: when admission happened.
    pub admitted_at: i64,
    pub cooldown_end: i64,
    pub stream_end: i64,
    /// `cash` at the moment of admission — the outflow cap's floor, so a claim admitted against a
    /// large pool cannot be strangled if the pool later shrinks.
    pub snapshot_cash: u64,
    pub last_pull_day: i64,
    pub pulled_today: u64,
    /// Updated at admission and at every successful pull. `expire_stale` reads it.
    pub last_activity_ts: i64,
    pub evidence_hash: [u8; 32],
    pub reserved: [u8; 24],
}

/// One per (market, borrower). Tracks the single live claim slot for that pair — phase 2 keeps one
/// slot, not the locked spec's separate open+queued pair (see job memory: a second facts submission
/// while one is unresolved is refused outright rather than queued in parallel).
#[account]
#[derive(InitSpace)]
pub struct BorrowerClaims {
    pub version: u8,
    pub bump: u8,
    pub market: Pubkey,
    pub borrower: Pubkey,
    /// `Pubkey::default()` when free. Cleared to the new claim once the occupant is terminal.
    pub open: Pubkey,
    pub reserved: [u8; 32],
}
