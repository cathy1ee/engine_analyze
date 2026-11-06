# Options engine

A paper-only research engine in Rust. For any stock it combines news, implied
volatility and factor signals, picks a defined-risk option structure by expected
value after costs, runs it through a risk manager and records a simulated fill.

**This is not a live trading system.** It contains no code that can reach a real
broker, and its forecasts have not been validated. The bundled data in
`data/sample` is synthetic, so any trade it shows means nothing about real
markets. Nothing here is investment advice.

## Run it

```bash
cargo run --release -- analyze INTC --theme blue-wave
cargo run --release -- scan
cargo run --release -- analyze NVDA --paper
cargo run --release -- exits
cargo run --release -- exits --paper
cargo run --release -- positions
cargo test
```

Create a file named `KILL_SWITCH` in the project folder to block all new trades.
Exits still work with it on, because closing a position reduces risk.

## Exits

`engine exits` previews what would close, and `engine exits --paper` records it.
Run it after each new data update, because exits are only checked when it runs.

| Rule | Closes when |
|---|---|
| Expired | The expiry date has passed; settles at that day's close |
| Stop loss | The loss reaches 50% of the risk taken |
| Take profit | The profit reaches 50% of the risk taken |
| Time exit | 7 or fewer days remain |

Exits use the prices you could actually get: sell at the bid, buy at the ask, pay
commission, and include any hedge. A position with no price is held, not guessed.
A ticker can't be re-entered on the day it closes. `engine positions` compares
realized PnL with the forecast EV, overall and for each theme.

To simulate time on sample data, write later snapshots of the same history:

```bash
cargo run --release -- gen-sample --data /tmp/day10 --advance 10
cargo run --release -- exits --paper --data /tmp/day10
```

## Pipeline

| Stage | File | Job |
|---|---|---|
| Data | `src/data.rs` | Loads prices, option chains, news and earnings dates |
| Signals | `src/signals.rs` | IV, IV rank, skew, realized vol, alpha and beta |
| News | `src/news.rs` | Removes repeats, blocks future news, scores direction |
| Selection | `src/selection.rs` | Forecasts, builds structures, ranks by EV after costs |
| Risk | `src/risk.rs` | Enforces limits, sizes trades, proposes hedges |
| Broker | `src/broker.rs` | Simulates fills at the bid and ask |
| Exits | `src/exits.rs` | Decides when an open trade should close |
| Journal | `src/journal.rs` | Records trades and exits, and marks positions at exit prices |

## Checking the news scorer

Grade headlines yourself, then measure how often the scorer agrees. The scorer's
answers stay hidden while you grade, and stopping part way loses nothing.

```bash
cargo run --release -- news-grade
cargo run --release -- news-check
```

`data/news_practice.csv` has 60 invented headlines about fictional companies, many
written to be tricky. Once real snapshots exist, grade those too:

```bash
cargo run --release -- news-grade --source data/live/latest/news.csv
```

Compare agreement with the "always guessing" line, which is the score to beat. Your
grades stay in `journal/news_labels.csv`, outside Git. This shows whether the scorer
reads headlines the way you do, not whether those headlines predict returns.

## Default limits

On a $100,000 paper account: 1% risk per trade, 4% per sector group, 10% in total,
and new trades stop once open losses plus today's closed losses reach 2%.
The account value used for these limits moves with realized PnL. Only defined-risk structures are built.

## Using real data from IBKR

`engine fetch` reads market data from TWS or IB Gateway on this Mac. It refuses
IBKR's live ports and contains no order code.

1. Install TWS or IB Gateway and log in to your **paper** account.
2. In TWS, open Global Configuration, then API, then Settings. Turn on socket
   connections and **Read-Only API**, and keep the socket port at 7497.
3. For current prices, subscribe to market data on your live account and share it
   with the paper account. Otherwise use `--delayed`.
4. After the market closes, run the daily routine:

```bash
cargo run --release -- fetch
cargo run --release -- exits --paper --data data/live/latest
cargo run --release -- scan --data data/live/latest
```

Each run saves a dated folder in `data/live`, and `data/live/latest` points to the
newest. Snapshots stay out of Git because vendors usually restrict sharing their data.

| Option | Effect |
|---|---|
| `fetch INTC AMD` | Fetches only those tickers, plus anything held in open trades |
| `fetch --delayed` | Uses free delayed data, and every report shows a warning |
| `fetch --port 4002` | Connects on a specific paper port; 7497 and 4002 are tried by default |

To stay under IBKR's 100 market data lines, options are requested 40 at a time, so
a full watchlist takes a few minutes. Only monthly expiries and strikes within 20%
of the stock are fetched, plus the exact contracts in any open trade.

- **IV rank** shows `n/a` until 60 days of snapshots have built up.
- **Earnings dates** aren't supplied, so the engine assumes no volatility edge for
  those stocks rather than risk ignoring an earnings jump.
- **News** comes from the providers your account can use through the API, and uses
  each story's publish time. Anything after the close appears in the next snapshot,
  because the option quotes in this one came before it.

## Using other data

Point `--data` at a folder containing these CSV files:

| File | Columns |
|---|---|
| `prices.csv` | `date,ticker,close,atm_iv` |
| `options.csv` | `ticker,expiry_days,strike,right,bid,ask,volume,open_interest` |
| `news.csv` | `available_at,ticker,source,headline` |
| `earnings.csv` | `ticker,days_to_earnings` |

`available_at` is when the story reached you, not when it was published.

## Known limits

- Stops are only checked when `exits` runs, and a gap can take a loss well past 50%.
- Exit thresholds are judgement calls, and there is no exit on a news reversal yet.
- Expiry settles in cash at intrinsic value, with no assignment or stock delivery.
- Pricing is European Black-Scholes, so early exercise and dividends are ignored.
- Expected value assumes a lognormal price at expiry, which understates jumps.
- Forecast weights in `src/selection.rs` are judgement calls, not fitted values.
- Picking the best of many structures flatters the top result's estimated edge.
- News grades reflect one person's judgement, and practice headlines are not a random sample of real news.
- News scoring is a keyword baseline, and `NewsScorer` is where a better model plugs in.
- Themes only tag news and journal rows. They do not change forecasts.
- There is no backtester yet, because it needs historical options data.
- Earnings dates have no live source yet.
- The IBKR connection itself has not been run against a real TWS or IB Gateway yet.
