use engine::analyze::{analyze, Analysis, Context};
use engine::broker::{Broker, Order, PaperBroker};
use engine::data::{self, Market};
use engine::journal::{self, Entry};
use engine::news::KeywordScorer;
use engine::risk::{Book, Decision, Limits};
use engine::sample;
use engine::watchlist::{self, Theme, WATCHLIST};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "\
Paper-only options engine

USAGE:
  engine analyze <TICKER> [--paper] [--theme NAME] [--data DIR] [--journal FILE]
  engine scan [--theme NAME] [--data DIR] [--journal FILE]
  engine positions [--data DIR] [--journal FILE]
  engine gen-sample [--data DIR] [--seed N]

  --paper   record the approved trade in the paper journal
  --theme   tag news and journal rows with a theme, e.g. blue-wave

Create a file named KILL_SWITCH in this folder to block all new trades.";

#[derive(Default)]
struct Args {
    positional: Vec<String>,
    paper: bool,
    theme: Option<String>,
    data: Option<PathBuf>,
    journal: Option<PathBuf>,
    seed: Option<u64>,
}

fn parse(raw: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = raw.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--paper" => a.paper = true,
            "--theme" => a.theme = Some(value()?),
            "--data" => a.data = Some(value()?.into()),
            "--journal" => a.journal = Some(value()?.into()),
            "--seed" => {
                a.seed = Some(
                    value()?
                        .parse()
                        .map_err(|_| "--seed needs a whole number")?,
                )
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.positional.push(s.to_string()),
        }
    }
    Ok(a)
}

fn sample_banner(m: &Market) {
    if m.is_sample {
        println!("*** SAMPLE DATA: invented prices and news, not real market information ***\n");
    }
}

fn print_analysis(a: &Analysis, m: &Market, elapsed_ms: f64) {
    let s = &a.signals;
    println!(
        "{} ({}) as of {}   spot {:.2}",
        a.member.ticker, a.member.group, m.as_of_date, s.spot
    );
    println!("\nVolatility");
    println!(
        "  ATM IV {:.1}%   realized 20d {:.1}%   IV rank {:.0}%",
        s.atm_iv * 100.0,
        s.realized_vol * 100.0,
        s.iv_rank * 100.0
    );
    println!(
        "  skew {:+.1} pts   implied move {:.1}%",
        s.skew * 100.0,
        s.implied_move * 100.0
    );
    match s.days_to_earnings {
        Some(d) => println!("  earnings in {d} days"),
        None => println!("  no earnings date on file"),
    }
    println!("\nFactors (60 days)");
    println!(
        "  beta vs SPY {:.2}   beta vs {} {:.2}",
        s.beta_market, a.member.hedge, s.beta_sector
    );
    println!(
        "  alpha {:+.1}%/yr   5-day residual {:+.1}%",
        s.alpha_annual * 100.0,
        s.residual_5d * 100.0
    );

    let n = &a.news;
    println!("\nNews");
    println!(
        "  direction {:+.2}   confidence {:.2}   {} stories, {} repeats removed, {} not yet available",
        n.direction, n.confidence, n.stories.len(), n.duplicates_removed, n.not_yet_available
    );
    for st in &n.stories {
        let tag = if st.theme_match { " [theme]" } else { "" };
        println!(
            "  {:+.2}  {:>4.0}h ago  {}{tag}",
            st.score, st.age_hours, st.headline
        );
    }

    println!("\nForecast");
    println!(
        "  expected move {:+.2}%   volatility {:.1}%",
        a.forecast.expected_move * 100.0,
        a.forecast.vol * 100.0
    );

    println!("\nTrade ({} structures considered)", a.considered);
    if let Some(c) = &a.best {
        println!("  {}, {} days: {}", c.name, c.expiry_days, c.legs_code());
        let kind = if c.entry >= 0.0 { "debit" } else { "credit" };
        println!(
            "  {kind} ${:.2}/sh   max loss ${:.2}/sh   EV ${:+.2}/sh",
            c.entry.abs(),
            c.max_loss,
            c.ev
        );
        println!(
            "  edge {:.1}% of risk   model P(profit) {:.0}%   delta {:+.2}",
            c.edge() * 100.0,
            c.prob_profit * 100.0,
            c.delta
        );
    }
    match &a.decision {
        Decision::Approved {
            contracts,
            risk,
            hedge,
        } => {
            println!("\nRisk check: APPROVED  {contracts} contracts, ${risk:.0} at risk");
            if let Some(h) = hedge {
                println!(
                    "  hedge: {} {} {} at about {:.2}",
                    if h.shares < 0 { "short" } else { "buy" },
                    h.shares.abs(),
                    h.ticker,
                    h.price
                );
            }
        }
        Decision::Blocked(why) => println!("\nRisk check: NO TRADE  {why}"),
    }
    println!("\nAnalysis took {elapsed_ms:.2} ms");
}

struct Session {
    market: Market,
    book: Book,
    limits: Limits,
    theme: Option<Theme>,
    kill: bool,
    journal: PathBuf,
}

