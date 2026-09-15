import { useState } from "react";
import {
  useConnect,
  useConnectedWallet,
  useDisconnect,
  useWallets,
  WalletReadyGate,
} from "@solana/kit-plugin-wallet/react";
import type { AppClient } from "../lib/client";
import { ALLOWED_WALLET_NAMES } from "../lib/client";

function short(addr: string): string {
  return `${addr.slice(0, 4)}...${addr.slice(-4)}`;
}

function WalletButtonInner({ client }: { client: AppClient }) {
  const [open, setOpen] = useState(false);
  const wallets = useWallets(client);
  const connected = useConnectedWallet(client);
  const { dispatch: connect } = useConnect(client);
  const { dispatch: disconnect } = useDisconnect(client);

  const allowed = wallets.filter((w) =>
    (ALLOWED_WALLET_NAMES as readonly string[]).includes(w.name),
  );

  if (connected) {
    return (
      <div className="wallet-menu">
        <button className="connect-btn" onClick={() => disconnect()}>
          {short(connected.account.address)} — Disconnect
        </button>
      </div>
    );
  }

  return (
    <div className="wallet-menu">
      <button className="connect-btn" onClick={() => setOpen((v) => !v)}>
        Connect Wallet
      </button>
      {open ? (
        <div className="wallet-dropdown">
          {allowed.length === 0 ? (
            <div className="empty">No supported wallet found. Install Phantom, Solflare, or Backpack.</div>
          ) : (
            allowed.map((w) => (
              <button
                key={w.name}
                onClick={() => {
                  connect(w);
                  setOpen(false);
                }}
              >
                {w.name}
              </button>
            ))
          )}
        </div>
      ) : null}
    </div>
  );
}

export function WalletButton({ client }: { client: AppClient }) {
  return (
    <WalletReadyGate client={client} fallback={<button className="connect-btn" disabled>Loading...</button>}>
      <WalletButtonInner client={client} />
    </WalletReadyGate>
  );
}
