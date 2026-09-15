import { useEffect, useState } from "react";
import { useAction, useClient, usePayer } from "@solana/react";
import type { AppClient } from "../lib/client";
import { backerDeposit, backerFinalizeWithdraw, backerRequestWithdraw } from "../lib/actions";
import { fmtShares, fmtUsdc, readBacker, readUsdcBalance, toRaw } from "../lib/reads";
import { TxStatus } from "./TxStatus";

export function BackstopPanel() {
  const client = useClient<AppClient>();
  const payer = usePayer(client);
  const [depositInput, setDepositInput] = useState("");
  const [withdrawInput, setWithdrawInput] = useState("");
  const [usdcBalance, setUsdcBalance] = useState<bigint | null>(null);
  const [shares, setShares] = useState<bigint | null>(null);
  const [withdrawShares, setWithdrawShares] = useState<bigint | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    if (!payer) return;
    let cancelled = false;
    (async () => {
      const [usdc, backer] = await Promise.all([
        readUsdcBalance(client.rpc, payer.address),
        readBacker(client.rpc, payer.address),
      ]);
      if (cancelled) return;
      setUsdcBalance(usdc);
      setShares(backer?.shares ?? 0n);
      setWithdrawShares(backer?.withdrawShares ?? 0n);
    })();
    return () => {
      cancelled = true;
    };
  }, [client, payer, refreshKey]);

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

  const hasPendingWithdraw = withdrawShares !== null && withdrawShares > 0n;

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
      </div>
    </div>
  );
}
