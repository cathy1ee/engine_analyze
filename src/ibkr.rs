//! Reads market data from a running TWS or IB Gateway paper session.
//!
//! This file only ever requests data. It refuses IBKR's live ports and live
//! accounts, and it never
//! uses the order functions in the `ibapi` crate; a test in `tests/fetch.rs`
//! checks that. For a second lock, turn on Read-Only API in TWS.
use crate::data::OptionQuote;
use crate::dates::{civil_from_days, days_from_civil};
use crate::fetch::{stamp, Closes, HeldLeg, Source, SourceError, Story};
use crate::pricing::Right;
use ibapi::client::blocking::Client;
use ibapi::contracts::tick_types::TickType;
use ibapi::contracts::{Contract, OptionRight, SecurityType};
use ibapi::market_data::historical::{BarSize, BarTimestamp, Duration as BarSpan, WhatToShow};
use ibapi::market_data::realtime::TickTypes;
use ibapi::market_data::{MarketDataType, TradingHours};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;
use time::OffsetDateTime;

pub const PAPER_PORTS: [u16; 2] = [7497, 4002];
pub const LIVE_PORTS: [u16; 2] = [7496, 4001];
const HOST: &str = "127.0.0.1";
const CLIENT_ID: i32 = 77;

const MAX_EXPIRY_DAYS: i64 = 75;
const STRIKE_RANGE: f64 = 0.2;
const STRIKE_TARGET_STEP: f64 = 0.025;
/// IBKR accounts start with 100 market data lines. Staying well below that
/// leaves room for anything else open in TWS.
const BATCH: usize = 40;
const BATCH_WAIT: Duration = Duration::from_secs(3);
const CHAIN_TIMEOUT: Duration = Duration::from_secs(15);
const NEWS_DAYS: i64 = 4;
const NEWS_PER_TICKER: u8 = 50;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Needs paid data subscriptions. After hours this returns the last close.
    Subscribed,
    /// Free, but 15+ minutes old.
    Delayed,
}

impl Mode {
    pub fn feed(self) -> &'static str {
        match self {
            Mode::Subscribed => "ibkr",
            Mode::Delayed => "ibkr-delayed",
        }
    }

    fn data_type(self) -> MarketDataType {
        match self {
            Mode::Subscribed => MarketDataType::Frozen,
            Mode::Delayed => MarketDataType::DelayedFrozen,
        }
    }
}

/// The ports to try, or an error for a live trading port.
pub fn ports_to_try(requested: Option<u16>) -> Result<Vec<u16>, String> {
    match requested {
        Some(p) if LIVE_PORTS.contains(&p) => Err(format!(
            "port {p} is a live trading port. This engine only connects to paper sessions."
        )),
        Some(p) => Ok(vec![p]),
        None => Ok(PAPER_PORTS.to_vec()),
    }
}

/// Ports can be changed in TWS, so this checks the session itself. IBKR paper
/// account IDs start with D.
pub fn check_paper_accounts(accounts: &[String]) -> Result<(), String> {
    if accounts.is_empty() {
        return Err(
            "IBKR reported no accounts, so this can't be confirmed as a paper session.".into(),
        );
    }
    if accounts.iter().any(|a| !a.starts_with('D')) {
        return Err(
            "this looks like a live IBKR session. Log in to Paper Trading and try again.".into(),
        );
    }
    Ok(())
}

/// What to request: the usual grid plus any held contracts not already on it.
/// An expired contract is left out, because it settles from the closing price.
pub fn wanted_contracts(
    expiries: &[(String, u32)],
    strikes: &[f64],
    held: &[&HeldLeg],
    today: i64,
) -> Vec<(String, u32, f64, Right)> {
    let mut out = Vec::new();
    for (expiry, days) in expiries {
        for k in strikes {
            for r in [Right::Call, Right::Put] {
                out.push((expiry.clone(), *days, *k, r));
            }
        }
    }
    for h in held.iter().filter(|h| h.expiry_day > today) {
        let (y, m, d) = civil_from_days(h.expiry_day);
        let days = (h.expiry_day - today) as u32;
        if !out
            .iter()
            .any(|w| w.1 == days && w.2 == h.strike && w.3 == h.right)
        {
            out.push((format!("{y:04}{m:02}{d:02}"), days, h.strike, h.right));
        }
    }
    out
}

