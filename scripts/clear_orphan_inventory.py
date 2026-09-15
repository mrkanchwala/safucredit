"""One-off: buy out the two orphaned throwaway markets' seized inventory that was blocking
BackstopConfig.inventory_cost_total (the GLOBAL pause gate shared across every market on this
backstop deployment) -- found 2026-09-15 while building the front-end buy-inventory UI, which
correctly showed nothing to buy on the SHARED market (its own Inventory account never existed;
these two demo_devnet.py throwaway markets are where today's P4 liquidations actually landed).

Not meant to be reusable -- hardcodes the two specific orphan markets found via getProgramAccounts.
"""

from __future__ import annotations

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO / "scripts"))

import tokens  # noqa: E402
from idl_client import Program, pda  # noqa: E402
from rpc import Rpc  # noqa: E402
from solders.keypair import Keypair  # noqa: E402
from solders.pubkey import Pubkey  # noqa: E402

DEVNET = "https://api.devnet.solana.com"
DEPLOYER_PATH = Path.home() / "SAFU" / "safucredit-keys" / "devnet-deployer-keypair.json"
USDC_MINT = Pubkey.from_string("Cujm3S48mCeQHnNfAEarYfuwoSwgp9gLPFVVrdDVV8wn")
TOKEN_PROGRAM = Pubkey.from_string("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
TOKEN_2022_PROGRAM = Pubkey.from_string("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")

ORPHAN_MARKETS = [
    "55rSpZyktJGBxmGjaxgUk7n8QGfubMPgUUrddJzjwXwk",
    "FA3jZjBQ5cxB5cSUqCEnGv4XgWPnzgB7Y36UwXSV1xbd",
]


def main() -> int:
    deployer = Keypair.from_json(DEPLOYER_PATH.read_text())
    vault = Program("stock_vault")
    backstop = Program("backstop")
    rpc = Rpc(DEVNET)

    backstop_program = backstop.program_id
    buyer_usdc = tokens.ata(deployer.pubkey(), USDC_MINT, TOKEN_PROGRAM)

    for market_str in ORPHAN_MARKETS:
        market = Pubkey.from_string(market_str)
        market_state = vault.decode_account("Market", rpc.account(market))
        collateral_mint = market_state["collateral_mint"]
        current_price = market_state["price"]["last_price"]

        inventory_pda = pda(backstop_program, b"inv", market)
        inv_state = backstop.decode_account("Inventory", rpc.account(inventory_pda))
        raw = inv_state["raw"]
        print(f"{market_str}: raw={raw}, cost_total={inv_state['cost_total']}, price={current_price}")

        if raw == 0:
            print("  already clear, skipping")
            continue

        # Create the buyer's Token-2022 collateral ATA for this market's mock mint (each
        # throwaway market has its own mint, so this account has never existed before).
        buyer_collateral = tokens.fund(collateral_mint, deployer.pubkey(), 0, DEPLOYER_PATH, TOKEN_2022_PROGRAM, url=DEVNET)

        ix = backstop.instruction(
            "buy_inventory",
            {
                "buyer": deployer.pubkey(),
                "config": pda(backstop_program, b"bconfig"),
                "market": market,
                "inventory": inventory_pda,
                "collateral_mint": collateral_mint,
                "usdc_mint": USDC_MINT,
                "inventory_vault": pda(backstop_program, b"invault", market),
                "pool_usdc_vault": pda(backstop_program, b"busdc"),
                "buyer_usdc": buyer_usdc,
                "buyer_collateral": buyer_collateral,
                "collateral_token_program": TOKEN_2022_PROGRAM,
                "usdc_token_program": TOKEN_PROGRAM,
            },
            # Generous ceiling -- deliberately clearing state, not optimizing for price.
            {"raw": raw, "max_price_per_share": current_price},
        )
        sig = rpc.send([ix], [deployer], f"buy_inventory({market_str[:8]})")
        print(f"  bought out, sig={sig}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
