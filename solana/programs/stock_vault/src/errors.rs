use anchor_lang::prelude::*;

#[error_code]
pub enum VaultError {
    #[msg("Signer is not authorized for this action")]
    Unauthorized,
    #[msg("Vault is paused")]
    Paused,
    #[msg("Market parameters are outside their hard bounds")]
    InvalidMarketParams,
    #[msg("Collateral mint must be Token-2022 with the Scaled UI Amount extension")]
    UnsupportedCollateralMint,
    #[msg("Fee-bearing or closable mints are not accepted")]
    UnsupportedMintExtension,
    #[msg("Scaled UI multiplier is not a usable number")]
    InvalidMultiplier,
    #[msg("Price must be greater than zero")]
    InvalidPrice,
    #[msg("Price updates must be strictly newer than the last one")]
    StalePriceUpdate,
    #[msg("No usable price: none recorded, too old, or flagged")]
    PriceUnavailable,
    #[msg("Issuer has paused the mint or added a transfer hook")]
    IssuerHalt,
    #[msg("Paused around a multiplier activation, or held until a split is repriced")]
    CorporateActionHold,
    #[msg("Borrow or collateral cap reached")]
    CapReached,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Borrow would exceed the loan-to-value limit")]
    ExceedsLtv,
    #[msg("Not enough USDC available in the market")]
    InsufficientCash,
    #[msg("Not enough shares or collateral")]
    InsufficientBalance,
    #[msg("Position is not past its liquidation threshold")]
    NotLiquidatable,
    #[msg("The vault's collateral account is frozen")]
    CollateralAccountFrozen,
    #[msg("Vault holds less collateral than the market has recorded")]
    ReconciliationFailed,
    #[msg("Liquidation would seize no collateral")]
    NothingSeized,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Only the program's upgrade authority can initialize")]
    NotUpgradeAuthority,
    #[msg("Admin cannot be the default key")]
    InvalidAdmin,
    #[msg("Payback address cannot be the default key, the vault admin or the feed authority")]
    InvalidPayoutAddress,
    #[msg("Only the backstop pool may liquidate until the position has been liquidatable for the grace period")]
    PoolPriority,
    #[msg("Pool liquidator cannot be the vault admin or the feed authority")]
    InvalidPoolLiquidator,
    #[msg("Fallback grace period is outside its bounds")]
    InvalidFallbackGrace,
    #[msg("Cluster tag must be localnet (0), devnet (1) or mainnet (2)")]
    InvalidClusterTag,
}

impl From<safu_core::CoreError> for VaultError {
    fn from(e: safu_core::CoreError) -> Self {
        match e {
            safu_core::CoreError::InvalidMultiplier => VaultError::InvalidMultiplier,
            safu_core::CoreError::InvalidParameter => VaultError::InvalidMarketParams,
            _ => VaultError::MathOverflow,
        }
    }
}
