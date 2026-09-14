//! Market logic, kept out of the instruction contexts so it can be read in one place.

use anchor_lang::prelude::*;
use anchor_spl::token_2022::spl_token_2022::extension::{
    mint_close_authority::MintCloseAuthority, pausable::PausableConfig,
    scaled_ui_amount::ScaledUiAmountConfig, transfer_fee::TransferFeeConfig,
    transfer_hook::TransferHook,
};
use anchor_spl::token_interface::get_mint_extension_data;
use safu_core::{
    collateral::{collateral_value, effective_multiplier},
    lending::{accrue_index, borrow_rate_bps, debt_for_shares, utilization_bps, RateCurve},
    price::{deviation_exceeded, twap, Sample},
    BPS, MULT_SCALE,
};

use crate::errors::VaultError;
use crate::state::{Market, PriceState, TWAP_SLOTS};

pub fn core<T>(r: safu_core::Result<T>) -> Result<T> {
    r.map_err(|e| VaultError::from(e).into())
}

/// Token-2022 stores the multiplier as f64. Soroban cannot run floats, so the conversion to fixed point happens
/// here, once, and never inside safu-core. Rounds down.
pub fn multiplier_to_fp(m: f64) -> Result<u128> {
    require!(
        m.is_finite() && m > 0.0 && m <= 1_000_000.0,
        VaultError::InvalidMultiplier
    );
    // Bounded above by 1e6 * 1e12 = 1e18, far inside u128; the value is finite and positive.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let fp = (m * MULT_SCALE as f64).floor() as u128;
    require!(fp > 0, VaultError::InvalidMultiplier);
    Ok(fp)
}

/// The mint's multiplier schedule: (stored, scheduled, activation timestamp). The stored field is not rewritten
/// when the timestamp passes, so both are always read.
pub struct MultiplierSchedule {
    pub stored: u128,
    pub scheduled: u128,
    pub activation_ts: i64,
}

impl MultiplierSchedule {
    pub fn read(mint: &AccountInfo) -> Result<Self> {
        let cfg = get_mint_extension_data::<ScaledUiAmountConfig>(mint)
            .map_err(|_| VaultError::UnsupportedCollateralMint)?;
        let stored = multiplier_to_fp(f64::from(cfg.multiplier))?;
        let scheduled_raw = f64::from(cfg.new_multiplier);
        let scheduled = if scheduled_raw == 0.0 {
            stored
        } else {
            multiplier_to_fp(scheduled_raw)?
        };
        Ok(Self {
            stored,
            scheduled,
            activation_ts: i64::from(cfg.new_multiplier_effective_timestamp),
        })
    }

    /// Multiplier in effect at `now` (spec 4).
    pub fn effective(&self, now: i64) -> u128 {
        effective_multiplier(now, self.stored, self.scheduled, self.activation_ts)
    }

    fn change_bps(&self) -> u128 {
        self.stored.abs_diff(self.scheduled).saturating_mul(BPS) / self.stored
    }

    fn has_activation(&self) -> bool {
        self.scheduled != self.stored
    }
}

/// Issuer guidance: pause all interactions ±`activation_pause_secs` around every multiplier activation. For a
/// split or reverse split, also hold until the feed has published a market-open price after the activation
/// (price and multiplier otherwise desync), for at most `split_max_hold_secs`.
pub fn corporate_action_hold(m: &Market, schedule: &MultiplierSchedule, now: i64) -> bool {
    if !schedule.has_activation() {
        return false;
    }
    let ts = schedule.activation_ts;
    let pause = m.params.activation_pause_secs;
    if now >= ts.saturating_sub(pause) && now <= ts.saturating_add(pause) {
        return true;
    }
    let is_split = schedule.change_bps() > m.params.split_cap_bps as u128;
    if is_split && now > ts && now <= ts.saturating_add(m.params.split_max_hold_secs) {
        let repriced = m.price.market_open && m.price.last_update > ts;
        return !repriced;
    }
    false
}

/// Spec 14: an issuer pause or an added transfer hook halts new borrows and liquidations.
pub fn issuer_blocks(mint: &AccountInfo) -> bool {
    let paused = get_mint_extension_data::<PausableConfig>(mint)
        .map(|p| bool::from(p.paused))
        .unwrap_or(false);
    let hooked = get_mint_extension_data::<TransferHook>(mint)
        .map(|h| Option::<Pubkey>::from(h.program_id).is_some())
        .unwrap_or(false);
    paused || hooked
}

/// Fee-bearing mints break 1:1 accounting; closable mints can be re-created with different rules.
pub fn reject_risky_mint(mint: &AccountInfo) -> Result<()> {
    require!(
        get_mint_extension_data::<TransferFeeConfig>(mint).is_err(),
        VaultError::UnsupportedMintExtension
    );
    let closable = get_mint_extension_data::<MintCloseAuthority>(mint)
        .map(|c| Option::<Pubkey>::from(c.close_authority).is_some())
        .unwrap_or(false);
    require!(!closable, VaultError::UnsupportedMintExtension);
    Ok(())
}

pub fn total_borrows(m: &Market) -> Result<u64> {
    core(debt_for_shares(m.total_borrow_shares, m.borrow_index))
}

