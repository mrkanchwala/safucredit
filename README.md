# SAFU Credit

**Borrow USDC against your stocks. If a broken price liquidates you, you get paid back automatically.**

[![CI](https://github.com/mrkanchwala/safucredit/actions/workflows/ci.yml/badge.svg)](https://github.com/mrkanchwala/safucredit/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Live app: **[credit.safustaking.com](https://credit.safustaking.com)** · Demo video: **[youtu.be/dFGS2Kc049U](https://youtu.be/dFGS2Kc049U)** · Built for the Solana Foundation Stocklana hackathon

## The problem

Tokenized-stock lending markets liquidate you the moment a price feed says you're underwater, even when the feed is wrong. When that happens, the money is gone and nobody pays it back. This isn't a hypothetical:

| Event | What happened |
|---|---|
| Binance, Oct 2025 | ~$19B liquidated on a self-priced collateral gap. Compensation manual, decided weeks later. |
| Vesu, Sept 2026 | $3M lost across 47 positions from a 2-minute bad price feed. |
| Edel Finance, July 2026 | $403K in tokenized-stock lending, the same category as this product. A wrapped stock's rate inflated about 78x. |
| Aave CAPO, Mar 2026 | ~$26M lost, showing even a major, careful protocol wasn't immune. |

Existing markets either ignore this or pledge discretionary compensation after the fact. SAFU Credit checks every liquidation against a second reference price and pays back what a bad feed took, automatically and on-chain.

## How it works

Deposit tokenized stock (xStocks) as collateral and borrow USDC against it, same as any lending market. The difference shows up when a price feed lies to the liquidation engine:

1. Every liquidation is checked against a second, delayed reference price after the fact.
2. If the two prices agree, the liquidation stands. It was real, so nothing is paid.
3. If they disagree, the liquidation is ruled wrongful and you're repaid 1:1, automatically, on-chain. No vote, no appeal, no human decides.

**Liquidation mechanics.** Liquidations happen gradually, so a single bad moment can't wipe out your whole position:

| LTV | What happens |
|---|---|
| Up to 50% | Healthy, nothing happens |
| 50%–95% | Up to 25% of your debt liquidated per event, 1–5% bonus to whoever liquidates |
| Above 95% | Full position can be liquidated at once |

**Covered / not covered.** Every row below links straight to the test that proves it.

| Situation | What happens | Costs SAFU anything? | Proven by |
|---|---|---|---|
| Slow wrong-price drift | Liquidated, then repaid 1:1 | Yes, a real payout | [`valid_facts_admit_the_claim`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/backstop/tests/verdict_litesvm.rs#L607) |
| Single price spike | Blocked before it can liquidate you | No, never liquidated | [`a_spike_beyond_the_cap_is_flagged_and_kept_out_of_the_twap`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/stock_vault/tests/vault_litesvm.rs#L1395) |
| Stock split | Held until repriced correctly | No, never liquidated | [`liquidation_is_blocked_inside_a_scheduled_split_hold`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/stock_vault/tests/vault_litesvm.rs#L2849) |
| Unannounced split | Frozen until acknowledged | No, never liquidated | [`an_unannounced_split_holds_borrowing`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/stock_vault/tests/vault_litesvm.rs#L2005) |
| Genuine price crash | Liquidated, nothing paid. On-chain denial shown. | No, correct outcome | [`a_price_that_was_not_actually_wrong_is_denied_on_chain`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/backstop/tests/verdict_litesvm.rs#L656) |
| Over-liquidation / cascades | Capped per event (close factor), bonus scales with how underwater the position is, never the whole position at once below 95% LTV | No, size and cost are both bounded | [`the_chunk_cap_bounds_a_single_liquidation`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/stock_vault/tests/vault_litesvm.rs#L2674) |
| Liquidator front-running | The backstop pool liquidates first; an outside liquidator can only step in after a 15-minute grace window | No, same cost either way | [`an_outsider_may_liquidate_once_the_position_has_been_marked_for_the_grace_period`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/stock_vault/tests/vault_litesvm.rs#L3193) |
| Issuer freezes the stock | Market halts. Nothing liquidated or paid. | N/A, collateral itself is stuck | [`liquidation_is_blocked_by_an_issuer_pause`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/stock_vault/tests/vault_litesvm.rs#L2773) |
| Same wallet backs the pool and borrows from it | Both allowed together, but the payout is blocked while that wallet still holds backer shares | N/A, self-dealing guard | [`a_borrower_who_backs_the_pool_cannot_be_paid_from_it`](https://github.com/mrkanchwala/safucredit/blob/main/solana/programs/backstop/tests/payout_litesvm.rs#L1901) |
| Gap at market open | Not covered. Closed market, no price to verify. | N/A, disclosed at deposit | This is a deliberate design boundary with nothing to guard against, so no test exists for it. |

Collateral is native xStocks only (Token-2022), never wrapped variants. Valuation reads the token's own on-chain multiplier schedule in fixed-point math, never the floating-point UI conversion, so a stock split can't desync price from share count. Loans are USDC only, and the vault and the backstop (the pool that funds paybacks) are separate programs.

A wallet can back the pool and borrow from it at the same time; nothing stops that at deposit or borrow. The one place it matters is a wrongful-liquidation payout: if the wallet claiming it still holds backer shares in this pool, the claim is rejected (`BorrowerIsABacker`, `backstop/src/lib.rs`). It guards against a backer paying itself out of its own backstop capital.

## Proven on real devnet

Three scenarios, run against the live `stock_vault`/`backstop` deployment via `scripts/demo_devnet.py`, captured in the demo video above:

1. **Bad data, paid back.** A feed drifts inside the deviation cap, still enough to liquidate. The engine rules it wrongful and repays 1.4156 seized AAPLx at the reference TWAP, $144.18 back, exactly 1:1.
2. **Real crash, nothing paid.** A genuine price fall triggers a real liquidation. The engine denies the claim on-chain (`MOVE_HELD`) and pays nothing. This is the scenario that matters most: proving the engine says no when it should, with no human in the loop.
3. **Stock split, never liquidated.** A scheduled 4:1 multiplier change holds liquidations through the transition window. Nothing gets wrongly liquidated in the first place.

## Repository layout

```
crates/safu-core/   Shared Rust math: collateral valuation, lending, loss pricing.
                     Compiles to wasm32v1-none too, which is the Stellar target (see Roadmap).
solana/programs/     stock_vault (deposit/borrow/repay/liquidate) and backstop
                     (backer capital, payback pool, seized-inventory resale), both Anchor.
verdict/             Python engine that mirrors the on-chain rules against the
                     same golden vectors, plus the devnet oracle key + price recorder.
scripts/             Devnet setup, demo scenarios, IDL-driven Python client.
app/                 The dapp at credit.safustaking.com, built with React and @solana/kit.
stellar/, crosschain/  Reserved for treasury collateral (Stellar Pro Hackathon) and
                     cross-chain liquidity (Colosseum), not built yet. See Roadmap.
```

## Running the tests

```bash
# Python: verdict engine + IDL client, mirrored against the same golden vectors as the Rust core
python3 -m pytest -q
# 1,481 passed

# Shared math core: unit + golden-vector + property tests
cd crates/safu-core && cargo test --locked
# 36 unit + 1 golden + 17 property = 54 passed
```

The Solana programs need an SBF build (`cargo build-sbf`) before their LiteSVM test suite can run, which takes a few minutes locally. [CI](https://github.com/mrkanchwala/safucredit/actions/workflows/ci.yml) runs it on every push, along with a cross-language check that fires a real transaction from the Python verdict engine straight at the compiled `backstop.so`, plus a dependency audit gate.

## Devnet deployment

| Program | Address |
|---|---|
| `stock_vault` | `GkQw6VGDKYBWJtgtWUmFDkHNqGrnyjSviQeQzVMW35K2` |
| `backstop` | `H1hApKnNkYPsqQ9WVZGzqZQZYirxCLpXDj2uEmzMK3YV` |

Wallets: Phantom, Solflare, Backpack.

## Known issues

- **Devnet price source is a mock replay of recorded prices.** The reference price the verdict engine checks liquidations against replays real recorded AAPLx prices, through the same oracle-adapter interface the mainnet design targets (the Chainlink xStocks oracle). This is the documented demo setup, disclosed here so it's clear the devnet build pulls no live third-party feed.

## Roadmap

This repository is a shared build across three entries from the same team, one product with collateral split by chain: tokenized stocks on Solana (this hackathon), treasuries on Stellar (Stellar Pro Hackathon, Sept 19–20), and cross-chain liquidity between the two backstops via CCTP (Colosseum Crypto World's Fair). The `stellar/` and `crosschain/` directories are reserved for that work and are not built yet.

Longer term, the direction is multichain and multi-asset. The shared math core (`crates/safu-core`) already targets both Solana and Stellar from one codebase, and the verdict engine only needs a reference price and a liquidation to check, not a specific collateral type. Most real-world assets (real estate, commodities, other equities beyond AAPLx) and major crypto collateral (ETH, SOL, BTC) fit that same design on any chain SAFU deploys to. Neither is scoped or scheduled yet.

Past that, ideas already discussed but not built:

- **Senior / junior backer tranches**, the same pattern as Maple and Goldfinch: senior backers take a steadier, lower return and lose only after junior capital runs out, junior backers take the first loss for a higher return. Illustrative only, never a promised number: a $100K pool split $70K senior / $30K junior would put senior near a fixed 5% and junior near 22% in a good year, negative in a bad one, under the caps this design already enforces.
- **An independent on-chain reference price** (Pyth or Chainlink), replacing the devnet build's mock price replay (see Known issues).
- **On-chain market sale of seized inventory through Jupiter**, instead of today's buyer-initiated resale only.

## Disclosures

Some shared logic (the deterministic, no-vote payback design and parts of the price-verification math) is adapted from SAFU's existing protocol on Stellar, which received Stellar Community Fund grant funding unrelated to this Solana submission. This section names those reused pieces directly so nothing here is presented as new.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
