import { useEffect, useState } from "react";
import { useAction, useClient, usePayer } from "@solana/react";
import type { AppClient } from "../lib/client";
import { supply, withdrawSupply } from "../lib/actions";
import { fmtUsdc, readMarket, readSupplier, readUsdcBalance, supplierValueRaw, toRaw } from "../lib/reads";
import { TxStatus } from "./TxStatus";

export function LendPanel() {
  const client = useClient<AppClient>();
  const payer = usePayer(client);
  const [supplyInput, setSupplyInput] = useState("");
  const [supplyMaxRaw, setSupplyMaxRaw] = useState<bigint | null>(null);
  const [withdrawInput, setWithdrawInput] = useState("");
  const [usdcBalance, setUsdcBalance] = useState<bigint | null>(null);
  const [supplierShares, setSupplierShares] = useState<bigint | null>(null);
  const [supplierValue, setSupplierValue] = useState<bigint | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    if (!payer) return;
    let cancelled = false;
    (async () => {
      const [usdc, market, supplier] = await Promise.all([
        readUsdcBalance(client.rpc, payer.address),
        readMarket(client.rpc),
        readSupplier(client.rpc, payer.address),
      ]);
      if (cancelled) return;
      setUsdcBalance(usdc);
      setSupplierShares(supplier?.shares ?? 0n);
      setSupplierValue(supplier && market ? supplierValueRaw(market, supplier.shares) : 0n);
    })();
    return () => {
      cancelled = true;
    };
  }, [client, payer, refreshKey]);

  const supplyAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    const amt = supplyMaxRaw ?? toRaw(Number(supplyInput || "0"), 6);
    if (amt === 0n) throw new Error("enter a supply amount");
    const sig = await supply(client, payer, amt);
    setSupplyInput("");
    setSupplyMaxRaw(null);
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const withdrawAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    if (!supplierShares || supplierShares === 0n) throw new Error("nothing supplied yet");
    const pct = Number(withdrawInput || "0");
    if (pct <= 0 || pct > 100) throw new Error("enter a percent between 1 and 100");
    const shares = (supplierShares * BigInt(Math.round(pct * 100))) / 10_000n;
    const sig = await withdrawSupply(client, payer, shares);
    setWithdrawInput("");
    setRefreshKey((k) => k + 1);
    return sig;
  });

  return (
    <div className="panel">
      <div>
        <div className="field-group">
          <div className="field-label">
            <span>Supply USDC</span>
            <span>Wallet: {usdcBalance !== null ? fmtUsdc(usdcBalance) : "..."} USDC</span>
          </div>
          <div className="field-input">
            <input
              placeholder="0.00"
              value={supplyInput}
              onChange={(e) => {
                setSupplyInput(e.target.value);
                setSupplyMaxRaw(null);
              }}
              inputMode="decimal"
            />
            <span className="unit">USDC</span>
            {usdcBalance !== null ? (
              <button
                className="max"
                onClick={() => {
                  setSupplyInput(fmtUsdc(usdcBalance));
                  setSupplyMaxRaw(usdcBalance);
                }}
              >
                MAX
              </button>
            ) : null}
          </div>
        </div>
        <button
          className="primary-action"
          disabled={!payer || supplyAction.isRunning}
          onClick={() => supplyAction.dispatch()}
        >
          {supplyAction.isRunning ? "Sending..." : "Supply"}
        </button>
        <TxStatus action={supplyAction} />

        <div className="field-group" style={{ marginTop: 16 }}>
          <div className="field-label">
            <span>Withdraw (% of your position)</span>
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
        <button
          className="secondary-action"
          style={{ width: "100%" }}
          disabled={!payer || withdrawAction.isRunning}
          onClick={() => withdrawAction.dispatch()}
        >
          {withdrawAction.isRunning ? "Sending..." : "Withdraw"}
        </button>
        <TxStatus action={withdrawAction} />
      </div>
      <div>
        <div className="side-stat">
          <div className="k">Your supplied value</div>
          <div className="v">{supplierValue !== null ? fmtUsdc(supplierValue) : "..."} USDC</div>
        </div>
        <div className="side-stat">
          <div className="k">Lender share of borrower interest</div>
          <div className="v">85%</div>
        </div>
        <div className="side-stat">
          <div className="k">Protection</div>
          <div className="v good">Backer capital absorbs bad debt first</div>
        </div>
      </div>
    </div>
  );
}
