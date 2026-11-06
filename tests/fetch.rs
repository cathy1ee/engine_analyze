//! Snapshot saving with a fake data source, and IBKR rules with real tick types.
use engine::analyze::{analyze, Context};
use engine::data::{load, OptionQuote};
use engine::dates::{days_from_civil, format_date};
use engine::fetch::{run, Closes, Config, HeldLeg, Source, SourceError, Story};
use engine::ibkr::{
    clean_headline, gather_news, pick_expiries, pick_strikes, ports_to_try, quote_from_ticks,
    wanted_contracts, Mode,
};
use engine::journal::{exits_path, held_legs, EXIT_HEADER, HEADER};
use engine::news::KeywordScorer;
use engine::pricing::{price, Inputs, Right, RATE};
use engine::risk::{Book, Limits};
use ibapi::contracts::tick_types::TickType;
use ibapi::market_data::realtime::{TickPrice, TickSize, TickTypes};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn temp() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("engine-fetch-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn friday() -> i64 {
    days_from_civil(2026, 11, 6)
}

/// That Friday at 22:00 UTC, after the close, plus any extra days.
fn now(extra_days: i64) -> i64 {
    (friday() + extra_days) * 86400 + 22 * 3600
}

fn history(last_day: i64, start: f64) -> Vec<(String, f64)> {
    (0..120)
        .map(|i| {
            (
                format_date(last_day - (119 - i)),
                start * (1.0 + 0.01 * ((i * 7 % 11) as f64 - 5.0) / 5.0),
            )
        })
        .collect()
}

fn fair_chain(spot: f64) -> Vec<OptionQuote> {
    let mut out = vec![];
    for strike in [85.0, 90.0, 95.0, 100.0, 105.0, 110.0, 115.0] {
        for right in [Right::Call, Right::Put] {
            let i = Inputs {
                spot,
                strike,
                t: 35.0 / 365.0,
                r: RATE,
                q: 0.0,
                vol: 0.35,
            };
            let p = price(right, &i);
            out.push(OptionQuote {
                expiry_days: 35,
                strike,
                right,
                bid: p - 0.02,
                ask: p + 0.02,
                volume: 900,
                open_interest: 2000,
            });
        }
    }
    out
}

struct Fake {
    last_day: i64,
    chain: Result<Vec<OptionQuote>, SourceError>,
    news: Result<Vec<Story>, SourceError>,
    held_seen: RefCell<Vec<HeldLeg>>,
}

impl Fake {
    fn healthy() -> Self {
        let story = Story {
            available_at: "2026-11-06T14:00:00Z".into(),
            ticker: "TST".into(),
            source: "BRFG".into(),
            headline: "TST beats estimates, raises guidance".into(),
        };
        Fake {
            last_day: friday(),
            chain: Ok(fair_chain(100.0)),
            news: Ok(vec![story]),
            held_seen: RefCell::default(),
        }
    }
}

impl Source for Fake {
    fn closes(&self, symbols: &[String]) -> Result<Closes, SourceError> {
        let mut out = Closes::new();
        for s in symbols {
            out.insert(s.clone(), history(self.last_day, 100.0));
        }
        Ok(out)
    }

    fn chain(
        &self,
        ticker: &str,
        _: f64,
        _: i64,
        held: &[&HeldLeg],
    ) -> Result<Vec<OptionQuote>, SourceError> {
        self.held_seen
            .borrow_mut()
            .extend(held.iter().map(|h| (*h).clone()));
        if ticker == "BAD" {
            return Err(SourceError::Skip("no standard options listed".into()));
        }
        self.chain.clone()
    }

    fn news(&self, _: &[String], _: i64) -> Result<Vec<Story>, SourceError> {
        self.news.clone()
    }
}

fn config(dir: &Path) -> Config {
    Config {
        dir: dir.to_path_buf(),
        tickers: vec!["TST".into()],
        held: vec![],
        feed: "ibkr".into(),
    }
}

#[test]
fn a_snapshot_runs_through_the_whole_engine() {
    let dir = temp();
    let r = run(&Fake::healthy(), &config(&dir), now(0)).unwrap();
    assert_eq!(r.date, "2026-11-06");
    assert_eq!((r.chains, r.quotes, r.stories), (1, 14, 1));

    let m = load(&dir.join("latest")).unwrap();
    assert!(!m.is_sample && m.earnings_unknown);
    assert_eq!(m.source(), "ibkr");

    let (l, b) = (Limits::default(), Book::default());
    let ctx = Context {
        market: &m,
        scorer: &KeywordScorer,
        theme: None,
        limits: &l,
        book: &b,
        kill_switch: false,
    };
    let a = analyze(&ctx, "TST").unwrap();
    assert!(a.considered > 0, "liquid quotes should produce structures");
    assert_eq!(a.news.stories.len(), 1);
    assert_eq!(
        a.signals.iv_rank, None,
        "one day of IV isn't enough history"
    );
    assert!(a.signals.earnings_unknown);
    assert_eq!(
        a.forecast.vol, a.signals.atm_iv,
        "no volatility edge without an earnings calendar"
    );
}

