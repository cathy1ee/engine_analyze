//! Downloads market data from Alpaca into a dated snapshot folder in the format
//! `data::load` reads. Read-only: nothing here can place an order.
//!
//! Each snapshot holds only what was available when it was fetched, which is
//! what keeps later analysis of old snapshots free of hindsight.
use crate::csv::{quote, Table};
use crate::data::OptionQuote;
use crate::dates::{format_date, parse_ts};
use crate::pricing::Right;
use crate::signals::atm_iv;
use crate::watchlist::HEDGES;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DATA_API: &str = "https://data.alpaca.markets";
// Option contract details are served by the trading API. Only GET is used here.
const CONTRACTS_API: &str = "https://paper-api.alpaca.markets";
const HISTORY_DAYS: i64 = 400;
const MAX_EXPIRY_DAYS: i64 = 75;
const STRIKE_RANGE: f64 = 0.4;
const NEWS_DAYS: i64 = 4;
const MAX_PAGES: usize = 100;
const MAX_NEWS_PAGES: usize = 20;
const RATE_LIMIT_RETRIES: u32 = 3;
const RATE_LIMIT_WAIT_SECS: u64 = 20;
/// US markets have closed by this hour UTC all year round.
const CLOSE_HOUR_UTC: i64 = 21;

#[derive(Clone, Debug, PartialEq)]
pub enum HttpError {
    Status(u16, String),
    Other(String),
}

impl HttpError {
    fn message(&self) -> String {
        match self {
            HttpError::Status(401, _) => {
                "Alpaca rejected the API keys. Check ALPACA_API_KEY_ID and ALPACA_API_SECRET_KEY in .env.".into()
            }
            HttpError::Status(403, path) => format!("Alpaca says your plan can't access {path}."),
            HttpError::Status(429, path) => format!("Alpaca's rate limit was still hit after retrying {path}."),
            HttpError::Status(code, path) => format!("Alpaca returned HTTP {code} for {path}."),
            HttpError::Other(e) => e.clone(),
        }
    }

    fn is_access_problem(&self) -> bool {
        matches!(self, HttpError::Status(401 | 403, _))
    }
}

pub trait Http {
    fn get(&self, url: &str) -> Result<String, HttpError>;
}

pub struct Keys {
    pub id: String,
    pub secret: String,
}

/// Reads keys from the environment, then from a `.env` file. Key values are
/// never included in messages.
pub fn load_keys(env_file: &Path) -> Result<Keys, String> {
    let mut found: HashMap<String, String> = fs::read_to_string(env_file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_string(),
                v.trim().trim_matches(['"', '\'']).to_string(),
            )
        })
        .collect();
    for name in ["ALPACA_API_KEY_ID", "ALPACA_API_SECRET_KEY"] {
        if let Ok(v) = std::env::var(name) {
            found.insert(name.to_string(), v);
        }
    }
    let mut get = |name: &str| found.remove(name).filter(|v| !v.is_empty());
    match (get("ALPACA_API_KEY_ID"), get("ALPACA_API_SECRET_KEY")) {
        (Some(id), Some(secret)) => Ok(Keys { id, secret }),
        _ => Err(
            "Alpaca keys not found. Copy .env.example to .env and add your paper account keys."
                .into(),
        ),
    }
}

pub struct AlpacaHttp {
    keys: Keys,
    agent: ureq::Agent,
}

impl AlpacaHttp {
    pub fn new(keys: Keys) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        AlpacaHttp { keys, agent }
    }
}

/// The URL without its host or query, safe to show in messages.
fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let path = rest.split_once('/').map_or("", |x| x.1);
    format!("/{}", path.split('?').next().unwrap_or(""))
}

