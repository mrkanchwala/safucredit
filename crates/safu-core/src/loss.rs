//! Wrongful-liquidation loss pricing (spec 13d) and payout caps.

use crate::{apply_bps, collateral::collateral_value, Result};

/// `loss = collateral seized × reference price − debt repaid on the borrower's behalf`, floored at zero.
/// `reference_price_fp` comes from an independent source, never the feed that liquidated.
pub fn wrongful_loss(
    seized_raw: u64,
    decimals: u8,
    multiplier_fp: u128,
    reference_price_fp: u64,
    debt_repaid: u64,
) -> Result<u64> {
    let fair_value = collateral_value(seized_raw, decimals, multiplier_fp, reference_price_fp)?;
    Ok(fair_value.saturating_sub(debt_repaid))
}

/// Amount actually paid: the loss, 1:1, never above the wallet's tier ceiling nor the per-claim share of the
/// backstop. Never a multiple of the loss.
pub fn payout(
    loss: u64,
    ceiling_base_value: u64,
    tier_ceiling_bps: u32,
    backstop_balance: u64,
    per_claim_cap_bps: u32,
) -> Result<u64> {
    let tier_cap = apply_bps(ceiling_base_value, tier_ceiling_bps)?;
    let backstop_cap = apply_bps(backstop_balance, per_claim_cap_bps)?;
    Ok(loss.min(tier_cap).min(backstop_cap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MULT_SCALE;

    #[test]
    fn loss_is_fair_value_minus_debt_repaid() {
        // 1 share seized at 8 decimals, reference $400, debt repaid $250 → $150 loss.
        let loss = wrongful_loss(100_000_000, 8, MULT_SCALE, 40_000_000_000, 250_000_000).unwrap();
        assert_eq!(loss, 150_000_000);
    }

    #[test]
    fn no_loss_when_debt_repaid_covers_fair_value() {
        assert_eq!(
            wrongful_loss(100_000_000, 8, MULT_SCALE, 10_000_000_000, 250_000_000),
            Ok(0)
        );
    }

    #[test]
    fn payout_takes_the_smallest_cap() {
        assert_eq!(payout(150, 1_000, 10_000, 10_000, 10_000), Ok(150));
        assert_eq!(payout(150, 1_000, 1_000, 10_000, 10_000), Ok(100));
        assert_eq!(payout(150, 1_000, 10_000, 1_000, 500), Ok(50));
    }
}
