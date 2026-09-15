import { useState } from "react";
import { WalletButton } from "./components/WalletButton";
import { BorrowPanel } from "./components/BorrowPanel";
import { LendPanel } from "./components/LendPanel";
import { BackstopPanel } from "./components/BackstopPanel";
import { ClaimsPanel } from "./components/ClaimsPanel";
import { HowItWorks } from "./components/HowItWorks";
import { client } from "./lib/client";

type Tab = "borrow" | "lend" | "backstop" | "claims";

const TABS: { id: Tab; label: string }[] = [
  { id: "borrow", label: "Borrow" },
  { id: "lend", label: "Lend" },
  { id: "backstop", label: "Backstop" },
  { id: "claims", label: "Claims" },
];

export default function App() {
  const [tab, setTab] = useState<Tab>("borrow");
  const [showDetails, setShowDetails] = useState(false);

  if (showDetails) {
    return (
      <>
        <div className="devnet-badge">DEVNET — REAL TRANSACTIONS, NO REAL FUNDS</div>
        <HowItWorks onBack={() => setShowDetails(false)} />
      </>
    );
  }

  return (
    <>
      <div className="devnet-badge">DEVNET — REAL TRANSACTIONS, NO REAL FUNDS</div>

      <header>
        <div className="wrap">
          <div className="logo">
            <img src="/logo.png" alt="SAFU" />
            <span className="name">SAFU Credit</span>
            <span className="fam">by SAFU</span>
          </div>
          <WalletButton client={client} />
        </div>
      </header>

      <section className="hero">
        <div className="wrap">
          <h1>
            Wrongfully liquidated?
            <br />
            <em>Paid back automatically.</em>
          </h1>
          <p>
            Borrow USDC against tokenized stocks. If a bad price wrongly liquidates you, you're
            repaid 1:1, automatically, with no vote and no appeal.
          </p>
        </div>
      </section>

      <section className="dapp">
        <div className="wrap">
          <div className="dapp-shell">
            <div className="tabs">
              {TABS.map((t) => (
                <button
                  key={t.id}
                  className={`tab${tab === t.id ? " active" : ""}`}
                  onClick={() => setTab(t.id)}
                >
                  {t.label}
                </button>
              ))}
            </div>
            {tab === "borrow" ? <BorrowPanel /> : null}
            {tab === "lend" ? <LendPanel /> : null}
            {tab === "backstop" ? <BackstopPanel /> : null}
            {tab === "claims" ? <ClaimsPanel /> : null}
          </div>
        </div>
      </section>

      <section className="numbers">
        <div className="wrap">
          <h2>The numbers</h2>
          <div className="sub">Real figures from the locked spec, not estimates.</div>
          <div className="stat-grid">
            <div className="stat-card">
              <div className="v">40%</div>
              <div className="k">Max LTV</div>
            </div>
            <div className="stat-card">
              <div className="v">1–5%</div>
              <div className="k">Liquidation bonus (dynamic)</div>
            </div>
            <div className="stat-card">
              <div className="v">~10%</div>
              <div className="k">Backer return, good year</div>
            </div>
            <div className="stat-card">
              <div className="v">−1.9%</div>
              <div className="k">Backer return, bad year</div>
            </div>
          </div>
        </div>
      </section>

      <section className="coverage">
        <div className="wrap">
          <h2>Where liquidations go wrong, and what we do about it</h2>
          <div className="sub">
            Only one of these actually costs us money. The other two, we just refuse to liquidate
            on.
          </div>
          <div className="cov-groups">
            <div className="cov-group prevented">
              <h3>Prevented</h3>
              <div className="note">Nothing lost. The liquidation just doesn't happen</div>
              <ul>
                <li>Price spike → blocked before it can liquidate you</li>
                <li>Stock split → held until repriced correctly</li>
              </ul>
            </div>
            <div className="cov-group paid">
              <h3>Paid back</h3>
              <div className="note">A real liquidation happens on bad data, and we cover it</div>
              <ul>
                <li>Slow wrong-price drift → liquidated wrongly → repaid 1:1 on-chain</li>
              </ul>
              <div
                className="note"
                style={{ marginTop: 12, paddingTop: 12, borderTop: "1px solid var(--border)" }}
              >
                This happens regularly, with real money. Slow, manual, or no compensation each
                time:
              </div>
              <div style={{ display: "flex", flexDirection: "column", gap: 6, marginTop: 8 }}>
                <div style={{ fontSize: 12 }}>
                  <b style={{ color: "var(--text)" }}>Binance, Oct 2025</b>{" "}
                  <span style={{ color: "var(--text-muted)" }}>: ~$19B liquidated</span>
                </div>
                <div style={{ fontSize: 12 }}>
                  <b style={{ color: "var(--text)" }}>Vesu, Sept 2026</b>{" "}
                  <span style={{ color: "var(--text-muted)" }}>: $3M, 2-min bad feed</span>
                </div>
                <div style={{ fontSize: 12 }}>
                  <b style={{ color: "var(--text)" }}>Edel Finance, Jul 2026</b>{" "}
                  <span style={{ color: "var(--text-muted)" }}>: $403K, tokenized stocks</span>
                </div>
                <div style={{ fontSize: 12 }}>
                  <b style={{ color: "var(--text)" }}>Aave CAPO, Mar 2026</b>{" "}
                  <span style={{ color: "var(--text-muted)" }}>: ~$26M</span>
                </div>
              </div>
            </div>
            <div className="cov-group not-covered">
              <h3>Not covered</h3>
              <div className="note">Disclosed upfront, not hidden</div>
              <ul>
                <li>Gap at market open: closed market, no price to verify</li>
                <li>Issuer freezes the stock: collateral itself is stuck</li>
              </ul>
            </div>
          </div>
          <button className="details-link" onClick={() => setShowDetails(true)}>
            How it works, in full →
          </button>
        </div>
      </section>

      <section className="example">
        <div className="wrap">
          <h2>What a liquidation actually looks like</h2>
          <div className="sub">Worked example, real parameters.</div>
          <div className="example-box">
            <div className="step">
              <div className="n">1</div>
              <div>
                Deposit $10,000 in AAPLx and borrow $4,000 USDC. That's 40% LTV, the maximum
                allowed.
              </div>
            </div>
            <div className="step">
              <div className="n">2</div>
              <div>Price falls, and your LTV crosses 50%, so liquidation can start.</div>
            </div>
            <div className="step">
              <div className="n">3</div>
              <div>
                Only up to 25% of your debt ($1,000) is liquidated at once, protecting the rest of
                your position, at a 1–5% bonus that depends on how stressed the pool is.
              </div>
            </div>
            <div className="step">
              <div className="n">4</div>
              <div>Only past 95% LTV can the full position go at once.</div>
            </div>
            <div className="step">
              <div className="n">5</div>
              <div>
                If any of that liquidation happened on bad data, you're repaid 1:1, bonus
                included.
              </div>
            </div>
          </div>
        </div>
      </section>

      <footer>
        <div className="wrap">
          <p>
            <img src="/logo.png" style={{ height: 16, verticalAlign: -3, marginRight: 6 }} />
            SAFU Credit is built by SAFU, the same protocol behind SAFU Staking.
          </p>
          <div className="links">
            <a href="https://safustaking.com" target="_blank" rel="noreferrer">
              safustaking.com
            </a>
            <a href="https://github.com/mrkanchwala/safucredit" target="_blank" rel="noreferrer">
              GitHub
            </a>
          </div>
        </div>
      </footer>
    </>
  );
}
