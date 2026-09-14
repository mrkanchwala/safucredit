use anchor_lang::prelude::*;

#[error_code]
pub enum VerdictError {
    #[msg("No Ed25519 precompile instruction directly before this instruction")]
    MissingEd25519Instruction,
    #[msg("Ed25519 instruction data is malformed or offsets are out of bounds")]
    MalformedEd25519Instruction,
    #[msg("Ed25519 instruction must carry exactly one signature")]
    WrongSignatureCount,
    #[msg("Ed25519 offsets must all point into the precompile instruction itself")]
    OffsetsOutsideEd25519Instruction,
    #[msg("Verdict was not signed by the configured oracle")]
    WrongVerdictSigner,
    #[msg("Signed message does not match this verdict")]
    VerdictMessageMismatch,
    #[msg("Verdict must be attested by a top-level instruction")]
    VerdictNotTopLevel,
    #[msg("Verdict deadline has passed")]
    VerdictExpired,
    #[msg("Verdict deadline is too far in the future")]
    VerdictDeadlineTooFar,
    #[msg("The claim must be filed within the claim window of the liquidation")]
    ClaimWindowExpired,
    #[msg("Check (b)'s reference-after timestamp is outside the allowed wait window")]
    InvalidAfterWindow,
    #[msg("The 60-day gate has not elapsed yet")]
    GateNotElapsed,
    #[msg("Claim is not in the expected status for this instruction")]
    WrongClaimStatus,
    #[msg("Cooldown has not elapsed yet")]
    CooldownNotElapsed,
    #[msg("Claim has not gone stale yet")]
    NotYetStale,
    #[msg("Queued claim has not reached the claim window yet")]
    NotYetExpired,
    #[msg("Borrower already has an unresolved claim in this market")]
    BorrowerClaimsFull,
    #[msg("Payout address cannot be a privileged role")]
    PrivilegedPayout,
    #[msg("Signer is not authorized for this action")]
    Unauthorized,
    #[msg("Parameter is outside its hard bounds")]
    InvalidParams,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Not enough shares or balance")]
    InsufficientBalance,
    #[msg("No withdrawal has been requested")]
    NoWithdrawalPending,
    #[msg("Withdrawal delay has not elapsed")]
    WithdrawalNotReady,
    #[msg("Attestation does not match the liquidation record supplied")]
    RecordMismatch,
    #[msg("Liquidation happened while the issuer had intervened; never covered")]
    IssuerHaltedLiquidation,
    #[msg("Liquidator and borrower are the same party")]
    SelfDealtLiquidation,
    #[msg("A backer cannot be paid from the pool they back")]
    BorrowerIsABacker,
    #[msg("Backstop has no capital available to pay")]
    NothingToPay,
    #[msg("Market has no uncovered bad debt")]
    NoBadDebt,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Only the program's upgrade authority can initialize")]
    NotUpgradeAuthority,
    // --- phase 3: pool-as-liquidator -----------------------------------------------------------
    #[msg("Deposits and withdrawals pause while the pool holds inventory")]
    PausedForInventory,
    #[msg("Repay amount, after caps, is below the minimum the pool will liquidate for")]
    BelowMinPoolRepay,
    #[msg("write_off_inventory is only for a market the issuer has frozen or halted")]
    NotFrozenOrHalted,
    #[msg("Resale is blocked while the price is flagged or a corporate-action hold is active")]
    ResaleBlocked,
    #[msg("Sale price exceeds the buyer's max_price_per_share")]
    PriceAboveMax,
    #[msg("Nothing to buy: raw amount is zero or exceeds what the market's inventory holds")]
    NothingToBuy,
    #[msg("Inventory account does not belong to the market supplied")]
    MarketMismatch,
    // --- phase 4: overrides ---------------------------------------------------------------------
    #[msg("admin, verdict_oracle and co_signer must all be distinct")]
    RoleCollision,
    #[msg("Borrower cannot be a privileged role")]
    PrivilegedBorrower,
    #[msg("This attestation was revoked before submission")]
    AttestationRevoked,
    #[msg("Claim is not in a cancellable status")]
    ClaimNotCancellable,
    #[msg("Claim is already suspended")]
    AlreadySuspended,
    #[msg("Claim is not suspended")]
    NotSuspended,
    #[msg("This claim is suspended")]
    ClaimSuspended,
    #[msg("Caller is neither the admin nor the co-signer")]
    CallerNotAdminOrCoSigner,
    #[msg("Override parameters do not match the pending request")]
    OverrideParamsMismatch,
    #[msg("This override request has already executed")]
    OverrideAlreadyExecuted,
    #[msg("A completed claim cannot be overridden")]
    ClaimAlreadyCompleted,
}
