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
    lending::{
        accrue_index, borrow_rate_bps, debt_for_shares, ramp_bps, utilization_bps, RateCurve,
    },
    price::{deviation_exceeded, twap, Sample},
    BPS, MULT_SCALE,
};

use crate::errors::VaultError;
use crate::state::{
    LiquidationTerms, Market, Position, PriceState, LIQUIDATION_TERMS_RAMP_SECS, TWAP_SLOTS,
};

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

/// A multiplier change that arrived with no schedule this guard could see.
///
/// Token-2022 applies a back-dated or immediate `update_multiplier` at once, collapsing the new
/// value into BOTH the stored and the scheduled field. `corporate_action_hold` therefore has
/// nothing to key on, and the mint's own activation timestamp cannot stand in for one: it is chosen
/// by the issuer, so a value of 0 would put every time window in the distant past and defeat any
/// rule counted from it. The only trustworthy evidence such a change happened is that the effective
/// multiplier differs from the value this market last acted on.
///
/// Fail-closed by design: this hold has **no time-based expiry** and is cleared only by
/// `acknowledge_multiplier`, precisely because there is no timestamp the contract could safely
/// count from.
///
/// Gated on `split_cap_bps`, the same threshold that separates a split from a routine dividend
/// step: a change at or below it can move collateral value by no more than the price deviation cap
/// already tolerates, so it updates the baseline without halting the market. Known limit of that
/// choice: many sub-threshold steps in a row each update the baseline and so never aggregate into a
/// hold. For the real corporate actions this guards against — splits and reverse splits, which move
/// the multiplier by whole multiples — that is not reachable.
pub fn unobserved_multiplier_change(m: &Market, schedule: &MultiplierSchedule, now: i64) -> bool {
    if m.observed_multiplier_fp == 0 || schedule.has_activation() {
        return false;
    }
    let effective = schedule.effective(now);
    if effective == m.observed_multiplier_fp {
        return false;
    }
    let change_bps = m
        .observed_multiplier_fp
        .abs_diff(effective)
        .saturating_mul(BPS)
        / m.observed_multiplier_fp;
    change_bps > m.params.split_cap_bps as u128
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

/// Supplier-owned value: cash plus what borrowers still owe, minus interest already credited to
/// backers (eng review addendum -- `backer_interest_owed` is the backers' 15%, excluded here so
/// lenders' share price reflects only their 85%).
///
/// `bad_debt` is deliberately NOT subtracted here. Writing debt off already removes its shares from
/// `total_borrow_shares`, so the loss lands on suppliers at that moment through `total_borrows`;
/// subtracting it again would charge them twice. The field is a memo of what the backstop owes this
/// market, and is reduced when the backstop pays it in.
pub fn total_assets(m: &Market) -> Result<u64> {
    let gross = m
        .cash
        .checked_add(total_borrows(m)?)
        .ok_or(VaultError::MathOverflow)?;
    gross
        .checked_sub(m.backer_interest_owed)
        .ok_or_else(|| VaultError::MathOverflow.into())
}

/// D4: the issuer holds a permanent delegate over the mint and can burn straight out of the vault's
/// own token account. Any risk action first checks that the tokens the market thinks it holds are
/// actually there — seizing or releasing collateral on the strength of a stale number would take it
/// from whoever is still in the pool.
pub fn reconciliation_short(vault_amount: u64, m: &Market) -> bool {
    vault_amount < m.total_collateral_raw
}

/// Grows `borrow_index` by the elapsed time's borrow rate, then credits backers their share of the
/// interest that just accrued (eng review addendum, founder-locked 15%). The share is computed from
/// the actual growth in `total_borrows` over this step -- not the rate directly -- so it can never
/// drift from what borrowers are really being charged.
pub fn accrue(m: &mut Market, now: i64) -> Result<()> {
    if now <= m.last_accrual_ts {
        return Ok(());
    }
    let before = total_borrows(m)?;
    let util = core(utilization_bps(before, m.cash))?;
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

    let after = total_borrows(m)?;
    let interest = after.saturating_sub(before);
    if interest > 0 && m.params.backer_interest_share_bps > 0 {
        let share = u64::try_from(mul_div_floor(
            interest as u128,
            m.params.backer_interest_share_bps as u128,
            BPS,
        )?)
        .map_err(|_| VaultError::MathOverflow)?;
        m.backer_interest_owed = m
            .backer_interest_owed
            .checked_add(share)
            .ok_or(VaultError::MathOverflow)?;
        m.backer_interest_cumulative = m
            .backer_interest_cumulative
            .checked_add(share)
            .ok_or(VaultError::MathOverflow)?;
    }
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

/// One per-token price re-quoted from units of multiplier `from_fp` into units of `to_fp`, keeping
/// `price × multiplier` (the value of one raw unit) unchanged. Rounded to nearest, so neither a borrower nor
/// a lender is systematically favoured; a non-zero price never rounds to zero.
pub fn requote(price: u64, from_fp: u128, to_fp: u128) -> Result<u64> {
    if price == 0 {
        return Ok(0);
    }
    require!(from_fp > 0 && to_fp > 0, VaultError::InvalidMultiplier);
    let scaled = (price as u128)
        .checked_mul(from_fp)
        .and_then(|v| v.checked_add(to_fp / 2))
        .ok_or(VaultError::MathOverflow)?
        / to_fp;
    let out = u64::try_from(scaled).map_err(|_| VaultError::MathOverflow)?;
    Ok(out.max(1))
}

/// Split-adjusts the stored price history to `effective`, the multiplier now in force.
///
/// Prices are per whole token. After a 4-for-1 split one token is a quarter of what it was, so every sample
/// recorded before the split reads four times too high against the new multiplier (and a reverse split, four
/// times too low). Left alone, the TWAP undervalues collateral after a reverse split — a wrongful liquidation
/// of a healthy loan — and overvalues it after a forward split, stalling a genuine one while every correct new
/// print is flagged as a spike against the stale history. Re-quoting the history is what equity data vendors
/// do for splits. Timestamps are untouched, so the TWAP's time weighting is unchanged.
///
/// Must run before any read or write of `m.price` in an instruction that can see the mint. Sub-threshold
/// dividend steps are re-quoted too; they are tiny but real.
pub fn sync_price_units(m: &mut Market, effective: u128) -> Result<()> {
    let from = m.price_units_fp;
    if from != 0 && from != effective {
        let p = &mut m.price;
        for slot in p.prices.iter_mut() {
            *slot = requote(*slot, from, effective)?;
        }
        p.last_price = requote(p.last_price, from, effective)?;
        p.last_close = requote(p.last_close, from, effective)?;
        p.flagged_price = requote(p.flagged_price, from, effective)?;
    }
    m.price_units_fp = effective;
    Ok(())
}

/// Whether the stored price history is quoted in the multiplier in force now. Read-only callers (the
/// backstop's resale) cannot re-quote it, so they refuse instead of pricing off a stale history.
pub fn price_units_current(m: &Market, schedule: &MultiplierSchedule, now: i64) -> bool {
    m.price_units_fp == schedule.effective(now)
}

/// The *other* multiplier a feed print could mistakenly be quoted in while a split is in play, if any: before
/// a scheduled activation, the post-split value (a feed publishing the new price early); after it, the
/// pre-split value (a feed still quoting the old price); during an unannounced change, the market's last
/// acknowledged baseline. `None` when nothing split-sized is in play, so routine dividend steps are unaffected.
pub fn other_price_units(m: &Market, schedule: &MultiplierSchedule, now: i64) -> Option<u128> {
    let effective = schedule.effective(now);
    let other = if schedule.has_activation() {
        if effective == schedule.stored {
            schedule.scheduled
        } else {
            schedule.stored
        }
    } else if m.observed_multiplier_fp != 0 {
        m.observed_multiplier_fp
    } else {
        return None;
    };
    let change_bps = effective.abs_diff(other).saturating_mul(BPS) / effective;
    (change_bps > m.params.split_cap_bps as u128).then_some(other)
}

/// Mock-adapter price update (spec 5, final 2026-09-14). A move beyond the spike cap vs TWAP is flagged and kept
/// out of the TWAP; a second consecutive update within the cap of the flagged price confirms a genuine move and is
/// accepted, so a real crash cannot be locked out. No off-hours clamp: it stalled liquidations in a real crash.
///
/// Exception: a spike that is exactly the split ratio away — the print lands inside the cap once re-quoted from
/// `other_units` — is a price quoted in the wrong split units, not a market move, and can never be confirmed.
/// A genuine crash on split day is not near the ratio (2-for-1 is already a 50% move), so it still confirms.
/// Returns whether the price was accepted.
pub fn apply_price(
    m: &mut Market,
    price: u64,
    market_open: bool,
    last_close: u64,
    other_units: Option<u128>,
    now: i64,
) -> Result<bool> {
    require!(price > 0 && last_close > 0, VaultError::InvalidPrice);
    require!(now > m.price.last_update, VaultError::StalePriceUpdate);

    if m.price.count > 0 {
        let reference = market_twap(m, now)?;
        let cap = m.params.deviation_cap_bps;
        let spike = core(deviation_exceeded(price, reference, cap))?;
        let wrong_units = spike
            && match other_units {
                Some(other) if m.price_units_fp > 0 => !core(deviation_exceeded(
                    requote(price, other, m.price_units_fp)?,
                    reference,
                    cap,
                ))?,
                _ => false,
            };
        let confirmed = !wrong_units
            && m.price.flagged
            && !core(deviation_exceeded(price, m.price.flagged_price, cap))?;
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

// ------------------------------------------------------------------ liquidation view (shared by liquidate and mark)

/// What `liquidate` needs once every guard has passed.
pub struct LiquidationView {
    pub multiplier_fp: u128,
    pub price: u64,
    pub value: u64,
    pub debt: u64,
    pub terms: LiquidationTerms,
}

/// Every guard `liquidate` applies before seizing collateral, then the liquidation test itself. Shared with
/// `mark_liquidatable`, so "liquidatable" means exactly the same thing to both: the grace clock only runs
/// while a liquidation could actually have happened.
pub fn liquidation_view(
    market: &mut Market,
    position: &Position,
    mint_info: &AccountInfo,
    vault_amount: u64,
    vault_frozen: bool,
    now: i64,
) -> Result<LiquidationView> {
    accrue(market, now)?;
    // D4 before anything else. If the collateral is not actually in the vault, refuse rather than hand a
    // liquidator tokens that belong to whoever is still in the pool. Raising the halt flag is
    // `sync_issuer_state`'s job.
    require!(
        !reconciliation_short(vault_amount, market),
        VaultError::ReconciliationFailed
    );
    require!(!vault_frozen, VaultError::CollateralAccountFrozen);
    require!(
        !market.issuer_halt && !issuer_blocks(mint_info),
        VaultError::IssuerHalt
    );
    let schedule = MultiplierSchedule::read(mint_info)?;
    require!(
        !corporate_action_hold(market, &schedule, now)
            && !unobserved_multiplier_change(market, &schedule, now),
        VaultError::CorporateActionHold
    );
    let multiplier_fp = schedule.effective(now);
    market.observed_multiplier_fp = multiplier_fp;
    sync_price_units(market, multiplier_fp)?;
    // Liquidation prices on the TWAP, never the last print: one bad tick must not seize anyone's collateral.
    let price = risk_price(market, now, PriceUse::Liquidate)?;
    let value = value_of(market, position.raw_collateral, multiplier_fp, price)?;
    let debt = core(debt_for_shares(position.debt_shares, market.borrow_index))?;
    // U3: the terms in force now, which may still be ramping toward a tightened `params`.
    let terms = effective_liquidation_terms(market, now);
    require!(
        core(safu_core::lending::is_liquidatable(
            debt,
            value,
            terms.liq_threshold_bps
        ))?,
        VaultError::NotLiquidatable
    );
    Ok(LiquidationView {
        multiplier_fp,
        price,
        value,
        debt,
        terms,
    })
}

/// Whether an outside liquidator may act: the position was first marked at least `grace` ago, and the
/// latest mark is no older than `grace` (a stale mark from before the position recovered does not count).
/// `liquidate` itself re-checks that the position is liquidatable now.
pub fn fallback_open(first_seen: i64, last_seen: i64, grace: i64, now: i64) -> bool {
    first_seen > 0
        && now.saturating_sub(first_seen) >= grace
        && now.saturating_sub(last_seen) <= grace
}

// ------------------------------------------------------------------ loan age

/// Debt-weighted borrow time after borrowing `amount` on top of `old_debt` carried at `old_age_ts`:
/// `(old_debt·old_age + amount·now) / (old_debt + amount)`, rounded toward `now` so a loan never reads
/// older than it is. With no existing debt it is simply `now`.
pub fn weighted_borrow_age(old_debt: u64, old_age_ts: i64, amount: u64, now: i64) -> Result<i64> {
    if old_debt == 0 {
        return Ok(now);
    }
    let weight = old_debt as i128 + amount as i128;
    let num = (old_debt as i128 * old_age_ts as i128)
        .checked_add(amount as i128 * now as i128)
        .ok_or(VaultError::MathOverflow)?;
    let mut age = num.div_euclid(weight);
    if num.rem_euclid(weight) != 0 {
        age += 1;
    }
    let age = i64::try_from(age).map_err(|_| VaultError::MathOverflow)?;
    Ok(age.min(now))
}

// ------------------------------------------------------------------ liquidation terms ramp (U3)

/// Terms moving from `from` to `to`, each on its own straight line over the same window.
pub fn interpolate_terms(
    from: LiquidationTerms,
    to: LiquidationTerms,
    start: i64,
    now: i64,
) -> LiquidationTerms {
    let r = |a, b| ramp_bps(a, b, start, now, LIQUIDATION_TERMS_RAMP_SECS);
    LiquidationTerms {
        liq_threshold_bps: r(from.liq_threshold_bps, to.liq_threshold_bps),
        insolvency_ltv_bps: r(from.insolvency_ltv_bps, to.insolvency_ltv_bps),
        close_factor_bps: r(from.close_factor_bps, to.close_factor_bps),
        min_liq_bonus_bps: r(from.min_liq_bonus_bps, to.min_liq_bonus_bps),
        max_liq_bonus_bps: r(from.max_liq_bonus_bps, to.max_liq_bonus_bps),
    }
}

/// The liquidation terms in force at `now`: the ramp from `ramp_from` toward `params`.
pub fn effective_liquidation_terms(m: &Market, now: i64) -> LiquidationTerms {
    interpolate_terms(
        m.ramp_from,
        m.params.liquidation_terms(),
        m.ramp_start_ts,
        now,
    )
}

/// Starting point of a new ramp toward `new`. A term the change LOOSENS jumps straight to its new
/// value; a term it TIGHTENS starts from where it is right now, so a change mid-ramp never jumps.
/// Tighter means: a lower liquidation line, a lower full-liquidation line, a larger close factor, a
/// larger bonus.
pub fn ramp_start_terms(current: LiquidationTerms, new: LiquidationTerms) -> LiquidationTerms {
    LiquidationTerms {
        liq_threshold_bps: current.liq_threshold_bps.max(new.liq_threshold_bps),
        insolvency_ltv_bps: current.insolvency_ltv_bps.max(new.insolvency_ltv_bps),
        close_factor_bps: current.close_factor_bps.min(new.close_factor_bps),
        min_liq_bonus_bps: current.min_liq_bonus_bps.min(new.min_liq_bonus_bps),
        max_liq_bonus_bps: current.max_liq_bonus_bps.min(new.max_liq_bonus_bps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requoting_keeps_the_value_of_a_raw_unit_within_rounding() {
        // Deterministic sweep over prices and split ratios (1:10 reverse to 10:1 forward) on top of
        // the live AAPLx multiplier.
        let base: u128 = 1_002_664_200_000;
        let mut seed: u64 = 0x5AF0_C0DE;
        for _ in 0..2_000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let price = 1 + seed % 1_000_000_000_000; // up to $10,000 at 8 decimals
            let ratio_num = 1 + (seed >> 20) % 10;
            let ratio_den = 1 + (seed >> 40) % 10;
            let to = base * ratio_num as u128 / ratio_den as u128;
            let out = requote(price, base, to).unwrap();
            let before = price as u128 * base;
            let after = out as u128 * to;
            // Nearest rounding: off by at most half of one price unit, in value terms.
            assert!(
                before.abs_diff(after) <= to / 2 || out == 1,
                "price {price} {base}->{to}: {before} vs {after}"
            );
        }
    }

    #[test]
    fn requoting_never_turns_a_price_into_zero_and_leaves_empty_slots_empty() {
        assert_eq!(requote(1, 1, 1_000_000).unwrap(), 1);
        assert_eq!(requote(0, 1, 4).unwrap(), 0);
        assert!(requote(5, 0, 4).is_err());
        assert!(
            requote(u64::MAX, 4, 1).is_err(),
            "overflow is refused, never wrapped"
        );
    }

    #[test]
    fn fallback_opens_only_after_a_fresh_mark_has_aged_past_the_grace_period() {
        let g = 900;
        assert!(!fallback_open(0, 0, g, 10_000), "never marked");
        assert!(
            !fallback_open(1_000, 1_000, g, 1_000 + g - 1),
            "one second short"
        );
        assert!(
            fallback_open(1_000, 1_000, g, 1_000 + g),
            "exactly the grace period"
        );
        assert!(
            !fallback_open(1_000, 1_000, g, 1_000 + g + 1),
            "the only mark has gone stale"
        );
        assert!(fallback_open(1_000, 2_500, g, 3_000), "re-marked recently");
        assert!(
            !fallback_open(i64::MIN + 1, i64::MIN + 1, g, i64::MAX),
            "saturating, stale"
        );
    }

    #[test]
    fn borrow_age_is_weighted_by_size_and_never_reads_older() {
        let day = 86_400;
        assert_eq!(
            weighted_borrow_age(0, 0, 500, 1_000).unwrap(),
            1_000,
            "first borrow"
        );
        // $100 at day 0, $900 at day 50 → day 45.
        assert_eq!(
            weighted_borrow_age(100, 0, 900, 50 * day).unwrap(),
            45 * day
        );
        // $1,000 at day 0, $10 at day 50 → about 11.9 hours in, rounded toward now.
        let small = weighted_borrow_age(1_000, 0, 10, 50 * day).unwrap();
        assert_eq!(small, (10 * 50 * day + 1_009) / 1_010);
        assert!(small > 0 && small < 12 * 3_600);
        // Rounds toward now: 1 part at t=0, 2 parts at t=1 → 2/3, reads as 1.
        assert_eq!(weighted_borrow_age(1, 0, 2, 1).unwrap(), 1);
        // A zero-sized top-up changes nothing.
        assert_eq!(weighted_borrow_age(1_000, 123, 0, 999).unwrap(), 123);
        // Extremes: no panic, never after now.
        assert!(weighted_borrow_age(u64::MAX, i64::MAX, u64::MAX, i64::MAX).is_err());
        assert_eq!(weighted_borrow_age(u64::MAX, 0, u64::MAX, 2).unwrap(), 1);
    }

    fn terms(thr: u32, ins: u32, close: u32, min: u32, max: u32) -> LiquidationTerms {
        LiquidationTerms {
            liq_threshold_bps: thr,
            insolvency_ltv_bps: ins,
            close_factor_bps: close,
            min_liq_bonus_bps: min,
            max_liq_bonus_bps: max,
        }
    }

    /// Deterministic generator of term sets that pass `MarketParams::validate`'s ordering rules.
    fn valid(seed: &mut u64, ltv: u32) -> LiquidationTerms {
        let mut next = |n: u32| {
            *seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((*seed >> 33) % n as u64) as u32
        };
        let thr = ltv + 1 + next(9_000 - ltv);
        let ins = thr + 1 + next(10_000 - thr);
        let max = next(2_001);
        let min = next(max + 1);
        terms(thr, ins, 1 + next(10_000), min, max)
    }

    #[test]
    fn loosening_is_instant_and_tightening_starts_where_the_terms_are() {
        let week = LIQUIDATION_TERMS_RAMP_SECS;
        let old = terms(5_000, 9_500, 2_500, 100, 500);
        let tighter = terms(4_200, 9_000, 5_000, 200, 800);
        let from = ramp_start_terms(old, tighter);
        assert_eq!(
            from, old,
            "every term tightened, so every term starts at its old value"
        );
        assert_eq!(interpolate_terms(from, tighter, 0, 0), old);
        assert_eq!(
            interpolate_terms(from, tighter, 0, week / 2),
            terms(4_600, 9_250, 3_750, 150, 650)
        );
        assert_eq!(interpolate_terms(from, tighter, 0, week), tighter);

        let looser = terms(6_000, 9_800, 1_000, 50, 300);
        let from = ramp_start_terms(old, looser);
        assert_eq!(
            from, looser,
            "every term loosened, so the new values apply at once"
        );
        assert_eq!(interpolate_terms(from, looser, 0, 1), looser);
    }

    #[test]
    fn a_second_change_mid_ramp_continues_from_the_live_value() {
        let week = LIQUIDATION_TERMS_RAMP_SECS;
        let old = terms(5_000, 9_500, 2_500, 100, 500);
        let first = terms(4_200, 9_500, 2_500, 100, 500);
        let from = ramp_start_terms(old, first);
        let live = interpolate_terms(from, first, 0, week / 2);
        assert_eq!(live.liq_threshold_bps, 4_600);
        // Tighten further half way through: the new ramp starts at 46%, not back at 50%.
        let second = terms(4_100, 9_500, 2_500, 100, 500);
        let from2 = ramp_start_terms(live, second);
        assert_eq!(from2.liq_threshold_bps, 4_600);
        assert_eq!(
            interpolate_terms(from2, second, week / 2, week / 2).liq_threshold_bps,
            4_600
        );
        // Loosen half way through instead: applies at once.
        let relief = terms(4_800, 9_500, 2_500, 100, 500);
        assert_eq!(ramp_start_terms(live, relief).liq_threshold_bps, 4_800);
    }

    /// The ordering rules `validate` enforces on the target (liquidation line below the
    /// full-liquidation line, above the borrow limit; min bonus at most max bonus) must hold at every
    /// moment of every ramp, including chains of changes made mid-ramp.
    #[test]
    fn ordering_rules_hold_at_every_moment_of_any_chain_of_changes() {
        let week = LIQUIDATION_TERMS_RAMP_SECS;
        let mut seed = 0x5AFE_u64;
        for _ in 0..400 {
            let ltv = 1 + (seed % 7_999) as u32;
            let mut current_target = valid(&mut seed, ltv);
            let mut from = current_target;
            let mut start = 0i64;
            let mut t = 0i64;
            for step in 0..6 {
                t += (seed % (2 * week as u64)) as i64 + step;
                let live = interpolate_terms(from, current_target, start, t);
                let next = valid(&mut seed, ltv);
                from = ramp_start_terms(live, next);
                start = t;
                current_target = next;
                for probe in [0, 1, week / 7, week / 3, week / 2, week - 1, week, 2 * week] {
                    let e = interpolate_terms(from, current_target, start, t + probe);
                    assert!(e.liq_threshold_bps > ltv, "{e:?} ltv {ltv}");
                    assert!(e.insolvency_ltv_bps > e.liq_threshold_bps, "{e:?}");
                    assert!(e.min_liq_bonus_bps <= e.max_liq_bonus_bps, "{e:?}");
                    assert!(e.insolvency_ltv_bps <= 10_000 && e.max_liq_bonus_bps <= 2_000);
                }
            }
        }
    }
}
