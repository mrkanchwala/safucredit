import { useEffect, useState } from "react";
import { useAction, useClient, usePayer } from "@solana/react";
import { address } from "@solana/kit";
import type { AppClient } from "../lib/client";
import { pullClaimPayout, setPayoutAddress } from "../lib/actions";
import { ClaimStatus, fmtUsdc, readActiveClaim } from "../lib/reads";
import { TxStatus } from "./TxStatus";

function statusLabel(status: ClaimStatus): string {
  switch (status) {
    case ClaimStatus.Denied:
      return "Denied — nothing owed, real drop in price";
    case ClaimStatus.PendingTime:
      return "Queued — held until the 60-day gate clears, nothing forfeited";
    case ClaimStatus.Queued:
      return "Queued for admission — genuine claim, waiting on daily cap";
    case ClaimStatus.Active:
      return "Active — streaming, pull below";
    case ClaimStatus.Completed:
      return "Completed — fully paid";
    case ClaimStatus.Expired:
      return "Expired";
    case ClaimStatus.Cancelled:
      return "Cancelled by admin override";
    default:
      return "Unknown";
  }
}

export function ClaimsPanel() {
  const client = useClient<AppClient>();
  const payer = usePayer(client);
  const [payoutInput, setPayoutInput] = useState("");
  const [confirmingPayout, setConfirmingPayout] = useState(false);
  const [claim, setClaim] = useState<Awaited<ReturnType<typeof readActiveClaim>>>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    if (!payer) return;
    let cancelled = false;
    (async () => {
      const c = await readActiveClaim(client.rpc, payer.address);
      if (!cancelled) setClaim(c);
    })();
    return () => {
      cancelled = true;
    };
  }, [client, payer, refreshKey]);

  const setPayoutAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    const target = payoutInput.trim() ? address(payoutInput.trim()) : payer.address;
    const sig = await setPayoutAddress(client, payer, target);
    setPayoutInput("");
    setConfirmingPayout(false);
    return sig;
  });

  const pullAction = useAction(async () => {
    if (!payer) throw new Error("connect a wallet first");
    if (!claim) throw new Error("no active claim");
    const sig = await pullClaimPayout(client, payer, claim.address, claim.data.payout);
    setRefreshKey((k) => k + 1);
    return sig;
  });

  const canPull = claim && claim.data.status === ClaimStatus.Active;

  return (
    <div className="panel">
      <div>
        <div className="field-group">
          <div className="field-label">
            <span>Payout address</span>
            <span>Defaults to your wallet</span>
          </div>
          <div className="field-input">
            <input
              placeholder={payer ? payer.address : "connect a wallet"}
              value={payoutInput}
              onChange={(e) => {
                setPayoutInput(e.target.value);
                setConfirmingPayout(false);
                setPayoutAction.reset();
              }}
            />
          </div>
        </div>
        <button
          className="secondary-action"
          style={{ width: "100%" }}
          disabled={!payer || setPayoutAction.isRunning}
          onClick={() => {
            if (confirmingPayout) {
              setPayoutAction.dispatch();
            } else {
              setConfirmingPayout(true);
            }
          }}
          onBlur={() => setConfirmingPayout(false)}
        >
          {setPayoutAction.isRunning
            ? "Sending..."
            : confirmingPayout
              ? "Click again to confirm — this redirects future payouts"
              : "Set payout address"}
        </button>
        <TxStatus action={setPayoutAction} />

        <div className="field-group" style={{ marginTop: 20 }}>
          <div className="field-label">
            <span>Your claim</span>
            <span>&nbsp;</span>
          </div>
          {!claim ? (
            <div className="side-stat">
              <div className="k">Status</div>
              <div className="v">No claim on this market</div>
            </div>
          ) : (
            <div className="side-stat">
              <div className="k">Status</div>
              <div className="v" style={{ fontFamily: "var(--font-body)", fontSize: 14 }}>
                {statusLabel(claim.data.status)}
              </div>
            </div>
          )}
        </div>
        <button
          className="primary-action"
          disabled={!payer || !canPull || pullAction.isRunning}
          onClick={() => pullAction.dispatch()}
        >
          {pullAction.isRunning ? "Sending..." : "Pull payout"}
        </button>
        <TxStatus action={pullAction} />
      </div>
      <div>
        {claim ? (
          <>
            <div className="side-stat">
              <div className="k">Loss (proven, on-chain)</div>
              <div className="v">{fmtUsdc(claim.data.loss)} USDC</div>
            </div>
            <div className="side-stat">
              <div className="k">Streamed so far</div>
              <div className="v">{fmtUsdc(claim.data.streamed)} USDC</div>
            </div>
          </>
        ) : (
          <div className="side-stat">
            <div className="k">How this works</div>
            <div className="v good">
              Pulling is permissionless — anyone can crank it, funds always land at your payout
              address
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
