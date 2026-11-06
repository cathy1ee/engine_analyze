//! Black-Scholes-Merton pricing, Greeks and implied volatility.
//! European exercise: a close approximation for short-dated US equity options
//! without large dividends, and not exact for American early exercise.

pub const RATE: f64 = 0.04;
pub const DIV_YIELD: f64 = 0.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Right {
    Call,
    Put,
}

impl Right {
    pub fn letter(self) -> char {
        match self {
            Right::Call => 'C',
            Right::Put => 'P',
        }
    }

    pub fn intrinsic(self, spot: f64, strike: f64) -> f64 {
        match self {
            Right::Call => (spot - strike).max(0.0),
            Right::Put => (strike - spot).max(0.0),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    pub spot: f64,
    pub strike: f64,
    pub t: f64,
    pub r: f64,
    pub q: f64,
    pub vol: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Greeks {
    pub delta: f64,
    pub gamma: f64,
    /// Price change per 1 volatility point.
    pub vega: f64,
    /// Price change per calendar day.
    pub theta: f64,
}

pub fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

/// Cumulative normal, Hart's double-precision algorithm as given by West (2005).
pub fn norm_cdf(x: f64) -> f64 {
    let a = x.abs();
    let c = if a > 37.0 {
        0.0
    } else {
        let e = (-a * a / 2.0).exp();
        if a < 7.07106781186547 {
            let num = [
                3.52624965998911e-02,
                0.700383064443688,
                6.37396220353165,
                33.912866078383,
                112.079291497871,
                221.213596169931,
                220.206867912376,
            ];
            let den = [
                8.83883476483184e-02,
                1.75566716318264,
                16.064177579207,
                86.7807322029461,
                296.564248779674,
                637.333633378831,
                793.826512519948,
                440.413735824752,
            ];
            let horner = |cs: &[f64]| cs.iter().fold(0.0, |acc, c| acc * a + c);
            e * horner(&num) / horner(&den)
        } else {
            let b = a + 0.65;
            let b = a + 4.0 / b;
            let b = a + 3.0 / b;
            let b = a + 2.0 / b;
            let b = a + 1.0 / b;
            e / b / 2.506628274631
        }
    };
    if x > 0.0 {
        1.0 - c
    } else {
        c
    }
}

fn d1_d2(i: &Inputs) -> (f64, f64) {
    let sd = i.vol * i.t.sqrt();
    let d1 = ((i.spot / i.strike).ln() + (i.r - i.q + 0.5 * i.vol * i.vol) * i.t) / sd;
    (d1, d1 - sd)
}

pub fn price(right: Right, i: &Inputs) -> f64 {
    if i.t <= 0.0 || i.vol <= 0.0 {
        return right.intrinsic(i.spot, i.strike);
    }
    let (d1, d2) = d1_d2(i);
    let (dq, dr) = ((-i.q * i.t).exp(), (-i.r * i.t).exp());
    match right {
        Right::Call => i.spot * dq * norm_cdf(d1) - i.strike * dr * norm_cdf(d2),
        Right::Put => i.strike * dr * norm_cdf(-d2) - i.spot * dq * norm_cdf(-d1),
    }
}

pub fn greeks(right: Right, i: &Inputs) -> Greeks {
    if i.t <= 0.0 || i.vol <= 0.0 {
        return Greeks::default();
    }
    let (d1, d2) = d1_d2(i);
    let (dq, dr) = ((-i.q * i.t).exp(), (-i.r * i.t).exp());
    let sq = i.t.sqrt();
    let pdf = norm_pdf(d1);
    let common = -i.spot * dq * pdf * i.vol / (2.0 * sq);
    let (delta, theta) = match right {
        Right::Call => (
            dq * norm_cdf(d1),
            common - i.r * i.strike * dr * norm_cdf(d2) + i.q * i.spot * dq * norm_cdf(d1),
        ),
        Right::Put => (
            -dq * norm_cdf(-d1),
            common + i.r * i.strike * dr * norm_cdf(-d2) - i.q * i.spot * dq * norm_cdf(-d1),
        ),
    };
    Greeks {
        delta,
        gamma: dq * pdf / (i.spot * i.vol * sq),
        vega: i.spot * dq * pdf * sq / 100.0,
        theta: theta / 365.0,
    }
}

/// Solves for volatility by bisection, which cannot diverge. `None` when the
/// price is outside what any volatility in 0.1%..500% can produce.
pub fn implied_vol(
    right: Right,
    target: f64,
    spot: f64,
    strike: f64,
    t: f64,
    r: f64,
    q: f64,
) -> Option<f64> {
    if !target.is_finite() || target <= 0.0 || t <= 0.0 {
        return None;
    }
    let at = |vol| {
        price(
            right,
            &Inputs {
                spot,
                strike,
                t,
                r,
                q,
                vol,
            },
        )
    };
    let (mut lo, mut hi) = (0.001, 5.0);
    if target < at(lo) || target > at(hi) {
        return None;
    }
    for _ in 0..100 {
        let mid = 0.5 * (lo + hi);
        if at(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
        if hi - lo < 1e-9 {
            break;
        }
    }
    Some(0.5 * (lo + hi))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Inputs {
        Inputs {
            spot: 100.0,
            strike: 100.0,
            t: 1.0,
            r: 0.05,
            q: 0.0,
            vol: 0.2,
        }
    }

    #[test]
    fn normal_cdf_known_values() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-14);
        assert!((norm_cdf(1.96) - 0.9750021048517795).abs() < 1e-12);
        assert!((norm_cdf(-1.0) - 0.15865525393145707).abs() < 1e-12);
        assert!((norm_cdf(8.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn textbook_prices() {
        assert!((price(Right::Call, &base()) - 10.450583572185565).abs() < 1e-9);
        assert!((price(Right::Put, &base()) - 5.573526022256971).abs() < 1e-9);
    }

    #[test]
    fn put_call_parity_with_dividends() {
        let i = Inputs {
            spot: 90.0,
            strike: 105.0,
            t: 0.4,
            r: 0.03,
            q: 0.02,
            vol: 0.45,
        };
        let lhs = price(Right::Call, &i) - price(Right::Put, &i);
        let rhs = i.spot * (-i.q * i.t).exp() - i.strike * (-i.r * i.t).exp();
        assert!((lhs - rhs).abs() < 1e-10);
    }

    #[test]
    fn greeks_match_finite_differences() {
        for right in [Right::Call, Right::Put] {
            let i = Inputs {
                spot: 100.0,
                strike: 110.0,
                t: 0.5,
                r: 0.04,
                q: 0.01,
                vol: 0.3,
            };
            let g = greeks(right, &i);
            let h = 1e-3;
            let p = |x: Inputs| price(right, &x);
            let delta = (p(Inputs {
                spot: i.spot + h,
                ..i
            }) - p(Inputs {
                spot: i.spot - h,
                ..i
            })) / (2.0 * h);
            let gamma = (p(Inputs {
                spot: i.spot + h,
                ..i
            }) - 2.0 * p(i)
                + p(Inputs {
                    spot: i.spot - h,
                    ..i
                }))
                / (h * h);
            let vega = (p(Inputs {
                vol: i.vol + h,
                ..i
            }) - p(Inputs {
                vol: i.vol - h,
                ..i
            })) / (2.0 * h)
                / 100.0;
            let theta = -(p(Inputs { t: i.t + h, ..i }) - p(Inputs { t: i.t - h, ..i }))
                / (2.0 * h)
                / 365.0;
            assert!((g.delta - delta).abs() < 1e-6);
            assert!((g.gamma - gamma).abs() < 1e-4);
            assert!((g.vega - vega).abs() < 1e-6);
            assert!((g.theta - theta).abs() < 1e-6);
        }
    }

    #[test]
    fn implied_vol_round_trips_and_rejects_arbitrage() {
        for vol in [0.08, 0.25, 0.9, 2.0] {
            let i = Inputs {
                vol,
                strike: 95.0,
                ..base()
            };
            let got = implied_vol(
                Right::Put,
                price(Right::Put, &i),
                100.0,
                95.0,
                1.0,
                0.05,
                0.0,
            )
            .unwrap();
            assert!((got - vol).abs() < 1e-6);
        }
        assert!(implied_vol(Right::Call, 0.5, 120.0, 100.0, 0.1, 0.0, 0.0).is_none());
    }
}
