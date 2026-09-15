#!/usr/bin/env bash
# Run this ON THE VPS. Fixes the broken conf.d file from the previous attempt
# (it referenced a cert that didn't exist yet, which could break nginx -t for
# every other site on this box until this runs), then does the cert issuance
# properly as two phases: stub first, cert, then the real config.
#
# Assumes credit-safustaking-stub.conf and credit-safustaking.conf are already
# in your home directory (~) -- scp'd there.
set -euo pipefail

echo "[1/7] Restoring a safe stub config (fixes the currently-broken conf.d file)..."
sudo cp ~/credit-safustaking-stub.conf /etc/nginx/conf.d/credit-safustaking.conf
sudo nginx -t
sudo systemctl reload nginx
echo "  -> nginx is safe again."

echo "[2/7] Requesting SSL certificate (certonly -- does not touch nginx config)..."
sudo certbot certonly --nginx -d credit.safustaking.com --non-interactive --agree-tos

echo "[3/7] Verifying the cert files actually exist..."
sudo test -f /etc/letsencrypt/live/credit.safustaking.com/fullchain.pem
sudo test -f /etc/letsencrypt/live/credit.safustaking.com/privkey.pem
echo "  -> cert files present."

echo "[4/7] Installing the real server block (now that the cert exists)..."
sudo cp ~/credit-safustaking.conf /etc/nginx/conf.d/credit-safustaking.conf

echo "[5/7] Testing nginx config..."
sudo nginx -t

echo "[6/7] Reloading nginx..."
sudo systemctl reload nginx

echo "[7/7] Verifying..."
systemctl is-active nginx
curl -sI https://credit.safustaking.com/ -o /dev/null -w 'HTTP status: %{http_code}\n'

echo ""
echo "Done. That 404 status above is expected -- the built frontend hasn't been"
echo "rsynced into /var/www/credit-safustaking yet. HTTPS + cert are live now."