fn open(a: &Args) -> Result<Session, String> {
    let dir = a.data.clone().unwrap_or_else(|| "data/sample".into());
    let market = data::load(&dir).map_err(|e| {
        if dir.join("prices.csv").exists() {
            e
        } else {
            format!("{e}\nRun `engine gen-sample` first.")
        }
    })?;
    let journal = a
        .journal
        .clone()
        .unwrap_or_else(|| "journal/paper_trades.csv".into());
    let (book, _) = journal::load(&journal, &market)?;
    let theme = match &a.theme {
        Some(n) => Some(watchlist::theme(n).ok_or(format!("unknown theme '{n}'"))?),
        None => None,
    };
    Ok(Session {
        market,
        book,
        limits: Limits::default(),
        theme,
        kill: Path::new("KILL_SWITCH").exists(),
        journal,
    })
}

impl Session {
    fn context(&self) -> Context<'_> {
        Context {
            market: &self.market,
            scorer: &KeywordScorer,
            theme: self.theme.as_ref(),
            limits: &self.limits,
            book: &self.book,
            kill_switch: self.kill,
        }
    }
}

fn cmd_analyze(a: &Args) -> Result<(), String> {
    let ticker = a
        .positional
        .get(1)
        .ok_or("analyze needs a ticker, e.g. engine analyze INTC")?;
    let s = open(a)?;
    sample_banner(&s.market);
    let start = Instant::now();
    let result = analyze(&s.context(), ticker)?;
    print_analysis(&result, &s.market, start.elapsed().as_secs_f64() * 1000.0);

    if a.paper {
        match (&result.decision, &result.best) {
            (
                Decision::Approved {
                    contracts,
                    risk,
                    hedge,
                },
                Some(c),
            ) => {
                let fill = PaperBroker.submit(&Order {
                    candidate: c,
                    contracts: *contracts,
                    hedge: hedge.as_ref(),
                })?;
                journal::append(
                    &s.journal,
                    &Entry {
                        analysis: &result,
                        fill: &fill,
                        risk: *risk,
                        hedge: hedge.as_ref(),
                        theme: a.theme.as_deref().unwrap_or(""),
                        as_of: &s.market.as_of_date,
                        is_sample: s.market.is_sample,
                    },
                )?;
                println!("Paper trade recorded in {}", s.journal.display());
            }
            _ => println!("No paper trade recorded."),
        }
    }
    Ok(())
}

fn cmd_scan(a: &Args) -> Result<(), String> {
    let s = open(a)?;
    sample_banner(&s.market);
    let start = Instant::now();
    let ctx = s.context();
    let mut tickers: Vec<String> = s.market.chains.keys().cloned().collect();
    tickers.sort();
    let mut rows = Vec::new();
    for t in &tickers {
        match analyze(&ctx, t) {
            Ok(r) => rows.push(r),
            Err(e) => println!("skipped {t}: {e}"),
        }
    }
    rows.sort_by(|x, y| {
        let edge = |r: &Analysis| r.best.as_ref().map_or(f64::MIN, |c| c.edge());
        edge(y).total_cmp(&edge(x))
    });
    println!(
        "{:<6} {:<11} {:>6} {:>6} {:>6}  {:<25} {:>6}  decision",
        "ticker", "group", "IV", "rank", "news", "best structure", "edge"
    );
    for r in &rows {
        let (name, edge) = r.best.as_ref().map_or(("-", "-".to_string()), |c| {
            (c.name, format!("{:.1}%", c.edge() * 100.0))
        });
        let decision = match &r.decision {
            Decision::Approved { contracts, .. } => format!("approved, {contracts} contracts"),
            Decision::Blocked(w) => format!("no trade: {w}"),
        };
        println!(
            "{:<6} {:<11} {:>5.1}% {:>5.0}% {:>+6.2}  {:<25} {:>6}  {decision}",
            r.member.ticker,
            r.member.group,
            r.signals.atm_iv * 100.0,
            r.signals.iv_rank * 100.0,
            r.news.direction,
            name,
            edge
        );
    }
    println!(
        "\nScanned {} tickers in {:.2} ms",
        rows.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn cmd_positions(a: &Args) -> Result<(), String> {
    let s = open(a)?;
    sample_banner(&s.market);
    let (book, marked) = journal::load(&s.journal, &s.market)?;
    if marked.is_empty() {
        println!("No open paper positions.");
        return Ok(());
    }
    println!(
        "{:<6} {:<25} {:>9} {:>9} {:>10}",
        "ticker", "structure", "contracts", "risk", "open PnL"
    );
    for p in &marked {
        let pnl = p.pnl.map_or("no quote".to_string(), |v| format!("{v:+.0}"));
        println!(
            "{:<6} {:<25} {:>9} {:>9.0} {:>10}",
            p.ticker, p.structure, p.contracts, p.risk, pnl
        );
    }
    println!(
        "\nTotal at risk ${:.0}   open PnL ${:+.0} (marked at exit prices)",
        book.total_risk(),
        book.unrealized_pnl
    );
    Ok(())
}

fn run() -> Result<(), String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let a = parse(&raw)?;
    match a.positional.first().map(String::as_str) {
        Some("analyze") => cmd_analyze(&a),
        Some("scan") => cmd_scan(&a),
        Some("positions") => cmd_positions(&a),
        Some("gen-sample") => {
            let dir = a.data.clone().unwrap_or_else(|| "data/sample".into());
            sample::generate(&dir, a.seed.unwrap_or(42))?;
            println!(
                "Wrote synthetic data for {} stocks to {}",
                WATCHLIST.len(),
                dir.display()
            );
            Ok(())
        }
        _ => {
            println!("{USAGE}");
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
