import { useEffect, useState } from "react";
import { useAction, useClient, usePayer } from "@solana/react";
import type { AppClient } from "../lib/client";
import { depositAndBorrow, repay, withdrawCollateral } from "../lib/actions";
import {
  debtForShares,
  fmtAaplx,
  fmtPrice,
  fmtUsdc,
  readAaplxBalance,
  readMarket,
  readPosition,
  readUsdcBalance,
  toRaw,
} from "../lib/reads";
import { MAX_LTV_BPS } from "../lib/market";
import { TxStatus } from "./TxStatus";

export function BorrowPanel() {
  const client = useClient<AppClient>();
  const payer = usePayer(client);
  const [collateralInput, setCollateralInput] = useState("");
  const [borrowInput, setBorrowInput] = useState("");
  const [repayInput, setRepayInput] = useState("");
  const [withdrawInput, setWithdrawInput] = useState("");
  const [aaplxBalance, setAaplxBalance] = useState<bigint | null>(null);
  const [usdcBalance, setUsdcBalance] = useState<bigint | null>(null);
  const [price, setPrice] = useState<bigint | null>(null);
  const [debtRaw, setDebtRaw] = useState<bigint | null>(null);
  const [collateralRaw, setCollateralRaw] = useState<bigint | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    if (!payer) return;
    let cancelled = false;
    (async () => {
      const [aaplx, usdc, market, position] = await Promise.all([
        readAaplxBalance(client.rpc, payer.address),
        readUsdcBalance(client.rpc, payer.address),
        readMarket(client.rpc),
        readPosition(client.rpc, payer.address),
      ]);
      if (cancelled) return;
      setAaplxBalance(aaplx);
      setUsdcBalance(usdc);
      setPrice(market?.price.lastPrice ?? null);
      setDebtRaw(position && market ? debtForShares(position.debtShares, market.borrowIndex) : 0n);
      setCollateralRaw(position?.rawCollateral ?? 0n);
    })();
    return () => {
      cancelled = true;
    };
  }, [client, payer, refreshKey]);

  const depositBorrowAction = useAction(async (signal: AbortSignal) => {
    if (!payer) throw new Error("connect a wallet first");
    const c = toRaw(Number(collateralInput || "0"), 8);
    const b = toRaw(Number(borrowInput || "0"), 6);
    if (c === 0n && b === 0n) throw new Error("enter a collateral or borrow amount");
    const sig = await depositAndBorrow(client, payer, c, b);
    void signal;
    setCollateralInput("");
    setBorrowInput("");
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const repayAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    const amt = toRaw(Number(repayInput || "0"), 6);
    if (amt === 0n) throw new Error("enter a repay amount");
    const sig = await repay(client, payer, amt);
    setRepayInput("");
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const withdrawAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    const amt = toRaw(Number(withdrawInput || "0"), 8);
    if (amt === 0n) throw new Error("enter a withdraw amount");
    const sig = await withdrawCollateral(client, payer, amt);
    setWithdrawInput("");
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const priceUsd = price !== null && price > 0n ? Number(fmtPrice(price)) : null;

  return (
    <div className="panel">
      <div>
        <div className="field-group">
          <div className="field-label">
            <span>Collateral</span>
            <span>Balance: {aaplxBalance !== null ? fmtAaplx(aaplxBalance) : "..."} AAPLx</span>
          </div>
          <div className="field-input">
            <input
              placeholder="0.00"
              value={collateralInput}
              onChange={(e) => {
                setCollateralInput(e.target.value);
                depositBorrowAction.reset();
              }}
              inputMode="decimal"
            />
            <span className="unit">AAPLx</span>
            {aaplxBalance !== null ? (
              <button className="max" onClick={() => setCollateralInput(fmtAaplx(aaplxBalance))}>
                MAX
              </button>
            ) : null}
          </div>
        </div>
        <div className="field-group">
          <div className="field-label">
            <span>Borrow</span>
            <span>Wallet: {usdcBalance !== null ? fmtUsdc(usdcBalance) : "..."} USDC</span>
          </div>
          <div className="field-input">
            <input
              placeholder="0.00"
              value={borrowInput}
              onChange={(e) => {
                setBorrowInput(e.target.value);
                depositBorrowAction.reset();
              }}
              inputMode="decimal"
            />
            <span className="unit">USDC</span>
          </div>
        </div>
        <button
          className="primary-action"
          disabled={!payer || depositBorrowAction.isRunning}
          onClick={() => depositBorrowAction.dispatch()}
        >
          {depositBorrowAction.isRunning ? "Sending..." : "Deposit & Borrow"}
        </button>
        <TxStatus action={depositBorrowAction} />

        <div className="secondary-row">
          <div className="field-group" style={{ marginBottom: 0 }}>
            <div className="field-label">
              <span>Repay</span>
              <span>Debt: {debtRaw !== null ? fmtUsdc(debtRaw) : "..."} USDC</span>
            </div>
            <div className="field-input">
              <input
                placeholder="0.00"
                value={repayInput}
                onChange={(e) => {
                  setRepayInput(e.target.value);
                  depositBorrowAction.reset();
                }}
                inputMode="decimal"
              />
              <span className="unit">USDC</span>
            </div>
            <button
              className="secondary-action"
              style={{ width: "100%", marginTop: 8 }}
              disabled={!payer || repayAction.isRunning}
              onClick={() => repayAction.dispatch()}
            >
              {repayAction.isRunning ? "Sending..." : "Repay"}
            </button>
          </div>
          <div className="field-group" style={{ marginBottom: 0 }}>
            <div className="field-label">
              <span>Withdraw collateral</span>
              <span>Deposited: {collateralRaw !== null ? fmtAaplx(collateralRaw) : "..."} AAPLx</span>
            </div>
            <div className="field-input">
              <input
                placeholder="0.00"
                value={withdrawInput}
                onChange={(e) => {
                  setWithdrawInput(e.target.value);
                  depositBorrowAction.reset();
                }}
                inputMode="decimal"
              />
              <span className="unit">AAPLx</span>
              {collateralRaw !== null && collateralRaw > 0n ? (
                <button className="max" onClick={() => setWithdrawInput(fmtAaplx(collateralRaw))}>
                  MAX
                </button>
              ) : null}
            </div>
            <button
              className="secondary-action"
              style={{ width: "100%", marginTop: 8 }}
              disabled={!payer || withdrawAction.isRunning}
              onClick={() => withdrawAction.dispatch()}
            >
              {withdrawAction.isRunning ? "Sending..." : "Withdraw"}
            </button>
          </div>
        </div>
        <TxStatus action={repayAction} />
        <TxStatus action={withdrawAction} />
      </div>
      <div>
        <div className="side-stat">
          <div className="k">AAPLx price</div>
          <div className="v">{priceUsd !== null ? `$${priceUsd.toFixed(2)}` : "no live price"}</div>
        </div>
        <div className="side-stat">
          <div className="k">Max LTV</div>
          <div className="v">{MAX_LTV_BPS / 100}%</div>
        </div>
        <div className="side-stat">
          <div className="k">Your protection</div>
          <div className="v good">1:1 wrongful-liquidation payback</div>
        </div>
      </div>
    </div>
  );
}