/// Supplier-owned value: cash plus what borrowers owe, less recognised bad debt.
pub fn total_assets(m: &Market) -> Result<u64> {
    let assets = m
        .cash
        .checked_add(total_borrows(m)?)
        .ok_or(VaultError::MathOverflow)?;
    Ok(assets.saturating_sub(m.bad_debt))
}

pub fn accrue(m: &mut Market, now: i64) -> Result<()> {
    if now <= m.last_accrual_ts {
        return Ok(());
    }
    let util = core(utilization_bps(total_borrows(m)?, m.cash))?;
    let curve = RateCurve {
        base_bps: m.params.rate.base_bps,
        slope1_bps: m.params.rate.slope1_bps,
        slope2_bps: m.params.rate.slope2_bps,
        kink_bps: m.params.rate.kink_bps,
    };
    let rate = core(borrow_rate_bps(util, curve))?;
    let elapsed = u64::try_from(now - m.last_accrual_ts).map_err(|_| VaultError::MathOverflow)?;
    m.borrow_index = core(accrue_index(m.borrow_index, rate, elapsed))?;
    m.last_accrual_ts = now;
    Ok(())
}

/// `amount × numerator / denominator`, rounded down.
pub fn mul_div_floor(amount: u128, numerator: u128, denominator: u128) -> Result<u128> {
    amount
        .checked_mul(numerator)
        .and_then(|v| v.checked_div(denominator))
        .ok_or_else(|| VaultError::MathOverflow.into())
}

fn ordered_samples(p: &PriceState) -> Vec<Sample> {
    let count = p.count as usize;
    let start = (p.head as usize + TWAP_SLOTS - count) % TWAP_SLOTS;
    (0..count)
        .map(|i| {
            let idx = (start + i) % TWAP_SLOTS;
            Sample {
                price_fp: p.prices[idx],
                ts: p.timestamps[idx],
            }
        })
        .collect()
}

pub fn market_twap(m: &Market, now: i64) -> Result<u64> {
    core(twap(&ordered_samples(&m.price), now))
}

/// Mock-adapter price update (spec 5, final 2026-09-14). A move beyond the spike cap vs TWAP is flagged and kept
/// out of the TWAP; a second consecutive update within the cap of the flagged price confirms a genuine move and is
/// accepted, so a real crash cannot be locked out. No off-hours clamp: it stalled liquidations in a real crash.
/// Returns whether the price was accepted.
pub fn apply_price(
    m: &mut Market,
    price: u64,
    market_open: bool,
    last_close: u64,
    now: i64,
) -> Result<bool> {
    require!(price > 0 && last_close > 0, VaultError::InvalidPrice);
    require!(now > m.price.last_update, VaultError::StalePriceUpdate);

    if m.price.count > 0 {
        let reference = market_twap(m, now)?;
        let cap = m.params.deviation_cap_bps;
        let spike = core(deviation_exceeded(price, reference, cap))?;
        let confirmed =
            m.price.flagged && !core(deviation_exceeded(price, m.price.flagged_price, cap))?;
        if spike && !confirmed {
            m.price.flagged = true;
            m.price.flagged_price = price;
            return Ok(false);
        }
    }

    let head = m.price.head as usize;
    m.price.prices[head] = price;
    m.price.timestamps[head] = now;
    m.price.head = ((head + 1) % TWAP_SLOTS) as u8;
    m.price.count = (m.price.count as usize + 1).min(TWAP_SLOTS) as u8;
    m.price.last_price = price;
    m.price.last_update = now;
    m.price.last_close = last_close;
    m.price.market_open = market_open;
    m.price.flagged = false;
    m.price.flagged_price = 0;
    Ok(true)
}

/// Which action is asking for a price; the age limit is asymmetric by design.
pub enum PriceUse {
    /// Borrow or collateral withdrawal: fresh price, lowest of TWAP and last.
    Borrow,
    /// Liquidation: TWAP, tolerates the feed's own heartbeat.
    Liquidate,
}

pub fn risk_price(m: &Market, now: i64, use_for: PriceUse) -> Result<u64> {
    require!(
        m.price.count > 0 && !m.price.flagged,
        VaultError::PriceUnavailable
    );
    let age = now
        .checked_sub(m.price.last_update)
        .ok_or(VaultError::MathOverflow)?;
    let max_age = match (&use_for, m.price.market_open) {
        (_, false) => m.params.max_price_age_closed_secs,
        (PriceUse::Borrow, true) => m.params.borrow_max_price_age_secs,
        (PriceUse::Liquidate, true) => m.params.liquidation_max_price_age_open_secs,
    };
    require!(age >= 0 && age <= max_age, VaultError::PriceUnavailable);
    let twap = market_twap(m, now)?;
    Ok(match use_for {
        PriceUse::Borrow => twap.min(m.price.last_price),
        PriceUse::Liquidate => twap,
    })
}

pub fn value_of(m: &Market, raw: u64, multiplier_fp: u128, price: u64) -> Result<u64> {
    core(collateral_value(
        raw,
        m.collateral_decimals,
        multiplier_fp,
        price,
    ))
}
