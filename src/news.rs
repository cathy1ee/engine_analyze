//! News scoring. `NewsScorer` is the swap point: the keyword scorer here is the
//! baseline that any language-model scorer must beat before it earns its cost.
use crate::data::NewsItem;
use crate::watchlist::Theme;
use std::collections::HashSet;

pub const HALF_LIFE_HOURS: f64 = 24.0;
pub const MAX_AGE_HOURS: f64 = 72.0;
const DUPLICATE_SIMILARITY: f64 = 0.6;

pub trait NewsScorer {
    /// Direction of a headline for its stock, from -1 (bad) to +1 (good).
    fn score(&self, headline: &str) -> f64;
}

pub struct KeywordScorer;

const LEXICON: &[(&str, f64)] = &[
    ("beats", 0.6),
    ("beat", 0.6),
    ("raises guidance", 0.8),
    ("raises", 0.4),
    ("upgrade", 0.5),
    ("upgraded", 0.5),
    ("wins", 0.4),
    ("record", 0.3),
    ("surges", 0.5),
    ("soars", 0.5),
    ("jumps", 0.4),
    ("approval", 0.4),
    ("contract", 0.3),
    ("buyback", 0.3),
    ("strong", 0.3),
    ("misses", -0.6),
    ("miss", -0.6),
    ("cuts guidance", -0.8),
    ("cuts", -0.4),
    ("downgrade", -0.5),
    ("downgraded", -0.5),
    ("lawsuit", -0.4),
    ("probe", -0.5),
    ("investigation", -0.5),
    ("recall", -0.5),
    ("plunges", -0.5),
    ("falls", -0.3),
    ("warns", -0.5),
    ("delay", -0.4),
    ("delays", -0.4),
    ("weak", -0.3),
    ("layoffs", -0.2),
];

const NEGATIONS: &[&str] = &["not", "no", "never", "without", "fails", "failed"];

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

impl NewsScorer for KeywordScorer {
    fn score(&self, headline: &str) -> f64 {
        let toks = tokens(headline);
        let mut total = 0.0;
        let mut i = 0;
        while i < toks.len() {
            // Two-word phrases win over their single words.
            let hit = LEXICON
                .iter()
                .filter_map(|(k, w)| {
                    let parts: Vec<&str> = k.split(' ').collect();
                    let n = parts.len();
                    (i + n <= toks.len() && toks[i..i + n].iter().zip(&parts).all(|(a, b)| a == b))
                        .then_some((n, *w))
                })
                .max_by_key(|h| h.0);
            match hit {
                Some((n, w)) => {
                    let negated = toks[i.saturating_sub(2)..i]
                        .iter()
                        .any(|t| NEGATIONS.contains(&t.as_str()));
                    total += if negated { -w } else { w };
                    i += n;
                }
                None => i += 1,
            }
        }
        total.tanh()
    }
}

fn similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        a.intersection(b).count() as f64 / union as f64
    }
}

#[derive(Clone, Debug)]
pub struct ScoredStory {
    pub headline: String,
    pub source: String,
    pub age_hours: f64,
    pub score: f64,
    pub weight: f64,
    pub theme_match: bool,
}

#[derive(Clone, Debug, Default)]
pub struct NewsSignal {
    /// Recency-weighted average direction, -1 to +1.
    pub direction: f64,
    /// 0 with no scored evidence, rising towards 1 as evidence accumulates.
    pub confidence: f64,
    pub stories: Vec<ScoredStory>,
    pub duplicates_removed: usize,
    pub not_yet_available: usize,
}

