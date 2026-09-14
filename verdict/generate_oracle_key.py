"""Generates the devnet throwaway Ed25519 oracle keypair for the Stocklana verdict engine.

Writes a standard 64-byte Solana keypair JSON array (secret || public) to
verdict/keys/oracle-devnet-keypair.json -- gitignored (.gitignore: `keys/`). Devnet-only,
rotatable at will; this key never signs anything with real funds behind it.

Usage:
    python3 verdict/generate_oracle_key.py
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

from solders.keypair import Keypair

KEY_PATH = Path(__file__).resolve().parent / "keys" / "oracle-devnet-keypair.json"


def main() -> int:
    if KEY_PATH.exists():
        print(
            f"{KEY_PATH} already exists -- refusing to overwrite. Delete it first if you really "
            "mean to rotate the devnet oracle key (the backstop config's verdict_oracle admin "
            "setter must then be called with the new pubkey too).",
            file=sys.stderr,
        )
        return 1
    kp = Keypair()
    KEY_PATH.parent.mkdir(parents=True, exist_ok=True)
    KEY_PATH.write_text(json.dumps(list(bytes(kp))))
    print(f"wrote {KEY_PATH}")
    print(f"oracle pubkey: {kp.pubkey()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
