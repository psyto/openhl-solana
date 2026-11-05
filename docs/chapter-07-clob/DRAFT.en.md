# Chapter 7 — On-Chain CLOB Data Structures

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`Order` + `OrderBook`), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_create_order_book` at 1080–1144, `process_place_order` at 1151–1242, `process_cancel_order` at 1249–1316), [`scripts/book/src/main.rs`](../../scripts/book/src/main.rs).

---

## §7.0  Framing

A perp DEX without an order book is a price feed with a database. The book is where price discovery happens, where partial fills attribute liquidity, and where you spend almost all of your compute budget. Every business-relevant question downstream of "how does this exchange make money" eventually hits the data structure that holds the resting orders.

On Solana the constraints are unusual. You cannot allocate dynamically per transaction (heap is 32 KiB and the bump allocator never frees). You cannot grow accounts arbitrarily (data length is fixed at creation, only `realloc` to within MAX_PERMITTED_DATA_INCREASE per tx). You cannot have unbounded loops (200 KCU default, 1.4 M max). And whatever you build must be readable and writable by every other program on the chain via `bytemuck` casts — because there is no Rust runtime on the other side to do anything smarter.

This chapter is where Phase A's CU lecture starts to bite. We will:

1. Pick a layout — `Order` slot, `OrderBook` containing a fixed-capacity array — and explain why it is *the simplest correct one*, not the production one.
2. Compare it explicitly against the two real production choices (slab and critbit), and pin down what trade we are making.
3. Walk `place_order` and `cancel_order` as linear scans, and read the CU log to see exactly what "linear" costs.
4. Watch the book fill up, the per-instruction CU rise with it, and stop just before the matcher chapter — where the question becomes "how do you avoid this growth entirely."

We will not implement matching in this chapter. The book is *passive*: orders go in, orders come out by ID. Crossing bids and asks is Chapter 8.

---

## §7.1  Why an array, and not the right thing

A real CLOB on Solana looks like one of two things:

1. **Slab** — a contiguous arena of nodes, linked into a doubly-linked FIFO queue per price level, with price levels themselves stored in a sorted critbit tree (Serum, Phoenix). O(log N) for place/cancel, O(1) for best-bid/best-ask lookup, no compaction needed on cancellation.
2. **Critbit-of-orders** — every order is a leaf in a critbit tree keyed by price (and a secondary timestamp for FIFO ordering at the same price). O(log N) for everything, simpler to reason about than slab, slightly worse memory locality.

What we are building is neither. It is **a fixed-capacity flat array, scanned linearly.** From `crates/state/src/lib.rs`:

```rust
pub const ORDER_CAPACITY: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct OrderBook {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub next_order_id: u64,
    pub active_count: u32,
    pub _pad1: [u8; 4],
    pub slots: [Order; ORDER_CAPACITY],
}
```

2112 bytes per market. Bids and asks share the same slot pool, distinguished by the `side` byte on each `Order`. `size == 0` means "empty slot." There is no sort order — slot positions are determined by "first empty slot wins" in linear scan order.

This is the wrong choice for production for one specific reason: **best-bid / best-ask lookup is O(N).** Every matching engine starts by asking "what is the highest bid?" and "what is the lowest ask?", and our array forces a scan of all 32 slots to answer either question. A slab or critbit answers in O(1) or O(log N).

So why pick it?

- **It exposes the cost.** The CU log shows the linear scan happening. A student can run `book --place` and watch the CU rise as the book fills (by a few CU per order; §7.3).
- **It is easy to verify.** Two `for` loops over an array. No invariants to break. No subtle off-by-one in tree rebalancing.
- **It is enough for Chapter 7.** This chapter is about the data layout and the cost shape. Chapter 8 will introduce matching, and *that* is where the wrong choice becomes a real problem — at which point we have permission to refactor to a slab.

Building the wrong thing first, correctly, then refactoring once we know what the right thing must do, is a reasonable pedagogical sequence. Building the right thing first, before understanding what makes it right, would skip the lesson.

> **Exercise §7.1.** Look at `solana-program-2.3.0/src/system_instruction.rs:9` (the deprecation note) and consider: what if our `_reserved` were a `Vec<u8>` instead of a fixed array? Why does that break Pod? (Hint: re-read Chapter 1's discussion of layout.)

---

## §7.2  The `Order` slot and the empty-slot convention

`Order` is 64 bytes, Pod, repr(C). From `crates/state/src/lib.rs`:

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Order {
    pub order_id: u64,    // 0..8
    pub price: u64,       // 8..16
    pub size: u64,        // 16..24  (size == 0 ⇒ empty slot)
    pub owner: [u8; 32],  // 24..56
    pub side: u8,         // 56
    pub _pad: [u8; 7],    // 57..64
}
```

