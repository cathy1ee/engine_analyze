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
cargo run --release -- positions
cargo test
```

Create a file named `KILL_SWITCH` in the project folder to block all new trades.

## Pipeline

| Stage | File | Job |
|---|---|---|
| Data | `src/data.rs` | Loads prices, option chains, news and earnings dates |
| Signals | `src/signals.rs` | IV, IV rank, skew, realized vol, alpha and beta |
| News | `src/news.rs` | Removes repeats, blocks future news, scores direction |
| Selection | `src/selection.rs` | Forecasts, builds structures, ranks by EV after costs |
| Risk | `src/risk.rs` | Enforces limits, sizes trades, proposes hedges |
| Broker | `src/broker.rs` | Simulates fills at the bid and ask |
| Journal | `src/journal.rs` | Records each trade and marks it at exit prices |

## Default limits

On a $100,000 paper account: 1% risk per trade, 4% per sector group, 10% in total,
and new trades stop once open losses reach 2%. Only defined-risk structures are built.

## Using real data

Point `--data` at a folder containing these CSV files:

| File | Columns |
|---|---|
| `prices.csv` | `date,ticker,close,atm_iv` |
| `options.csv` | `ticker,expiry_days,strike,right,bid,ask,volume,open_interest` |
| `news.csv` | `available_at,ticker,source,headline` |
| `earnings.csv` | `ticker,days_to_earnings` |

`available_at` is when the story reached you, not when it was published.

## Known limits

- Positions never close: there is no exit logic, and expiry is not tracked yet.
- Pricing is European Black-Scholes, so early exercise and dividends are ignored.
- Expected value assumes a lognormal price at expiry, which understates jumps.
- Forecast weights in `src/selection.rs` are judgement calls, not fitted values.
- Picking the best of many structures flatters the top result's estimated edge.
- News scoring is a keyword baseline, and `NewsScorer` is where a better model plugs in.
- Themes only tag news and journal rows. They do not change forecasts.
- There is no backtester yet, because it needs historical options data.