/// The monthly expiries inside the fetch window, as (`YYYYMMDD`, days left).
/// Monthlies keep the request count down, and a held contract stays listed.
pub fn pick_expiries(expirations: &[String], today: i64) -> Vec<(String, u32)> {
    let mut by_month: HashMap<(i64, i64), (i64, String)> = HashMap::new();
    for e in expirations {
        let (Some(y), Some(m), Some(d)) = (
            e.get(0..4).and_then(|x| x.parse().ok()),
            e.get(4..6).and_then(|x| x.parse().ok()),
            e.get(6..8).and_then(|x| x.parse::<i64>().ok()),
        ) else {
            continue;
        };
        // The last expiry in the third week is the monthly, even in holiday
        // weeks when it moves from Friday to Thursday.
        if !(15..=21).contains(&d) {
            continue;
        }
        let day = days_from_civil(y, m, d);
        if day <= today || day - today > MAX_EXPIRY_DAYS {
            continue;
        }
        let slot = by_month.entry((y, m)).or_insert((day, e.clone()));
        if day > slot.0 {
            *slot = (day, e.clone());
        }
    }
    let mut out: Vec<(String, u32)> = by_month
        .into_values()
        .map(|(d, e)| (e, (d - today) as u32))
        .collect();
    out.sort();
    out
}

/// Strikes around spot on a round grid about 2.5% wide.
pub fn pick_strikes(strikes: &[f64], spot: f64) -> Vec<f64> {
    let target = spot * STRIKE_TARGET_STEP;
    let step = [0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0]
        .into_iter()
        .find(|s| *s >= target)
        .unwrap_or(100.0);
    let mut out: Vec<f64> = strikes
        .iter()
        .copied()
        .filter(|k| (k / spot - 1.0).abs() <= STRIKE_RANGE && (k / step).fract() == 0.0)
        .collect();
    out.sort_by(f64::total_cmp);
    out.dedup();
    out
}

/// News headlines can start with `{A:800015:L:en}` metadata.
pub fn clean_headline(h: &str) -> String {
    let t = h.trim();
    let t = if t.starts_with('{') {
        t.split_once('}').map_or(t, |x| x.1)
    } else {
        t
    };
    t.trim_start_matches('!').trim().to_string()
}

/// Turns one option's ticks into a quote. `None` unless both sides are real:
/// IBKR sends -1 for a missing price, and a crossed market isn't a price at all.
pub fn quote_from_ticks(
    ticks: &[TickTypes],
    expiry_days: u32,
    strike: f64,
    right: Right,
) -> Option<OptionQuote> {
    let (mut bid, mut ask, mut volume, mut open_interest) = (None, None, 0.0, 0.0);
    for t in ticks {
        match t {
            TickTypes::Price(p) => match p.tick_type {
                TickType::Bid | TickType::DelayedBid => bid = Some(p.price),
                TickType::Ask | TickType::DelayedAsk => ask = Some(p.price),
                _ => {}
            },
            TickTypes::PriceSize(p) => match p.price_tick_type {
                TickType::Bid | TickType::DelayedBid => bid = Some(p.price),
                TickType::Ask | TickType::DelayedAsk => ask = Some(p.price),
                _ => {}
            },
            TickTypes::Size(s) => match s.tick_type {
                TickType::Volume | TickType::DelayedVolume => volume = s.size,
                TickType::OptionCallOpenInterest if right == Right::Call => open_interest = s.size,
                TickType::OptionPutOpenInterest if right == Right::Put => open_interest = s.size,
                _ => {}
            },
            _ => {}
        }
    }
    let (bid, ask) = (bid?, ask?);
    (bid >= 0.0 && ask > 0.0 && ask >= bid).then_some(OptionQuote {
        expiry_days,
        strike,
        right,
        bid,
        ask,
        volume: volume.max(0.0) as u32,
        open_interest: open_interest.max(0.0) as u32,
    })
}

