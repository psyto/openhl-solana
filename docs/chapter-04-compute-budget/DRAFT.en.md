# Chapter 4 — Compute Budget and Heap Discipline

> Status: draft (v0.1).
> Companion code: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_bench` at lines 355–402), [`scripts/bench/src/main.rs`](../../scripts/bench/src/main.rs).
> Tested against: solana-program 2.3.0, solana-program-entrypoint 2.3.0, solana-compute-budget-interface 2.2.2.

---

## §4.0  Framing

So far every instruction we have written has cost roughly the same amount of compute: a few thousand units. The runtime's per-transaction ceiling — 200,000 compute units (CU) by default — has not even been visible. We could afford anything we wrote because we wrote very little.

That free ride ends the moment you do something proportional to anything. A hash inside a loop. A linear scan of an order book. A Borsh decode of a non-trivial struct. Each of these has a CU profile that grows with input, and that profile is the first thing that breaks before any other constraint does.

This chapter is where you learn to count. We will:

1. Open `solana-program::log` and read `sol_log_compute_units` — the only instrument programs have for measuring themselves at runtime.
2. Open `solana-program-entrypoint` and read the `BumpAllocator` that backs every program's heap. Understand why `dealloc` is a no-op and what that means for `Vec` and `Box`.
3. Look at the hard limits — 32 KiB default heap, 200,000 default CU, 1.4M CU absolute max — and the constants they live behind.
4. Open `solana-compute-budget-interface` and read the `ComputeBudgetInstruction` enum that raises the CU ceiling per transaction.
5. Walk the new `process_bench` handler, which allocates a heap buffer and iterates sha256 between `sol_log_compute_units` brackets so each phase's CU cost is readable.
6. Walk the new `bench` client, which optionally prepends a `set_compute_unit_limit` and surfaces both the program logs and the runtime's `units_consumed` number.

By the end you will be able to predict, before you ship anything, whether an instruction can fit in the default budget — and what to ask for when it cannot.

---

## §4.1  The compute unit and where it comes from

Solana's VM is a sandboxed BPF interpreter. Every instruction executed inside that VM has a fixed compute-unit cost: simple ALU ops cost 1, a hash syscall costs more, a CPI to another program costs more still. The runtime keeps a per-transaction counter, starts it at the limit (default 200,000), and decrements as each operation executes. When it hits zero, the transaction aborts with `ComputationalBudgetExceeded`.

These are not real-world milliseconds. They are an abstract economic unit that exists so the runtime can:

1. **Charge for work fairly** — priority fees scale with CU consumed.
2. **Bound transaction execution time** without timing the host clock (which would be non-deterministic across validators).
3. **Make scheduling predictable** — the runtime can decide ahead of time whether a transaction fits in a block.

The default per-transaction limit is 200,000 CU. The maximum a transaction can request is 1,400,000 CU. Both are network constants that have changed over time; they are not in any single Rust file (they live in runtime feature gates), so the right place to look them up is the validator's CLI: `solana program-buffer-info` and friends, or the official docs.

What *is* in code, and what programs use to measure themselves, is `sol_log_compute_units` at `solana-program-2.3.0/src/log.rs:92–101`:

```rust
/// Print the remaining compute units available to the program.
#[inline]
pub fn sol_log_compute_units() {
    #[cfg(target_os = "solana")]
    unsafe {
        crate::syscalls::sol_log_compute_units_();
    }
    #[cfg(not(target_os = "solana"))]
    crate::program_stubs::sol_log_compute_units();
}
```

A single syscall. It logs the *remaining* CU available at the moment of the call. Subtract two consecutive readings, and you have the cost of the work between them. That is the entire toolkit.

**What the SDK hides:** Anchor does not insert these calls for you. If you want CU measurement in an Anchor program, you must add `solana_program::log::sol_log_compute_units();` yourself — exactly as we do here.

> **Exercise §4.1.** Compute the CU cost of a single `sol_log_compute_units` call by issuing two in a row in a noop instruction (one line apart). The first reading minus the second is the call's own cost. Most programs treat this as zero in their accounting; in fact it is about a dozen CU.

---

## §4.2  The heap — a bump allocator that never frees

Solana programs run in a fixed-size heap arena. The default size is at `solana-program-entrypoint-2.3.0/src/lib.rs:40–42`:

```rust
pub const HEAP_START_ADDRESS: u64 = 0x300000000;
// ...
pub const HEAP_LENGTH: usize = 32 * 1024;
```

32 kibibytes. That is the total amount of heap memory available for every `Vec`, `Box`, `String`, and `HashMap` your program allocates over the course of a single instruction. Run out, and the global allocator returns a null pointer; Rust's allocation-failure handler then aborts the program.

The allocator backing the heap is at `lib.rs:291–302`:

```rust
pub struct BumpAllocator {
    pub start: usize,
    pub len: usize,
}
```

And the `GlobalAlloc` impl at `lib.rs:342–364`:

```rust
unsafe impl std::alloc::GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pos_ptr = self.start as *mut usize;
        let mut pos = *pos_ptr;
        if pos == 0 {
            pos = self.start + self.len;
        }
        pos = pos.saturating_sub(layout.size());
        pos &= !(layout.align().wrapping_sub(1));
        if pos < self.start + size_of::<*mut u8>() {
            return null_mut();
        }
        *pos_ptr = pos;
        pos as *mut u8
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {
        // I'm a bump allocator, I don't free
    }
}
```

Two things to absorb.

First, **`dealloc` is a no-op.** Drop a `Vec`, drop a `Box`, return from a function — the memory it claimed stays claimed for the rest of the instruction. Every allocation is permanent until the program returns. This is a deliberate trade: a real free-list allocator would cost CU to maintain, and an instruction's lifetime is short enough that fragmentation is bounded by the heap size anyway.

Second, **the bump pointer grows from the end downward** (see `pos = pos.saturating_sub(layout.size())`). When `pos` falls below the heap base, `alloc` returns null and your program panics. In our `Bench` we exercise this on purpose — passing `--heap-bytes 65536` (twice the heap size) is enough to OOM.

**What the SDK hides:** Anchor never *encourages* you to allocate, but its `Vec`-backed accounts (`Vec<Pubkey>`, `BTreeMap`, etc.) silently rely on this heap. A 10,000-entry vector deserialized from an account will gleefully claim ~80 KiB of heap and crash the program with no clue why — until you remember that "the heap is 32 KiB and nothing frees."

> **Exercise §4.2.** Add a second `vec![0u8; heap_bytes]` allocation in `process_bench`, right after the first one. With `--heap-bytes 8192`, the program will succeed (8 KiB + 8 KiB ≈ 16 KiB, under the 32 KiB limit). With `--heap-bytes 16384`, it will OOM. Confirm both outcomes.

---

## §4.3  Raising the ceiling — `ComputeBudgetInstruction`

The default per-transaction CU limit is fine for short instructions. For anything longer — a Borsh decode, a CLOB match, a multi-CPI chain — you must explicitly request more. The mechanism is a special transaction-level instruction processed by the runtime before any user program runs.

From `solana-compute-budget-interface-2.2.2/src/lib.rs:24–38`:

```rust
pub enum ComputeBudgetInstruction {
    Unused,
    /// Request a specific transaction-wide program heap region size in bytes.
    /// The value requested must be a multiple of 1024.
    RequestHeapFrame(u32),
    /// Set a specific compute unit limit that the transaction is allowed to consume.
    SetComputeUnitLimit(u32),
    /// Set a compute unit price in "micro-lamports" to pay a higher transaction
    /// fee for higher transaction prioritization.
    SetComputeUnitPrice(u64),
    /// Set a specific transaction-wide account data size limit, in bytes, is allowed to load.
    SetLoadedAccountsDataSizeLimit(u32),
}
```

Four levers, each with a constructor at `lib.rs:55–67`. The two you will reach for most:

- **`set_compute_unit_limit(units)`** — raises the CU ceiling for the whole transaction. Pass any value up to 1,400,000.
- **`request_heap_frame(bytes)`** — raises the per-program heap size. Must be a multiple of 1024. Useful when you genuinely need >32 KiB of heap.

Our `bench` client uses `set_compute_unit_limit` at `scripts/bench/src/main.rs:101–102`:

```rust
if let Some(limit) = cli.cu_limit {
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
}
```

The compute-budget instruction is processed by the runtime regardless of where it appears in the transaction, but by convention it goes first for readability. There is no account list — it is interpreted entirely as data by the runtime's pre-execution phase.

Three properties to remember:

1. **One per kind per transaction.** A second `SetComputeUnitLimit` in the same tx is rejected with `DuplicateInstruction`.
2. **No partial refunds.** If you request 1M CU and your program uses 50K, the fee is still calculated against the 1M ceiling for priority-fee purposes. (Plain transaction fees are unaffected.)
3. **It costs CU itself.** The compute-budget instruction processing is ~150 CU, baked into your transaction's total.

**What the SDK hides:** Anchor does not automatically prepend compute-budget instructions. You add them on the client side, before the Anchor-generated instruction. Many Anchor users forget this and get mysterious "transaction simulation failed" errors when their handlers grow past 200K CU.

> **Exercise §4.3.** Without `--cu-limit`, run `bench --rounds 200 --heap-bytes 256`. Note the `units_consumed` value. Now add `--cu-limit 50000`. Does the program succeed or fail? Why? (Hint: compare `units_consumed` from the first run to the limit you set.)

---

## §4.4  Walking `process_bench`

The handler is small — about 50 lines at `programs/openhl-core/src/lib.rs:355–402`. Its structure is three phases bracketed by `sol_log_compute_units`:

**Entry.** Decode the 8-byte payload (`rounds: u32 LE` then `heap_bytes: u32 LE`) and log the starting CU:

```rust
msg!("bench: start (rounds={}, heap_bytes={})", rounds, heap_bytes);
sol_log_compute_units();
```

**Phase A — heap.** Allocate the buffer and log again:

```rust
let mut buf = vec![0u8; heap_bytes as usize];
msg!("bench: after heap alloc ({} bytes)", buf.len());
sol_log_compute_units();
```

The `vec!` macro calls into the bump allocator. The first log reading minus this one is the *cost* of allocating `heap_bytes` bytes. Surprisingly small — the bump allocator is just a single pointer subtraction — but proportional to the syscall stub overhead, not the byte count.

**Phase B — hash loop.** Iterate sha256 `rounds` times, feeding each digest back into the buffer:

```rust
for i in 0..rounds {
    let digest = sha256(&buf);
    let bytes = digest.to_bytes();
    let copy_len = bytes.len().min(buf.len());
    buf[..copy_len].copy_from_slice(&bytes[..copy_len]);
    if !buf.is_empty() {
        buf[0] ^= i as u8;
    }
}
```

This is the workload that actually burns CU. `sha256` is a syscall on BPF (`sol_sha256_`), and its cost depends on the input length. Subtracting the Phase A reading from the Phase B reading gives the per-round CU cost — roughly `(sha256 syscall base) + (per-byte cost × heap_bytes)`.

The XOR-with-counter on line `lib.rs:392` exists to keep the optimizer honest. Without it, every iteration would hash the same bytes, and a sufficiently aggressive optimizer could collapse the loop. Stirring `i` into the buffer makes every iteration's input genuinely different.

The `_` final reading is captured automatically by `sol_log_compute_units` when the function returns (in the form of the runtime's own "consumed N of M compute units" log line, emitted after the program exits).

> **Exercise §4.4.** Run `bench --rounds 0 --heap-bytes 0` and `bench --rounds 0 --heap-bytes 1024`. Subtract the "after heap alloc" CU readings. That difference is the cost of allocating 1024 bytes from the bump heap. Is it bigger or smaller than you expected? Why?

---

## §4.5  Reading the bench output

A typical run looks like:

```
bench --rounds 50 --heap-bytes 1024

