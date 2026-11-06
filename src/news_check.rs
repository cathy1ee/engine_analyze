//! Measures a news scorer against headlines you graded yourself. The scorer's
//! answer is hidden while grading, so it can't sway your judgement.
use crate::csv::{quote, Table};
use crate::news::NewsScorer;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, Write};
use std::path::Path;

/// Scores closer to zero than this count as neutral.
pub const NEUTRAL_BAND: f64 = 0.1;
/// Below this many graded headlines, the report warns that it's only a rough guide.
pub const ENOUGH_LABELS: usize = 200;
const LABELS_HEADER: &str = "headline,label";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Label {
    Bad = 0,
    Neutral = 1,
    Good = 2,
}

pub const LABELS: [Label; 3] = [Label::Bad, Label::Neutral, Label::Good];

impl Label {
    pub fn name(self) -> &'static str {
        match self {
            Label::Bad => "bad",
            Label::Neutral => "neutral",
            Label::Good => "good",
        }
    }

    fn parse(s: &str) -> Option<Label> {
        LABELS.into_iter().find(|l| l.name() == s)
    }

    pub fn from_score(score: f64) -> Label {
        if score >= NEUTRAL_BAND {
            Label::Good
        } else if score <= -NEUTRAL_BAND {
            Label::Bad
        } else {
            Label::Neutral
        }
    }
}

/// Headlines from any CSV with a `headline` column, without repeats.
pub fn load_headlines(path: &Path) -> Result<Vec<String>, String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in Table::read(path)?.iter() {
        let h = r.get("headline")?.to_string();
        if !h.is_empty() && seen.insert(h.clone()) {
            out.push(h);
        }
    }
    Ok(out)
}

/// Grades so far. If a headline was graded twice, the later grade counts.
pub fn load_labels(path: &Path) -> Result<HashMap<String, Label>, String> {
    let mut out = HashMap::new();
    if path.exists() {
        for r in Table::read(path)?.iter() {
            let v = r.get("label")?;
            let label = Label::parse(v).ok_or_else(|| r.err(&format!("unknown label '{v}'")))?;
            out.insert(r.get("headline")?.to_string(), label);
        }
    }
    Ok(out)
}

fn save_label(path: &Path, headline: &str, label: Label) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let new = !path.exists();
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let header = if new {
        format!("{LABELS_HEADER}\n")
    } else {
        String::new()
    };
    writeln!(f, "{header}{},{}", quote(headline), label.name()).map_err(|e| e.to_string())
}

/// Asks about each ungraded headline, saving after every answer so stopping
/// part way loses nothing. Returns how many were graded.
pub fn grade(
    headlines: &[String],
    labels_path: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<usize, String> {
    let done = load_labels(labels_path)?;
    let todo: Vec<&String> = headlines
        .iter()
        .filter(|h| !done.contains_key(*h))
        .collect();
    let say = |o: &mut dyn Write, text: String| write!(o, "{text}").map_err(|e| e.to_string());
    say(
        output,
        format!(
            "{} to grade. Judge each one for the stock it's about.\n",
            todo.len()
        ),
    )?;
    say(
        output,
        "g = good   b = bad   n = neutral   s = skip   q = quit\n".to_string(),
    )?;

    let mut graded = 0;
    'headlines: for (i, h) in todo.iter().enumerate() {
        say(output, format!("\n[{}/{}] {h}\n", i + 1, todo.len()))?;
        loop {
            say(output, "> ".to_string())?;
            output.flush().map_err(|e| e.to_string())?;
            let mut line = String::new();
            if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                break 'headlines;
            }
            let label = match line.trim().to_lowercase().as_str() {
                "g" => Label::Good,
                "b" => Label::Bad,
                "n" => Label::Neutral,
                "s" => continue 'headlines,
                "q" => break 'headlines,
                _ => {
                    say(output, "Please type g, b, n, s or q.\n".to_string())?;
                    continue;
                }
            };
            save_label(labels_path, h, label)?;
            graded += 1;
            break;
        }
    }
    Ok(graded)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Miss {
    pub headline: String,
    pub yours: Label,
    pub scorer: Label,
    pub score: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub total: usize,
    pub correct: usize,
    /// `matrix[yours][scorer]`, indexed bad, neutral, good.
    pub matrix: [[usize; 3]; 3],
    pub misses: Vec<Miss>,
}

impl Report {
    pub fn accuracy(&self) -> f64 {
        self.correct as f64 / self.total.max(1) as f64
    }

