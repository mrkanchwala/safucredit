use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"bconfig";
pub const ATTESTATION_SEED: &[u8] = b"verdict";

/// Current layout version of every account in this program (upgradeability U4).
pub const ACCOUNT_VERSION: u8 = 1;

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
    /// Room for later fields without a migration (U4).
    pub reserved: [u8; 64],
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
