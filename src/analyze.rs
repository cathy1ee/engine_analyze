//! The full pipeline for one ticker: signals, news, forecast, selection, risk.
use crate::data::Market;
use crate::news::{self, NewsScorer, NewsSignal};
use crate::risk::{decide, Book, Decision, Limits, Request};
use crate::selection::{candidates, forecast, Candidate, Forecast, MIN_EDGE};
use crate::signals::{compute, Signals};
use crate::watchlist::{member, Member, Theme};

#[derive(Clone, Debug)]
pub struct Analysis {
    pub member: Member,
    pub signals: Signals,
    pub news: NewsSignal,
    pub forecast: Forecast,
    pub considered: usize,
    pub best: Option<Candidate>,
    pub decision: Decision,
}

pub struct Context<'a> {
    pub market: &'a Market,
    pub scorer: &'a dyn NewsScorer,
    pub theme: Option<&'a Theme>,
    pub limits: &'a Limits,
    pub book: &'a Book,
    pub kill_switch: bool,
}

pub fn analyze(ctx: &Context, ticker: &str) -> Result<Analysis, String> {
    let m = member(ticker);
    let signals = compute(ctx.market, &m)?;
    let news = news::analyze(
        &ctx.market.news,
        &m.ticker,
        ctx.market.as_of,
        ctx.scorer,
        ctx.theme,
    );
    let chain = ctx
        .market
        .chains
        .get(&m.ticker)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let all = candidates(chain, &signals, &news);
    let considered = all.len();
    let best = all.into_iter().next().filter(|c| c.edge() >= MIN_EDGE);

    let decision = match &best {
        None if considered == 0 => Decision::Blocked("no liquid option structures".into()),
        None => Decision::Blocked(format!(
            "no structure clears the {:.0}% minimum edge",
            MIN_EDGE * 100.0
        )),
        Some(c) => decide(
            ctx.limits,
            ctx.kill_switch,
            ctx.book,
            &Request {
                ticker: &m.ticker,
                group: m.group,
                candidate: c,
                spot: signals.spot,
                beta_sector: signals.beta_sector,
                hedge_ticker: m.hedge,
                hedge_price: ctx.market.spot(m.hedge),
            },
        ),
    };
    let forecast = forecast(&signals, &news, best.as_ref().map(|c| c.expiry_days));
    Ok(Analysis {
        member: m,
        signals,
        news,
        forecast,
        considered,
        best,
        decision,
    })
}