/// Collects news ticker by ticker, so one failure can't lose the rest. News only
/// counts as unavailable when every ticker fails.
pub fn gather_news(
    tickers: &[String],
    mut fetch: impl FnMut(&str) -> Result<Vec<Story>, String>,
) -> Result<Vec<Story>, SourceError> {
    let mut out = Vec::new();
    let (mut worked, mut last_error) = (0, None);
    for t in tickers {
        match fetch(t) {
            Ok(stories) => {
                worked += 1;
                out.extend(stories);
            }
            Err(e) => {
                eprintln!("warning: no news for {t}: {e}");
                last_error = Some(e);
            }
        }
    }
    match last_error {
        Some(e) if worked == 0 => Err(SourceError::Skip(e)),
        _ => Ok(out),
    }
}

pub struct Ibkr {
    client: Client,
    contract_ids: RefCell<HashMap<String, i32>>,
}

fn skip(what: &str, e: impl std::fmt::Display) -> SourceError {
    SourceError::Skip(format!("{what}: {e}"))
}

fn stock(ticker: &str) -> Contract {
    Contract::stock(ticker).build()
}

fn option(ticker: &str, expiry: &str, strike: f64, right: Right) -> Contract {
    Contract {
        symbol: ticker.into(),
        security_type: SecurityType::Option,
        last_trade_date_or_contract_month: expiry.to_string(),
        strike,
        right: Some(match right {
            Right::Call => OptionRight::Call,
            Right::Put => OptionRight::Put,
        }),
        multiplier: "100".to_string(),
        exchange: "SMART".into(),
        currency: "USD".into(),
        trading_class: ticker.to_string(),
        ..Contract::default()
    }
}

impl Ibkr {
    pub fn connect(port: Option<u16>, mode: Mode) -> Result<Ibkr, String> {
        let ports = ports_to_try(port)?;
        let client = ports
            .iter()
            .find_map(|p| Client::connect(&format!("{HOST}:{p}"), CLIENT_ID).ok())
            .ok_or_else(|| {
                let list = ports
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(" or ");
                format!(
                    "could not reach IBKR on port {list}. Open TWS or IB Gateway, log in to your \
                     paper account, and turn on API socket connections."
                )
            })?;
        let accounts = client
            .managed_accounts()
            .map_err(|e| format!("could not read the IBKR account list: {e}"))?;
        check_paper_accounts(&accounts)?;
        client
            .switch_market_data_type(mode.data_type())
            .map_err(|e| format!("IBKR refused the market data setting: {e}"))?;
        Ok(Ibkr {
            client,
            contract_ids: RefCell::new(HashMap::new()),
        })
    }

    fn contract_id(&self, ticker: &str) -> Result<i32, SourceError> {
        if let Some(id) = self.contract_ids.borrow().get(ticker) {
            return Ok(*id);
        }
        let details = self
            .client
            .contract_details(&stock(ticker))
            .map_err(|e| skip("contract lookup failed", e))?;
        let id = details
            .first()
            .map(|d| d.contract.contract_id)
            .ok_or_else(|| SourceError::Skip("IBKR doesn't know this ticker".into()))?;
        self.contract_ids
            .borrow_mut()
            .insert(ticker.to_string(), id);
        Ok(id)
    }

    /// Streams a batch at once, waits, then reads what arrived. Snapshots can't
    /// be used here because IBKR doesn't send open interest in them.
    fn quote_batch(&self, ticker: &str, batch: &[(String, u32, f64, Right)]) -> Vec<OptionQuote> {
        let contracts: Vec<Contract> = batch
            .iter()
            .map(|(e, _, k, r)| option(ticker, e, *k, *r))
            .collect();
        // A strike that doesn't exist for one expiry just fails on its own.
        let subs: Vec<_> = contracts
            .iter()
            .zip(batch)
            .filter_map(|(c, b)| {
                Some((
                    self.client
                        .market_data(c)
                        .generic_ticks(&["101"])
                        .subscribe()
                        .ok()?,
                    b,
                ))
            })
            .collect();
        std::thread::sleep(BATCH_WAIT);
        let quotes = subs
            .iter()
            .filter_map(|(sub, (_, days, strike, right))| {
                let ticks: Vec<TickTypes> = sub.try_iter_data().filter_map(Result::ok).collect();
                quote_from_ticks(&ticks, *days, *strike, *right)
            })
            .collect();
        for (sub, _) in &subs {
            sub.cancel();
        }
        quotes
    }
}

