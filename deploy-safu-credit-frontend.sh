#!/usr/bin/env bash
# Deploys the safucredit frontend (Stocklana) to credit.safustaking.com.
# Run from ~/SAFU/safucredit/. Requires the nginx server block + cert to
# already exist on the VPS (one-time setup, done 2026-09-15 -- see
# deploy/nginx/ for the config + the runbook that set it up).
set -euo pipefail
cd "$(dirname "$0")/app"

echo "[1/3] Building..."
npx vite build

echo "[2/3] Rsyncing to VPS webroot..."
rsync -avz --delete dist/ murtaza@46.225.110.140:/var/www/credit-safustaking/

echo "[3/3] Smoke test..."
STATUS=$(curl -sI https://credit.safustaking.com/ -o /dev/null -w '%{http_code}')
echo "HTTP status: $STATUS"
if [ "$STATUS" != "200" ]; then
    echo "FAILED -- expected 200, got $STATUS"
    exit 1
fi
TITLE=$(curl -s https://credit.safustaking.com/ | grep -o '<title>[^<]*</title>')
echo "Title: $TITLE"
if [ "$TITLE" != "<title>SAFU Credit</title>" ]; then
    echo "FAILED -- unexpected title, check for a routing/fallback issue"
    exit 1
fi
echo "Deploy OK."
