//! Order tracking against a fake broker that can fail in the ways real ones do.
use engine::orders::{
    read_orders, BrokerOrder, BrokerStatus, Fill, GatewayError, Intent, OrderGateway, OrderLog,
    OrderManager, State, MAX_CONTRACTS, MAX_ORDERS_PER_DAY,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const NOON: i64 = 20_000 * 86400 + 43_200;

fn log_path() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!("engine-orders-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d.join("orders.csv")
}

#[derive(Clone, Copy, PartialEq)]
enum Reply {
    Accept,
    Reject,
    /// The order arrives, but the reply is lost.
    TimeoutArrived,
    /// The order never arrives, and the reply is lost.
    TimeoutLost,
}

struct Fake {
    reply: Reply,
    paper: bool,
    reachable: bool,
    /// Every order that reached the broker, by our ID.
    book: HashMap<String, BrokerOrder>,
    /// How many times each order reached the broker.
    arrivals: HashMap<String, usize>,
    cancels: Vec<String>,
    next_fill: usize,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            reply: Reply::Accept,
            paper: true,
            reachable: true,
            book: HashMap::new(),
            arrivals: HashMap::new(),
            cancels: vec![],
            next_fill: 0,
        }
    }
}

impl Fake {
    fn arrive(&mut self, id: &str) -> String {
        *self.arrivals.entry(id.to_string()).or_default() += 1;
        let broker_id = format!("B-{id}");
        self.book.insert(
            id.to_string(),
            BrokerOrder {
                broker_id: broker_id.clone(),
                status: BrokerStatus::Working,
                fills: vec![],
            },
        );
        broker_id
    }

    fn fill(&mut self, id: &str, quantity: u32, price: f64) {
        self.next_fill += 1;
        let fill_id = format!("F{}", self.next_fill);
        self.book.get_mut(id).unwrap().fills.push(Fill {
            fill_id,
            quantity,
            price,
        });
    }

    fn status(&mut self, id: &str, status: BrokerStatus) {
        self.book.get_mut(id).unwrap().status = status;
    }
}

impl OrderGateway for Fake {
    fn is_paper(&mut self) -> Result<bool, String> {
        if self.reachable {
            Ok(self.paper)
        } else {
            Err("connection lost".into())
        }
    }

    fn send(&mut self, i: &Intent) -> Result<String, GatewayError> {
        match self.reply {
            Reply::Accept => Ok(self.arrive(&i.id)),
            Reply::Reject => Err(GatewayError::Rejected("not enough buying power".into())),
            Reply::TimeoutArrived => {
                self.arrive(&i.id);
                Err(GatewayError::Uncertain("timed out".into()))
            }
            Reply::TimeoutLost => Err(GatewayError::Uncertain("timed out".into())),
        }
    }

    fn cancel(&mut self, broker_id: &str) -> Result<(), String> {
        self.cancels.push(broker_id.to_string());
        Ok(())
    }

    fn lookup(&mut self, id: &str) -> Result<Option<BrokerOrder>, String> {
        if self.reachable {
            Ok(self.book.get(id).cloned())
        } else {
            Err("connection lost".into())
        }
    }
}

fn intent(id: &str) -> Intent {
    Intent {
        id: id.into(),
        ticker: "TST".into(),
        structure: "long call".into(),
        legs: "+C100@30".into(),
        quantity: 5,
        limit: 2.5,
    }
}

/// Reopening the log for every step, as a restarted process would.
fn manager<'a>(path: &Path, fake: &'a mut Fake) -> OrderManager<'a> {
    OrderManager::new(OrderLog::open(path).unwrap(), fake)
}

fn state(path: &Path, id: &str) -> State {
    read_orders(path)
        .unwrap()
        .iter()
        .find(|o| o.intent.id == id)
        .unwrap()
        .state
}

#[test]
fn an_order_runs_from_sent_to_filled_with_partial_fills() {
    let (p, mut f) = (log_path(), Fake::default());
    assert_eq!(
        manager(&p, &mut f).submit(intent("A"), NOON, false),
        Ok(State::Sent)
    );

    f.fill("A", 2, 2.4);
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(state(&p, "A"), State::PartiallyFilled);

    f.fill("A", 3, 2.5);
    f.status("A", BrokerStatus::Filled);
    manager(&p, &mut f).sync(NOON).unwrap();
    let orders = read_orders(&p).unwrap();
    assert_eq!(orders[0].state, State::Filled);
    assert_eq!(orders[0].filled(), 5);
    assert!((orders[0].average_price().unwrap() - 2.46).abs() < 1e-12);
}

