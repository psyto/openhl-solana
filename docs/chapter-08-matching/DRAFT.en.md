# Chapter 8 — Matching Engine Under CU Pressure

> Status: draft (v0.1).
> Companion code: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_match` at lines 1361–1489), [`scripts/match/src/main.rs`](../../scripts/match/src/main.rs).
> Builds on Chapter 7's `OrderBook` data structure.

---

## §8.0  Framing — what this chapter teaches

Chapter 7 hooked a slab implementation here; that promise moved to its own chapter (15) because the slab grew big enough to deserve one. This chapter is about the **CU cost shape** of matching against the flat-array book Chapter 7 shipped — measured precisely, with the three responses to CU exhaustion (raise the budget, paginate via `max_fills`, refactor to a slab) named and costed. The slab pseudocode in §8.4 is the design spec that Chapter 15's code implements.

So this chapter:

1. Walks the `Match` algorithm on the flat book.
2. Reads its CU cost shape from real logs and shows it is O(fills × N).
3. Demonstrates the three CU-pressure responses (budget raise, pagination, slab refactor) and explains which costs what.
4. Provides slab pseudocode + diagrams as the design spec; Chapter 15 ships the implementation.

---

## §8.1  The Match algorithm on a flat book

From `programs/openhl-core/src/lib.rs:1361–1489`. The handler takes four payload fields:

```text
[side u8][limit_price u64 LE][size u64 LE][max_fills u8]
```

`side` is the *taker's* side (a bid taker buys against asks, an ask taker sells against bids). `limit_price` is the worst price the taker will accept. `size` is total base units to take. `max_fills` is the per-instruction cap on how many resting maker orders to cross — the pagination knob.

The matching loop at lines 1420–1479:

```rust
let mut fills_done: u8 = 0;
while remaining > 0 && fills_done < max_fills {
    // (a) linear scan for best opposite-side resting order
    //     whose price is acceptable to the taker
    let mut best: Option<(usize, u64)> = None;
    for (i, slot) in book.slots.iter().enumerate() {
        if slot.size == 0 || slot.side != maker_side { continue; }
        let price_acceptable = match taker_side {
            side::BID => slot.price <= limit_price,
            side::ASK => slot.price >= limit_price,
            _ => unreachable!(),
        };
        if !price_acceptable { continue; }
        // ... "is this better than current best?" check ...
    }

    // (b) if no acceptable maker exists, stop
    let (maker_idx, fill_price) = match best { Some(b) => b, None => break };

    // (c) cross: min(taker_remaining, maker_remaining)
    let maker = &mut book.slots[maker_idx];
    let fill_size = remaining.min(maker.size);
    maker.size -= fill_size;
    remaining -= fill_size;
    fills_done += 1;

    // (d) if maker fully filled, vacate slot
    if maker.size == 0 {
        *maker = <Order as bytemuck::Zeroable>::zeroed();
        book.active_count = book.active_count.saturating_sub(1);
    }
}
```

Five steps per fill. Two things to absorb.

**Each fill costs a full O(N) scan.** Step (a) walks every slot in the book to find the best opposite-side order. This is the design decision in `OrderBook` from Chapter 7 — bids and asks share an unsorted slot pool. There is no shortcut. Finding the lowest ask requires looking at every slot.

**The work is multiplicative.** With `fills_done` fills in a single instruction call, total scan work is `fills_done × ORDER_CAPACITY`. With ORDER_CAPACITY = 32 and a 10-fill cross, that's 320 slot inspections plus the per-inspection comparison overhead. Each inspection is cheap (~30 CU), so 320 inspections is ~10 KCU just for the scan. The actual write work is constant.

The handler deliberately omits the *settlement* step. A real exchange would also move quote tokens from taker to maker for each fill (via SPL Token CPI). Each such CPI is ~3,000 CU. Inside a 10-fill match that adds 30,000 CU on top of the scan work, putting us at ~40 KCU minimum before any logging or program housekeeping. The default 200 KCU budget would still cover this — but the headroom is shrinking, and N = 32 is the *smallest* book worth talking about.

> **Exercise §8.1.** Pre-populate the book with 10 asks at increasing prices (e.g., 100, 101, 102, ...). Run `match-cli --side bid --limit-price 110 --size 50 --max-fills 5`. Trace the program log: which 5 makers does the matcher cross, in what order, and what is the resulting `taker_remaining`?

---

## §8.2  Reading the CU cost shape

Run the matcher with `--max-fills` varying from 1 to 16 against a half-full book (16 active makers, all bids, taker is an ask matching against them):

| max_fills | sim units_consumed | per-fill marginal |
|-----------|--------------------|-------------------|
|   1       |    ~ 5,000         |   ~ 3,500         |
|   2       |    ~ 8,500         |   ~ 3,500         |
|   4       |    ~16,000         |   ~ 3,750         |
|   8       |    ~30,500         |   ~ 3,600         |
|  16       |    ~58,500         |   ~ 3,500         |

Per-fill marginal cost is roughly constant at ~3.5 KCU for a 16-maker book. This breaks down into:

- ~1,000 CU per fill for the linear scan (16 slots × ~60 CU each)
- ~1,200 CU per fill for the maker mutation + slot zero
- ~1,000 CU per fill for the `msg!` log line documenting the fill
- ~300 CU per fill for loop housekeeping and the `Order` write

Doubling the book size doubles the scan portion but leaves the rest constant. At ORDER_CAPACITY = 32 with all slots active, per-fill marginal becomes ~5 KCU. At 256 slots (a more realistic book), it would be ~20 KCU — and a 10-fill cross would consume 200 KCU on scans alone, hitting the default budget before any work.

This is the cost shape the chapter exists to make visible. The flat matcher's per-fill cost grows linearly with the book size. The total per-instruction cost is `O(fills × N)`. Both factors are knobs you might want to push (more fills per call, bigger book). Push either far enough and the budget breaks.

**What the SDK hides:** Anchor's program logs include the same "consumed N of M compute units" line because it comes from the runtime, not the user code. But Anchor does not surface a typed `units_consumed` field anywhere — you read it from the simulation result like we do here, with `sim.value.units_consumed`.

> **Exercise §8.2.** With the book at 30/32 capacity, run the matcher with `--max-fills 30`. The simulation should report units_consumed near or above 200,000. Now add `--cu-limit 400000` and re-run. Does the on-chain commit succeed? At what `max_fills` does it stop succeeding even at `--cu-limit 1400000` (the network maximum)?

---

## §8.3  Three responses to CU pressure

When a matcher (or any handler) starts pressing against the budget, you have three real options. Each has costs.

### (1) Raise the per-tx compute unit limit

The easiest fix and the worst long-term answer. Prepend a `ComputeBudgetInstruction::set_compute_unit_limit(N)` to your transaction, with N up to 1,400,000. From Chapter 4, we know this works for any single transaction. From `scripts/match/src/main.rs`:

```rust
if let Some(limit) = cli.cu_limit {
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
}
instructions.push(match_ix);
```

**Cost:** Priority fees scale with the requested limit, not the actual consumption. A transaction that requests 1.4M CU and uses 100K still pays the priority fee against a 1.4M ceiling. At network capacity (~50 K-CU/slot at high contention), large requests can also starve other transactions in the same slot.

**When it's the right answer:** for instructions that genuinely need >200K CU one time per call (initial setup, occasional bulk operations) and where priority fee dilution is acceptable.

### (2) Paginate via `max_fills`

The matcher already supports this. Cap the work per call, expose the cap to the caller, and let clients iterate until done:

```text
match --max-fills 8
match --max-fills 8     # next page
match --max-fills 8     # next page
...
```

Each call fits comfortably in the budget. Total network cost is the same (you do the same matching work either way), but it's spread across multiple transactions instead of one.

**Cost:** Multiple round trips. Risk that another transaction modifies the book between your matcher pages (you lose atomicity across pages — book state can change between page N and page N+1, and your matcher needs to handle that gracefully). Per-transaction fee overhead multiplied by page count.

**When it's the right answer:** for matchers where atomicity across the whole cross isn't required (HFT-style takers expecting to slice anyway), or as an emergency stopgap before a slab refactor.

### (3) Refactor to a slab

The production answer. Replace the flat `[Order; N]` slots array with:

- A **node pool**: a fixed-size arena of nodes, with a separate free-list of indices for vacated slots. Insertions and cancellations are O(1) once the right slot is found.
- A **per-price-level FIFO**: at each price, a doubly-linked list of order nodes in FIFO order. Best-price advance during matching is just `head.next`.
- A **critbit tree of price levels**: keyed on price, sorted, supports O(log N) "best price" and "insert new price level" operations.

The matcher becomes:

```text
while remaining > 0 and fills_done < max_fills:
    level = tree.find_best_acceptable(taker_side, limit_price)   # O(log N)
    if level is None: break
    fill_size = min(remaining, level.head.size)
    cross with level.head
    if level.head.size == 0:
        level.pop_head()        # O(1)
        if level.is_empty():
            tree.remove(level)  # O(log N)
    fills_done += 1
