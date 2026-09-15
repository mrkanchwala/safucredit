import { useEffect, useState } from "react";
import { useAction, useClient, usePayer } from "@solana/react";
import type { AppClient } from "../lib/client";
import { backerDeposit, backerFinalizeWithdraw, backerRequestWithdraw, buyInventory } from "../lib/actions";
import {
  fmtAaplx,
  fmtPrice,
  fmtShares,
  fmtUsdc,
  readAaplxBalance,
  readBacker,
  readBackstopConfig,
  readInventory,
  readMarket,
  readUsdcBalance,
  toRaw,
} from "../lib/reads";
import { TxStatus } from "./TxStatus";

export function BackstopPanel() {
  const client = useClient<AppClient>();
  const payer = usePayer(client);
  const [depositInput, setDepositInput] = useState("");
  const [withdrawInput, setWithdrawInput] = useState("");
  const [usdcBalance, setUsdcBalance] = useState<bigint | null>(null);
  const [aaplxBalance, setAaplxBalance] = useState<bigint | null>(null);
  const [shares, setShares] = useState<bigint | null>(null);
  const [withdrawShares, setWithdrawShares] = useState<bigint | null>(null);
  const [inventoryRaw, setInventoryRaw] = useState<bigint | null>(null);
  const [resaleDiscountBps, setResaleDiscountBps] = useState<number | null>(null);
  const [marketPrice, setMarketPrice] = useState<bigint | null>(null);
  const [buyInput, setBuyInput] = useState("");
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    if (!payer) return;
    let cancelled = false;
    (async () => {
      const [usdc, aaplx, backer] = await Promise.all([
        readUsdcBalance(client.rpc, payer.address),
        readAaplxBalance(client.rpc, payer.address),
        readBacker(client.rpc, payer.address),
      ]);
      if (cancelled) return;
      setUsdcBalance(usdc);
      setAaplxBalance(aaplx);
      setShares(backer?.shares ?? 0n);
      setWithdrawShares(backer?.withdrawShares ?? 0n);
    })();
    return () => {
      cancelled = true;
    };
  }, [client, payer, refreshKey]);

  // Inventory / resale state is public -- anyone should see it, connected or not, since
  // buyInventory is permissionless and not limited to existing backers.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      const [inventory, config, market] = await Promise.all([
        readInventory(client.rpc),
        readBackstopConfig(client.rpc),
        readMarket(client.rpc),
      ]);
      if (cancelled) return;
      setInventoryRaw(inventory?.data.raw ?? 0n);
      setResaleDiscountBps(config?.resaleDiscountBps ?? null);
      setMarketPrice(market?.price.lastPrice ?? null);
    })();
    return () => {
      cancelled = true;
    };
  }, [client, refreshKey]);

  const depositAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    const amt = toRaw(Number(depositInput || "0"), 6);
    if (amt === 0n) throw new Error("enter a deposit amount");
    const sig = await backerDeposit(client, payer, amt);
    setDepositInput("");
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const requestWithdrawAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    if (!shares || shares === 0n) throw new Error("nothing backing yet");
    const pct = Number(withdrawInput || "0");
    if (pct <= 0 || pct > 100) throw new Error("enter a percent between 1 and 100");
    const s = (shares * BigInt(Math.round(pct * 100))) / 10_000n;
    const sig = await backerRequestWithdraw(client, payer, s);
    setWithdrawInput("");
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const finalizeWithdrawAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    const sig = await backerFinalizeWithdraw(client, payer);
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const buyInventoryAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    if (!marketPrice) throw new Error("no live price yet");
    const amt = toRaw(Number(buyInput || "0"), 8);
    if (amt === 0n) throw new Error("enter an amount");
    // Slippage guard: the live market price as a ceiling. The actual fill is always at a
    // discount below this (or floor-protected near the pool's original cost), so this never
    // blocks a normal buy -- it only protects against the price moving up before confirmation.
    const sig = await buyInventory(client, payer, amt, marketPrice);
    setBuyInput("");
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const hasPendingWithdraw = withdrawShares !== null && withdrawShares > 0n;
  const hasInventory = inventoryRaw !== null && inventoryRaw > 0n;
  const discountedPrice =
    marketPrice !== null && resaleDiscountBps !== null
      ? (marketPrice * BigInt(10_000 - resaleDiscountBps)) / 10_000n
      : null;

  return (
    <div className="panel">
      <div>
        <div className="field-group">
          <div className="field-label">
            <span>Back the pool (USDC)</span>
            <span>Wallet: {usdcBalance !== null ? fmtUsdc(usdcBalance) : "..."} USDC</span>
          </div>
          <div className="field-input">
            <input
              placeholder="0.00"
              value={depositInput}
              onChange={(e) => setDepositInput(e.target.value)}
              inputMode="decimal"
            />
            <span className="unit">USDC</span>
            {usdcBalance !== null ? (
              <button className="max" onClick={() => setDepositInput(fmtUsdc(usdcBalance))}>
                MAX
              </button>
            ) : null}
          </div>
        </div>
        <button
          className="primary-action"
          disabled={!payer || depositAction.isRunning}
          onClick={() => depositAction.dispatch()}
        >
          {depositAction.isRunning ? "Sending..." : "Deposit backer capital"}
        </button>
        <TxStatus action={depositAction} />

        <div className="field-group" style={{ marginTop: 16 }}>
          <div className="field-label">
            <span>Request withdraw (% of your position)</span>
            <span>&nbsp;</span>
          </div>
          <div className="field-input">
            <input
              placeholder="0-100"
              value={withdrawInput}
              onChange={(e) => setWithdrawInput(e.target.value)}
              inputMode="decimal"
            />
            <span className="unit">%</span>
            <button className="max" onClick={() => setWithdrawInput("100")}>
              MAX
            </button>
          </div>
        </div>
        <div className="secondary-row">
          <button
            className="secondary-action"
            disabled={!payer || requestWithdrawAction.isRunning}
            onClick={() => requestWithdrawAction.dispatch()}
          >
            {requestWithdrawAction.isRunning ? "Sending..." : "1. Request"}
          </button>
          <button
            className="secondary-action"
            disabled={!payer || !hasPendingWithdraw || finalizeWithdrawAction.isRunning}
            onClick={() => finalizeWithdrawAction.dispatch()}
          >
            {finalizeWithdrawAction.isRunning ? "Sending..." : "2. Finalize"}
          </button>
        </div>
        <TxStatus action={requestWithdrawAction} />
        <TxStatus action={finalizeWithdrawAction} />

        <div className="field-group" style={{ marginTop: 16 }}>
          <div className="field-label">
            <span>Buy seized collateral (unpauses the pool)</span>
            <span>
              {hasInventory
                ? `Available: ${fmtAaplx(inventoryRaw!)} AAPLx${discountedPrice !== null ? ` at ~$${fmtPrice(discountedPrice)}` : ""}`
                : "Nothing to buy right now"}
            </span>
          </div>
          <div className="field-input">
            <input
              placeholder="0.00"
              value={buyInput}
              onChange={(e) => {
                setBuyInput(e.target.value);
                buyInventoryAction.reset();
              }}
              inputMode="decimal"
              disabled={!hasInventory}
            />
            <span className="unit">AAPLx</span>
            {hasInventory ? (
              <button className="max" onClick={() => setBuyInput(fmtAaplx(inventoryRaw!))}>
                MAX
              </button>
            ) : null}
          </div>
          <button
            className="secondary-action"
            style={{ width: "100%", marginTop: 8 }}
            disabled={!payer || !hasInventory || buyInventoryAction.isRunning}
            onClick={() => buyInventoryAction.dispatch()}
          >
            {buyInventoryAction.isRunning ? "Sending..." : "Buy"}
          </button>
          {!hasInventory ? (
            <div className="tx-status-hint" style={{ marginTop: 6 }}>
              This fills whenever the pool liquidates a position and seizes collateral. Deposits and
              withdrawals pause on the pool until it's resold, and anyone can buy it here at a discount
              to clear that.
            </div>
          ) : null}
          <TxStatus action={buyInventoryAction} />
        </div>
      </div>
      <div>
        <div className="side-stat">
          <div className="k">Your backer shares</div>
          <div className="v">{shares !== null ? fmtShares(shares) : "..."}</div>
        </div>
        <div className="side-stat">
          <div className="k">Backer return, good year</div>
          <div className="v good">~10%</div>
        </div>
        <div className="side-stat">
          <div className="k">Backer return, bad year</div>
          <div className="v">−1.9%</div>
        </div>
        {hasPendingWithdraw ? (
          <div className="side-stat">
            <div className="k">Pending withdraw</div>
            <div className="v">{fmtShares(withdrawShares!)} shares queued</div>
          </div>
        ) : null}
        {hasInventory ? (
          <div className="side-stat">
            <div className="k">Your AAPLx wallet</div>
            <div className="v">{aaplxBalance !== null ? fmtAaplx(aaplxBalance) : "..."} AAPLx</div>
          </div>
        ) : null}
      </div>
      <div className="panel-note" style={{ gridColumn: "1 / -1" }}>
        You can back this pool and borrow from it with the same wallet, but if that wallet is
        wrongfully liquidated, it will not receive the payout while it still holds backer shares
        here. Withdraw fully from the backstop, or borrow from a different wallet, if you want
        wrongful-liquidation protection.
      </div>
    </div>
  );
}
