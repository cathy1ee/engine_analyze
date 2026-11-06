//! Loads prices, option chains, news and earnings dates from a data folder.
use crate::csv::Table;
use crate::dates::parse_ts;
use crate::pricing::Right;
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct PriceRow {
    pub date: String,
    pub close: f64,
    pub atm_iv: f64,
}

#[derive(Clone, Debug)]
pub struct OptionQuote {
    pub expiry_days: u32,
    pub strike: f64,
    pub right: Right,
    pub bid: f64,
    pub ask: f64,
    pub volume: u32,
    pub open_interest: u32,
}

impl OptionQuote {
    pub fn mid(&self) -> f64 {
        0.5 * (self.bid + self.ask)
    }
}

#[derive(Clone, Debug)]
pub struct NewsItem {
    /// When the story reached us, which may be later than its publish time.
    pub available_at: i64,
    pub ticker: String,
    pub source: String,
    pub headline: String,
}

#[derive(Default)]
pub struct Market {
    pub as_of: i64,
    pub as_of_date: String,
    pub is_sample: bool,
    pub prices: HashMap<String, Vec<PriceRow>>,
    pub chains: HashMap<String, Vec<OptionQuote>>,
    pub news: Vec<NewsItem>,
    pub earnings_days: HashMap<String, u32>,
}

impl Market {
    pub fn spot(&self, ticker: &str) -> Option<f64> {
        self.prices.get(ticker)?.last().map(|p| p.close)
    }
}

pub fn load(dir: &Path) -> Result<Market, String> {
    let mut m = Market {
        is_sample: dir.join("SAMPLE_DATA").exists(),
        ..Market::default()
    };

    for r in Table::read(&dir.join("prices.csv"))?.iter() {
        let row = PriceRow {
            date: r.get("date")?.to_string(),
            close: r.num("close")?,
            atm_iv: r.num("atm_iv")?,
        };
        if !(row.close > 0.0 && row.close.is_finite()) {
            return Err(r.err("close must be a positive number"));
        }
        m.prices
            .entry(r.get("ticker")?.to_uppercase())
            .or_default()
            .push(row);
    }
    if m.prices.is_empty() {
        return Err("prices.csv has no rows".into());
    }
    for rows in m.prices.values_mut() {
        rows.sort_by(|a, b| a.date.cmp(&b.date));
    }
    m.as_of_date = m
        .prices
        .values()
        .filter_map(|v| v.last())
        .map(|p| p.date.clone())
        .max()
        .unwrap_or_default();
    // Daily closes are only known after the US close, 21:00 UTC at the latest.
    m.as_of = parse_ts(&m.as_of_date).ok_or("prices.csv: dates must be YYYY-MM-DD")? + 21 * 3600;

    for r in Table::read(&dir.join("options.csv"))?.iter() {
        let right = match r.get("right")? {
            "C" | "c" => Right::Call,
            "P" | "p" => Right::Put,
            other => return Err(r.err(&format!("right must be C or P, found '{other}'"))),
        };
        let q = OptionQuote {
            expiry_days: r.num("expiry_days")?,
            strike: r.num("strike")?,
            right,
            bid: r.num("bid")?,
            ask: r.num("ask")?,
            volume: r.num("volume")?,
            open_interest: r.num("open_interest")?,
        };
        if q.bid < 0.0 || q.ask < q.bid {
            return Err(r.err("quote needs 0 <= bid <= ask"));
        }
        m.chains
            .entry(r.get("ticker")?.to_uppercase())
            .or_default()
            .push(q);
    }

    for r in Table::read(&dir.join("news.csv"))?.iter() {
        let at = r.get("available_at")?;
        m.news.push(NewsItem {
            available_at: parse_ts(at).ok_or_else(|| r.err(&format!("bad timestamp '{at}'")))?,
            ticker: r.get("ticker")?.to_uppercase(),
            source: r.get("source")?.to_string(),
            headline: r.get("headline")?.to_string(),
        });
    }

    for r in Table::read(&dir.join("earnings.csv"))?.iter() {
        m.earnings_days
            .insert(r.get("ticker")?.to_uppercase(), r.num("days_to_earnings")?);
    }
    Ok(m)
}