    /// What you'd score by always answering your most common grade. A scorer
    /// has to beat this to be doing anything useful.
    pub fn baseline(&self) -> f64 {
        let top = self
            .matrix
            .iter()
            .map(|r| r.iter().sum::<usize>())
            .max()
            .unwrap_or(0);
        top as f64 / self.total.max(1) as f64
    }

    /// Agreement beyond chance: 0 is no better than chance, 1 is perfect.
    pub fn kappa(&self) -> f64 {
        let n = self.total as f64;
        if n == 0.0 {
            return 0.0;
        }
        let chance: f64 = (0..3)
            .map(|i| {
                let row: usize = self.matrix[i].iter().sum();
                let col: usize = self.matrix.iter().map(|r| r[i]).sum();
                row as f64 / n * (col as f64 / n)
            })
            .sum();
        if chance >= 1.0 {
            return 0.0;
        }
        (self.accuracy() - chance) / (1.0 - chance)
    }

    /// 95% Wilson interval for the accuracy.
    pub fn interval(&self) -> (f64, f64) {
        let n = self.total as f64;
        if n == 0.0 {
            return (0.0, 1.0);
        }
        let (p, z) = (self.accuracy(), 1.96f64);
        let centre = p + z * z / (2.0 * n);
        let spread = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt();
        let scale = 1.0 + z * z / n;
        (
            ((centre - spread) / scale).max(0.0),
            ((centre + spread) / scale).min(1.0),
        )
    }

    /// Of the headlines the scorer called this, how many you agreed with.
    pub fn precision(&self, l: Label) -> Option<f64> {
        let i = l as usize;
        let called: usize = self.matrix.iter().map(|r| r[i]).sum();
        (called > 0).then(|| self.matrix[i][i] as f64 / called as f64)
    }

    /// Of the headlines you called this, how many the scorer caught.
    pub fn recall(&self, l: Label) -> Option<f64> {
        let i = l as usize;
        let yours: usize = self.matrix[i].iter().sum();
        (yours > 0).then(|| self.matrix[i][i] as f64 / yours as f64)
    }

    /// Calling good news bad or the reverse: the mistakes that lose money.
    pub fn opposite_calls(&self) -> usize {
        self.matrix[0][2] + self.matrix[2][0]
    }
}