impl Source for Ibkr {
    fn closes(&self, symbols: &[String]) -> Result<Closes, SourceError> {
        let mut out = Closes::new();
        for s in symbols {
            let contract = stock(s);
            // A failure here means no prices for one ticker, reported by `run`.
            let Ok(data) = self
                .client
                .historical_data(&contract, BarSize::Day)
                .what_to_show(WhatToShow::Trades)
                .trading_hours(TradingHours::Regular)
                .duration(BarSpan::years(1))
                .fetch()
            else {
                continue;
            };
            let rows: Vec<(String, f64)> = data
                .bars
                .iter()
                .filter_map(|b| match b.date {
                    BarTimestamp::Date(d) => Some((d.to_string(), b.close)),
                    BarTimestamp::DateTime(_) => None,
                })
                .collect();
            if !rows.is_empty() {
                out.insert(s.clone(), rows);
            }
        }
        if out.is_empty() {
            return Err(SourceError::Fatal(
                "IBKR returned no prices. Check that your data subscriptions are shared with the paper \
                 account, or try --delayed."
                    .into(),
            ));
        }
        Ok(out)
    }

    fn chain(
        &self,
        ticker: &str,
        spot: f64,
        today: i64,
        held: &[&HeldLeg],
    ) -> Result<Vec<OptionQuote>, SourceError> {
        let id = self.contract_id(ticker)?;
        let listings = self
            .client
            .option_chain(ticker, SecurityType::Stock, id)
            .subscribe()
            .map_err(|e| skip("option chain request failed", e))?
            .collect_to_end_within(CHAIN_TIMEOUT)
            .map_err(|e| skip("option chain request failed", e))?;
        // SMART routing with the plain trading class and 100 shares leaves out
        // adjusted contracts, which standard pricing would get wrong.
        let listing = listings
            .iter()
            .find(|c| c.exchange == "SMART" && c.trading_class == ticker && c.multiplier == "100")
            .ok_or_else(|| SourceError::Skip("no standard options listed".into()))?;

        let strikes = pick_strikes(&listing.strikes, spot);
        let expiries = pick_expiries(&listing.expirations, today);
        let wanted = wanted_contracts(&expiries, &strikes, held, today);
        Ok(wanted
            .chunks(BATCH)
            .flat_map(|b| self.quote_batch(ticker, b))
            .collect())
    }

    fn news(&self, tickers: &[String], today: i64) -> Result<Vec<Story>, SourceError> {
        let providers = self
            .client
            .news_providers()
            .map_err(|e| skip("news providers unavailable", e))?;
        if providers.is_empty() {
            return Err(SourceError::Skip(
                "your IBKR account has no API news providers".into(),
            ));
        }
        let codes: Vec<&str> = providers.iter().map(|p| p.code.as_str()).collect();
        let (start, end) = (
            OffsetDateTime::from_unix_timestamp((today - NEWS_DAYS) * 86400)
                .map_err(|e| skip("bad date", e))?,
            OffsetDateTime::now_utc(),
        );
        gather_news(tickers, |t| {
            let id = self.contract_id(t).map_err(|e| format!("{e:?}"))?;
            let articles = self
                .client
                .historical_news(id, &codes, start, end, NEWS_PER_TICKER)
                .map_err(|e| e.to_string())?;
            Ok(articles
                .iter_data()
                .filter_map(Result::ok)
                .map(|a| Story {
                    available_at: stamp(a.time.unix_timestamp()),
                    ticker: t.to_string(),
                    source: a.provider_code.clone(),
                    headline: clean_headline(&a.headline),
                })
                .collect())
        })
    }
}