simulation:
  units_consumed: 87412
  err:            (none)

program logs:
  Program <openhl-core ID> invoke [1]
  Program log: bench: start (rounds=50, heap_bytes=1024)
  Program consumption: 199772 units remaining
  Program log: bench: after heap alloc (1024 bytes)
  Program consumption: 199639 units remaining
  Program log: bench: after 50 hash rounds
  Program consumption: 112433 units remaining
  Program <openhl-core ID> consumed 87412 of 200000 compute units
  Program <openhl-core ID> success

on-chain signature: ...
```

Three numbers to focus on. The CU remaining after entry (199,772) tells you the fixed cost of program startup: instruction-data decode, dispatcher, logging — about 230 CU in this build. Phase A burned (199,772 − 199,639) = 133 CU to allocate 1 KiB. Phase B burned (199,639 − 112,433) = 87,206 CU for 50 sha256 rounds of 1 KiB input — about 1,744 CU per round.

From here you can extrapolate. 100 rounds: ~175K CU. 120 rounds: ~209K CU — over the default 200K limit. Run it without `--cu-limit` and you will see `ProgramFailedToComplete` with `ComputationalBudgetExceeded`. Add `--cu-limit 400000` and it succeeds again.

This is the whole point: measure once, predict, then either fit your work into the budget or explicitly raise the ceiling. Guessing is for projects that have not yet shipped under load.

**What the SDK hides:** Anchor's logs include the same "consumed N of M compute units" line because it comes from the runtime, not from your program. But Anchor does not surface a typed `units_consumed` field anywhere — you read it from the validator logs like we do here.

> **Exercise §4.5.** Use `--cu-limit` to set a value just barely below what the previous run reported as `units_consumed`. The transaction should fail with `ComputationalBudgetExceeded`. Try a value just above. It should succeed. The boundary is exact, which is what makes CU a useful planning tool.

---

## §4.6  What Anchor hides about CU

Anchor inserts no CU instrumentation. It does not auto-raise the budget. It does not warn at compile time that your handler is too long. CU is one of the few things Anchor leaves entirely to you — because there is no general-purpose answer, and any default would be wrong.

What Anchor *does* do is add roughly 2,000–5,000 CU of overhead per typed account, for the deserialization + discriminator-check + owner-check it performs automatically. A handler with five `Account<'info, T>` parameters might pay 15,000–25,000 CU just for the typed-account wrappers before any of your code runs. We pay roughly 600–1,000 CU for the equivalent manual checks in `process_initialize` and `process_create_market`, because we hand-roll exactly what we need.

