//! Generates clearly marked synthetic data so the engine runs without a data
//! provider. Prices, quotes and headlines here are invented, not market data.
use crate::csv::quote;
use crate::dates::{days_from_civil, format_date};
use crate::pricing::{price, Inputs, Right, DIV_YIELD, RATE};
use crate::watchlist::{HEDGES, WATCHLIST};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

const DAYS: usize = 300;
const AS_OF: (i64, i64, i64) = (2026, 11, 5);

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn normal(&mut self) -> f64 {
        let u = self.next().max(1e-12);
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * self.next()).cos()
    }
}

fn weekdays() -> Vec<String> {
    let mut d = days_from_civil(AS_OF.0, AS_OF.1, AS_OF.2);
    let mut out = Vec::new();
    while out.len() < DAYS {
        if (d + 4).rem_euclid(7) % 6 != 0 {
            out.push(format_date(d));
        }
        d -= 1;
    }
    out.reverse();
    out
}

const NEWS: &[(&str, &str, &str)] = &[
    (
        "2026-11-05T14:10:00Z",
        "INTC",
        "SAMPLE: Intel wins foundry contract with strong demand outlook",
    ),
    (
        "2026-11-05T14:40:00Z",
        "INTC",
        "SAMPLE: Intel wins foundry contract with strong demand outlook, analysts say",
    ),
    (
        "2026-11-06T09:00:00Z",
        "INTC",
        "SAMPLE: Future story that must be ignored because Intel beats estimates",
    ),
    (
        "2026-11-05T15:00:00Z",
        "NVDA",
        "SAMPLE: Nvidia supplier warns of data center delays",
    ),
    (
        "2026-11-04T18:00:00Z",
        "AMD",
        "SAMPLE: AMD raises guidance on strong server demand",
    ),
    (
        "2026-11-05T13:00:00Z",
        "XOM",
        "SAMPLE: Exxon falls as crude plunges on Iran peace hopes",
    ),
    (
        "2026-11-05T12:30:00Z",
        "VLO",
        "SAMPLE: Valero downgraded after refining margins weaken",
    ),
    (
        "2026-11-05T16:00:00Z",
        "FSLR",
        "SAMPLE: First Solar soars on clean energy transmission bill hopes",
    ),
    (
        "2026-11-04T20:00:00Z",
        "NEE",
        "SAMPLE: NextEra upgraded on transmission buildout",
    ),
    (
        "2026-11-05T11:00:00Z",
        "ENPH",
        "SAMPLE: Enphase jumps on clean energy optimism",
    ),
];

const EARNINGS: &[(&str, u32)] = &[
    ("NVDA", 14),
    ("AMD", 82),
    ("INTC", 77),
    ("XOM", 85),
    ("FSLR", 90),
];

pub fn generate(dir: &Path, seed: u64) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut rng = Rng(seed);
    let dates = weekdays();

    let sectors = ["oil", "refiners", "renewables", "utilities", "chips"];
    let market: Vec<f64> = (0..DAYS).map(|_| 0.0004 + 0.009 * rng.normal()).collect();
    let factors: Vec<Vec<f64>> = sectors
        .iter()
        .map(|_| (0..DAYS).map(|_| 0.008 * rng.normal()).collect())
        .collect();
    let sector_index = |g: &str| sectors.iter().position(|s| *s == g).unwrap_or(0);

    let mut series: Vec<(String, Vec<f64>, f64, u32)> = Vec::new();
    for h in HEDGES {
        let (beta, group) = match *h {
            "XLE" => (0.8, Some("oil")),
            "TAN" => (1.2, Some("renewables")),
            "XLU" => (0.5, Some("utilities")),
            "SMH" => (1.3, Some("chips")),
            _ => (1.0, None),
        };
        let r: Vec<f64> = (0..DAYS)
            .map(|i| {
                beta * market[i]
                    + group.map_or(0.0, |g| factors[sector_index(g)][i])
                    + 0.001 * rng.normal()
            })
            .collect();
        series.push((h.to_string(), r, 0.0, 0));
    }
    for (t, g, _) in WATCHLIST {
        let beta = 0.7 + 0.8 * rng.next();
        let idio = 0.008 + 0.012 * rng.next();
        let r: Vec<f64> = (0..DAYS)
            .map(|i| beta * market[i] + factors[sector_index(g)][i] + idio * rng.normal())
            .collect();
        // ENPH is deliberately thin so the liquidity filter has something to reject.
        let volume = if *t == "ENPH" { 20 } else { 800 };
        series.push((t.to_string(), r, 0.25 + 0.2 * rng.next(), volume));
    }

    let mut prices = String::from("date,ticker,close,atm_iv\n");
    let mut options =
        String::from("ticker,expiry_days,strike,right,bid,ask,volume,open_interest\n");
    for (ticker, rets, base_iv, volume) in &series {
        let mut close = 100.0;
        let mut iv = 0.0;
        for (i, r) in rets.iter().enumerate() {
            close *= r.exp();
            let wave = 1.0 + 0.25 * (i as f64 / 35.0).sin();
            iv = (base_iv * wave * (1.0 + 0.03 * rng.normal())).max(0.0);
            writeln!(prices, "{},{ticker},{close:.2},{iv:.4}", dates[i])
                .map_err(|e| e.to_string())?;
        }
        if *volume == 0 {
            continue;
        }
        for days in [21u32, 45] {
            for step in -8i32..=8 {
                let strike = (close * (1.0 + 0.025 * step as f64) * 2.0).round() / 2.0;
                let vol = (iv - 0.2 * (strike / close).ln() + 0.005 * rng.normal()).max(0.05);
                let i = Inputs {
                    spot: close,
                    strike,
                    t: days as f64 / 365.0,
                    r: RATE,
                    q: DIV_YIELD,
                    vol,
                };
                for right in [Right::Call, Right::Put] {
                    let mid = price(right, &i);
                    let half = (mid * (0.01 + 0.03 * rng.next())).max(0.01);
                    let (bid, ask) = ((mid - half).max(0.0), mid + half);
                    writeln!(
                        options,
                        "{ticker},{days},{strike},{},{bid:.2},{ask:.2},{volume},{}",
                        right.letter(),
                        volume * 5
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }
    }

    let mut news = String::from("available_at,ticker,source,headline\n");
    for (at, t, h) in NEWS {
        writeln!(news, "{at},{t},sample,{}", quote(h)).map_err(|e| e.to_string())?;
    }
    let mut earnings = String::from("ticker,days_to_earnings\n");
    for (t, d) in EARNINGS {
        writeln!(earnings, "{t},{d}").map_err(|e| e.to_string())?;
    }

    let write = |name: &str, text: &str| fs::write(dir.join(name), text).map_err(|e| e.to_string());
    write("prices.csv", &prices)?;
    write("options.csv", &options)?;
    write("news.csv", &news)?;
    write("earnings.csv", &earnings)?;
    write(
        "SAMPLE_DATA",
        "Everything in this folder is synthetic. It is not market data.\n",
    )
}
