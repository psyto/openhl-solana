//! Rest asks at two prices, then cross them with one bid.
//!
//! cargo run -p openhl-slab --example slab_quickstart

use openhl_slab::{best, init, insert_order, match_order};
use openhl_state::{side, Slab};

fn main() {
    // On-chain this is `bytemuck::from_bytes_mut(&mut account_data)`.
    let mut slab: Box<Slab> = bytemuck::zeroed_box();
    init(&mut slab);
    println!("slab is {} bytes, one account", Slab::LEN);

    for (price, size) in [(101, 4), (100, 3), (100, 2)] {
        let id = insert_order(&mut slab, side::ASK, price, [7; 32], size).unwrap();
        println!("ask #{id}: {size} @ {price}");
    }
    println!("best ask: {}", best(&slab, false).unwrap().price);

    let out = match_order(&mut slab, side::BID, 101, 6, 8, |f| {
        println!(
            "fill {}: {} @ {} from #{} (taker left {})",
            f.n, f.size, f.price, f.maker_order_id, f.remaining
        );
    })
    .unwrap();
    println!(
        "{} fills, {} unfilled, {} orders resting",
        out.fills, out.remaining, slab.active_count
    );
}
