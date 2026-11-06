//! Runs the fetcher against a fake Alpaca that replies in the documented formats.
use engine::analyze::{analyze, Context};
use engine::data::load;
use engine::dates::{days_from_civil, format_date};
use engine::fetch::{run, Config, Feed, Http, HttpError};
use engine::news::KeywordScorer;
use engine::pricing::{price, Inputs, Right, RATE};
use engine::risk::{Book, Limits};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn temp() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("engine-fetch-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Friday 2026-11-06 at 22:00 UTC, after the close.
fn now(day_offset: i64) -> i64 {
    (days_from_civil(2026, 11, 6) + day_offset) * 86400 + 22 * 3600
}

fn bars(last_day: i64, n: i64, start: f64) -> Value {
    let rows: Vec<Value> = (0..n)
        .map(|i| {
            let d = last_day - (n - 1 - i);
            let c = start * (1.0 + 0.01 * ((i * 7 % 11) as f64 - 5.0) / 5.0);
            json!({"t": format!("{}T05:00:00Z", format_date(d)), "o": c, "h": c, "l": c, "c": c, "v": 1000, "n": 10, "vw": c})
        })
        .collect();
    Value::Array(rows)
}

struct Fake {
    replies: Vec<(&'static str, Result<String, HttpError>)>,
    calls: RefCell<HashMap<String, usize>>,
    urls: RefCell<Vec<String>>,
}

impl Fake {
    fn new(replies: Vec<(&'static str, Result<String, HttpError>)>) -> Self {
        Fake {
            replies,
            calls: RefCell::new(HashMap::new()),
            urls: RefCell::new(vec![]),
        }
    }
}

impl Http for Fake {
    /// The first rule matching the URL wins, so page 2 rules go first.
    fn get(&self, url: &str) -> Result<String, HttpError> {
        self.urls.borrow_mut().push(url.to_string());
        for (needle, reply) in &self.replies {
            if url.contains(needle) {
                *self
                    .calls
                    .borrow_mut()
                    .entry(needle.to_string())
                    .or_default() += 1;
                return reply.clone();
            }
        }
        Err(HttpError::Status(404, url.to_string()))
    }
}

fn contract(sym: &str, right: &str, strike: f64, expiry: &str, oi: Value) -> Value {
    json!({"symbol": sym, "root_symbol": "TST", "type": right, "strike_price": strike.to_string(),
           "expiration_date": expiry, "open_interest": oi, "multiplier": "100"})
}

fn snap(bid: f64, ask: f64, vol: u64) -> Value {
    json!({"latestQuote": {"bp": bid, "ap": ask, "bs": 1, "as": 1}, "dailyBar": {"v": vol}})
}

/// A realistic chain for TST at $100 with 35-day options.
fn fair_chain(today: i64) -> (Value, Value) {
    let expiry = format_date(today + 35);
    let (mut contracts, mut snaps) = (vec![], serde_json::Map::new());
    for strike in [85.0, 90.0, 95.0, 100.0, 105.0, 110.0, 115.0] {
        for (right, letter, r) in [(Right::Call, "C", "call"), (Right::Put, "P", "put")] {
            let i = Inputs {
                spot: 100.0,
                strike,
                t: 35.0 / 365.0,
                r: RATE,
                q: 0.0,
                vol: 0.35,
            };
            let p = price(right, &i);
            let sym = format!("TST{letter}{strike}");
            contracts.push(contract(&sym, r, strike, &expiry, json!("2000")));
            snaps.insert(sym, snap(p - 0.02, p + 0.02, 900));
        }
    }
    (
        json!({"option_contracts": contracts, "next_page_token": null}),
        json!({"snapshots": snaps, "next_page_token": null}),
    )
}

fn healthy(today: i64) -> Fake {
    let (contracts, snaps) = fair_chain(today);
    let bars_json = json!({"bars": {
        "TST": bars(today, 120, 100.0), "SPY": bars(today, 120, 500.0), "XLE": bars(today, 120, 90.0),
        "TAN": bars(today, 120, 40.0), "XLU": bars(today, 120, 70.0), "SMH": bars(today, 120, 250.0)
    }, "next_page_token": null});
    let news = json!({"news": [
        {"id": 1, "headline": "TST beats estimates, raises guidance", "created_at": "2026-11-06T14:00:00.123Z",
         "updated_at": "2026-11-06T20:00:00Z", "symbols": ["TST", "OTHER"], "source": "benzinga"}
    ], "next_page_token": null});
    Fake::new(vec![
        ("/v2/stocks/bars", Ok(bars_json.to_string())),
        ("/v2/options/contracts", Ok(contracts.to_string())),
        ("/options/snapshots/TST", Ok(snaps.to_string())),
        ("/v1beta1/news", Ok(news.to_string())),
    ])
}

fn config(dir: &std::path::Path) -> Config {
    Config {
        dir: dir.to_path_buf(),
        tickers: vec!["TST".into()],
        feed: Feed::Indicative,
    }
}

#[test]
fn a_snapshot_runs_through_the_whole_engine() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    let r = run(&healthy(today), &config(&dir), now(0)).unwrap();
    assert_eq!(r.date, "2026-11-06");
    assert_eq!((r.chains, r.quotes, r.stories), (1, 14, 1));

    let m = load(&dir.join("latest")).unwrap();
    assert!(!m.is_sample && m.earnings_unknown);
    assert_eq!(m.source(), "indicative");
    assert_eq!(m.as_of_date, "2026-11-06");

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
    assert!(
        (a.signals.atm_iv - 0.35).abs() < 0.01,
        "{}",
        a.signals.atm_iv
    );
    assert!(a.considered > 0, "liquid quotes should produce structures");
    assert_eq!(a.news.stories.len(), 1);
}

#[test]
fn iv_rank_waits_for_enough_history_and_unknown_earnings_claim_no_vol_edge() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    run(&healthy(today), &config(&dir), now(0)).unwrap();
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
    let a = analyze(&ctx, "TST").unwrap();
    assert_eq!(a.signals.iv_rank, None);
    assert!(a.signals.earnings_unknown);
    assert_eq!(a.forecast.vol, a.signals.atm_iv);
}

#[test]
fn iv_history_builds_up_across_snapshots() {
    let dir = temp();
    std::fs::create_dir_all(&dir).unwrap();
    let today = days_from_civil(2026, 11, 6);
    let mut hist = String::from("date,ticker,atm_iv\n");
    for i in 1..70 {
        hist.push_str(&format!(
            "{},TST,{:.4}\n",
            format_date(today - i),
            0.2 + i as f64 / 500.0
        ));
    }
    std::fs::write(dir.join("iv_history.csv"), hist).unwrap();
    run(&healthy(today), &config(&dir), now(0)).unwrap();

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
    let saved = std::fs::read_to_string(dir.join("iv_history.csv")).unwrap();
    assert_eq!(
        saved.lines().count(),
        71,
        "today's IV is added to the history"
    );
}

#[test]
fn news_uses_first_publish_time_and_only_watched_tickers() {
    let dir = temp();
    run(
        &healthy(days_from_civil(2026, 11, 6)),
        &config(&dir),
        now(0),
    )
    .unwrap();
    let text = std::fs::read_to_string(dir.join("latest/news.csv")).unwrap();
    let rows: Vec<&str> = text.lines().skip(1).collect();
    assert_eq!(rows.len(), 1, "OTHER isn't watched: {text}");
    assert!(
        rows[0].starts_with("2026-11-06T14:00:00Z,TST,benzinga,"),
        "{}",
        rows[0]
    );
    assert!(rows[0].contains("\"TST beats estimates, raises guidance\""));
    assert!(rows[0].ends_with("2026-11-06T22:00:00Z"));
}

#[test]
fn pages_are_followed_with_an_encoded_token() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    let mut fake = healthy(today);
    let page1 = json!({"bars": {"SPY": bars(today, 120, 500.0), "SMH": bars(today, 120, 250.0)}, "next_page_token": "a+b="});
    let page2 = json!({"bars": {"TST": bars(today, 120, 100.0)}, "next_page_token": null});
    fake.replies.retain(|r| r.0 != "/v2/stocks/bars");
    fake.replies
        .insert(0, ("page_token=a%2Bb%3D", Ok(page2.to_string())));
    fake.replies
        .insert(1, ("/v2/stocks/bars", Ok(page1.to_string())));
    run(&fake, &config(&dir), now(0)).unwrap();
    assert!(load(&dir.join("latest"))
        .unwrap()
        .prices
        .contains_key("TST"));
}