#[test]
fn iv_history_builds_up_across_snapshots() {
    let dir = temp();
    std::fs::create_dir_all(&dir).unwrap();
    let mut hist = String::from("date,ticker,atm_iv\n");
    for i in 1..70 {
        hist.push_str(&format!(
            "{},TST,{:.4}\n",
            format_date(friday() - i),
            0.2 + i as f64 / 500.0
        ));
    }
    std::fs::write(dir.join("iv_history.csv"), hist).unwrap();
    run(&Fake::healthy(), &config(&dir), now(0)).unwrap();

    let m = load(&dir.join("latest")).unwrap();
    let (l, b) = (Limits::default(), Book::default());
    let ctx = Context {
        market: &m,
        scorer: &KeywordScorer,
        theme: None,
        limits: &l,
        book: &b,
        kill_switch: false,
    };
    assert!(analyze(&ctx, "TST").unwrap().signals.iv_rank.is_some());
    assert_eq!(
        std::fs::read_to_string(dir.join("iv_history.csv"))
            .unwrap()
            .lines()
            .count(),
        71
    );
}

#[test]
fn news_is_saved_with_its_publish_and_fetch_times() {
    let dir = temp();
    run(&Fake::healthy(), &config(&dir), now(0)).unwrap();
    let text = std::fs::read_to_string(dir.join("latest/news.csv")).unwrap();
    let row = text.lines().nth(1).unwrap();
    assert!(
        row.starts_with("2026-11-06T14:00:00Z,TST,BRFG,\"TST beats"),
        "{row}"
    );
    assert!(row.ends_with("2026-11-06T22:00:00Z"), "{row}");
}

#[test]
fn missing_news_access_still_saves_a_snapshot_with_a_warning() {
    let dir = temp();
    let fake = Fake {
        news: Err(SourceError::Skip("no API news providers".into())),
        ..Fake::healthy()
    };
    let r = run(&fake, &config(&dir), now(0)).unwrap();
    assert_eq!(r.stories, 0);
    assert!(r.warnings.iter().any(|w| w.contains("no news")));
    assert!(load(&dir.join("latest")).is_ok());
}

#[test]
fn a_lost_connection_keeps_the_previous_snapshot() {
    let dir = temp();
    run(&Fake::healthy(), &config(&dir), now(0)).unwrap();
    let fake = Fake {
        last_day: friday() + 3,
        chain: Err(SourceError::Fatal("connection lost".into())),
        ..Fake::healthy()
    };
    assert_eq!(
        run(&fake, &config(&dir), now(3)).unwrap_err(),
        "connection lost"
    );
    assert_eq!(load(&dir.join("latest")).unwrap().as_of_date, "2026-11-06");
    assert!(!dir.join(format_date(friday() + 3)).exists());
}

#[test]
fn one_failing_chain_is_skipped_but_none_at_all_is_an_error() {
    let dir = temp();
    let mut cfg = config(&dir);
    cfg.tickers.push("BAD".into());
    let r = run(&Fake::healthy(), &cfg, now(0)).unwrap();
    assert_eq!(r.chains, 1);
    assert!(r.warnings.iter().any(|w| w.starts_with("skipped BAD")));

    let dir2 = temp();
    let empty = Fake {
        chain: Ok(vec![]),
        ..Fake::healthy()
    };
    assert!(run(&empty, &config(&dir2), now(0)).is_err());
    assert!(!dir2.join("latest").exists());
}

#[test]
fn weekend_runs_date_the_snapshot_by_the_last_session() {
    let dir = temp();
    assert_eq!(
        run(&Fake::healthy(), &config(&dir), now(1)).unwrap().date,
        "2026-11-06"
    );
}

#[test]
fn a_midday_run_warns_that_closes_may_not_be_final() {
    let dir = temp();
    let r = run(
        &Fake::healthy(),
        &config(&dir),
        friday() * 86400 + 15 * 3600,
    )
    .unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("still be open")));
}