```

Per-fill cost is `O(log N)` instead of `O(N)`. For N = 1024, that's ~10 vs ~1000 inspections per fill. The matcher fits in budget for realistic books.

**Cost:** Significant implementation effort. A correct critbit + node pool + FIFO is roughly 1,500 lines of careful Rust with strong invariants (every node either lives in the tree+queue or on the free-list; every level either has ≥1 order or is gone from the tree). Audit cost is correspondingly higher. The whole thing is `bytemuck::Pod`-friendly but requires care: the tree nodes carry indices into the pool, not pointers.

**When it's the right answer:** for production CLOBs with non-trivial liquidity. Phoenix and Serum both use this design for good reasons.

The slab implementation lives in Chapter 15, which walks the code produced from the design spec below.

---

## §8.4  Slab pseudocode

A minimum slab structure, in pseudo-Rust:

```rust
const POOL_CAPACITY: usize = 1024;
const TREE_CAPACITY: usize = 256;   // unique price levels

#[repr(C, Pod)]
struct Slab {
    discriminator: [u8; 8],
    bump: u8,
    _pad: [u8; 7],

    // Node pool — each slot is either a live OrderNode or part of free_list
    nodes: [OrderNode; POOL_CAPACITY],
    free_head: u16,         // index of first free node, or NONE_INDEX