impl Http for AlpacaHttp {
    fn get(&self, url: &str) -> Result<String, HttpError> {
        for attempt in 0..=RATE_LIMIT_RETRIES {
            let result = self
                .agent
                .get(url)
                .set("APCA-API-KEY-ID", &self.keys.id)
                .set("APCA-API-SECRET-KEY", &self.keys.secret)
                .call();
            match result {
                Ok(r) => return r.into_string().map_err(|e| HttpError::Other(e.to_string())),
                Err(ureq::Error::Status(429, _)) if attempt < RATE_LIMIT_RETRIES => {
                    std::thread::sleep(Duration::from_secs(RATE_LIMIT_WAIT_SECS));
                }
                Err(ureq::Error::Status(code, _)) => {
                    return Err(HttpError::Status(code, path_of(url)))
                }
                Err(e) => {
                    return Err(HttpError::Other(format!(
                        "could not reach Alpaca: {}",
                        e.kind()
                    )))
                }
            }
        }
        Err(HttpError::Status(429, path_of(url)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Feed {
    Indicative,
    Opra,
}

impl Feed {
    pub fn parse(s: &str) -> Result<Feed, String> {
        match s {
            "indicative" => Ok(Feed::Indicative),
            "opra" => Ok(Feed::Opra),
            other => Err(format!("--feed must be indicative or opra, not '{other}'")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Feed::Indicative => "indicative",
            Feed::Opra => "opra",
        }
    }

    /// The stock feed each options plan comes with.
    fn stocks(self) -> &'static str {
        match self {
            Feed::Indicative => "iex",
            Feed::Opra => "sip",
        }
    }
}

pub struct Config {
    pub dir: PathBuf,
    /// Tickers to fetch option chains for. Hedge ETFs get prices automatically.
    pub tickers: Vec<String>,
    pub feed: Feed,
}

#[derive(Debug)]
pub struct Report {
    pub date: String,
    pub path: PathBuf,
    pub chains: usize,
    pub quotes: usize,
    pub stories: usize,
    pub warnings: Vec<String>,
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Walks every page of a response. Returns true if `max_pages` cut it short.
fn paged(
    http: &dyn Http,
    url: &str,
    max_pages: usize,
    mut on_page: impl FnMut(&Value) -> Result<(), String>,
) -> Result<bool, HttpError> {
    let mut token: Option<String> = None;
    for _ in 0..max_pages {
        let full = match &token {
            Some(t) => format!("{url}&page_token={}", encode(t)),
            None => url.to_string(),
        };
        let body = http.get(&full)?;
        let page: Value = serde_json::from_str(&body).map_err(|e| {
            HttpError::Other(format!("unreadable reply from {}: {e}", path_of(url)))
        })?;
        on_page(&page).map_err(HttpError::Other)?;
        token = page["next_page_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(String::from);
        if token.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

type Closes = BTreeMap<String, Vec<(String, f64)>>;

fn fetch_prices(
    http: &dyn Http,
    symbols: &[String],
    feed: Feed,
    today: i64,
) -> Result<Closes, String> {
    let url = format!(
        "{DATA_API}/v2/stocks/bars?symbols={}&timeframe=1Day&start={}&adjustment=split&feed={}&limit=10000",
        encode(&symbols.join(",")),
        format_date(today - HISTORY_DAYS),
        feed.stocks()
    );
    let mut out = Closes::new();
    let cut = paged(http, &url, MAX_PAGES, |page| {
        let bars = page["bars"].as_object().ok_or("price reply has no bars")?;
        for (symbol, rows) in bars {
            for bar in rows.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let date = bar["t"]
                    .as_str()
                    .and_then(|t| t.get(..10))
                    .ok_or("bar has no date")?;
                let close = number(&bar["c"]).ok_or("bar has no close")?;
                out.entry(symbol.clone())
                    .or_default()
                    .push((date.to_string(), close));
            }
        }
        Ok(())
    })
    .map_err(|e| e.message())?;
    if cut {
        return Err(
            "price history was longer than expected, so it was not saved incomplete".into(),
        );
    }
    Ok(out)
}

struct Contract {
    symbol: String,
    right: Right,
    strike: f64,
    expiry_days: u32,
    open_interest: u32,
}

fn fetch_chain(
    http: &dyn Http,
    ticker: &str,
    spot: f64,
    today: i64,
    feed: Feed,
) -> Result<Vec<OptionQuote>, HttpError> {
    let filter = format!(
        "expiration_date_gte={}&expiration_date_lte={}&strike_price_gte={:.2}&strike_price_lte={:.2}",
        format_date(today + 1),
        format_date(today + MAX_EXPIRY_DAYS),
        spot * (1.0 - STRIKE_RANGE),
        spot * (1.0 + STRIKE_RANGE)
    );
    let t = encode(ticker);

    let mut contracts = Vec::new();
    let url = format!("{CONTRACTS_API}/v2/options/contracts?underlying_symbols={t}&status=active&limit=10000&{filter}");
    let cut = paged(http, &url, MAX_PAGES, |page| {
        for c in page["option_contracts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            // Adjusted contracts after splits or mergers deliver something other
            // than 100 shares, so standard pricing would be wrong for them.
            if c["root_symbol"].as_str() != Some(ticker)
                || number(&c["multiplier"]).is_some_and(|m| m != 100.0)
            {
                continue;
            }
            let right = match c["type"].as_str() {
                Some("call") => Right::Call,
                Some("put") => Right::Put,
                _ => continue,
            };
            let (Some(symbol), Some(strike), Some(expiry)) = (
                c["symbol"].as_str(),
                number(&c["strike_price"]),
                c["expiration_date"].as_str().and_then(parse_ts),
            ) else {
                continue;
            };
            contracts.push(Contract {
                symbol: symbol.to_string(),
                right,
                strike,
                expiry_days: (expiry / 86400 - today).max(0) as u32,
                open_interest: number(&c["open_interest"]).unwrap_or(0.0) as u32,
            });
        }
        Ok(())
    })?;
    if cut {
        return Err(HttpError::Other(format!(
            "{ticker} contract list was too long"
        )));
    }

    let mut snapshots: HashMap<String, (f64, f64, u32)> = HashMap::new();
    let url = format!(
        "{DATA_API}/v1beta1/options/snapshots/{t}?feed={}&limit=1000&{filter}",
        feed.name()
    );
    let cut = paged(http, &url, MAX_PAGES, |page| {
        for (symbol, s) in page["snapshots"].as_object().into_iter().flatten() {
            let q = &s["latestQuote"];
            if let (Some(bid), Some(ask)) = (number(&q["bp"]), number(&q["ap"])) {
                let volume = number(&s["dailyBar"]["v"]).unwrap_or(0.0) as u32;
                snapshots.insert(symbol.clone(), (bid, ask, volume));
            }
        }
        Ok(())
    })?;
    if cut {
        return Err(HttpError::Other(format!(
            "{ticker} option chain was too long"
        )));
    }

    Ok(contracts
        .into_iter()
        .filter_map(|c| {
            let (bid, ask, volume) = *snapshots.get(&c.symbol)?;
            // Skip empty or crossed quotes rather than saving a false price.
            (bid >= 0.0 && ask > 0.0 && ask >= bid).then_some(OptionQuote {
                expiry_days: c.expiry_days,
                strike: c.strike,
                right: c.right,
                bid,
                ask,
                volume,
                open_interest: c.open_interest,
            })
        })
        .collect())
}

fn fetch_news(
    http: &dyn Http,
    tickers: &[String],
    today: i64,
    now: i64,
    warnings: &mut Vec<String>,
) -> String {
    let mut csv = String::from("available_at,ticker,source,headline,fetched_at\n");
    let fetched = stamp(now);
    let url = format!(
        "{DATA_API}/v1beta1/news?symbols={}&start={}&sort=desc&limit=50",
        encode(&tickers.join(",")),
        format_date(today - NEWS_DAYS)
    );
    let result = paged(http, &url, MAX_NEWS_PAGES, |page| {
        for n in page["news"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            // created_at, not updated_at: an edited story is still old news.
            let at = n["created_at"]
                .as_str()
                .and_then(|t| t.get(..19))
                .map(|t| format!("{t}Z"));
            let (Some(at), Some(headline)) =
                (at.filter(|t| parse_ts(t).is_some()), n["headline"].as_str())
            else {
                continue;
            };
            let source = n["source"].as_str().unwrap_or("alpaca");
            for s in n["symbols"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                if let Some(t) = s.as_str().filter(|s| tickers.iter().any(|x| x == s)) {
                    let _ = writeln!(
                        csv,
                        "{at},{t},{},{},{fetched}",
                        quote(source),
                        quote(headline)
                    );
                }
            }
        }
        Ok(())
    });
    match result {
        Ok(false) => {}
        Ok(true) => warnings
            .push("news was cut off at the page limit, so older stories may be missing".into()),
        Err(e) => {
            warnings.push(format!("no news saved: {}", e.message()));
            return "available_at,ticker,source,headline,fetched_at\n".to_string();
        }
    }
    csv
}

fn stamp(secs: i64) -> String {
    let t = secs.rem_euclid(86400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        format_date(secs.div_euclid(86400)),
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

type IvHistory = BTreeMap<(String, String), f64>;

fn read_iv_history(path: &Path) -> Result<IvHistory, String> {
    let mut out = IvHistory::new();
    if path.exists() {
        for r in Table::read(path)?.iter() {
            out.insert(
                (r.get("ticker")?.to_string(), r.get("date")?.to_string()),
                r.num("atm_iv")?,
            );
        }
    }
    Ok(out)
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// Downloads everything first and only then writes, so a failed fetch leaves
/// earlier snapshots and the `latest` link untouched.
pub fn run(http: &dyn Http, cfg: &Config, now: i64) -> Result<Report, String> {
    let today = now.div_euclid(86400);
    let mut warnings = Vec::new();

    let mut symbols: Vec<String> = cfg.tickers.clone();
    symbols.extend(HEDGES.iter().map(|h| h.to_string()));
    symbols.sort();
    symbols.dedup();

    let closes = fetch_prices(http, &symbols, cfg.feed, today)?;
    let date = closes
        .values()
        .filter_map(|rows| rows.last())
        .map(|r| r.0.clone())
        .max()
        .ok_or("Alpaca returned no prices")?;
    let snapshot_day = parse_ts(&date).ok_or("price dates were not YYYY-MM-DD")? / 86400;
    if snapshot_day == today && now.rem_euclid(86400) / 3600 < CLOSE_HOUR_UTC {
        warnings
            .push("the market may still be open, so today's prices may not be final closes".into());
    }
    for s in &symbols {
        if !closes.contains_key(s) {
            warnings.push(format!("no prices for {s}"));
        }
    }

    let history_path = cfg.dir.join("iv_history.csv");
    let mut history = read_iv_history(&history_path)?;
    let mut options =
        String::from("ticker,expiry_days,strike,right,bid,ask,volume,open_interest\n");
    let (mut chains, mut quotes) = (0, 0);
    for ticker in &cfg.tickers {
        let Some(spot) = closes.get(ticker).and_then(|r| r.last()).map(|r| r.1) else {
            continue;
        };
        let chain = match fetch_chain(http, ticker, spot, snapshot_day, cfg.feed) {
            Ok(c) => c,
            Err(e) if e.is_access_problem() => return Err(e.message()),
            Err(e) => {
                warnings.push(format!("skipped {ticker}: {}", e.message()));
                continue;
            }
        };
        if chain.is_empty() {
            warnings.push(format!("no usable option quotes for {ticker}"));
            continue;
        }
        for q in &chain {
            let _ = writeln!(
                options,
                "{ticker},{},{},{},{},{},{},{}",
                q.expiry_days,
                q.strike,
                q.right.letter(),
                q.bid,
                q.ask,
                q.volume,
                q.open_interest
            );
        }
        if let Some(iv) = atm_iv(&chain, spot) {
            history.insert((ticker.clone(), date.clone()), iv);
        }
        chains += 1;
        quotes += chain.len();
    }
    if chains == 0 {
        return Err("no option chains could be fetched, so no snapshot was saved".into());
    }

    let news = fetch_news(http, &cfg.tickers, snapshot_day, now, &mut warnings);
    let stories = news.lines().count() - 1;

    let mut prices = String::from("date,ticker,close,atm_iv\n");
    for (ticker, rows) in &closes {
        for (d, close) in rows {
            let iv = history
                .get(&(ticker.clone(), d.clone()))
                .copied()
                .unwrap_or(0.0);
            let _ = writeln!(prices, "{d},{ticker},{close},{iv:.4}");
        }
    }
    let mut iv_csv = String::from("date,ticker,atm_iv\n");
    for ((ticker, d), iv) in &history {
        let _ = writeln!(iv_csv, "{d},{ticker},{iv:.4}");
    }

    fs::create_dir_all(&cfg.dir).map_err(io)?;
    let tmp = cfg.dir.join(".incoming");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(io)?;
    let files = [
        ("prices.csv", prices.as_str()),
        ("options.csv", options.as_str()),
        ("news.csv", news.as_str()),
        ("earnings.csv", "ticker,days_to_earnings\n"),
        ("FEED", cfg.feed.name()),
        (
            "EARNINGS_UNKNOWN",
            "This snapshot has no earnings calendar.\n",
        ),
    ];
    for (name, text) in files {
        fs::write(tmp.join(name), text).map_err(io)?;
    }
    let dest = cfg.dir.join(&date);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(io)?;
    }
    fs::rename(&tmp, &dest).map_err(io)?;
    fs::write(&history_path, iv_csv).map_err(io)?;

    let latest = cfg.dir.join("latest");
    let _ = fs::remove_file(&latest);
    std::os::unix::fs::symlink(&date, &latest).map_err(io)?;

    Ok(Report {
        date,
        path: dest,
        chains,
        quotes,
        stories,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_page_tokens() {
        assert_eq!(encode("aB9-_.~"), "aB9-_.~");
        assert_eq!(encode("a+b/c="), "a%2Bb%2Fc%3D");
        assert_eq!(encode("A,B"), "A%2CB");
    }

    #[test]
    fn paths_in_messages_drop_host_and_query() {
        assert_eq!(
            path_of("https://x.y/v2/stocks/bars?symbols=A&page_token=z"),
            "/v2/stocks/bars"
        );
    }

    #[test]
    fn stamps_are_readable_timestamps() {
        assert_eq!(stamp(86400 + 3661), "1970-01-02T01:01:01Z");
        assert_eq!(parse_ts(&stamp(1_800_000_000)), Some(1_800_000_000));
    }

    #[test]
    fn keys_come_from_the_env_file_and_messages_stay_clean() {
        let p = std::env::temp_dir().join(format!("engine-env-{}", std::process::id()));
        fs::write(
            &p,
            "# comment\nALPACA_API_KEY_ID=\"PKTEST\"\nALPACA_API_SECRET_KEY=shh=1\n",
        )
        .unwrap();
        let k = load_keys(&p).unwrap();
        assert_eq!((k.id.as_str(), k.secret.as_str()), ("PKTEST", "shh=1"));

        fs::write(&p, "ALPACA_API_KEY_ID=PKTEST\nALPACA_API_SECRET_KEY=\n").unwrap();
        let e = load_keys(&p).err().unwrap();
        assert!(e.contains(".env") && !e.contains("PKTEST"));
    }
}
