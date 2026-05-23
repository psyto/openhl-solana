# Chapter 10 — Funding Rate Mechanics

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`FundingState`), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_create_funding_state` 1605–1680, `process_update_funding` 1682–1758), [`scripts/funding/src/main.rs`](../../scripts/funding/src/main.rs).

---

## §10.0  Framing

A perpetual futures contract has no expiry. Without one, there is nothing in its structure that pulls the contract's price back to the spot price of its underlying. Funding is the mechanism that does it: longs and shorts periodically pay each other a small amount proportional to how far the perp's mark deviates from the underlying. When the perp trades above spot, longs pay shorts (encouraging shorts to enter, longs to exit); when it trades below, shorts pay longs.

The economics is simple. The engineering is not.

Solana programs cannot iterate over every position in a market in a single transaction — there are too many, and the per-tx CU budget would exhaust before the first hundred. They cannot rely on a wall clock (Clock sysvar's `unix_timestamp` is the only legal time signal). They cannot trust an off-chain process to update the rate honestly (every keeper is a trust assumption you must minimize). And they must produce per-position settlement amounts that match a global accumulator exactly, no matter how many times a position is touched between funding intervals.

The pattern that solves all four constraints simultaneously is the **time-windowed cumulative index**. This chapter walks it. We will:

1. Read what funding is in formal terms, and why the accumulator pattern falls out of the constraints.
2. Walk the `FundingState` account layout — one fixed-size PDA per market, holding the cumulative index and the current rate.
3. Walk `UpdateFunding` — the keeper crank that advances the index in piecewise-linear segments.
4. Pseudocode the per-position `SettleFunding` half of the pattern (Position lands in Chapter 11; the read-side pattern is too important to defer entirely).
5. Connect the design back to Chapter 5's parallelism argument: funding-settlement-as-touch is the canonical case where the *wrong* design (a singleton "totals" account) destroys throughput.

This chapter is short on novel syscalls and long on architectural taste. The code is small. The pattern is the lesson.

---

## §10.1  What funding is, in formal terms

Two quantities anchor the calculation:

- **Mark price** — what your program (or the rest of the chain) considers the current price of the underlying. Chapter 9's oracle. Read with a staleness check.
- **Premium index** — a smoothed measure of how far perp prices have deviated from mark over the recent past. In practice, exchanges compute this as `(perp_mid - mark) / mark`, averaged over the funding window with some clamps.

The funding rate is roughly proportional to the premium index:

```
funding_rate ≈ k × clamp(premium_index, -max_rate, +max_rate)
```

Sign convention: positive rate means longs pay shorts; negative means shorts pay longs.

Over a window of length `T` seconds, a position of size `s` (positive for long, negative for short) accrues funding:

```
funding_owed(s, T) = funding_rate × T × s
```

This is paid in quote currency (usually USDC). Longs and shorts net to zero across the whole market — funding is *redistribution*, not a fee.

Two design observations fall out of this:

1. **The integral matters, not the instantaneous rate.** A position that exists for half a funding window owes half a window's worth of funding. The settlement amount depends on the time-integral of the rate over the position's lifetime, not the rate at any particular moment.
2. **The integral is the same for every position in the market.** Whether a position is opened at the start of a window or in the middle, the *rate* applied to it is the market's rate, not a per-position rate. So instead of recomputing for every position, we maintain a single market-wide running total — the **cumulative funding index** — and let each position subtract its snapshot of the index at open time.

This is the pattern. The rest of the chapter implements it.

> **Exercise §10.1.** Suppose `funding_rate = 0.0001 / hour` (10 bps/hour), constant for 24 hours, and you hold a long position of size 100 the entire time. How much funding did you pay (or receive)? Now suppose the rate was +0.0001/hour for the first 12 hours and -0.0001/hour for the second 12. Same answer? Why?

---

## §10.2  The `FundingState` account

One PDA per market, 120 bytes. From `crates/state/src/lib.rs`:

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct FundingState {
    pub discriminator: [u8; 8],          // 0..8
    pub bump: u8,                         // 8
    pub _pad0: [u8; 7],                   // 9..16
    pub market: [u8; 32],                 // 16..48
    pub cumulative_funding_index: i64,    // 48..56  — scaled by 1e9
    pub last_update_ts: i64,              // 56..64  — Clock.unix_timestamp
    pub last_update_slot: u64,            // 64..72
    pub current_rate_per_sec: i64,        // 72..80  — scaled by 1e9
    pub window_seconds: u64,              // 80..88
    pub _reserved: [u8; 32],              // 88..120
}
```

Three fields are load-bearing.