#[test]
fn bad_contracts_and_quotes_are_left_out() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    let expiry = format_date(today + 35);
    let mut adjusted = contract("ADJ", "call", 100.0, &expiry, json!("9000"));
    adjusted["root_symbol"] = json!("TST1");
    let mut mini = contract("MINI", "call", 100.0, &expiry, json!("9000"));
    mini["multiplier"] = json!("10");
    let contracts = json!({"option_contracts": [
        contract("GOOD", "call", 100.0, &expiry, json!("2000")),
        contract("NOOI", "put", 100.0, &expiry, Value::Null),
        contract("CROSSED", "call", 105.0, &expiry, json!("2000")),
        contract("EMPTY", "call", 110.0, &expiry, json!("2000")),
        contract("UNQUOTED", "call", 115.0, &expiry, json!("2000")),
        adjusted, mini
    ], "next_page_token": null});
    let snaps = json!({"snapshots": {
        "GOOD": snap(4.0, 4.1, 300), "NOOI": snap(3.0, 3.1, 300), "CROSSED": snap(2.0, 1.9, 300),
        "EMPTY": snap(0.0, 0.0, 0), "ADJ": snap(4.0, 4.1, 300), "MINI": snap(4.0, 4.1, 300)
    }, "next_page_token": null});
    let mut fake = healthy(today);
    fake.replies
        .retain(|r| !r.0.contains("contracts") && !r.0.contains("snapshots"));
    fake.replies
        .push(("/v2/options/contracts", Ok(contracts.to_string())));
    fake.replies
        .push(("/options/snapshots/TST", Ok(snaps.to_string())));
    run(&fake, &config(&dir), now(0)).unwrap();

    let m = load(&dir.join("latest")).unwrap();
    let chain = &m.chains["TST"];
    assert_eq!(chain.len(), 2);
    let good = chain.iter().find(|q| q.right == Right::Call).unwrap();
    assert_eq!(
        (good.expiry_days, good.volume, good.open_interest),
        (35, 300, 2000)
    );
    assert_eq!(
        chain
            .iter()
            .find(|q| q.right == Right::Put)
            .unwrap()
            .open_interest,
        0
    );
}

