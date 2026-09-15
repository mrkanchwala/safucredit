"""SUPERSEDED 2026-09-15 by scripts/push_live_price_kms.py -- kept only as a manual/emergency
fallback. VaultConfig.feed_authority was repointed to an AWS KMS-backed signer the same day
(alias/safu-credit-price-feed, eu-north-1), so this script's deployer-key signature is no longer
authorized and will fail with an on-chain `Unauthorized` error. It would only work again if
`set_feed_authority` were called to point back at the deployer key.

Push one fresh AAPLx price to the SHARED devnet market (app/src/devnet-market.json) that the
live front end (credit.safustaking.com) reads from.

Standalone gap this fills: `verdict/cmc_recorder.py` (cron, every 5 min on the VPS) only *records*
CMC's AAPL quote to a local JSONL file for later claim-forensics use -- it never calls the vault's
`push_price` instruction. `scripts/demo.py` / `scripts/demo_devnet.py` do call `push_price`, but only
against a throwaway market each creates fresh for its own scenario run, never the shared one. Nothing
has ever kept the shared market's on-chain price fresh, so `Borrow` / `WithdrawCollateral` started
failing with `PriceUnavailable` (6008) / `StalePriceUpdate` (6007) once the one price set at
`setup_devnet_market.py` init time aged past `borrow_max_price_age_secs`. Found 2026-09-15 when the
founder hit that error live-testing the deployed front end.

Same key convention as `verdict/cmc_recorder.py` (plain env var, never hardcoded) -- this script
lives in a public repo and must not assume any one machine's directory layout.

Usage:
    COINMARKETCAP_API_KEY=... SSL_CERT_FILE=$(python3 -c "import certifi; print(certifi.where())") \\
        python3 scripts/push_live_price.py
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "scripts"))
sys.path.insert(0, str(REPO / "verdict"))

import cmc_recorder  # noqa: E402
from idl_client import Program, pda  # noqa: E402
from rpc import Rpc  # noqa: E402
from solders.keypair import Keypair  # noqa: E402
from solders.pubkey import Pubkey  # noqa: E402

DEVNET = "https://api.devnet.solana.com"
KEYS_DIR = Path.home() / "SAFU" / "safucredit-keys"
DEPLOYER_PATH = KEYS_DIR / "devnet-deployer-keypair.json"
MARKET_JSON = REPO / "app" / "src" / "devnet-market.json"
PRICE_SCALE = 10**8
AAPLX_CMC_ID = 36994


def main() -> int:
    key = os.environ.get("COINMARKETCAP_API_KEY")
    if not key:
        print("COINMARKETCAP_API_KEY is not set", file=sys.stderr)
        return 2

    quote = cmc_recorder.record(AAPLX_CMC_ID, "AAPLX", key)
    price_usd = quote["price_usd"]
    price_fp8 = int(round(price_usd * PRICE_SCALE))
    print(f"CMC AAPLx quote: ${price_usd:,.4f} -> price_fp8={price_fp8}")

    market_cfg = json.loads(MARKET_JSON.read_text())
    vault_program = Pubkey.from_string(market_cfg["stockVaultProgram"])
    market = Pubkey.from_string(market_cfg["market"])
    collateral_mint = Pubkey.from_string(market_cfg["aaplxMint"])
    vconfig = pda(vault_program, b"vconfig")

    deployer = Keypair.from_json(DEPLOYER_PATH.read_text())
    vault = Program("stock_vault")
    rpc = Rpc(DEVNET)

    ix = vault.instruction(
        "push_price",
        {
            "feed_authority": deployer.pubkey(),
            "config": vconfig,
            "market": market,
            "collateral_mint": collateral_mint,
        },
        {"price": price_fp8, "market_open": True, "last_close": price_fp8},
    )
    sig = rpc.send([ix], [deployer], "push_price")
    print(f"push_price OK, sig={sig}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
