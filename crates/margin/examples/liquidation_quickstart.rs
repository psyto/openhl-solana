//! A 10x long checked as the mark falls, then liquidated.
//!
//! cargo run -p openhl-margin --example liquidation_quickstart

use openhl_margin::{
    equity, initial_margin, is_liquidatable, liquidation_split, maintenance_margin, notional,
};

const INITIAL_MARGIN_BPS: u64 = 1_000; // 10%
const MAINT_MARGIN_BPS: u64 = 500; // 5%
const PENALTY_BPS: u64 = 100; // 1% of notional
const FUND_SHARE_BPS: u64 = 5_000; // half the penalty to the insurance fund

fn main() {
    let (size, entry, collateral) = (10i64, 100_000u64, 100_000u64);
    let im = initial_margin(notional(size, entry), INITIAL_MARGIN_BPS);
    println!("open long {size} @ {entry}, collateral {collateral} (initial margin {im})");

    for mark in [100_000u64, 96_000, 95_000, 94_500] {
        let eq = equity(collateral, size, entry, mark, 0, 0);
        let mm = maintenance_margin(notional(size, mark), MAINT_MARGIN_BPS);
        let state = if is_liquidatable(eq, mm) {
            "LIQUIDATABLE"
        } else {
            "ok"
        };
        println!("mark {mark:>7}: equity {eq:>7}  maintenance {mm:>6}  {state}");
        if is_liquidatable(eq, mm) {
            let s = liquidation_split(eq, notional(size, mark), PENALTY_BPS, FUND_SHARE_BPS);
            println!(
                "liquidate: penalty {} → liquidator {}, insurance fund {}; owner gets {}",
                s.penalty, s.to_liquidator, s.to_insurance_fund, s.to_owner
            );
            break;
        }
    }
}
