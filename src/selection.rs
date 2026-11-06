//! Forecasts the stock, builds defined-risk option structures from liquid
//! quotes and ranks them by expected value after spread and commission.
use crate::data::OptionQuote;
use crate::news::NewsSignal;
use crate::pricing::{greeks, implied_vol, norm_pdf, Inputs, Right, DIV_YIELD, RATE};
use crate::signals::Signals;

pub const COMMISSION_PER_SHARE: f64 = 0.0065; // $0.65 per contract per leg
pub const MIN_EDGE: f64 = 0.05; // EV must be at least 5% of the maximum loss
pub const MAX_SPREAD_PCT: f64 = 0.15;
pub const MIN_VOLUME: u32 = 100;
pub const MIN_OPEN_INTEREST: u32 = 500;
const MIN_DAYS: u32 = 14;
const MAX_DAYS: u32 = 60;

// Judgement calls, not fitted values. The journal exists to test them.
const NEWS_MOVE_SHARE: f64 = 0.25; // share of a 1-month sigma that full-strength news implies
const ALPHA_SHRINK: f64 = 0.1; // 60-day alpha is mostly noise, so keep only 10%
const REALIZED_WEIGHT: f64 = 0.5;

#[derive(Clone, Copy, Debug)]
pub struct Forecast {
    /// One-off expected move beyond the risk-free drift, e.g. 0.02 = +2%.
    pub expected_move: f64,
    pub alpha_annual: f64,
    pub vol: f64,
}

pub fn forecast(s: &Signals, n: &NewsSignal, days: Option<u32>) -> Forecast {
    let month_sigma = s.atm_iv * (30.0f64 / 365.0).sqrt();
    let earnings_soon = matches!((s.days_to_earnings, days), (Some(e), Some(d)) if e <= d)
        || s.days_to_earnings
            .is_some_and(|e| e <= MAX_DAYS && days.is_none());
    // Realized vol says nothing about an earnings jump, so claim no volatility
    // edge over the market when earnings fall inside the option's life, or
    // when there's no calendar to rule that out.
    let vol = if earnings_soon || s.earnings_unknown {
        s.atm_iv
    } else {
        REALIZED_WEIGHT * s.realized_vol + (1.0 - REALIZED_WEIGHT) * s.atm_iv
    };
    Forecast {
        expected_move: n.direction * n.confidence * NEWS_MOVE_SHARE * month_sigma,
        alpha_annual: ALPHA_SHRINK * s.alpha_annual,
        vol,
    }
}

#[derive(Clone, Debug)]
pub struct Leg {
    pub right: Right,
    pub strike: f64,
    pub expiry_days: u32,
    /// +1 bought, -1 sold.
    pub qty: i32,
    pub bid: f64,
    pub ask: f64,
}

impl Leg {
    pub fn code(&self) -> String {
        format!(
            "{}{}{}@{}",
            if self.qty > 0 { '+' } else { '-' },
            self.right.letter(),
            self.strike,
            self.expiry_days
        )
    }
}

/// All money figures are per share; multiply by 100 for one contract.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub name: &'static str,
    pub legs: Vec<Leg>,
    pub expiry_days: u32,
    /// Positive = paid, negative = received. Includes commission.
    pub entry: f64,
    pub max_loss: f64,
    pub ev: f64,
    pub prob_profit: f64,
    pub delta: f64,
}

impl Candidate {
    pub fn edge(&self) -> f64 {
        self.ev / self.max_loss
    }

    pub fn legs_code(&self) -> String {
        self.legs
            .iter()
            .map(Leg::code)
            .collect::<Vec<_>>()
            .join("|")
    }
}

pub fn is_liquid(q: &OptionQuote) -> bool {
    let mid = q.mid();
    q.bid > 0.0
        && (q.ask - q.bid) / mid <= MAX_SPREAD_PCT
        && q.volume >= MIN_VOLUME
        && q.open_interest >= MIN_OPEN_INTEREST
}

fn payoff(legs: &[Leg], spot: f64) -> f64 {
    legs.iter()
        .map(|l| l.qty as f64 * l.right.intrinsic(spot, l.strike))
        .sum()
}

fn entry_cost(legs: &[Leg]) -> f64 {
    legs.iter()
        .map(|l| if l.qty > 0 { l.ask } else { -l.bid } + COMMISSION_PER_SHARE)
        .sum()
}

