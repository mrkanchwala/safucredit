"""Minimal Solana JSON-RPC client over the standard library. No new dependency: transactions are built and signed
with `solders` (already pinned in CI), and only the handful of calls the scripts need are wrapped here."""

from __future__ import annotations

import base64
import json
import time
import urllib.error
import urllib.request

from solders.hash import Hash
from solders.instruction import Instruction
from solders.keypair import Keypair
from solders.message import Message
from solders.pubkey import Pubkey
from solders.transaction import Transaction

LOCALNET = "http://127.0.0.1:8899"


class RpcError(Exception):
    pass


class Rpc:
    def __init__(self, url: str = LOCALNET, commitment: str = "confirmed"):
        self.url = url
        self.commitment = commitment
        self._id = 0

    def call(self, method: str, params: list | None = None):
        self._id += 1
        body = json.dumps({"jsonrpc": "2.0", "id": self._id, "method": method, "params": params or []}).encode()
        req = urllib.request.Request(self.url, data=body, headers={"Content-Type": "application/json"})
        backoff = 1.0
        for attempt in range(6):
            try:
                with urllib.request.urlopen(req, timeout=30) as resp:
                    out = json.loads(resp.read())
                break
            except urllib.error.HTTPError as e:
                if e.code != 429 or attempt == 5:
                    raise
                time.sleep(backoff)
                backoff = min(backoff * 2, 16.0)
        if "error" in out:
            raise RpcError(f"{method}: {out['error']}")
        return out["result"]

    def healthy(self) -> bool:
        try:
            return self.call("getHealth") == "ok"
        except Exception:
            return False

    def blockhash(self) -> Hash:
        r = self.call("getLatestBlockhash", [{"commitment": self.commitment}])
        return Hash.from_string(r["value"]["blockhash"])

    def unix_time(self) -> int:
        slot = self.call("getSlot", [{"commitment": self.commitment}])
        return self.call("getBlockTime", [slot])

    def account(self, key: Pubkey) -> bytes | None:
        r = self.call("getAccountInfo", [str(key), {"encoding": "base64", "commitment": self.commitment}])
        if r["value"] is None:
            return None
        return base64.b64decode(r["value"]["data"][0])

    def token_balance(self, key: Pubkey) -> int:
        r = self.call("getTokenAccountBalance", [str(key), {"commitment": self.commitment}])
        return int(r["value"]["amount"])

    def airdrop(self, key: Pubkey, lamports: int) -> None:
        sig = self.call("requestAirdrop", [str(key), lamports])
        self._confirm(sig)

    def send(self, instructions: list[Instruction], signers: list[Keypair], label: str = "") -> str:
        """Signs with every signer (the first pays fees), sends, waits for confirmation, and raises with the
        program logs on failure so a refused instruction names its own error."""
        msg = Message.new_with_blockhash(instructions, signers[0].pubkey(), self.blockhash())
        tx = Transaction(signers, msg, msg.recent_blockhash)
        wire = base64.b64encode(bytes(tx)).decode()
        try:
            sig = self.call("sendTransaction", [wire, {"encoding": "base64", "preflightCommitment": self.commitment}])
        except RpcError as e:
            raise RpcError(f"{label or 'transaction'} refused: {e}") from None
        self._confirm(sig, label)
        return sig

    def _confirm(self, sig: str, label: str = "", timeout: float = 60.0) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            st = self.call("getSignatureStatuses", [[sig]])["value"][0]
            if st and st.get("confirmationStatus") in ("confirmed", "finalized"):
                if st.get("err"):
                    logs = self.logs(sig)
                    raise RpcError(f"{label or sig} failed: {st['err']} | {logs}")
                return
            time.sleep(0.25)
        raise RpcError(f"{label or sig} not confirmed within {timeout}s")

    def logs(self, sig: str) -> list[str]:
        r = self.call("getTransaction", [sig, {"encoding": "json", "commitment": self.commitment, "maxSupportedTransactionVersion": 0}])
        return (r or {}).get("meta", {}).get("logMessages", [])
