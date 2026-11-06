//! Append-only record of every paper trade and the reasoning behind it, which
//! is what later shows whether the scores and themes actually predict anything.
use crate::analyze::Analysis;
use crate::broker::Fill;
use crate::csv::{quote, Table};
use crate::data::Market;
use crate::pricing::Right;
use crate::risk::{Book, Hedge, OpenPosition};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const HEADER: &str = "as_of,data_source,ticker,group,theme,news_direction,news_confidence,atm_iv,forecast_vol,structure,legs,contracts,fill_per_share,risk,ev_total,hedge_ticker,hedge_shares,hedge_price";

pub struct Entry<'a> {
    pub analysis: &'a Analysis,
    pub fill: &'a Fill,
    pub risk: f64,
    pub hedge: Option<&'a Hedge>,
    pub theme: &'a str,
    pub as_of: &'a str,
    pub is_sample: bool,
}

pub fn append(path: &Path, e: &Entry) -> Result<(), String> {
    let a = e.analysis;
    let c = a
        .best
        .as_ref()
        .ok_or("nothing to journal: no trade selected")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    }
    let new = !path.exists();
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| err.to_string())?;
    let mut text = String::new();
    if new {
        text.push_str(HEADER);
        text.push('\n');
    }
    let cells = [
        e.as_of.to_string(),
        if e.is_sample { "sample" } else { "market" }.to_string(),
        a.member.ticker.clone(),
        a.member.group.to_string(),
        e.theme.to_string(),
        format!("{:.4}", a.news.direction),
        format!("{:.4}", a.news.confidence),
        format!("{:.4}", a.signals.atm_iv),
        format!("{:.4}", a.forecast.vol),
        c.name.to_string(),
        c.legs_code(),
        e.fill.contracts.to_string(),
        format!("{:.4}", e.fill.price),
        format!("{:.2}", e.risk),
        format!("{:.2}", c.ev * 100.0 * e.fill.contracts as f64),
        e.hedge.map_or(String::new(), |h| h.ticker.clone()),
        e.hedge.map_or(String::new(), |h| h.shares.to_string()),
        e.fill
            .hedge_price
            .map_or(String::new(), |p| format!("{p:.4}")),
    ];
    text.push_str(&cells.iter().map(|c| quote(c)).collect::<Vec<_>>().join(","));
    text.push('\n');
    f.write_all(text.as_bytes()).map_err(|err| err.to_string())
}

#[derive(Clone, Debug)]
pub struct MarkedPosition {
    pub ticker: String,
    pub structure: String,
    pub contracts: u32,
    pub risk: f64,
    pub pnl: Option<f64>,
}

/// Closing value per share at bid/ask, the price we could actually exit at.
fn exit_value(code: &str, chain: &[crate::data::OptionQuote]) -> Option<f64> {
    code.split('|').try_fold(0.0, |acc, part| {
        let long = part.starts_with('+');
        let right = if part[1..].starts_with('C') {
            Right::Call
        } else {
            Right::Put
        };
        let (strike, days) = part[2..].split_once('@')?;
        let (strike, days): (f64, u32) = (strike.parse().ok()?, days.parse().ok()?);
        let q = chain
            .iter()
            .find(|q| q.right == right && q.strike == strike && q.expiry_days == days)?;
        Some(acc + if long { q.bid } else { -q.ask })
    })
}

pub fn load(path: &Path, market: &Market) -> Result<(Book, Vec<MarkedPosition>), String> {
    let mut book = Book::default();
    let mut marked = Vec::new();
    if !path.exists() {
        return Ok((book, marked));
    }
    for r in Table::read(path)?.iter() {
        let ticker = r.get("ticker")?.to_string();
        let contracts: u32 = r.num("contracts")?;
        let fill: f64 = r.num("fill_per_share")?;
        let risk: f64 = r.num("risk")?;
        let option_pnl = market
            .chains
            .get(&ticker)
            .and_then(|c| exit_value(r.get("legs").ok()?, c))
            .map(|v| (v - fill) * 100.0 * contracts as f64);
        let hedge_pnl = match r.get("hedge_ticker")? {
            "" => Some(0.0),
            h => {
                let shares: f64 = r.num("hedge_shares")?;
                let entry: f64 = r.num("hedge_price")?;
                market.spot(h).map(|p| shares * (p - entry))
            }
        };
        let pnl = option_pnl.zip(hedge_pnl).map(|(a, b)| a + b);
        book.unrealized_pnl += pnl.unwrap_or(0.0);
        book.positions.push(OpenPosition {
            ticker: ticker.clone(),
            group: r.get("group")?.to_string(),
            risk,
        });
        marked.push(MarkedPosition {
            ticker,
            structure: r.get("structure")?.to_string(),
            contracts,
            risk,
            pnl,
        });
    }
    Ok((book, marked))
}
