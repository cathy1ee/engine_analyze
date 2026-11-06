//! Order execution. `PaperBroker` is the only implementation: this crate has
//! no code that can reach a real brokerage.
use crate::risk::Hedge;
use crate::selection::Candidate;

pub struct Order<'a> {
    pub candidate: &'a Candidate,
    pub contracts: u32,
    pub hedge: Option<&'a Hedge>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    /// Per share including commission: bought at the ask, sold at the bid.
    pub price: f64,
    pub contracts: u32,
    pub hedge_price: Option<f64>,
}

pub struct CloseOrder {
    /// Per share including commission, at the prices the position can exit at.
    pub value: f64,
    pub hedge_price: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CloseFill {
    pub value: f64,
    pub hedge_price: Option<f64>,
}

pub trait Broker {
    fn submit(&mut self, order: &Order) -> Result<Fill, String>;
    fn close(&mut self, order: &CloseOrder) -> Result<CloseFill, String>;
}

const HEDGE_SLIPPAGE: f64 = 0.0002;

/// Unwinding a hedge also fills slightly against us: sell lower, buy back higher.
pub fn hedge_exit_price(price: f64, shares: f64) -> f64 {
    price * (1.0 - HEDGE_SLIPPAGE * shares.signum())
}

#[derive(Default)]
pub struct PaperBroker;

impl Broker for PaperBroker {
    fn submit(&mut self, o: &Order) -> Result<Fill, String> {
        if o.contracts == 0 {
            return Err("order has no contracts".into());
        }
        Ok(Fill {
            price: o.candidate.entry,
            contracts: o.contracts,
            hedge_price: o
                .hedge
                .map(|h| h.price * (1.0 + HEDGE_SLIPPAGE * (h.shares as f64).signum())),
        })
    }

    fn close(&mut self, o: &CloseOrder) -> Result<CloseFill, String> {
        Ok(CloseFill {
            value: o.value,
            hedge_price: o.hedge_price,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hedges_fill_slightly_against_us() {
        let c = Candidate {
            name: "t",
            legs: vec![],
            expiry_days: 30,
            entry: 2.0,
            max_loss: 2.0,
            ev: 0.1,
            prob_profit: 0.5,
            delta: 0.5,
        };
        let h = Hedge {
            ticker: "SMH".into(),
            shares: -100,
            price: 100.0,
        };
        let f = PaperBroker
            .submit(&Order {
                candidate: &c,
                contracts: 2,
                hedge: Some(&h),
            })
            .unwrap();
        assert_eq!(f.price, 2.0);
        assert!(f.hedge_price.unwrap() < 100.0);
        assert!(PaperBroker
            .submit(&Order {
                candidate: &c,
                contracts: 0,
                hedge: None
            })
            .is_err());
    }
}
