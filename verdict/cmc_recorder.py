"""Record one CoinMarketCap quote for a tokenized stock.

One fetch per run, so it can be called from cron. Each run:
  1. saves the raw API response, byte for byte, under recordings/raw/
  2. appends one normalized line (price, CMC timestamp, fetch time, sha256 of the raw bytes) to
     recordings/<symbol>.jsonl

The raw file is what makes a verdict auditable later: its sha256 goes into the signed verdict hash.

Key: COINMARKETCAP_API_KEY environment variable. Never hardcode it; this repository is public.

Usage:
    COINMARKETCAP_API_KEY=... python3 verdict/cmc_recorder.py --cmc-id 36994 --symbol AAPLX
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import ssl
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

API = "https://pro-api.coinmarketcap.com/v2/cryptocurrency/quotes/latest"
RECORDINGS = Path(__file__).resolve().parent / "recordings"


def _ssl_context() -> ssl.SSLContext:
    # Some local Python installs lack a CA bundle; use certifi when present.
    try:
        import certifi

        return ssl.create_default_context(cafile=certifi.where())
    except ImportError:
        return ssl.create_default_context()


def fetch(cmc_id: int, key: str) -> bytes:
    req = urllib.request.Request(
        f"{API}?id={cmc_id}",
        headers={"X-CMC_PRO_API_KEY": key, "Accept": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=20, context=_ssl_context()) as resp:
        return resp.read()


def normalize(raw: bytes, cmc_id: int, fetched_at: str) -> dict:
    body = json.loads(raw)
    status = body.get("status", {})
    if status.get("error_code"):
        raise RuntimeError(f"CMC error {status.get('error_code')}: {status.get('error_message')}")
    asset = body["data"][str(cmc_id)]
    usd = asset["quote"]["USD"]
    if usd.get("price") is None:
        raise RuntimeError("CMC returned no USD price")
    return {
        "cmc_id": cmc_id,
        "symbol": asset["symbol"],
        "price_usd": usd["price"],
        "cmc_last_updated": usd["last_updated"],
        "volume_24h": usd.get("volume_24h"),
        "num_market_pairs": asset.get("num_market_pairs"),
        "fetched_at": fetched_at,
        "raw_sha256": hashlib.sha256(raw).hexdigest(),
    }


def record(cmc_id: int, symbol: str, key: str) -> dict:
    fetched_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    raw = fetch(cmc_id, key)
    line = normalize(raw, cmc_id, fetched_at)

    raw_dir = RECORDINGS / "raw" / symbol.lower()
    raw_dir.mkdir(parents=True, exist_ok=True)
    (raw_dir / f"{fetched_at}_{line['raw_sha256'][:12]}.json").write_bytes(raw)

    with (RECORDINGS / f"{symbol.lower()}.jsonl").open("a") as f:
        f.write(json.dumps(line, sort_keys=True) + "\n")
    return line


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--cmc-id", type=int, required=True, help="CMC asset id (AAPLx = 36994)")
    parser.add_argument("--symbol", required=True)
    args = parser.parse_args()

    key = os.environ.get("COINMARKETCAP_API_KEY")
    if not key:
        print("COINMARKETCAP_API_KEY is not set", file=sys.stderr)
        return 2
    try:
        line = record(args.cmc_id, args.symbol, key)
    except (urllib.error.URLError, RuntimeError, KeyError, ValueError) as exc:
        print(f"record failed: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(line, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