#[test]
fn live_ports_are_refused_and_paper_ports_are_the_default() {
    assert_eq!(ports_to_try(None).unwrap(), vec![7497, 4002]);
    assert_eq!(ports_to_try(Some(4002)).unwrap(), vec![4002]);
    assert!(ports_to_try(Some(7496)).unwrap_err().contains("live"));
    assert!(ports_to_try(Some(4001)).unwrap_err().contains("live"));
    assert_eq!(Mode::Delayed.feed(), "ibkr-delayed");
}

#[test]
fn the_ibkr_source_has_no_order_code() {
    let text = std::fs::read_to_string("src/ibkr.rs").unwrap();
    for word in [
        "place_order",
        "submit_order",
        "order(",
        "ibapi::orders",
        "Action::",
    ] {
        assert!(!text.contains(word), "found {word}");
    }
}

#[test]
fn only_monthly_expiries_in_the_window_are_used() {
    let list: Vec<String> = [
        "20261113", "20261120", "20261127", "20261218", "20270115", "20270319", "bad",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let got = pick_expiries(&list, friday());
    assert_eq!(
        got,
        vec![
            ("20261120".to_string(), 14),
            ("20261218".to_string(), 42),
            ("20270115".to_string(), 70)
        ]
    );
}

#[test]
fn a_holiday_thursday_monthly_is_kept() {
    // Good Friday 2027 is March 26, so test the shape on a third-week Thursday.
    let list = vec!["20261216".to_string(), "20261217".to_string()];
    assert_eq!(
        pick_expiries(&list, friday()),
        vec![("20261217".to_string(), 41)]
    );
}

#[test]
fn strikes_stay_near_spot_on_a_round_grid() {
    let listed: Vec<f64> = (100..=300).map(|i| i as f64 * 0.5).collect();
    let got = pick_strikes(&listed, 100.0);
    assert_eq!(got.first(), Some(&80.0));
    assert_eq!(got.last(), Some(&120.0));
    assert!(got.contains(&90.0) && got.contains(&102.5) && !got.contains(&101.0));
    // A small move in the stock doesn't change the grid, so held strikes stay listed.
    assert!(pick_strikes(&listed, 98.0).contains(&102.5));
}

#[test]
fn headlines_lose_their_metadata() {
    assert_eq!(
        clean_headline("{A:800015:L:en:K:n/a:C:0.9}!Intel beats"),
        "Intel beats"
    );
    assert_eq!(clean_headline("  Plain headline "), "Plain headline");
}

fn px(tick_type: TickType, price: f64) -> TickTypes {
    TickTypes::Price(TickPrice {
        tick_type,
        price,
        attributes: Default::default(),
    })
}

fn sz(tick_type: TickType, size: f64) -> TickTypes {
    TickTypes::Size(TickSize { tick_type, size })
}

#[test]
fn ticks_become_a_quote_with_the_right_open_interest() {
    let ticks = [
        px(TickType::Bid, 2.0),
        px(TickType::Ask, 2.1),
        sz(TickType::Volume, 350.0),
        sz(TickType::OptionCallOpenInterest, 1200.0),
        sz(TickType::OptionPutOpenInterest, 900.0),
    ];
    let call = quote_from_ticks(&ticks, 35, 100.0, Right::Call).unwrap();
    assert_eq!(
        (call.bid, call.ask, call.volume, call.open_interest),
        (2.0, 2.1, 350, 1200)
    );
    assert_eq!(
        quote_from_ticks(&ticks, 35, 100.0, Right::Put)
            .unwrap()
            .open_interest,
        900
    );
}

#[test]
fn delayed_ticks_count_too() {
    let ticks = [
        px(TickType::DelayedBid, 1.0),
        px(TickType::DelayedAsk, 1.2),
        sz(TickType::DelayedVolume, 50.0),
    ];
    assert_eq!(
        quote_from_ticks(&ticks, 35, 100.0, Right::Put)
            .unwrap()
            .volume,
        50
    );
}

#[test]
fn missing_negative_and_crossed_prices_are_rejected() {
    assert!(quote_from_ticks(&[px(TickType::Bid, 2.0)], 35, 100.0, Right::Call).is_none());
    assert!(quote_from_ticks(
        &[px(TickType::Bid, -1.0), px(TickType::Ask, 2.0)],
        35,
        100.0,
        Right::Call
    )
    .is_none());
    assert!(quote_from_ticks(
        &[px(TickType::Bid, 1.0), px(TickType::Ask, -1.0)],
        35,
        100.0,
        Right::Call
    )
    .is_none());
    assert!(quote_from_ticks(
        &[px(TickType::Bid, 2.0), px(TickType::Ask, 1.9)],
        35,
        100.0,
        Right::Call
    )
    .is_none());
}

fn held(ticker: &str, expiry_day: i64, strike: f64, right: Right) -> HeldLeg {
    HeldLeg {
        ticker: ticker.into(),
        expiry_day,
        strike,
        right,
    }
}

#[test]
fn open_trades_keep_their_exact_contracts_in_the_request() {
    let (today, expiry) = (friday(), friday() + 14);
    let expiries = vec![("20261120".to_string(), 14)];
    let off_grid = held("TST", expiry, 101.0, Right::Put);
    let on_grid = held("TST", expiry, 100.0, Right::Call);
    let other_expiry = held("TST", friday() + 20, 100.0, Right::Call);
    let expired = held("TST", friday() - 1, 100.0, Right::Call);
    let got = wanted_contracts(
        &expiries,
        &[100.0],
        &[&off_grid, &on_grid, &other_expiry, &expired],
        today,
    );
    assert_eq!(
        got.len(),
        4,
        "two on the grid, plus two held that weren't: {got:?}"
    );
    assert!(got.contains(&("20261120".to_string(), 14, 101.0, Right::Put)));
    assert!(got.contains(&("20261126".to_string(), 20, 100.0, Right::Call)));
}

#[test]
fn held_tickers_are_fetched_even_when_left_off_the_command() {
    let dir = temp();
    let mut cfg = config(&dir);
    cfg.held = vec![
        held("OLD", friday() + 14, 100.0, Right::Call),
        held("TST", friday() + 14, 95.0, Right::Put),
    ];
    let fake = Fake::healthy();
    let r = run(&fake, &cfg, now(0)).unwrap();
    assert_eq!(r.chains, 2);
    assert_eq!(*fake.held_seen.borrow(), cfg.held);
}

#[test]
fn closed_trades_are_not_held() {
    let dir = temp();
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("trades.csv");
    let row = |t: &str| {
        format!("2026-11-06,ibkr,{t},chips,,0,0,0.3,0.3,bear put spread,+P100@14|-P95@14,1,2.0000,200.00,15.00,,,")
    };
    std::fs::write(&path, format!("{HEADER}\n{}\n{}\n", row("AAA"), row("BBB"))).unwrap();
    std::fs::write(
        exits_path(&path),
        format!("{EXIT_HEADER}\n1,2026-11-10,stop loss,4,0.9000,,-110.00\n"),
    )
    .unwrap();
    let got = held_legs(&path).unwrap();
    assert_eq!(
        got,
        vec![
            held("BBB", friday() + 14, 100.0, Right::Put),
            held("BBB", friday() + 14, 95.0, Right::Put)
        ]
    );
}

#[test]
fn a_headline_with_line_breaks_cant_break_a_snapshot() {
    let dir = temp();
    let story = Story {
        available_at: "2026-11-06T14:00:00Z".into(),
        ticker: "TST".into(),
        source: "BR\nFG".into(),
        headline: "  Maker beats\r\nestimates\tagain ".into(),
    };
    let fake = Fake {
        news: Ok(vec![story]),
        ..Fake::healthy()
    };
    run(&fake, &config(&dir), now(0)).unwrap();
    let m = load(&dir.join("latest")).unwrap();
    assert_eq!(m.news[0].headline, "Maker beats estimates again");
    assert_eq!(m.news[0].source, "BR FG");
}

fn story(ticker: &str) -> Story {
    Story {
        available_at: "2026-11-06T14:00:00Z".into(),
        ticker: ticker.into(),
        source: "BRFG".into(),
        headline: "Maker beats estimates".into(),
    }
}

#[test]
fn one_failed_ticker_keeps_the_news_from_the_others() {
    let tickers = vec!["AAA".to_string(), "BAD".to_string(), "CCC".to_string()];
    let got = gather_news(&tickers, |t| {
        if t == "BAD" {
            Err("timed out".to_string())
        } else {
            Ok(vec![story(t)])
        }
    })
    .unwrap();
    assert_eq!(
        got.iter().map(|s| s.ticker.as_str()).collect::<Vec<_>>(),
        ["AAA", "CCC"]
    );
}

#[test]
fn news_only_fails_when_every_ticker_fails() {
    let tickers = vec!["AAA".to_string(), "BBB".to_string()];
    let got = gather_news(&tickers, |_| Err("no access".to_string()));
    assert_eq!(got, Err(SourceError::Skip("no access".to_string())));
}
