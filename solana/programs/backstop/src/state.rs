use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"bconfig";
pub const ATTESTATION_SEED: &[u8] = b"verdict";
pub const BACKER_SEED: &[u8] = b"backer";
pub const CLAIM_SEED: &[u8] = b"claim";
pub const BAD_DEBT_SEED: &[u8] = b"bd";
pub const USDC_VAULT_SEED: &[u8] = b"busdc";

/// Current layout version of every account in this program (upgradeability U4).
pub const ACCOUNT_VERSION: u8 = 1;
/// Hard ceiling on the per-call share of backer capital any single payout may take. Strictly below
/// 100% on purpose: it is what keeps `cash` from reaching zero while shares are still outstanding,
/// which would leave the share price undefined and block every future deposit.
pub const MAX_PER_CLAIM_CAP_BPS: u32 = 5_000;

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
    /// Ceiling on a single claim, as a share of backer capital. E4: the hackathon build caps and
    /// discloses rather than running true pro-rata claim epochs.
    pub per_claim_cap_bps: u32,
    /// Attested verdicts not yet paid. Backers cannot exit while any are outstanding (A5 §9).
    pub open_claims: u64,
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

/// One per liquidation. `init` is the idempotency: a second payout for the same liquidation cannot
/// be created, so a replayed transaction pays nothing twice.
#[account]
#[derive(InitSpace)]
pub struct ClaimReceipt {
    pub version: u8,
    pub bump: u8,
    pub liquidation_record: Pubkey,
    pub borrower: Pubkey,
    /// What the oracle attested was owed.
    pub attested: u64,
    /// What the backstop could actually pay. Lower than `attested` means the per-claim cap bit, and
    /// the difference is disclosed rather than queued — E4.
    pub paid: u64,
    pub verdict_hash: [u8; 32],
    pub paid_at: i64,
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

/// One per liquidation. Created only after the oracle's signature over this exact verdict is verified;
/// `init` makes a second attestation for the same liquidation impossible.
#[account]
#[derive(InitSpace)]
pub struct VerdictAttestation {
    pub version: u8,
    pub liquidation_record: Pubkey,
    pub borrower: Pubkey,
    pub payout: u64,
    pub tier: u8,
    pub verdict_hash: [u8; 32],
    pub attested_at: i64,
    pub bump: u8,
    pub reserved: [u8; 32],
}