That is the entire CU case for native programs: when you write the deserializer, the owner check, and the borrow yourself, you control their cost. When Anchor does, they cost what Anchor's defaults cost. For a handler that runs once per market creation, the difference is negligible. For a handler that runs in a per-fill loop, the difference is the entire business.

---

## §4.7  Recap + verify yourself

### Recap diagram

```
Per-transaction limits
┌────────────────────────────────────────────────────────────┐
│                                                            │
│  CU         default 200,000     max 1,400,000              │
│  heap       default 32 KiB      max 256 KiB (via RHF)      │
│  data size  default ~64 MB      adjustable per tx          │
│                                                            │
└────────────────────────────────────────────────────────────┘

Inside one tx:
  ┌─────────────────────────┐     ┌──────────────────────┐
  │ ComputeBudgetInstruction│ ──► │ runtime applies      │
  │ (set_compute_unit_limit │     │ limits to all user   │
  │  / request_heap_frame)  │     │ programs in this tx  │
  └─────────────────────────┘     └──────────────────────┘
                                            │
                                            ▼
  ┌─────────────────────────┐     ┌──────────────────────┐
  │ openhl-core::Bench      │ ──► │ sol_log_compute_units│
  │   - decode payload      │     │ at each phase, runs  │
  │   - vec![0u8; n] heap   │     │ until success or CU=0│
  │   - sha256 loop         │     │                      │
  └─────────────────────────┘     └──────────────────────┘
```

