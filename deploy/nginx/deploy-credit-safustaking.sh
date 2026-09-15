#!/usr/bin/env bash
# Run this ON THE VPS (ssh murtaza@46.225.110.140, then run this script there).
# Assumes credit-safustaking.conf and credit-safustaking-security-headers.conf
# are already in your home directory (~) -- scp'd there already.
set -euo pipefail

echo "[1/6] Creating webroot..."
sudo mkdir -p /var/www/credit-safustaking
sudo chown murtaza:murtaza /var/www/credit-safustaking
ls -ld /var/www/credit-safustaking

echo "[2/6] Installing nginx snippet (CSP + security headers)..."
sudo cp ~/credit-safustaking-security-headers.conf /etc/nginx/snippets/credit-safustaking-security-headers.conf

echo "[3/6] Installing nginx server block..."
sudo cp ~/credit-safustaking.conf /etc/nginx/conf.d/credit-safustaking.conf

echo "[4/6] Requesting SSL certificate via certbot (reuses existing account, no new email needed)..."
sudo certbot --nginx -d credit.safustaking.com --non-interactive --agree-tos --redirect

echo "[5/6] Testing nginx config..."
sudo nginx -t

echo "[6/6] Reloading nginx..."
sudo systemctl reload nginx
systemctl is-active nginx

echo ""
echo "Done. credit.safustaking.com should now resolve over HTTPS (currently a 404 placeholder"
echo "since the built frontend hasn't been rsynced yet -- that's the next step, not this script)."
