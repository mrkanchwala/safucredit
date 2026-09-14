#!/usr/bin/env bash
# Dependency security gate for the SAFU Credit Solana workspace (plan step P3).
#
# Two parts, because `cargo audit` alone cannot tell a test-only advisory from a real one:
#   1. cargo audit, with the documented ignores in .cargo/audit.toml
#   2. a re-derivation of the claim those ignores rest on — that each ignored crate is absent from
#      the NON-DEV dependency graph. If one ever becomes a real dependency, this fails loudly
#      instead of staying quietly muted.
#
# Usage: scripts/audit-gate.sh   (run from the solana/ workspace root)
set -euo pipefail

cd "$(dirname "$0")/.."

# Crates whose advisories are ignored in .cargo/audit.toml. Keep the two lists in step.
TEST_ONLY_CRATES=(ed25519-dalek curve25519-dalek rand)

echo "== cargo audit =="
cargo audit

echo
echo "== verifying every ignored advisory is still test-only =="
failed=0
for crate in "${TEST_ONLY_CRATES[@]}"; do
    # -e normal excludes dev and build edges; --target all so a platform-gated path cannot hide.
    if tree=$(cargo tree --quiet -i "$crate" -e normal --target all 2>/dev/null) \
       && [ -n "$tree" ] && ! grep -q "nothing to print" <<<"$tree"; then
        echo "FAIL: $crate is now a non-dev dependency — its ignore in .cargo/audit.toml is no"
        echo "      longer justified. Remove the ignore and fix the advisory."
        echo "$tree" | head -10
        failed=1
    else
        echo "ok: $crate is dev-only"
    fi
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi
echo
echo "audit gate PASS"
