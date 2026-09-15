export function HowItWorks({ onBack }: { onBack: () => void }) {
  return (
    <div className="details-layout">
      <aside>
        <button className="back" onClick={onBack}>
          <img src="/logo.png" alt="" />← Back to app
        </button>
        <nav>
          <a href="#mechanism">
            <b>01</b>How it works
          </a>
          <a href="#numbers">
            <b>02</b>Rates &amp; returns
          </a>
          <a href="#liquidation">
            <b>03</b>Liquidation mechanics
          </a>
          <a href="#coverage">
            <b>04</b>Covered / not covered
          </a>
          <a href="#built">
            <b>05</b>Built by SAFU
          </a>
        </nav>
      </aside>
      <main>
        <div className="slide" id="mechanism">
          <div className="tag">// 01</div>
          <h2>How it works</h2>
          <p>
            Deposit tokenized stock as collateral and borrow USDC against it, the same as any
            lending market. The real difference shows up when a price feed lies to the
            liquidation engine.
          </p>
          <p>
            <b>This happens regularly, with real money.</b> Wrongful liquidations from bad price
            data are a recurring problem:
          </p>
          <table className="mini-table">
            <tbody>
              <tr>
                <th>Event</th>
                <th>What happened</th>
              </tr>
              <tr>
                <td>Binance, Oct 2025</td>
                <td>
                  ~$19B liquidated on a self-priced collateral gap. Compensation manual, decided
                  weeks later.
                </td>
              </tr>
              <tr>
                <td>Vesu, Sept 2026</td>
                <td>$3M lost across 47 positions from a 2-minute bad price feed.</td>
              </tr>
              <tr>
                <td>Edel Finance, July 2026</td>
                <td>
                  $403K in tokenized-stock lending, the same category as this product. A wrapped
                  stock's rate inflated about 78x.
                </td>
              </tr>
              <tr>
                <td>Aave CAPO, Mar 2026</td>
                <td>~$26M lost, showing even a major, careful protocol wasn't immune.</td>
              </tr>
            </tbody>
          </table>
          <div className="step-list">
            <div className="step">
              <div className="n">1</div>
              <div>
                Every liquidation is checked against a second, delayed reference price after the
                fact.
              </div>
            </div>
            <div className="step">
              <div className="n">2</div>
              <div>
                If the two prices agree, the liquidation stands: it was real, so{" "}
                <b>nothing is paid</b>.
              </div>
            </div>
            <div className="step">
              <div className="n">3</div>
              <div>
                If they disagree, the liquidation is ruled wrongful and you're repaid 1:1,
                automatically, on-chain. <b>No vote, no appeal, no human decides.</b>
              </div>
            </div>
          </div>
        </div>

        <div className="slide" id="numbers">
          <div className="tag">// 02</div>
          <h2>Rates &amp; returns</h2>
          <p>
            <b>Borrowers</b> pay a variable rate that rises with how much of the market's USDC is
            borrowed, priced slightly above comparable markets like Kamino, since a slice of that
            rate funds the payback pool.
          </p>
          <p>
            <b>Lenders</b> supplying USDC earn that borrower interest directly, same as any
            lending market.
          </p>
          <p>
            <b>Backers</b> who fund the payback pool take 15% of borrower interest as their share,
            lenders keep 85%. Backers carry real risk for it, since their capital pays for it when
            wrongful liquidations are frequent.
          </p>
          <div className="pill-row">
            <div className="pill">
              <span className="v">40%</span>max LTV
            </div>
            <div className="pill">
              <span className="v">50%</span>liquidation starts
            </div>
            <div className="pill">
              <span className="v">1–5%</span>liquidation bonus
            </div>
            <div className="pill">
              <span className="v">~10%</span>backer return, good year
            </div>
            <div className="pill">
              <span className="v">−1.9%</span>backer return, bad year
            </div>
          </div>
        </div>

        <div className="slide" id="liquidation">
          <div className="tag">// 03</div>
          <h2>Liquidation mechanics</h2>
          <p>Liquidations happen gradually, so a single bad moment can't wipe out your whole position.</p>
          <table className="mini-table">
            <tbody>
              <tr>
                <th>LTV</th>
                <th>What happens</th>
              </tr>
              <tr>
                <td>Up to 50%</td>
                <td>Healthy, nothing happens</td>
              </tr>
              <tr>
                <td>50%–95%</td>
                <td>Up to 25% of your debt liquidated per event, 1–5% bonus to whoever liquidates</td>
              </tr>
              <tr>
                <td>Above 95%</td>
                <td>Full position can be liquidated at once</td>
              </tr>
            </tbody>
          </table>
          <p style={{ marginTop: 14 }}>
            If any of that liquidation turns out to be based on bad data, the bonus and the
            collateral you lost are both repaid.
          </p>
        </div>

        <div className="slide" id="coverage">
          <div className="tag">// 04</div>
          <h2>Covered / not covered</h2>
          <table className="mini-table">
            <tbody>
              <tr>
                <th>Situation</th>
                <th>What happens</th>
                <th>Costs SAFU anything?</th>
              </tr>
              <tr>
                <td>Slow wrong-price drift</td>
                <td>Liquidated, then repaid 1:1</td>
                <td>
                  <b>Yes</b>, a real payout
                </td>
              </tr>
              <tr>
                <td>Single price spike</td>
                <td>Blocked before it can liquidate you</td>
                <td>No, never liquidated</td>
              </tr>
              <tr>
                <td>Stock split</td>
                <td>Held until repriced correctly</td>
                <td>No, never liquidated</td>
              </tr>
              <tr>
                <td>Unannounced split</td>
                <td>Frozen until acknowledged</td>
                <td>No, never liquidated</td>
              </tr>
              <tr>
                <td>Genuine price crash</td>
                <td>Liquidated, nothing paid. On-chain denial shown.</td>
                <td>No, correct outcome</td>
              </tr>
              <tr>
                <td>Gap at market open</td>
                <td>Not covered. Closed market, no price to verify.</td>
                <td>N/A, disclosed at deposit</td>
              </tr>
              <tr>
                <td>Issuer freezes the stock</td>
                <td>Market halts. Nothing liquidated or paid.</td>
                <td>N/A, collateral itself is stuck</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div className="slide" id="built">
          <div className="tag">// 05</div>
          <h2>Built by SAFU</h2>
          <p>
            SAFU Credit runs on the same deterministic, no-vote payback logic as SAFU Staking,
            SAFU's live protocol on Stellar.
          </p>
          <p>
            <a href="https://safustaking.com" target="_blank" rel="noreferrer" style={{ color: "var(--text)" }}>
              safustaking.com
            </a>{" "}
            &nbsp;·&nbsp;{" "}
            <a
              href="https://github.com/mrkanchwala/safucredit"
              target="_blank"
              rel="noreferrer"
              style={{ color: "var(--text)" }}
            >
              GitHub
            </a>
          </p>
        </div>
      </main>
    </div>
  );
}