### Three things to verify yourself

1. **Default ceiling is real.** Run `bench --rounds 150 --heap-bytes 1024` without `--cu-limit`. It should fail with `ComputationalBudgetExceeded`. The simulation `units_consumed` will show a number greater than 200,000 — but the runtime stops counting once you exceed the limit, so the number may be capped.
2. **Heap ceiling is real.** Run `bench --rounds 0 --heap-bytes 65536`. The bump allocator at [`solana-program-entrypoint-2.3.0/src/lib.rs:342`](#) returns null; Rust's alloc-error handler aborts the program. The error you see is not `ComputationalBudgetExceeded` but a memory abort.
3. **Compute-budget instruction must be in the same tx.** Modify `bench` to send the `ComputeBudgetInstruction` in a *separate* transaction from the bench instruction. The next bench tx still gets only the default 200,000. The compute-budget instruction's effect is scoped to its own transaction only — it does not persist across.

---

## Hook into Chapter 5

You can now measure what your code costs and request the budget you need. But CU is only half the throughput story. The other half is *parallelism*: how many transactions can execute concurrently against the same accounts. Solana's headline feature — the reason it can process tens of thousands of transactions per second — is the Sealevel scheduler, which runs transactions in parallel whenever their account access sets don't conflict.

Chapter 5 walks the read/write set model that Sealevel uses to decide what can run in parallel. We will see why putting all markets behind a single global "registry" account would single-thread the entire program, and why our `CreateMarket` PDA scheme allows arbitrarily many markets to be created concurrently. We will also add a deliberately-conflicting `Stats` account to `openhl-core` to demonstrate what serialization looks like in the scheduler's eyes — and then refactor it out, the same way you would refactor it out of any real program before it shipped.