/// Worst outcome at expiry. Piecewise-linear payoffs bottom out at a strike or
/// at the extremes, so those are the only prices that need checking.
fn max_loss(legs: &[Leg], entry: f64) -> f64 {
    let top = legs.iter().map(|l| l.strike).fold(0.0, f64::max) * 10.0;
    let mut points: Vec<f64> = legs.iter().map(|l| l.strike).collect();
    points.extend([0.0, top]);
    points
        .iter()
        .map(|s| entry - payoff(legs, *s))
        .fold(f64::MIN, f64::max)
}

/// Expected PnL and win probability under the forecast's lognormal distribution.
pub fn evaluate(legs: &[Leg], entry: f64, spot: f64, days: u32, f: &Forecast) -> (f64, f64) {
    let t = days as f64 / 365.0;
    let sd = f.vol * t.sqrt();
    let mean = (1.0 + f.expected_move).ln() + (RATE + f.alpha_annual) * t - 0.5 * sd * sd;
    let disc = (-RATE * t).exp();
    let (n, width) = (2001, 8.0);
    let dz = 2.0 * width / (n - 1) as f64;
    let (mut total_w, mut value, mut wins) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let z = -width + i as f64 * dz;
        let w = norm_pdf(z);
        let pay = disc * payoff(legs, spot * (mean + sd * z).exp());
        total_w += w;
        value += w * pay;
        if pay > entry {
            wins += w;
        }
    }
    (value / total_w - entry, wins / total_w)
}

fn leg_delta(l: &Leg, spot: f64, fallback_iv: f64) -> f64 {
    let t = l.expiry_days as f64 / 365.0;
    let mid = 0.5 * (l.bid + l.ask);
    let vol = implied_vol(l.right, mid, spot, l.strike, t, RATE, DIV_YIELD).unwrap_or(fallback_iv);
    let i = Inputs {
        spot,
        strike: l.strike,
        t,
        r: RATE,
        q: DIV_YIELD,
        vol,
    };
    l.qty as f64 * greeks(l.right, &i).delta
}

fn pick<'a>(quotes: &[&'a OptionQuote], right: Right, target: f64) -> Option<&'a OptionQuote> {
    quotes
        .iter()
        .filter(|q| q.right == right && (q.strike / target - 1.0).abs() <= 0.025)
        .min_by(|a, b| {
            (a.strike - target)
                .abs()
                .total_cmp(&(b.strike - target).abs())
        })
        .copied()
}

fn leg(q: &OptionQuote, qty: i32) -> Leg {
    Leg {
        right: q.right,
        strike: q.strike,
        expiry_days: q.expiry_days,
        qty,
        bid: q.bid,
        ask: q.ask,
    }
}

fn build(
    name: &'static str,
    legs: Vec<Leg>,
    days: u32,
    s: &Signals,
    n: &NewsSignal,
) -> Option<Candidate> {
    if legs.len() == 2 && legs[0].strike == legs[1].strike {
        return None;
    }
    let entry = entry_cost(&legs);
    let loss = max_loss(&legs, entry);
    if loss.is_nan() || loss <= 0.0 {
        return None;
    }
    let f = forecast(s, n, Some(days));
    let (ev, prob_profit) = evaluate(&legs, entry, s.spot, days, &f);
    let delta = legs.iter().map(|l| leg_delta(l, s.spot, s.atm_iv)).sum();
    Some(Candidate {
        name,
        legs,
        expiry_days: days,
        entry,
        max_loss: loss,
        ev,
        prob_profit,
        delta,
    })
}