#[test]
fn syncing_repeatedly_never_counts_a_fill_twice() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    f.fill("A", 2, 2.4);
    for _ in 0..4 {
        manager(&p, &mut f).sync(NOON).unwrap();
    }
    assert_eq!(read_orders(&p).unwrap()[0].filled(), 2);
}

#[test]
fn a_definite_rejection_closes_the_order_and_trading_continues() {
    let (p, mut f) = (log_path(), Fake::default());
    f.reply = Reply::Reject;
    assert_eq!(
        manager(&p, &mut f).submit(intent("A"), NOON, false),
        Ok(State::Rejected)
    );
    f.reply = Reply::Accept;
    assert_eq!(
        manager(&p, &mut f).submit(intent("B"), NOON, false),
        Ok(State::Sent)
    );
}

#[test]
fn a_timeout_stops_trading_until_the_broker_confirms() {
    let (p, mut f) = (log_path(), Fake::default());
    f.reply = Reply::TimeoutArrived;
    assert_eq!(
        manager(&p, &mut f).submit(intent("A"), NOON, false),
        Ok(State::Unknown)
    );

    f.reply = Reply::Accept;
    let blocked = manager(&p, &mut f)
        .submit(intent("B"), NOON, false)
        .unwrap_err();
    assert!(blocked.contains("stopped"), "{blocked}");

    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(state(&p, "A"), State::Sent);
    assert_eq!(f.arrivals["A"], 1, "it was found, not sent again");
    assert_eq!(
        manager(&p, &mut f).submit(intent("B"), NOON, false),
        Ok(State::Sent)
    );
}

#[test]
fn an_order_lost_in_a_timeout_stays_blocked_until_a_person_resolves_it() {
    let (p, mut f) = (log_path(), Fake::default());
    f.reply = Reply::TimeoutLost;
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(
        state(&p, "A"),
        State::Unknown,
        "it could still be in flight"
    );
    assert!(!f.arrivals.contains_key("A"));

    manager(&p, &mut f)
        .resolve("A", "checked in TWS", NOON)
        .unwrap();
    assert_eq!(state(&p, "A"), State::Cancelled);
    f.reply = Reply::Accept;
    assert!(manager(&p, &mut f).submit(intent("B"), NOON, false).is_ok());
}

#[test]
fn a_crash_before_sending_never_sends_it_later() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f).plan(intent("A"), NOON, false).unwrap();
    // The process dies here. After a restart the broker has never heard of it.
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(state(&p, "A"), State::Cancelled);
    assert!(!f.arrivals.contains_key("A"));
    assert!(manager(&p, &mut f).send("A", NOON, false).is_err());
}

#[test]
fn a_crash_after_sending_recovers_without_a_second_order() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f).plan(intent("A"), NOON, false).unwrap();
    f.arrive("A"); // Sent, then the process dies before recording it.
    f.fill("A", 1, 2.5);
    manager(&p, &mut f).sync(NOON).unwrap();
    let o = &read_orders(&p).unwrap()[0];
    assert_eq!((o.state, o.filled()), (State::PartiallyFilled, 1));
    assert_eq!(o.broker_id.as_deref(), Some("B-A"));
    assert_eq!(f.arrivals["A"], 1);
}

#[test]
fn the_same_order_can_never_be_sent_twice() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    assert!(manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap_err()
        .contains("exists"));
    assert!(manager(&p, &mut f)
        .send("A", NOON, false)
        .unwrap_err()
        .contains("already"));
    assert_eq!(f.arrivals["A"], 1);
}

#[test]
fn a_live_account_is_refused_before_anything_is_recorded() {
    let (p, mut f) = (log_path(), Fake::default());
    f.paper = false;
    let e = manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap_err();
    assert!(e.contains("not a paper account"));
    assert!(read_orders(&p).unwrap().is_empty());
    assert!(f.arrivals.is_empty());
}

#[test]
fn the_account_is_checked_again_right_before_sending() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f).plan(intent("A"), NOON, false).unwrap();
    f.paper = false;
    assert!(manager(&p, &mut f).send("A", NOON, false).is_err());
    assert!(f.arrivals.is_empty());

    f.paper = true;
    f.reachable = false;
    assert!(manager(&p, &mut f)
        .send("A", NOON, false)
        .unwrap_err()
        .contains("confirm"));
    assert!(f.arrivals.is_empty());
}

