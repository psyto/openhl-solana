//! A 60-second budget of $100K on a thin market: a burst fills it, the next
//! trade waits until the burst leaves the window.
use openhl_guard::{spend, used, Rolling};

fn main() {
    let (cap, window) = (100_000_000_000u64, 60u64); // $100K in micro-USD, 60 s
    let mut r = Rolling::default();
    for (t, n) in [(1_000u64, 60_000_000_000u64), (1_020, 40_000_000_000), (1_030, 1_000_000), (1_070, 50_000_000_000)] {
        let res = spend(&mut r, t, n, cap, window);
        println!("t={t} spend ${:>7} → {:?}, used ${}", n / 1_000_000, res.map(|_| "ok"), used(&r, t) / 1_000_000);
    }
}