**`cumulative_funding_index: i64`** is the running total. Every time `UpdateFunding` runs, this advances by `current_rate_per_sec × seconds_elapsed`. Signed because rates can be negative (shorts paying longs). Scaled by `1e9` so the smallest representable rate is one nanosecond's worth at one unit of base notional — comfortable precision for typical perp economics.

**`last_update_ts: i64`** is the Clock unix_timestamp at which the index was last advanced. The next update computes `elapsed = clock.unix_timestamp - last_update_ts` and uses that as the integration interval. This is the only way Solana programs know how much time has passed between two on-chain events.

**`current_rate_per_sec: i64`** is the rate that has been in effect *since* `last_update_ts`. When `UpdateFunding` runs, it first applies this prior rate over the elapsed window, then installs the new rate for the next window. This is the "step function" half — the index advances in piecewise-linear segments, one segment per keeper call.

The other fields are mechanical: discriminator for the standard check, bump for the PDA, market pubkey for traceability, window_seconds for callers that need to know the configured funding window, and 32 bytes of forward-compat slack.

> **Exercise §10.2.** Convert a funding rate of "0.01% per 8 hours" (Binance's default perp funding) into the scaled-1e9 `current_rate_per_sec` format used here. Show your arithmetic.

---

## §10.3  Walking `UpdateFunding`

`process_update_funding` at `programs/openhl-core/src/lib.rs:1682–1758` is the only mutator. Its body, ignoring validation:

```rust
let new_rate = new_rate_raw
    .max(-MAX_FUNDING_RATE_PER_SEC_ABS)
    .min(MAX_FUNDING_RATE_PER_SEC_ABS);

let clock = Clock::get()?;
let elapsed = clock.unix_timestamp.saturating_sub(funding.last_update_ts);
let elapsed_u = elapsed as u64;

let delta = (funding.current_rate_per_sec as i128) * (elapsed_u as i128);
let new_cumulative = (funding.cumulative_funding_index as i128).saturating_add(delta);
// ... i64 saturation clamp ...

funding.cumulative_funding_index = new_cumulative_clamped;
funding.current_rate_per_sec = new_rate;
funding.last_update_ts = clock.unix_timestamp;
funding.last_update_slot = clock.slot;
```

Four operations.

**Clamp the keeper rate.** `MAX_FUNDING_RATE_PER_SEC_ABS = 1_000_000` scaled-1e9 units = 0.001 / sec ≈ 86.4% / day. Loose for pedagogy; production caps are much tighter (perhaps 0.05% per hour, ~1.5% per day max). The clamp is your defense against a compromised or buggy keeper — the worst they can do is push the rate to the cap.

**Compute elapsed time from Clock.** `Clock::get()?` is the only legal way to know what time it is on-chain. `unix_timestamp` is a `i64` count of seconds since the Unix epoch. The runtime updates it once per slot, so a tx that runs within the same slot as a previous `UpdateFunding` would see `elapsed = 0` — no funding accrues, and the rate just gets re-set. That's fine; the keeper schedule is policy, not invariant.

**Apply the *prior* rate over the elapsed window.** This is the heart of the pattern. The rate that gets multiplied by `elapsed_u` is `funding.current_rate_per_sec` — the rate that was set on the *previous* `UpdateFunding` call. Then the new rate is installed. This is what makes the index a piecewise-linear function of time: it grows at rate `r₀` from `t₀` to `t₁`, then at rate `r₁` from `t₁` to `t₂`, etc.

**Promote to `i128` to avoid mid-computation overflow.** The intermediate `delta = rate × elapsed` can exceed `i64` for long intervals or large rates. We compute in `i128` and then saturate back to `i64` for storage. A position cumulative `i64` can hold ~9 × 10^18 scaled-1e9 units — enough for centuries of normal rates. Saturating instead of panicking means an overflow degrades gracefully (rate calculations cap rather than crash the program).

**What the SDK hides:** Anchor accounts with `Time::now()`-style helpers tend to obscure the fact that *every* time read is a Clock syscall. There is no "wall clock" you can read for free. Each `Clock::get()` is a CU cost. For handlers that read it twice (once to validate staleness, once to compute elapsed), you can cache the first read in a local variable rather than calling twice.

> **Exercise §10.3.** Call `UpdateFunding` three times in succession over a few seconds:
>   1. `--rate 100` (some non-zero rate)
>   2. wait ~5 seconds, `--rate 200`
>   3. wait ~5 seconds, `--rate 0`
>
> After each call, dump the funding state. The `cumulative_funding_index` should:
>   - After call 1: still be 0 (no prior rate to accumulate over).
>   - After call 2: ~ `100 × 5 = 500` (rate 100 for ~5 seconds).
>   - After call 3: ~ `500 + 200 × 5 = 1500`.
>
> The exact numbers depend on the actual elapsed seconds. The shape is the lesson.

---

## §10.4  The other half — per-position settlement (pseudocode)

`FundingState` carries the global index. The per-position half is the read side. When a position is opened, you snapshot the current index:

```rust
// On open_position:
position.funding_snapshot_index = funding.cumulative_funding_index;
```

When the position is touched later (closed, modified, liquidated, settled-without-closing), you compute the delta and apply it as PnL:

```rust
// On any position touch:
let index_delta = funding.cumulative_funding_index - position.funding_snapshot_index;
let funding_pnl_scaled = (index_delta as i128) * (position.size as i128);
let funding_pnl = (funding_pnl_scaled / 1_000_000_000) as i64; // un-scale 1e9
position.realized_pnl += funding_pnl;
position.funding_snapshot_index = funding.cumulative_funding_index;
```

This is the entire pattern. Three properties to notice:

1. **Constant time per touch.** No iteration. No matter how many UpdateFunding calls happened between open and touch, the per-position settle is a fixed-cost subtraction-and-multiply.
2. **No coordination across positions.** Position A and Position B can be settled in parallel — they touch different position accounts and only *read* the (single) `FundingState`. From Chapter 5: this is a read-on-shared-account pattern, which the Sealevel scheduler permits in full parallelism.
3. **Settlement is exact, not approximate.** Because the index is a monotone integral of the rate, the delta between any two snapshots is *exactly* the funding that accrued to a constant-size position over that interval. No drift, no rounding error beyond what the 1e9 scaling forces.

A real implementation lives in `process_settle_position` (Chapter 11 onward) and is called from every other instruction that touches a position. We will introduce `Position` properly in Chapter 11 and connect this half directly. For now, the pseudocode is correct and complete — implementing it is mechanical once Position exists.

> **Exercise §10.4.** A position is opened when `cumulative_funding_index = 1500`, size = 100. Three updates later, the index reads 1800. What is the funding PnL? Now another update advances the index to 1750 (i.e., it went *down* by 50 since the snapshot). What is the new PnL?

---

## §10.5  Crank / keeper — what runs UpdateFunding, and when

`UpdateFunding` is *not* called by traders. It is called by a keeper — an off-chain process whose only job is to drive the index forward by submitting `UpdateFunding` transactions on a schedule.

A minimal keeper loop, in pseudo-Python:

```python
import time
while True:
    mark = read_oracle_mark(market)            # Chapter 9
    perp_mid = read_book_mid(market)           # Chapter 7
    premium = clamp((perp_mid - mark) / mark, -MAX, +MAX)
    new_rate_per_sec = premium * RATE_SCALAR
    send_tx(UpdateFunding(new_rate_per_sec), market)
    time.sleep(60)  # tune per market
```

Three design questions a real keeper must answer:

**1. How often?** Too frequent and you waste tx fees + add jitter to the index. Too infrequent and the rate is stale; positions opened near the end of a long interval pay the wrong rate. Common choice: every 60 seconds for liquid markets, every 5 minutes for less liquid. The on-chain `window_seconds` is the *advertised* window (used in fee disclosures and external docs); the *actual* keeper cadence is policy.

**2. Who runs it?** Three patterns:
   - **The exchange itself** — simplest, single trust assumption, but a single point of failure.
   - **A permissioned set of keepers** — multiple operators with rotating responsibility, the program checks the signer against a whitelist.
   - **Permissionless crank** — anyone can call, the program clamps the rate it accepts. Resistant to censorship but requires very careful bounds (a malicious keeper can still push the rate to the cap repeatedly).
   
   Our `process_update_funding` accepts any signer for pedagogy. Production picks one of the three above.

**3. What if the keeper goes down?** A stalled keeper means a stale rate continues to apply for an extended window. Positions opened during the outage pay funding at the last-published rate, which may be wildly off from the actual premium. Mitigation: cap the maximum elapsed time per update (refuse calls where `elapsed > N seconds`) and require manual intervention to restart, or accept the rate drift as a known degradation mode.

**What Anchor hides:** Nothing here. Keeper patterns are entirely the program author's choice; neither Anchor nor any framework provides a "funding rate" abstraction because the policy is too domain-specific to default.

> **Exercise §10.5.** Write a 30-line Python script that runs the keeper loop above against your local validator. Hard-code the rate as a constant (e.g., 100). Verify by dumping the funding state every 30 seconds that `cumulative_funding_index` grows by about 3000 each time (100 × 30s).

---

## §10.6  Parallelism revisited — settlement as the canonical case

Chapter 5 introduced the singleton-write-shared antipattern. Funding settlement is where it goes operational.

Consider a perp DEX with 1,000 active positions. At each funding settlement moment, two designs are possible:

**Design A — singleton "totals" account.** A single `MarketAggregates` account holds `total_long_size`, `total_short_size`, and a running PnL. Every settlement increments these. Every position touch reads and writes this singleton.

Result: every position-touching transaction shares a write on `MarketAggregates`. Two such transactions cannot run in the same slot. Throughput collapses to single-transaction latency. With 1,000 positions touching once per hour, you serialize at ~1 tx/slot = 2.5 tx/sec maximum. The program is a single-threaded queue.

**Design B — per-position settlement (this chapter).** No singleton totals account. `FundingState` is read-only when settling — its write happens once per `UpdateFunding` call, off the position-touch hot path. Position A and Position B settle in parallel because their write sets are `{position_A}` and `{position_B}` — disjoint.

Result: position settlements scale to the number of cores the validator has. 1,000 positions can settle in a small number of slots. The program is parallel-friendly by construction.

This is the lesson of Chapter 5 paying off. Picking Design B doesn't *feel* like an optimization at the time you make the choice — it just feels like "don't store global totals if you don't need them." The reason it pays off in throughput is because of Sealevel's read/write set scheduling, which you cannot see directly when designing the data layout.

The same pattern applies anywhere a "global counter" would be tempting:
- Total volume traded? Don't store it. Index transactions off-chain.
- Total fees collected? Have the fee accumulate in the fee receiver token account, not in a counter.
- Total positions? `getProgramAccounts(programId, filter: discriminator == POSITION).len()`, off-chain.

The exception that proves the rule: things that are *load-bearing for program logic* (the funding index itself, the insurance fund balance, the oracle staleness check) genuinely require write-shared accounts. For those, accept the serialization and design around it (keeper-only writes, short critical sections, sharding where possible). For everything else, refuse the global counter.

---

## §10.7  Recap + verify yourself

### Recap diagram

```
Time:           t0 ───────── t1 ───────── t2 ───────── t3 ──── now
Keeper call:    UpdateFunding   UpdateFunding   UpdateFunding
Rate set:       r0              r1              r2
Elapsed:        Δ0              Δ1              Δ2

cumulative_funding_index over time:

  idx(t1) = idx(t0) + r0 × (t1 - t0)
  idx(t2) = idx(t1) + r1 × (t2 - t1)
  idx(t3) = idx(t2) + r2 × (t3 - t2)

Per-position settle (Chapter 11 will implement):

  position.funding_pnl_delta
    = (idx_now - position.funding_snapshot_index) × position.size / 1e9
  position.funding_snapshot_index = idx_now


Sealevel scheduling impact:

  UpdateFunding writes the singleton FundingState — serial across keepers.
  SettleFunding writes per-position accounts — parallel across positions.
  Result: settlements scale to N cores; rate updates happen once per minute
  per market and don't gate anything.
```

### Three things to verify yourself

1. **The accumulator is piecewise-linear.** Run three UpdateFunding calls a known number of seconds apart with different rates. The cumulative index after each call should match `prior_index + prior_rate × elapsed_seconds` exactly (within the integer division of the 1e9 scaling).
2. **The clamp is enforced.** Try `--rate 5000000` (way above the cap). The chapter's clamp at [`lib.rs:1703–1705`](../../programs/openhl-core/src/lib.rs#L1703) should reduce it to `MAX_FUNDING_RATE_PER_SEC_ABS = 1_000_000`, and you'll see the `clamped to` log line.
3. **The slot-vs-time distinction matters.** Run `UpdateFunding --rate 100`, immediately run another `UpdateFunding --rate 200` (same slot). The second call should report `elapsed=0s` and the index should not advance. Wait 10 seconds, run a third with `--rate 0` — now you'll see `elapsed≈10` and the index advanced by `200 × 10`.

---

## Hook into Chapter 11

You now have a market with vaults, a matcher, an oracle, and a funding accumulator. What you still don't have is **positions**. Every other primitive in the program either operates on accounts that are themselves not positions (the book, the funding state) or assumes positions will exist somewhere (Chapter 6's deposit moves funds into a vault, but doesn't open a position; Chapter 10's settle-on-touch pattern is incomplete because there is nothing to settle yet).

Chapter 11 introduces the `Position` account: per-user-per-market, holds size + entry price + funding snapshot + margin balance. We will add `OpenPosition`, `ClosePosition`, and `Liquidate` — the liquidation engine being the canonical use case where every other Phase A and Phase B primitive converges. Liquidation reads the oracle (with staleness check), reads the funding index (and settles), checks margin against the position size, calls into the matcher (or its slab cousin) to close the position, and sweeps the user's vault. Chapter 11 is where the program becomes a perp DEX in the full sense, not just a collection of primitives.
