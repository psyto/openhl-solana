//! Accrue funding over three keeper updates and value a long against it.
//!
//! cargo run -p openhl-funding --example funding_quickstart

use openhl_funding::{accrue, clamp_rate, funding_pnl};

const MAX_RATE_PER_SEC: i64 = 1_000_000;

fn main() {
    let mut index = 0i64;
    let mut rate = 0i64;
    let mut last_ts = 0i64;
    let long_size = 1_000i64;
    let snapshot = index; // the long opens at t=0

    // (time, rate the keeper proposes)
    for (now, proposed) in [(60, 500_000), (120, 2_000_000), (180, -250_000)] {
        let step = accrue(index, rate, last_ts, now).expect("clock moves forward");
        index = step.cumulative;
        rate = clamp_rate(proposed, MAX_RATE_PER_SEC);
        last_ts = now;
        println!(
            "t={now:>3}s  accrued {:>11} → index {:>11}  next rate {:>9}{}",
            step.delta,
            index,
            rate,
            if rate != proposed { " (clamped)" } else { "" }
        );
    }
    println!(
        "long of {long_size}: funding pnl {}",
        funding_pnl(long_size, snapshot, index)
    );
}