#[test]
fn a_blocked_news_plan_still_saves_a_snapshot_with_a_warning() {
    let dir = temp();
    let mut fake = healthy(days_from_civil(2026, 11, 6));
    fake.replies.retain(|r| r.0 != "/v1beta1/news");
    fake.replies.push((
        "/v1beta1/news",
        Err(HttpError::Status(403, "/v1beta1/news".into())),
    ));
    let r = run(&fake, &config(&dir), now(0)).unwrap();
    assert_eq!(r.stories, 0);
    assert!(r.warnings.iter().any(|w| w.contains("no news")));
    assert!(load(&dir.join("latest")).is_ok());
}

#[test]
fn bad_keys_stop_the_fetch_and_keep_the_previous_snapshot() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    run(&healthy(today), &config(&dir), now(0)).unwrap();

    let mut fake = healthy(today + 3);
    fake.replies.retain(|r| r.0 != "/v2/options/contracts");
    fake.replies.insert(
        0,
        (
            "/v2/options/contracts",
            Err(HttpError::Status(401, "/v2/options/contracts".into())),
        ),
    );
    let e = run(&fake, &config(&dir), now(3)).unwrap_err();
    assert!(e.contains("rejected"), "{e}");
    assert_eq!(load(&dir.join("latest")).unwrap().as_of_date, "2026-11-06");
    assert!(!dir.join(format_date(today + 3)).exists());
}

#[test]
fn one_failing_chain_is_skipped_but_none_at_all_is_an_error() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    let mut cfg = config(&dir);
    cfg.tickers.push("SPY".into());
    let r = run(&healthy(today), &cfg, now(0)).unwrap();
    assert_eq!(r.chains, 1);

    let mut fake = healthy(today);
    fake.replies.retain(|r| !r.0.contains("contracts"));
    let dir2 = temp();
    assert!(run(&fake, &config(&dir2), now(0)).is_err());
    assert!(!dir2.join("latest").exists());
}

#[test]
fn weekend_runs_date_the_snapshot_by_the_last_session() {
    let dir = temp();
    let friday = days_from_civil(2026, 11, 6);
    let r = run(&healthy(friday), &config(&dir), now(1)).unwrap();
    assert_eq!(r.date, "2026-11-06");
    let m = load(&dir.join("latest")).unwrap();
    assert!(m.chains["TST"].iter().all(|q| q.expiry_days == 35));
}

#[test]
fn a_midday_run_warns_that_closes_may_not_be_final() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    let r = run(&healthy(today), &config(&dir), today * 86400 + 15 * 3600).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("still be open")));
}

#[test]
fn requests_use_the_right_feeds_and_never_touch_orders() {
    let dir = temp();
    let today = days_from_civil(2026, 11, 6);
    let fake = healthy(today);
    let mut cfg = config(&dir);
    cfg.feed = Feed::Opra;
    run(&fake, &cfg, now(0)).unwrap();
    let urls = fake.urls.borrow();
    assert!(urls
        .iter()
        .any(|u| u.contains("/v2/stocks/bars") && u.contains("feed=sip")));
    assert!(urls
        .iter()
        .any(|u| u.contains("/options/snapshots/") && u.contains("feed=opra")));
    assert!(urls.iter().all(|u| !u.contains("/orders")));
}
