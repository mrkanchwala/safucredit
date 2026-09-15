"""One-time devnet setup: create the mock AAPLx mint and the AAPLx market on the live devnet deployment.

The 2026-09-15 devnet deploy deliberately only ran initialize_config / initialize_backstop -- no market,
no mints funded (see memory/jobs/2026-09-15_safu-stocklana-solana-devnet-deploy-and-init.md). This is the
follow-up step the frontend needs: same disclosed-stand-in mock-mint pattern as the local demo (tokens.py),
same locked AAPLx MarketParams as scripts/demo.py, but run once against real devnet with the real deployer
(admin) key instead of a throwaway localnet keypair.

    python3 scripts/setup_devnet_market.py

Idempotent-ish: if the AAPLx mint keypair file already exists it is reused rather than regenerated, and if
the market account already exists on-chain the script stops before create_market rather than erroring into a
duplicate-account failure.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent
sys.path.insert(0, str(REPO))

import tokens  # noqa: E402
from idl_client import Program, pda  # noqa: E402
from rpc import Rpc  # noqa: E402
from solders.keypair import Keypair  # noqa: E402
from solders.pubkey import Pubkey  # noqa: E402

DEVNET = "https://api.devnet.solana.com"
KEYS_DIR = Path.home() / "SAFU" / "safucredit-keys"
DEPLOYER_PATH = KEYS_DIR / "devnet-deployer-keypair.json"
MOCK_USDC_MINT = Pubkey.from_string("Cujm3S48mCeQHnNfAEarYfuwoSwgp9gLPFVVrdDVV8wn")
LIVE_MULTIPLIER = 1.0026642  # same value demo.py uses -- live AAPLx mint, read 2026-09-13
USDC = 1_000_000

# Locked 2026-09-14, outputs/2026-09-14_stocklana-market-config-research.md sec 2b -- identical to demo.py.
MARKET_PARAMS = {
    "ltv_bps": 4_000, "liq_threshold_bps": 5_000, "min_liq_bonus_bps": 100, "max_liq_bonus_bps": 500,
    "insolvency_ltv_bps": 9_500, "close_factor_bps": 2_500, "max_liquidation_debt": 100_000 * USDC,
    "deviation_cap_bps": 500, "activation_pause_secs": 900, "split_cap_bps": 500, "split_max_hold_secs": 86_400,
    "borrow_max_price_age_secs": 3_600, "liquidation_max_price_age_open_secs": 90_000,
    "max_price_age_closed_secs": 345_600,
    "rate": {"base_bps": 100, "slope1_bps": 500, "slope2_bps": 6_000, "kink_bps": 9_000},
    "borrow_cap": 500_000 * USDC, "collateral_cap_raw": 2_000 * 10**tokens.AAPLX_DECIMALS,
    "backer_interest_share_bps": 1_500,
}


def load_keypair(path: Path) -> Keypair:
    return Keypair.from_bytes(bytes(json.loads(path.read_text())))


def say(msg: str) -> None:
    print(f"[setup-devnet-market] {msg}", flush=True)


def main() -> None:
    rpc = Rpc(DEVNET)
    vault = Program("stock_vault")
    deployer = load_keypair(DEPLOYER_PATH)
    admin = deployer.pubkey()
    say(f"admin/deployer: {admin}")

    mint_path = KEYS_DIR / "devnet-aaplx-mint-keypair.json"
    candidate = Pubkey.from_string(tokens._pubkey_of(mint_path)) if mint_path.exists() else None
    if candidate is not None and rpc.account(candidate) is not None:
        aaplx_mint = candidate
        say(f"AAPLx mint already exists on-chain -> {aaplx_mint}")
    else:
        say("creating AAPLx mock mint (Token-2022, Scaled UI Amount + Pausable, 8 decimals)...")
        aaplx_mint = tokens.create_aaplx_mint(mint_path, DEPLOYER_PATH, LIVE_MULTIPLIER, url=DEVNET)
        say(f"AAPLx mint created -> {aaplx_mint}")

    vconfig = pda(vault.program_id, b"vconfig")
    market = pda(vault.program_id, b"market", bytes(aaplx_mint))
    coll_vault = pda(vault.program_id, b"coll_vault", bytes(market))
    usdc_vault = pda(vault.program_id, b"usdc_vault", bytes(market))
    say(f"market PDA: {market}")
    say(f"collateral_vault PDA: {coll_vault}")
    say(f"usdc_vault PDA: {usdc_vault}")

    if rpc.account(market) is not None:
        say("market account already exists on-chain -- stopping before create_market (nothing to do).")
        _write_summary(aaplx_mint, market, coll_vault, usdc_vault)
        return

    ix = vault.instruction(
        "create_market",
        {
            "admin": admin,
            "config": vconfig,
            "collateral_mint": aaplx_mint,
            "usdc_mint": MOCK_USDC_MINT,
            "market": market,
            "collateral_vault": coll_vault,
            "usdc_vault": usdc_vault,
            "collateral_token_program": tokens.TOKEN_2022,
            "usdc_token_program": tokens.TOKEN_CLASSIC,
            "system_program": Pubkey.from_string("11111111111111111111111111111111"),
        },
        {"params": MARKET_PARAMS},
    )
    say("submitting create_market...")
    sig = rpc.send([ix], [deployer], "create_market")
    say(f"create_market confirmed: {sig}")
    _write_summary(aaplx_mint, market, coll_vault, usdc_vault)


def _write_summary(aaplx_mint: Pubkey, market: Pubkey, coll_vault: Pubkey, usdc_vault: Pubkey) -> None:
    out = REPO.parent / "app" / "src" / "devnet-market.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(
        json.dumps(
            {
                "cluster": "devnet",
                "stockVaultProgram": str(Program("stock_vault").program_id),
                "backstopProgram": str(Program("backstop").program_id),
                "aaplxMint": str(aaplx_mint),
                "usdcMint": str(MOCK_USDC_MINT),
                "market": str(market),
                "collateralVault": str(coll_vault),
                "usdcVault": str(usdc_vault),
            },
            indent=2,
        )
        + "\n",
    )
    say(f"wrote frontend config -> {out}")


if __name__ == "__main__":
    main()
