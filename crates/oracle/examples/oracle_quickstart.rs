//! Gate an order on a Pyth price: parse the account bytes, check age, then
//! check the order price against a ±20% band around the mark.
//!
//! cargo run -p openhl-oracle --example oracle_quickstart

use openhl_oracle::{check_fresh, pyth_v2, sanity_band, within_band};
use openhl_state::{
    PYTH_V2_MIN_LEN, PYTH_V2_PRICE_UPDATE_DISCRIMINATOR, PYTH_V2_VERIFICATION_FULL,
};

const MAX_AGE_SLOTS: u64 = 25;
const BAND_BPS: u64 = 2_000;

fn main() {
    // A Full-verification PriceUpdateV2 posted at slot 1_000, price 60_000 (expo 0).
    let mut account = [0u8; PYTH_V2_MIN_LEN];
    account[..8].copy_from_slice(&PYTH_V2_PRICE_UPDATE_DISCRIMINATOR);
    account[40] = PYTH_V2_VERIFICATION_FULL;
    account[73..81].copy_from_slice(&60_000i64.to_le_bytes()); // price
    account[125..133].copy_from_slice(&1_000u64.to_le_bytes()); // posted_slot

    let update = pyth_v2::parse(&account).expect("a PriceUpdateV2");
    let mark = update.price as u64;
    let (low, high) = sanity_band(mark, BAND_BPS);
    println!(
        "{:?} price {} posted at slot {}, band [{low}, {high}]",
        update.verification_level, update.price, update.posted_slot
    );

    for now_slot in [1_010, 1_030] {
        match check_fresh(update.posted_slot, now_slot, MAX_AGE_SLOTS) {
            Ok(()) => println!("slot {now_slot}: fresh"),
            Err(s) => println!(
                "slot {now_slot}: stale ({} slots, max {})",
                s.age, s.max_age
            ),
        }
    }
    for price in [55_000, 75_000] {
        let verdict = if within_band(price, mark, BAND_BPS) {
            "accepted"
        } else {
            "rejected"
        };
        println!("order at {price}: {verdict}");
    }
}