64 bytes is a power of two and aligns well with cache lines on every reasonable architecture. The `_pad` exists so the struct ends on an 8-byte boundary and `[Order; N]` packs without gaps.

The **empty-slot convention** is `size == 0`. A fresh `OrderBook` has all slots zeroed (System program's `create_account` always zero-initializes). `place_order` searches for the first slot with `size == 0`, writes the new order there, and increments `active_count`. `cancel_order` overwrites the matched slot with `Order::zeroed()`, decrementing `active_count`. No compaction.

Why `size == 0` and not a separate `is_active: bool`?

- A boolean field would cost a byte plus padding to maintain alignment. The `size` field already exists and a real order always has `size > 0`. Repurposing it as the active sentinel saves the extra field.
- Zero-initialization on account creation makes the convention work for free: a newly allocated `OrderBook`'s slots are all "empty" without any explicit setup pass.

The cost is a one-line invariant the program must respect: never write an `Order` with `size == 0`. `place_order` rejects `size == 0` in the payload as its first guard (`cancel_order` takes only an order id, and writes an all-zero `Order` on purpose to empty the slot). From `process_place_order` (lib.rs:1172–1175):

```rust
if price == 0 || size == 0 {
    msg!("place_order: price and size must be > 0");
    return Err(ProgramError::InvalidInstructionData);
}
```

That's the price of repurposing the field. Anchor's `#[derive(BorshSerialize)]` accounts often have an explicit `is_active: bool` so the field semantics are independent — at the cost of an extra byte per slot times N slots per book times M books. For us, the tradeoff lands in favor of repurposing.

> **Exercise §7.2.** Add a field to `Order` called `_pad2: [u8; 16]` and re-run `cargo test -p openhl-state`. The `order_size_is_64_bytes` test will fail. Then add `pub const LEN: usize = 80;` to keep the rest of the code typing-correct, and observe what happens to `OrderBook::LEN` in the size test. What is the relationship?

---

## §7.3  Walking `place_order`

From `programs/openhl-core/src/lib.rs:1151–1242`. The handler decomposes into three parts.

**Validation** (lines 1156–1199): payload size, side byte (must be 0 or 1), price and size non-zero, user is a signer, book owner matches our program, book size matches `OrderBook::LEN`, book discriminator matches.

**The linear scan** at lines 1205–1211:

```rust
let mut chosen_slot: Option<usize> = None;
for (i, slot) in book.slots.iter().enumerate() {
    if slot.size == 0 {
        chosen_slot = Some(i);
        break;
    }
}
```

Scan from index 0, take the first slot with `size == 0`, break. Worst case: book is full. Then the loop runs all 32 iterations and returns `None`, which triggers the "book full" branch and returns `AccountDataTooSmall`. Best case: slot 0 is empty (the book is fresh). One iteration.

The CU cost shape, measured (whole instruction, LiteSVM, with `fuzz/matching`'s `ch7_grid` test; Chapter 4 §4.5 checked that LiteSVM counts like `solana-test-validator`):

| Book before the place | Iterations | CU |
|---|---|---|
| empty | 1 | 2,357 |
| 8 orders | 9 | 2,374 |
| 16 orders | 17 | 2,417 |
| 24 orders | 25 | 2,433 |
| 31 orders | 32 | 2,446 |
| 32 orders (full) | 32 + error | fails (we did not measure failed calls) |

The scan is linear, and cheap: 89 CU from an empty book to 31 orders, about 3 CU per slot visited. (An earlier draft estimated about 50 CU per slot, 1,550 CU at 31 orders; it was more than ten times too high.) Almost all of the 2,357 is the fixed cost of an instruction: entry, validation, the two `sol_log_compute_units` calls and the `msg!` lines. So the *shape* is the lesson here more than the size: O(N) means the cost grows with the data, but at 32 slots and a few CU per slot it hardly shows. It shows in Chapter 8, where the scan runs once per fill, and with a bigger book. For comparison, Chapter 15's slab places an order into 1,023 others for 1,954 CU, less than this array charges at 16.

**The write** (lines 1217–1231): increment `next_order_id`, increment `active_count`, copy the user pubkey into `owner`, construct an `Order` literal, drop it into the chosen slot. All O(1) once the slot is found.

The CU brackets at lines 1192 (before scan) and 1239 (after write) let you read the cost from the validator log. Two `sol_log_compute_units` calls, subtract, get the CU consumed between them: the scan and the write, plus the data borrow, the discriminator check and the logging in that range.

> **Exercise §7.3.** Pre-populate the book with N orders for various N (say 0, 8, 16, 24, 31) and then place a single new order. Record the CU consumed (between the two log calls) for each N. Plot it. It should be linear in N, with a slope of a few CU per order: compare your numbers with the table above.

---

## §7.4  Walking `cancel_order`

`process_cancel_order` (lines 1249–1316) is structurally the same as `place_order` — linear scan, then mutation — but with a different match key and a different mutation.

**The scan** at lines 1284–1290:

```rust
let mut found: Option<usize> = None;
for (i, slot) in book.slots.iter().enumerate() {
    if slot.size != 0 && slot.order_id == order_id {
        found = Some(i);
        break;
    }
}
```

`slot.size != 0` skips empty slots; `slot.order_id == order_id` selects by ID. Worst case is "order is in the last slot" or "order not found" — both pay full O(N).

**The authorization check** at lines 1296–1300:

```rust
if book.slots[slot_idx].owner != *user_ai.key.as_ref() {
    msg!("cancel_order: caller is not the order owner");
    return Err(ProgramError::IllegalOwner);
}
```

Only the order's original placer may cancel it. The `owner` field on the slot is the cancel-authorization mechanism. This is a per-slot check, not a per-book check — there is no single "operator can cancel anything" path, which is appropriate for an open CLOB but would be different for a vault-managed strategy.

**The zero** at line 1304:

```rust
book.slots[slot_idx] = <Order as bytemuck::Zeroable>::zeroed();
```

`Order::zeroed()` returns the all-zeros `Order`. The `<Order as bytemuck::Zeroable>::zeroed()` qualified-syntax is needed because `Order::zeroed` is not in scope as an inherent method — it comes from the `bytemuck::Zeroable` trait. Once written, the slot's `size` is 0 and the next `place_order` scanning for an empty slot will see this position as available.

The CU cost is symmetric to `place_order`: best case is "matched at slot 0" (1 iteration); worst case is "not found" (N iterations + error). We measured 1,994 CU for the order in the first slot of a full book and 2,177 for the one in the last, about 6 CU per slot visited (the "not found" path fails, and we did not measure failed calls). The brackets at lines 1273 and 1313 let you measure.

> **Exercise §7.4.** Place 10 orders, then cancel order_id 5. Then place another order. Which slot does the new order land in? Trace the slot allocation through the dump output of `book` (no flags) before and after each operation.

---

## §7.5  Why this design will not survive Chapter 8

Chapter 8 implements matching. The core matching loop is "for each incoming taker order, find the best resting maker on the opposite side and cross until exhausted." In O(log N) data structures that loop terminates with one tree lookup per crossing. In our flat array, the loop must:

1. Linear-scan every taker → O(N) per taker
2. For each match attempt, linear-scan every maker → O(N) per maker check
3. With M takers crossing K makers, the total cost is O(M × K × N) just to find the right slots, before any token movement happens

At ORDER_CAPACITY = 32, M = 10, K = 5, this is ~1600 array-traversals per match instruction. An earlier draft priced that at about 80,000 CU of pure scanning, using the 50 CU per slot above; with the measured few CU per slot it is a few thousand. Chapter 8's matcher, which scans the book once per fill, measured 12,662 CU for 5 fills against a full book and 22,250 for 10, everything included. The default budget is 200,000.

So the flat book's problem at 32 slots is not this instruction's budget. It is what happens as the book grows: every fill scans every slot, so the cost is O(fills × N), and Chapter 8 §8.2 shows a 128-slot book running out of budget. Adding token movement to each fill (a CPI to SPL Token per transfer) makes that come sooner. We will need to either:

- Refactor to a slab — the production answer
- Raise the CU limit via `ComputeBudgetInstruction::set_compute_unit_limit` — the band-aid
- Limit matching per-instruction to N crossings and require multiple matcher invocations — the workaround

Chapter 8 explores all three and explains why slab is the only one that actually scales; Chapter 15 ships the slab implementation. For now, the flat array is correct, slow, and visibly so. That visibility is the prerequisite for understanding what the refactor buys us.

**What Anchor hides:** Anchor's `#[account(zero_copy)]` attribute makes `bytemuck`-cast accounts available with typed field access. It does nothing about choosing the right data structure — that decision is yours regardless of the framework. Anchor programs with naive `Vec<Order>` book layouts blow CU budgets just as fast as ours would in the matcher.

---

## §7.6  Recap + verify yourself

### Recap diagram

```
OrderBook account (2112 bytes, owned by openhl-core):

  ┌──────── header (64 bytes) ────────────────────────────────────┐
  │ discriminator   bump  _pad0  market  next_order_id  active   │
  │                                       (u64)         (u32)    │
  └──────────────────────────────────────────────────────────────┘
  ┌──────── slots [Order; 32] (2048 bytes) ──────────────────────┐
  │ [0] Order or empty                                            │
  │ [1] Order or empty                                            │
  │ ...                                                           │
  │ [31] Order or empty                                           │
  └──────────────────────────────────────────────────────────────┘

  Each Order (64 bytes):
    order_id (u64) | price (u64) | size (u64, 0 = empty)
    owner ([u8;32]) | side (u8, 0=bid 1=ask) | _pad ([u8;7])

place_order(side, price, size):
  validate → scan slots[0..32] for first size==0 → write → done
  cost: O(active_count + 1)

cancel_order(order_id):
  validate → scan slots[0..32] for order_id → owner check → zero
  cost: O(N) worst case, O(matched_position) best
```

### Three things to verify yourself

1. **Linear cost.** Run `book --init`, then place 30 orders, then place a 31st. Compare the `sol_log_compute_units` delta between the start-of-handler reading and the after-write reading. The 31st-order placement should cost roughly 30× more in the scan portion than the 1st-order placement did. Subtract a constant (the validation/CPI/log overhead) to isolate the scan cost itself.
2. **Empty-slot convention.** Place 5 orders, cancel order 3, then place a 6th. The 6th order should land at slot index 2 (the slot vacated by cancel), not at slot index 5. The "first empty slot wins" rule fills holes left by cancellations.
3. **Pod-layout invariants stay enforced.** `cargo test -p openhl-state` runs three Order/OrderBook layout tests. Change `ORDER_CAPACITY` to 33, recompile. The `order_book_size_matches_layout` test should now show `2112 → 2176`. That stable, predictable byte count is why bytemuck works at all on this struct.


### What a fuzzer can and cannot check here

The checks in `place_order` and `cancel_order` are about two accounts, the signer and the book, and one rule: only an order's owner may cancel it (§7.4). That rule is out of reach of a [solinv](https://github.com/psyto/solinv)-style account swap, because the order to cancel is named by its id in the instruction data, not by an account. So it is pinned by a test instead: `cancel_refuses_another_users_order` in [`fuzz/matching/`](../../fuzz/matching) has someone else cancel the maker's order and expects `caller is not the order owner`. Delete the owner check and rebuild: the test fails (we checked). Chapters 3, 6 and 12 show what the fuzzer does find, the bindings between accounts; Chapter 13 §13.6 shows how to state a rule like this one as a guard when the values live in account fields.

The numbers in §7.3 and §7.4 come from the same harness: `cargo test --release --features invariant_ch8 -- --ignored --nocapture ch7_grid` in `fuzz/matching`, after building the program as in Chapter 3 §3.7.

---

## Hook into Chapter 8

You have a book. You can fill it. You can cancel orders out of it. What you cannot yet do is **cross** a bid against an ask. A taker order arrives, scans the opposite side for the best price, fills against the maker, repeats until either side runs out. This is matching, and it is the single most CU-hungry operation in any perp DEX.

Chapter 8 implements `Match` (or `Take`, depending on the flavor) — an instruction that takes a taker order, walks the resting book, and produces fills. We will see exactly why the flat-array layout from this chapter cannot survive any real load, write a working slab implementation as the replacement, and measure the CU difference. The matcher is the place where every Phase A constraint — CU budget, heap discipline, parallelism — converges into a single design problem.
