# safu-core changelog

Programs pin an exact version (`version = "=x.y.z"` next to the path dependency), so adopting a change
here is a deliberate edit in each consumer. Tag each release `safu-core-vX.Y.Z`.

## 0.2.0 — 2026-09-14
- **Behaviour change:** `collateral_value` and `raw_for_usd` compute in 256 bits and round down exactly
  once (eng review E2). Results are equal or higher than 0.1.0, never lower; inputs that overflowed
  in 0.1.0 may now return a value. Golden vectors regenerated and reviewed.
- Added `lending::ramp_bps`: linear, panic-free ramp for the liquidation-terms tightening window (U3).

## 0.1.0 — 2026-09-14
- Initial shared math: valuation, price guards, lending, loss and payout. Golden vectors shared with
  the Python verdict mirror (D3).
