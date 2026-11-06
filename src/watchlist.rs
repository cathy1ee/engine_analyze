//! Default watchlist, hedge instruments and theme definitions.

pub const BENCHMARK: &str = "SPY";

#[derive(Clone, Debug)]
pub struct Member {
    pub ticker: String,
    pub group: &'static str,
    pub hedge: &'static str,
}

pub const WATCHLIST: &[(&str, &str, &str)] = &[
    ("XOM", "oil", "XLE"),
    ("COP", "oil", "XLE"),
    ("MPC", "refiners", "XLE"),
    ("VLO", "refiners", "XLE"),
    ("FSLR", "renewables", "TAN"),
    ("ENPH", "renewables", "TAN"),
    ("NEE", "utilities", "XLU"),
    ("DUK", "utilities", "XLU"),
    ("AMD", "chips", "SMH"),
    ("INTC", "chips", "SMH"),
    ("NVDA", "chips", "SMH"),
];

pub const HEDGES: &[&str] = &["SPY", "XLE", "TAN", "XLU", "SMH"];

/// Any ticker works: unknown names fall back to a market hedge.
pub fn member(ticker: &str) -> Member {
    let t = ticker.to_uppercase();
    match WATCHLIST.iter().find(|m| m.0 == t) {
        Some(m) => Member {
            ticker: t,
            group: m.1,
            hedge: m.2,
        },
        None => Member {
            ticker: t,
            group: "other",
            hedge: BENCHMARK,
        },
    }
}

/// A theme tags headlines and journal rows so its results can be measured
/// separately. It never changes a forecast: that has to be earned with evidence.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub name: &'static str,
    pub keywords: &'static [&'static str],
}

pub const THEMES: &[Theme] = &[Theme {
    name: "blue-wave",
    keywords: &[
        "oil",
        "crude",
        "brent",
        "iran",
        "congress",
        "permitting",
        "pipeline",
        "energy bill",
        "clean energy",
        "transmission",
        "data center",
        "midterm",
    ],
}];

pub fn theme(name: &str) -> Option<Theme> {
    THEMES.iter().copied().find(|t| t.name == name)
}