    // Critbit-tree of price levels — bid tree separate from ask tree
    bid_tree: CritbitTree,
    ask_tree: CritbitTree,
}

#[repr(C, Pod)]
struct OrderNode {
    order_id: u64,
    owner: [u8; 32],
    size: u64,
    next: u16,              // next OrderNode in this price level's FIFO
    prev: u16,              // prev OrderNode in this price level's FIFO
    _pad: [u8; 4],
}
// 64 bytes per node, [OrderNode; 1024] = 64 KiB. Fits with a 64 KiB
// account if you don't pack much else.

#[repr(C, Pod)]
struct CritbitTree {
    nodes: [TreeNode; TREE_CAPACITY],
    root: u16,
    free_head: u16,
}

#[repr(C, Pod)]
struct TreeNode {
    // Critbit-style: either an INNER node (split bit + two child indices)
    // or a LEAF node (price + head/tail of FIFO queue).
    tag: u8,                // 0 = leaf, 1 = inner
    // ... layout depends on tag
}
```

Key operations and their costs:

- **insert(order)**: critbit walk to find the price level (O(log N) where N = number of distinct price levels), if level doesn't exist create it (one tree node allocation), pop a node from `free_head`, fill it, push to the level's tail. Total: O(log N) tree work + O(1) pool work.
- **best_price(side)**: critbit walk to the leftmost (asks) or rightmost (bids) leaf. O(log N).
- **match_top(side, max_size)**: best_price → head of FIFO → cross → if head fully filled, pop head, push node back to `free_head`, if level empty, remove from tree. O(log N) per fill.

The pedagogical points to absorb if you do implement this:

1. **Use indices, not pointers.** `bytemuck::Pod` doesn't allow pointer fields. Use `u16` indices into the pool and tree arrays. `u16` is enough for 65k entries — plenty for most books.
2. **A sentinel `NONE_INDEX = u16::MAX`.** No `Option<u16>` — that would push the struct out of Pod safety. Use the sentinel.
3. **Free-list as singly-linked stack.** Free nodes' `next` field points to the next free node. Allocation pops `free_head`; deallocation pushes to `free_head`. O(1) both ways.
4. **Critbit, not red-black.** Critbit trees have simpler rebalancing rules and don't need rotation logic. Serum uses critbit; Phoenix uses critbit. The pattern is well-trodden.
5. **One tree per side.** Bids and asks have different "best" semantics (max vs min). Keep two trees and you avoid threading a comparator through the tree code.

Implementing slab is a 3-4 day exercise if you've never done it before. The first day is the pool + free-list. The second is critbit insert/remove. The third is wiring it into a matcher. The fourth is testing edge cases (book full, level full, all-or-nothing fills). Chapter 15 walks the resulting implementation.

> **Exercise §8.4.** Build the pool + free-list piece *from scratch* before reading Chapter 15. Write a `Pool<OrderNode, 1024>` with `alloc() -> Option<u16>` and `free(idx: u16)` methods. Verify with 10,000 random alloc/free pairs that the pool always has the right `available_count`. Then compare to Chapter 15's `pool_alloc_node` / `pool_free_node`. The invariants are easy to break; checking against a working implementation is part of the value.

---

## §8.5  What we shipped vs. what's enough

This chapter shipped a working matcher on the flat book with pagination. That's enough for:

- A learning artifact: the matcher is auditable, ~110 lines of Rust, and the CU cost shape is observable.
- A low-throughput production deployment: a market with `<= 32` resting orders matching `<= 16` fills per call comfortably fits the 200 KCU budget.
- Educational research: experiment with matching rules (price-time priority is what we did; pro-rata, time-weighted-pro-rata, last-look — any of these slot into the same handler shape).

It is not enough for:

- A book holding hundreds or thousands of resting orders.
- A matcher that needs to atomically process large takers.
- Any HFT-style use case where latency-per-fill matters.

For those, ship the slab. The pseudo-code in §8.4 is the design spec and Chapter 15 is its implementation.

---

## §8.6  Recap + verify yourself

### Recap diagram

```
Match on flat book (this chapter):

  Per fill:    O(N) scan + O(1) write     → O(K × N) total
  Per K=16 on ORDER_CAPACITY=32 book:    ~60 KCU
  Failure mode:  CU exhaustion past ~30 fills