/// Every candidate from liquid quotes, best edge first. Only defined-risk
/// structures are built: nothing here can lose more than `max_loss`.
pub fn candidates(chain: &[OptionQuote], s: &Signals, n: &NewsSignal) -> Vec<Candidate> {
    let mut days: Vec<u32> = chain
        .iter()
        .map(|q| q.expiry_days)
        .filter(|d| (MIN_DAYS..=MAX_DAYS).contains(d))
        .collect();
    days.sort_unstable();
    days.dedup();

    let mut out = Vec::new();
    for d in days {
        let liquid: Vec<&OptionQuote> = chain
            .iter()
            .filter(|q| q.expiry_days == d && is_liquid(q))
            .collect();
        let c = |k: f64| pick(&liquid, Right::Call, s.spot * k);
        let p = |k: f64| pick(&liquid, Right::Put, s.spot * k);
        let plans: Vec<(&'static str, Option<Vec<Leg>>)> = vec![
            ("long call", c(1.0).map(|a| vec![leg(a, 1)])),
            ("long put", p(1.0).map(|a| vec![leg(a, 1)])),
            (
                "bull call spread",
                c(1.0)
                    .zip(c(1.05))
                    .map(|(a, b)| vec![leg(a, 1), leg(b, -1)]),
            ),
            (
                "bear put spread",
                p(1.0)
                    .zip(p(0.95))
                    .map(|(a, b)| vec![leg(a, 1), leg(b, -1)]),
            ),
            (
                "bull put credit spread",
                p(0.95)
                    .zip(p(0.90))
                    .map(|(a, b)| vec![leg(a, -1), leg(b, 1)]),
            ),
            (
                "bear call credit spread",
                c(1.05)
                    .zip(c(1.10))
                    .map(|(a, b)| vec![leg(a, -1), leg(b, 1)]),
            ),
        ];
        out.extend(
            plans
                .into_iter()
                .filter_map(|(name, legs)| build(name, legs?, d, s, n)),
        );
    }
    out.sort_by(|a, b| b.edge().total_cmp(&a.edge()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::price;

    fn fair_leg(right: Right, strike: f64, qty: i32, vol: f64) -> Leg {
        let p = price(
            right,
            &Inputs {
                spot: 100.0,
                strike,
                t: 30.0 / 365.0,
                r: RATE,
                q: 0.0,
                vol,
            },
        );
        Leg {
            right,
            strike,
            expiry_days: 30,
            qty,
            bid: p,
            ask: p,
        }
    }

    #[test]
    fn fairly_priced_options_have_zero_expected_value() {
        let f = Forecast {
            expected_move: 0.0,
            alpha_annual: 0.0,
            vol: 0.3,
        };
        let legs = [fair_leg(Right::Call, 100.0, 1, 0.3)];
        let (ev, _) = evaluate(&legs, legs[0].ask, 100.0, 30, &f);
        assert!(ev.abs() < 1e-3, "ev = {ev}");
    }

    #[test]
    fn a_bullish_forecast_helps_calls_and_hurts_puts() {
        let f = Forecast {
            expected_move: 0.03,
            alpha_annual: 0.0,
            vol: 0.3,
        };
        let call = [fair_leg(Right::Call, 100.0, 1, 0.3)];
        let put = [fair_leg(Right::Put, 100.0, 1, 0.3)];
        assert!(evaluate(&call, call[0].ask, 100.0, 30, &f).0 > 0.5);
        assert!(evaluate(&put, put[0].ask, 100.0, 30, &f).0 < -0.5);
    }

    #[test]
    fn max_loss_is_the_debit_or_width_minus_credit() {
        let mk = |right, strike, qty, bid, ask| Leg {
            right,
            strike,
            expiry_days: 30,
            qty,
            bid,
            ask,
        };
        let debit = [
            mk(Right::Call, 100.0, 1, 3.9, 4.0),
            mk(Right::Call, 105.0, -1, 2.0, 2.1),
        ];
        let e = entry_cost(&debit);
        assert!((e - (2.0 + 2.0 * COMMISSION_PER_SHARE)).abs() < 1e-12);
        assert!((max_loss(&debit, e) - e).abs() < 1e-12);

        let credit = [
            mk(Right::Put, 95.0, -1, 2.0, 2.1),
            mk(Right::Put, 90.0, 1, 1.0, 1.1),
        ];
        let e = entry_cost(&credit);
        assert!(e < 0.0);
        assert!((max_loss(&credit, e) - (5.0 + e)).abs() < 1e-12);
    }

    #[test]
    fn illiquid_quotes_are_rejected() {
        let q = OptionQuote {
            expiry_days: 30,
            strike: 100.0,
            right: Right::Call,
            bid: 1.0,
            ask: 1.05,
            volume: 500,
            open_interest: 1000,
        };
        assert!(is_liquid(&q));
        assert!(!is_liquid(&OptionQuote {
            bid: 0.5,
            ask: 1.5,
            ..q.clone()
        }));
        assert!(!is_liquid(&OptionQuote {
            volume: 5,
            ..q.clone()
        }));
        assert!(!is_liquid(&OptionQuote {
            open_interest: 5,
            ..q.clone()
        }));
        assert!(!is_liquid(&OptionQuote { bid: 0.0, ..q }));
    }
}
