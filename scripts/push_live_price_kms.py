"""VPS cron version of push_live_price.py: signs via AWS KMS (ECC_NIST_EDWARDS25519) instead of a
local keypair, so no Solana private key material ever sits on this server -- same posture as
SAFU's existing EVM oracle key (api/signer.py, SAFU_KMS_KEY_ID).

The KMS key's public key IS the signer's Solana address (Ed25519 raw bytes = Solana pubkey, no
derivation needed). VaultConfig.feed_authority was repointed to it via a one-time set_feed_authority
call from the deployer key (2026-09-15) -- the deployer key can no longer push prices after that.

Usage (VPS cron, AWS_PROFILE=default already grants this identity kms:Sign + kms:GetPublicKey on
the key via its key policy -- see the SAFU Credit KMS key creation job):
    COINMARKETCAP_API_KEY=... SSL_CERT_FILE=$(python3 -c "import certifi; print(certifi.where())") \\
        python3 scripts/push_live_price_kms.py
"""

from __future__ import annotations

import base64
import json
import os
import sys
from pathlib import Path

import boto3

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "scripts"))
sys.path.insert(0, str(REPO / "verdict"))

import cmc_recorder  # noqa: E402
from idl_client import Program, pda  # noqa: E402
from rpc import Rpc  # noqa: E402
from solders.message import Message  # noqa: E402
from solders.pubkey import Pubkey  # noqa: E402
from solders.signature import Signature  # noqa: E402
from solders.transaction import Transaction  # noqa: E402

DEVNET = "https://api.devnet.solana.com"
KMS_KEY_ALIAS = "alias/safu-credit-price-feed"
KMS_REGION = "eu-north-1"
MARKET_JSON = REPO / "app" / "src" / "devnet-market.json"
PRICE_SCALE = 10**8
AAPLX_CMC_ID = 36994


def kms_ed25519_pubkey(kms, key_id: str) -> Pubkey:
    r = kms.get_public_key(KeyId=key_id)
    der = r["PublicKey"]
    # RFC 8410 Ed25519 SubjectPublicKeyInfo: fixed 12-byte header + 32-byte raw key.
    raw = der[-32:]
    return Pubkey(raw)


def kms_sign(kms, key_id: str, message_bytes: bytes) -> Signature:
    r = kms.sign(
        KeyId=key_id,
        Message=message_bytes,
        MessageType="RAW",
        SigningAlgorithm="ED25519_SHA_512",
    )
    return Signature(r["Signature"])


def main() -> int:
    cmc_key = os.environ.get("COINMARKETCAP_API_KEY")
    if not cmc_key:
        print("COINMARKETCAP_API_KEY is not set", file=sys.stderr)
        return 2

    quote = cmc_recorder.record(AAPLX_CMC_ID, "AAPLX", cmc_key)
    price_usd = quote["price_usd"]
    price_fp8 = int(round(price_usd * PRICE_SCALE))
    print(f"CMC AAPLx quote: ${price_usd:,.4f} -> price_fp8={price_fp8}")

    market_cfg = json.loads(MARKET_JSON.read_text())
    vault_program = Pubkey.from_string(market_cfg["stockVaultProgram"])
    market = Pubkey.from_string(market_cfg["market"])
    collateral_mint = Pubkey.from_string(market_cfg["aaplxMint"])
    vconfig = pda(vault_program, b"vconfig")

    kms = boto3.client("kms", region_name=KMS_REGION)
    feed_pubkey = kms_ed25519_pubkey(kms, KMS_KEY_ALIAS)

    vault = Program("stock_vault")
    rpc = Rpc(DEVNET)

    ix = vault.instruction(
        "push_price",
        {
            "feed_authority": feed_pubkey,
            "config": vconfig,
            "market": market,
            "collateral_mint": collateral_mint,
        },
        {"price": price_fp8, "market_open": True, "last_close": price_fp8},
    )

    msg = Message.new_with_blockhash([ix], feed_pubkey, rpc.blockhash())
    signature = kms_sign(kms, KMS_KEY_ALIAS, bytes(msg))
    tx = Transaction.populate(msg, [signature])

    wire = base64.b64encode(bytes(tx)).decode()
    sig = rpc.call("sendTransaction", [wire, {"encoding": "base64", "preflightCommitment": rpc.commitment}])
    rpc._confirm(sig, "push_price (KMS)")
    print(f"push_price OK (KMS-signed), sig={sig}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