pub fn analyze(
    items: &[NewsItem],
    ticker: &str,
    as_of: i64,
    scorer: &dyn NewsScorer,
    theme: Option<&Theme>,
) -> NewsSignal {
    let mut out = NewsSignal::default();
    let mut mine: Vec<&NewsItem> = items.iter().filter(|n| n.ticker == ticker).collect();
    mine.sort_by_key(|n| n.available_at);

    let mut seen: Vec<HashSet<String>> = Vec::new();
    for n in mine {
        // A trade decided at `as_of` must not see anything that arrived later.
        if n.available_at > as_of {
            out.not_yet_available += 1;
            continue;
        }
        let age_hours = (as_of - n.available_at) as f64 / 3600.0;
        if age_hours > MAX_AGE_HOURS {
            continue;
        }
        let words: HashSet<String> = tokens(&n.headline).into_iter().collect();
        if seen
            .iter()
            .any(|s| similarity(s, &words) >= DUPLICATE_SIMILARITY)
        {
            out.duplicates_removed += 1;
            continue;
        }
        seen.push(words);
        let lower = n.headline.to_lowercase();
        out.stories.push(ScoredStory {
            headline: n.headline.clone(),
            source: n.source.clone(),
            age_hours,
            score: scorer.score(&n.headline),
            weight: 0.5f64.powf(age_hours / HALF_LIFE_HOURS),
            theme_match: theme.is_some_and(|t| t.keywords.iter().any(|k| lower.contains(k))),
        });
    }

    let total_weight: f64 = out
        .stories
        .iter()
        .filter(|s| s.score != 0.0)
        .map(|s| s.weight)
        .sum();
    if total_weight > 0.0 {
        out.direction = out.stories.iter().map(|s| s.weight * s.score).sum::<f64>() / total_weight;
        let evidence: f64 = out.stories.iter().map(|s| s.weight * s.score.abs()).sum();
        out.confidence = 1.0 - (-evidence).exp();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(hours_ago: i64, ticker: &str, headline: &str) -> NewsItem {
        NewsItem {
            available_at: 1_000_000 - hours_ago * 3600,
            ticker: ticker.into(),
            source: "t".into(),
            headline: headline.into(),
        }
    }

    fn run(items: &[NewsItem]) -> NewsSignal {
        analyze(items, "INTC", 1_000_000, &KeywordScorer, None)
    }

    #[test]
    fn keyword_direction_and_negation() {
        let s = KeywordScorer;
        assert!(s.score("Intel beats estimates") > 0.0);
        assert!(s.score("Intel misses estimates") < 0.0);
        assert!(s.score("Intel did not beat estimates") < 0.0);
        assert_eq!(s.score("Intel holds annual meeting"), 0.0);
    }

    #[test]
    fn phrases_take_priority_over_single_words() {
        let s = KeywordScorer;
        assert!(s.score("Maker cuts guidance") < s.score("Maker cuts"));
    }

    #[test]
    fn rewrites_of_a_story_count_once() {
        let n = run(&[
            item(
                3,
                "INTC",
                "Intel beats estimates on strong data center demand",
            ),
            item(
                2,
                "INTC",
                "Intel beats estimates on strong data center demand, shares up",
            ),
            item(1, "INTC", "Regulator opens probe into chip maker"),
        ]);
        assert_eq!(n.duplicates_removed, 1);
        assert_eq!(n.stories.len(), 2);
        assert_eq!(
            n.stories[0].age_hours, 3.0,
            "the first report is the one kept"
        );
    }

    #[test]
    fn future_news_is_never_used() {
        let n = run(&[item(-1, "INTC", "Intel beats estimates")]);
        assert_eq!(n.not_yet_available, 1);
        assert!(n.stories.is_empty());
        assert_eq!(n.direction, 0.0);
    }

    #[test]
    fn newer_stories_count_for_more_and_old_ones_expire() {
        let n = run(&[
            item(1, "INTC", "Intel beats estimates"),
            item(48, "INTC", "Regulator opens probe into Intel"),
        ]);
        assert!(n.direction > 0.0);
        assert!(run(&[item(100, "INTC", "Intel beats estimates")])
            .stories
            .is_empty());
    }

    #[test]
    fn other_tickers_are_ignored_and_no_news_means_no_confidence() {
        let n = run(&[item(1, "AMD", "AMD beats estimates")]);
        assert_eq!(n.confidence, 0.0);
    }
}