/// Compares a scorer with your grades. Misses come worst first: opposite calls,
/// then the most confident scores.
pub fn evaluate(labels: &HashMap<String, Label>, scorer: &dyn NewsScorer) -> Report {
    let mut r = Report::default();
    let mut rows: Vec<(&String, &Label)> = labels.iter().collect();
    rows.sort_by_key(|r| r.0);
    for (headline, yours) in rows {
        let score = scorer.score(headline);
        let call = Label::from_score(score);
        r.total += 1;
        r.matrix[*yours as usize][call as usize] += 1;
        if call == *yours {
            r.correct += 1;
        } else {
            r.misses.push(Miss {
                headline: headline.clone(),
                yours: *yours,
                scorer: call,
                score,
            });
        }
    }
    let opposite = |m: &Miss| (m.yours as i32 - m.scorer as i32).abs() == 2;
    r.misses.sort_by(|a, b| {
        opposite(b)
            .cmp(&opposite(a))
            .then(b.score.abs().total_cmp(&a.score.abs()))
    });
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::news::KeywordScorer;
    use std::io::Cursor;

    struct Fixed;
    impl NewsScorer for Fixed {
        fn score(&self, h: &str) -> f64 {
            h.parse().unwrap()
        }
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("engine-news-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d.join("labels.csv")
    }

    fn labels(rows: &[(&str, Label)]) -> HashMap<String, Label> {
        rows.iter().map(|(h, l)| (h.to_string(), *l)).collect()
    }

    fn run_grade(headlines: &[&str], path: &Path, answers: &str) -> (usize, String) {
        let list: Vec<String> = headlines.iter().map(|h| h.to_string()).collect();
        let mut out = Vec::new();
        let n = grade(&list, path, &mut Cursor::new(answers.to_string()), &mut out).unwrap();
        (n, String::from_utf8(out).unwrap())
    }

    #[test]
    fn scores_map_to_labels_at_the_band_edges() {
        assert_eq!(Label::from_score(0.1), Label::Good);
        assert_eq!(Label::from_score(0.09), Label::Neutral);
        assert_eq!(Label::from_score(-0.09), Label::Neutral);
        assert_eq!(Label::from_score(-0.1), Label::Bad);
    }

    #[test]
    fn grading_saves_answers_skips_and_resumes() {
        let path = temp("resume");
        let (n, _) = run_grade(&["A", "B, with comma", "C"], &path, "g\ns\nq\n");
        assert_eq!(n, 1);
        let (n, out) = run_grade(&["A", "B, with comma", "C"], &path, "b\nn\n");
        assert_eq!(n, 2);
        assert!(
            out.starts_with("2 to grade"),
            "graded headlines aren't asked again"
        );

        let got = load_labels(&path).unwrap();
        assert_eq!(got["A"], Label::Good);
        assert_eq!(got["B, with comma"], Label::Bad);
        assert_eq!(got["C"], Label::Neutral);
    }

    #[test]
    fn bad_input_asks_again_and_end_of_input_stops_cleanly() {
        let path = temp("retry");
        let (n, out) = run_grade(&["A", "B"], &path, "maybe\nG\n");
        assert_eq!(n, 1);
        assert!(out.contains("Please type"));
    }

    #[test]
    fn the_scorers_answer_is_never_shown_while_grading() {
        let path = temp("blind");
        let (_, out) = run_grade(&["Maker beats estimates"], &path, "g\n");
        assert!(
            !out.contains("0.5") && !out.to_lowercase().contains("score"),
            "{out}"
        );
    }

    #[test]
    fn later_grades_replace_earlier_ones() {
        let path = temp("regrade");
        save_label(&path, "A", Label::Good).unwrap();
        save_label(&path, "A", Label::Bad).unwrap();
        assert_eq!(load_labels(&path).unwrap()["A"], Label::Bad);
    }

    #[test]
    fn unknown_labels_are_reported_with_their_line() {
        let path = temp("unknown");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "headline,label\nA,great\n").unwrap();
        assert!(load_labels(&path).unwrap_err().contains(":2:"));
    }

    #[test]
    fn report_numbers_match_a_hand_worked_example() {
        // Scorer: good, good, bad, neutral. You: good, bad, bad, good.
        let r = evaluate(
            &labels(&[
                ("0.5", Label::Good),
                ("0.6", Label::Bad),
                ("-0.5", Label::Bad),
                ("0.0", Label::Good),
            ]),
            &Fixed,
        );
        assert_eq!((r.total, r.correct), (4, 2));
        assert_eq!(r.accuracy(), 0.5);
        assert_eq!(r.baseline(), 0.5);
        assert_eq!(r.opposite_calls(), 1);
        assert_eq!(r.precision(Label::Good), Some(0.5));
        assert_eq!(r.recall(Label::Good), Some(0.5));
        assert_eq!(r.precision(Label::Bad), Some(1.0));
        assert_eq!(r.recall(Label::Neutral), None);
        // Chance agreement = 0.5*0.5 + 0.5*0.25 = 0.375.
        assert!((r.kappa() - 0.2).abs() < 1e-12);
    }

    #[test]
    fn opposite_calls_lead_the_miss_list() {
        let r = evaluate(
            &labels(&[("0.9", Label::Neutral), ("0.2", Label::Bad)]),
            &Fixed,
        );
        assert_eq!(r.misses[0].headline, "0.2");
    }

    #[test]
    fn perfect_and_empty_reports_stay_sensible() {
        let r = evaluate(
            &labels(&[("0.5", Label::Good), ("-0.5", Label::Bad)]),
            &Fixed,
        );
        assert_eq!(r.kappa(), 1.0);
        assert!(r.misses.is_empty());

        let all_good = evaluate(&labels(&[("0.5", Label::Good)]), &Fixed);
        assert_eq!(all_good.kappa(), 0.0);

        let empty = evaluate(&HashMap::new(), &Fixed);
        assert_eq!((empty.accuracy(), empty.kappa()), (0.0, 0.0));
        assert_eq!(empty.interval(), (0.0, 1.0));
    }

    #[test]
    fn wilson_interval_matches_a_known_value() {
        let r = Report {
            total: 100,
            correct: 80,
            ..Report::default()
        };
        let (lo, hi) = r.interval();
        assert!(
            (lo - 0.7112).abs() < 0.001 && (hi - 0.8666).abs() < 0.001,
            "{lo} {hi}"
        );
    }

    #[test]
    fn works_with_the_real_keyword_scorer() {
        let r = evaluate(
            &labels(&[("Maker beats estimates", Label::Good)]),
            &KeywordScorer,
        );
        assert_eq!(r.correct, 1);
    }

    #[test]
    fn headlines_load_from_any_csv_without_repeats() {
        let path = temp("csv");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "ticker,headline\nA,\"Same, one\"\nB,\"Same, one\"\nC,Other\n",
        )
        .unwrap();
        assert_eq!(load_headlines(&path).unwrap(), vec!["Same, one", "Other"]);
    }
}