#[test]
fn the_kill_switch_blocks_orders_but_not_cancels() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    assert!(manager(&p, &mut f)
        .submit(intent("B"), NOON, true)
        .unwrap_err()
        .contains("kill"));
    manager(&p, &mut f).plan(intent("C"), NOON, false).unwrap();
    assert!(manager(&p, &mut f)
        .send("C", NOON, true)
        .unwrap_err()
        .contains("kill"));
    manager(&p, &mut f).cancel("A").unwrap();
    assert_eq!(f.cancels, ["B-A"]);
}

#[test]
fn a_cancelled_order_keeps_the_fills_it_already_had() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    f.fill("A", 2, 2.4);
    f.status("A", BrokerStatus::Cancelled);
    manager(&p, &mut f).sync(NOON).unwrap();
    let o = &read_orders(&p).unwrap()[0];
    assert_eq!((o.state, o.filled()), (State::Cancelled, 2));
}

#[test]
fn bad_orders_are_refused() {
    let (p, mut f) = (log_path(), Fake::default());
    let mut m = manager(&p, &mut f);
    let cases = [
        Intent {
            quantity: 0,
            ..intent("A")
        },
        Intent {
            quantity: MAX_CONTRACTS + 1,
            ..intent("A")
        },
        Intent {
            limit: 0.0,
            ..intent("A")
        },
        Intent {
            limit: f64::NAN,
            ..intent("A")
        },
        Intent {
            id: "bad,id".into(),
            ..intent("A")
        },
        Intent {
            id: String::new(),
            ..intent("A")
        },
    ];
    for c in cases {
        assert!(m.submit(c.clone(), NOON, false).is_err(), "{c:?}");
    }
    assert!(m.orders().is_empty());
    assert!(
        m.submit(
            Intent {
                limit: -1.2,
                ..intent("A")
            },
            NOON,
            false
        )
        .is_ok(),
        "credits are allowed"
    );
}

#[test]
fn the_daily_order_cap_resets_the_next_day() {
    let (p, mut f) = (log_path(), Fake::default());
    for i in 0..MAX_ORDERS_PER_DAY {
        manager(&p, &mut f)
            .submit(intent(&format!("A{i}")), NOON, false)
            .unwrap();
    }
    assert!(manager(&p, &mut f)
        .submit(intent("X"), NOON, false)
        .unwrap_err()
        .contains("daily"));
    assert!(manager(&p, &mut f)
        .submit(intent("X"), NOON + 86400, false)
        .is_ok());
}

#[test]
fn too_many_fills_stop_trading_and_need_a_person() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    f.fill("A", 6, 2.5);
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(state(&p, "A"), State::Mismatch);
    assert!(manager(&p, &mut f)
        .submit(intent("B"), NOON, false)
        .is_err());
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(
        state(&p, "A"),
        State::Mismatch,
        "checking again doesn't clear it"
    );
}

#[test]
fn a_filled_status_without_the_fills_is_a_mismatch() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    f.fill("A", 2, 2.5);
    f.status("A", BrokerStatus::Filled);
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(state(&p, "A"), State::Mismatch);
}

#[test]
fn an_order_that_vanishes_from_the_broker_is_a_mismatch() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    f.book.clear();
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(state(&p, "A"), State::Mismatch);
}

#[test]
fn losing_the_connection_during_sync_changes_nothing() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    f.reachable = false;
    assert!(manager(&p, &mut f).sync(NOON).is_err());
    assert_eq!(state(&p, "A"), State::Sent);
}

#[test]
fn a_half_written_last_line_is_dropped_and_appends_stay_clean() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    let mut text = std::fs::read_to_string(&p).unwrap();
    text.push_str("86400,A,fi");
    std::fs::write(&p, &text).unwrap();

    assert_eq!(read_orders(&p).unwrap().len(), 1, "reports ignore it");
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        text,
        "reports don't change the file"
    );

    f.fill("A", 1, 2.5);
    manager(&p, &mut f).sync(NOON).unwrap();
    assert_eq!(read_orders(&p).unwrap()[0].filled(), 1);
    assert!(!std::fs::read_to_string(&p).unwrap().contains("86400,A,fi"));
}

#[test]
fn damage_in_the_middle_of_the_log_is_an_error_not_a_guess() {
    let (p, mut f) = (log_path(), Fake::default());
    manager(&p, &mut f)
        .submit(intent("A"), NOON, false)
        .unwrap();
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace(",sent,", ",nonsense,");
    std::fs::write(&p, text).unwrap();
    assert!(OrderLog::open(&p).is_err());
}

