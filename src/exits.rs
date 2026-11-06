//! Rules for closing paper trades. Thresholds are measured against the
//! trade's maximum loss, so every structure is judged on the same scale.

#[derive(Clone, Debug)]
pub struct ExitRules {
    /// Close once profit reaches this share of the risk taken.
    pub take_profit: f64,
    /// Close once the loss reaches this share of the risk taken.
    pub stop_loss: f64,
    /// Close with this many days left, before expiry-week gamma and assignment risk.
    pub min_days_left: i64,
}

impl Default for ExitRules {
    fn default() -> Self {
        ExitRules {
            take_profit: 0.5,
            stop_loss: 0.5,
            min_days_left: 7,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Expired,
    StopLoss,
    TakeProfit,
    TimeExit,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Expired => "expired",
            Reason::StopLoss => "stop loss",
            Reason::TakeProfit => "take profit",
            Reason::TimeExit => "time exit",
        }
    }
}

/// `pnl` is `None` when the position cannot be priced. It is then held, since
/// closing on a made-up price would put a false result in the journal.
pub fn evaluate(rules: &ExitRules, risk: f64, pnl: Option<f64>, days_left: i64) -> Option<Reason> {
    let pnl = pnl?;
    if days_left <= 0 {
        Some(Reason::Expired)
    } else if pnl <= -rules.stop_loss * risk {
        Some(Reason::StopLoss)
    } else if pnl >= rules.take_profit * risk {
        Some(Reason::TakeProfit)
    } else if days_left <= rules.min_days_left {
        Some(Reason::TimeExit)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pnl: Option<f64>, days: i64) -> Option<Reason> {
        evaluate(&ExitRules::default(), 1_000.0, pnl, days)
    }

    #[test]
    fn thresholds_fire_at_exactly_the_limit() {
        assert_eq!(run(Some(499.0), 30), None);
        assert_eq!(run(Some(500.0), 30), Some(Reason::TakeProfit));
        assert_eq!(run(Some(-499.0), 30), None);
        assert_eq!(run(Some(-500.0), 30), Some(Reason::StopLoss));
        assert_eq!(run(Some(0.0), 8), None);
        assert_eq!(run(Some(0.0), 7), Some(Reason::TimeExit));
    }

    #[test]
    fn expiry_outranks_everything_and_stops_outrank_time() {
        assert_eq!(run(Some(900.0), 0), Some(Reason::Expired));
        assert_eq!(run(Some(-900.0), -3), Some(Reason::Expired));
        assert_eq!(run(Some(-900.0), 3), Some(Reason::StopLoss));
    }

    #[test]
    fn an_unpriced_position_is_held() {
        assert_eq!(run(None, 3), None);
        assert_eq!(run(None, 0), None);
    }
}
