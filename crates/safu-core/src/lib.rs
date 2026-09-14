//! safu-core: the math shared by SAFU Credit's Solana (Anchor) and Stellar (Soroban) programs.
//!
//! Rules this crate keeps, so both chains can use it unchanged:
//! - **No floats.** Soroban rejects float ops. Token-2022 stores the xStock multiplier as an f64; the Solana
//!   program converts it to [`MULT_SCALE`] fixed point before calling in here.
//! - **No allocation, no chain dependencies.** Builds for SBF and `wasm32v1-none`.
//! - **Never panics on input.** Overflow and bad input return [`CoreError`].
//! - **Rounding always favours the protocol:** collateral value rounds down, debt rounds up, seized collateral
//!   rounds down.
#![no_std]

pub mod collateral;
pub mod lending;
pub mod loss;
pub mod price;

/// Basis points in 100%.
pub const BPS: u128 = 10_000;
/// Multiplier fixed-point scale: 1.0 == 1_000_000_000_000.
pub const MULT_SCALE: u128 = 1_000_000_000_000;
/// Decimals of every price input (USD per whole share).
pub const PRICE_DECIMALS: u32 = 8;
/// Decimals of every USD output (matches USDC).
pub const USD_DECIMALS: u32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreError {
    Overflow,
    DivideByZero,
    EmptySamples,
    UnsortedSamples,
    InvalidMultiplier,
    InvalidParameter,
}

pub type Result<T> = core::result::Result<T, CoreError>;

pub(crate) fn mul(a: u128, b: u128) -> Result<u128> {
    a.checked_mul(b).ok_or(CoreError::Overflow)
}

pub(crate) fn add(a: u128, b: u128) -> Result<u128> {
    a.checked_add(b).ok_or(CoreError::Overflow)
}

pub(crate) fn div(a: u128, b: u128) -> Result<u128> {
    a.checked_div(b).ok_or(CoreError::DivideByZero)
}

pub(crate) fn div_ceil(a: u128, b: u128) -> Result<u128> {
    if b == 0 {
        return Err(CoreError::DivideByZero);
    }
    Ok(a.div_ceil(b))
}

pub(crate) fn to_u64(v: u128) -> Result<u64> {
    u64::try_from(v).map_err(|_| CoreError::Overflow)
}

pub(crate) fn pow10(n: u32) -> Result<u128> {
    10u128.checked_pow(n).ok_or(CoreError::Overflow)
}

/// `amount × bps / 10_000`, rounded down.
pub fn apply_bps(amount: u64, bps: u32) -> Result<u64> {
    to_u64(div(mul(amount as u128, bps as u128)?, BPS)?)
}

/// Absolute difference between `value` and `reference`, in basis points of `reference`.
pub fn bps_diff(value: u64, reference: u64) -> Result<u128> {
    div(mul(value.abs_diff(reference) as u128, BPS)?, reference as u128)
}