#[test]
fn awkward_text_survives_the_log() {
    let (p, mut f) = (log_path(), Fake::default());
    let i = Intent {
        structure: "bull \"put\", spread".into(),
        legs: "-P95@30|+P90@30".into(),
        ..intent("A")
    };
    manager(&p, &mut f).submit(i.clone(), NOON, false).unwrap();
    f.status(
        "A",
        BrokerStatus::Rejected("bad\nprice, \"too low\"".into()),
    );
    manager(&p, &mut f).sync(NOON).unwrap();
    let o = &read_orders(&p).unwrap()[0];
    assert_eq!(o.intent, i);
    assert_eq!(o.detail, "bad price, \"too low\"");
}

#[test]
fn ids_are_unique_and_readable() {
    let (p, mut f) = (log_path(), Fake::default());
    let mut m = manager(&p, &mut f);
    let first = m.next_id("INTC", NOON);
    assert!(first.ends_with("-INTC-1"), "{first}");
    m.submit(intent(&first), NOON, false).unwrap();
    assert_ne!(m.next_id("INTC", NOON), first);
}

/// Random failures and restarts. Whatever happens, an order never reaches the
/// broker twice, and the log ends up agreeing with the broker.
#[test]
fn chaos_never_duplicates_or_loses_an_order() {
    let (mut reached, mut filled) = (0, 0);
    for seed in 1..=300u64 {
        let (p, mut f) = (log_path(), Fake::default());
        let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15);
        let mut roll = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let mut made = 0;
        for step in 0..40 {
            f.reply = [
                Reply::Accept,
                Reply::Reject,
                Reply::TimeoutArrived,
                Reply::TimeoutLost,
            ][roll(4) as usize];
            f.reachable = roll(6) != 0;
            let now = NOON + step;
            match roll(5) {
                0 | 1 => {
                    made += 1;
                    let _ = manager(&p, &mut f).submit(intent(&format!("O{made}")), now, false);
                }
                2 => {
                    // Plan, then die, sometimes after the order got out.
                    made += 1;
                    let id = format!("O{made}");
                    if manager(&p, &mut f).plan(intent(&id), now, false).is_ok() && roll(2) == 0 {
                        f.arrive(&id);
                    }
                }
                3 => {
                    let ids: Vec<String> = f.book.keys().cloned().collect();
                    if !ids.is_empty() {
                        let id = ids[roll(ids.len() as u64) as usize].clone();
                        if f.book[&id].fills.iter().map(|x| x.quantity).sum::<u32>() < 5 {
                            f.fill(&id, 1, 2.5);
                        }
                    }
                }
                _ => {
                    let _ = manager(&p, &mut f).sync(now);
                    // A person clears anything the broker truly never received.
                    for o in read_orders(&p).unwrap() {
                        if o.state.blocks_trading() && !f.book.contains_key(&o.intent.id) {
                            let _ = manager(&p, &mut f).resolve(&o.intent.id, "checked", now);
                        }
                    }
                }
            }
        }
        f.reachable = true;
        manager(&p, &mut f).sync(NOON + 100).unwrap();

        assert!(
            f.arrivals.values().all(|n| *n == 1),
            "seed {seed}: duplicate order"
        );
        let orders = read_orders(&p).unwrap();
        for (id, b) in &f.book {
            let o = orders
                .iter()
                .find(|o| &o.intent.id == id)
                .unwrap_or_else(|| panic!("seed {seed}: lost {id}"));
            assert_eq!(
                o.filled(),
                b.fills.iter().map(|x| x.quantity).sum::<u32>(),
                "seed {seed}: {id}"
            );
        }
        reached += f.book.len();
        filled += orders.iter().filter(|o| o.filled() > 0).count();
        // Orders the broker never saw stay stuck or closed, and never count as filled.
        assert!(orders
            .iter()
            .filter(|o| !f.book.contains_key(&o.intent.id))
            .all(|o| o.filled() == 0));
    }
    println!("chaos: {reached} orders reached the broker, {filled} got fills");
    assert!(
        reached > 1000 && filled > 300,
        "too little coverage: {reached}, {filled}"
    );
}

#[test]
fn order_tracking_contains_no_brokerage_code() {
    let text = std::fs::read_to_string("src/orders.rs").unwrap();
    assert!(!text.contains("ibapi"));
    assert!(!std::fs::read_to_string("src/main.rs")
        .unwrap()
        .contains("OrderManager"));
}