Match on slab (Chapter 15, pseudocoded above):

  Per fill:    O(log N) tree walk + O(1) FIFO advance  →  O(K × log N) total
  Per K=16 on ~1024-order book:           ~30 KCU
  Failure mode:  pool exhaustion (handle with separate place_order rejection)


Three responses to CU pressure:

  1. ComputeBudgetInstruction::set_compute_unit_limit(N)
     - Budget extension up to 1.4M CU.
     - Cost: priority fees against the requested ceiling, not actual usage.

  2. max_fills pagination
     - Cap work per call, multiple round-trips.
     - Cost: lost atomicity across pages, fee per page.

  3. Slab refactor
     - O(log N) per fill instead of O(N).
     - Cost: implementation + audit work.
```

### Three things to verify yourself

1. **Cost grows multiplicatively.** Pre-populate the book with 8, 16, 24 active makers. For each, run `match-cli` with `--max-fills 4` (constant). Record `units_consumed`. The numbers should roughly track 4 × N, not just N or 4.
2. **Pagination has correct subtotals.** Run `match-cli` with `--max-fills 16` against a full 32-maker book and a 10-unit taker. Note `units_consumed` and the post-state `active_count`. Now do the same matcher work as two calls with `--max-fills 8`. The sum of both calls' `units_consumed` should be very close to the single 16-call's number (slightly higher due to two `process_match` setup overheads).
3. **Budget raise is real.** Run a deliberately-too-large match (e.g., 25 fills against a packed 32-maker book) without `--cu-limit`. It should fail. Add `--cu-limit 500000`. Should succeed. Confirm that `sim.value.units_consumed` is between the default 200,000 and 500,000.

---

## Hook into Chapter 9

You now have a market with a vault and a matcher. What you don't have is a **mark price**. The matcher crosses orders against each other, but the *price* in a perp DEX has two sources of truth: the trade tape (last fill price, which our matcher implicitly produces) and an external oracle (Pyth, Switchboard) that pins the mark to spot. Funding rate, liquidation triggers, and risk math all depend on the oracle mark, not the last fill.

Chapter 9 walks Pyth integration: the price-account layout, how to read it inside an instruction handler without trusting our own derserialization, how to handle stale prices (`slots_since_published`), and how to fall back to a Switchboard secondary if Pyth is unavailable. The chapter culminates in the program reading an external mark price and using it to validate that a `place_order` price is within a sanity band — the first real risk control in the program.
