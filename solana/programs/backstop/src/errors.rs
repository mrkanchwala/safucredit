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
    #[msg("Payout must be greater than zero")]
    ZeroPayout,
    #[msg("Tier must be 1, 2 or 3")]
    InvalidTier,
}
