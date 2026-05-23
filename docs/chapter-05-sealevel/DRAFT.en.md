# Chapter 5 — Sealevel Parallelism and Account Locks

> Status: draft (v0.1).
> Companion code: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_create_stats` at lines 442–501, `process_bump_stats` at lines 503–540), [`scripts/stats/src/main.rs`](../../scripts/stats/src/main.rs), [`scripts/create-market/src/main.rs`](../../scripts/create-market/src/main.rs).
> Tested against: solana-instruction 2.3.3, solana-program 2.3.0.

---

## §5.0  Framing

Solana's headline number — tens of thousands of transactions per second — is not bought by a faster VM or a denser block layout. It is bought by *running transactions in parallel*. The scheduler responsible for that is called **Sealevel**, and what Sealevel needs from your program is one specific piece of information per transaction: which accounts it will read, and which it will write.

That information comes from the `AccountMeta` array on each `Instruction` you submit. The runtime uses it as a reader-writer lock declaration: any two transactions whose write sets are disjoint can execute concurrently. Any two transactions that share a writable account must serialize, the way two threads contending on a `Mutex` would.

This chapter walks the model:

1. Open `solana-instruction` and read the three-field `AccountMeta` struct. Understand that **everything** Sealevel needs to know about a transaction's data-dependency is in those three fields.
2. Understand the reader-writer semantics: multiple `READ` locks coexist on an account, a single `WRITE` lock excludes everything else on that same account.
3. Walk the `AccountMeta` array of `CreateMarket`. See that every write is to a different PDA (one per `(base_mint, quote_mint)` pair), which means N concurrent CreateMarkets for N different pairs can execute in N parallel slots.
4. Walk the `AccountMeta` array of `BumpStats`. See that every BumpStats writes the *same* singleton Stats PDA — so two concurrent BumpStats *must* serialize, no matter what else they do.
5. Discuss the design patterns that pull contention out of hot paths: sharding the singleton, pre-aggregating off-chain, or removing the counter entirely.
6. Enumerate what Anchor does and does not generate for you in this area.

This is the last Foundations chapter. After it you can build a Solana program from scratch that is fast on a benchmark *and* fast in production — because the two diverge only when the scheduler is the bottleneck, and now you know how to read it.

---

## §5.1  What Sealevel sees: `AccountMeta` as the lock declaration

Open `solana-instruction-2.3.3/src/account_meta.rs:19–32`:

```rust
#[repr(C)]
// ...
pub struct AccountMeta {
    /// An account's public key.
    pub pubkey: Pubkey,
    /// True if an `Instruction` requires a `Transaction` signature matching `pubkey`.
    pub is_signer: bool,
    /// True if the account data or metadata may be mutated during program execution.
    pub is_writable: bool,
}
```

Three fields. That is the entire interface between your client code and the scheduler. The pubkey identifies the account; the two booleans declare what you intend to do with it. Once the client sends the transaction, the runtime treats this declaration as a contract: if a transaction marked an account as `READ` and the program tries to write to it, the write fails at commit time with `ReadonlyDataModified`. The scheduler can therefore trust the declaration when it decides what to run in parallel.

The two constructors at lines 61–67 and 97–103 make the intent obvious in client code:

```rust
pub fn new(pubkey: Pubkey, is_signer: bool) -> Self {
    Self { pubkey, is_signer, is_writable: true }
}

pub fn new_readonly(pubkey: Pubkey, is_signer: bool) -> Self {
    Self { pubkey, is_signer, is_writable: false }
}
```

`AccountMeta::new(...)` — writable. `AccountMeta::new_readonly(...)` — readable only. The signer bit is orthogonal to read/write; it controls a different runtime check (Chapter 2's owner story, in essence).

There is no "intent to read only one field" or "intent to write only at this offset." The granularity is the entire account. Either you might touch any of its bytes (write) or you only inspect them (read). This coarseness is what makes scheduling cheap — the lock table is keyed on 32-byte pubkeys, not on byte ranges.

> **Exercise §5.1.** Print the `AccountMeta` array of a `CreateMarket` instruction by adding a few `println!` lines to `scripts/create-market/src/main.rs` after the instruction is built. Confirm: payer is `WRITE + SIGNER`, market PDA is `WRITE`, system_program is `READ`.

---

## §5.2  Reader-writer semantics in the scheduler

The Sealevel scheduler treats each account as a single reader-writer lock. The rules are exactly the textbook ones:

- **N readers** can hold the lock on the same account concurrently.
- **One writer** holds the lock exclusively — no other readers, no other writers, on that same account.
- **Disjoint accounts** are independent — locks on different pubkeys do not interact.

When a transaction enters the scheduler, the runtime collects every `AccountMeta` across every instruction in that transaction, deduplicates them, and forms the transaction's read set and write set. Two transactions are *runnable in parallel* if and only if:

```
(A.write_set ∩ B.write_set) == ∅
AND (A.write_set ∩ B.read_set)  == ∅
AND (A.read_set  ∩ B.write_set) == ∅
```

Two `read_set ∩ read_set` overlaps do not block — that is the whole point of distinguishing reads from writes.

What this means concretely:

1. **Two CreateMarkets for different `(base_mint, quote_mint)` pairs** — disjoint write sets (different market PDAs, same payer if you signed both yourself). The shared payer is the only contention point, and the runtime handles that by serializing transactions from the same fee payer (a separate constraint, not a Sealevel one). For different payers, fully parallel.

2. **Two BumpStats** — both write the singleton Stats PDA. Their write sets intersect on that one pubkey. They serialize, full stop.

3. **A CreateMarket and a BumpStats** — different write sets (one writes a market PDA, the other writes Stats). They parallelize.

4. **Two reads of the same market** (e.g., two front-ends rendering it) — overlap on a read, no conflict. Both run.

The runtime makes these decisions per slot, before any program code runs. Your program never knows about scheduling; it just runs when its turn comes.

**What the SDK hides:** Neither `solana-sdk` nor Anchor exposes "is this transaction parallelizable with that one?" as an API. The scheduler decides at runtime, opaquely. You reason about it by reading your own `AccountMeta` declarations and asking the questions above.

> **Exercise §5.2.** A `Transfer` from wallet A to wallet B has read/write set `{A: W, B: W}`. A `Transfer` from C to D has set `{C: W, D: W}`. Can they run in parallel? What about A→B and B→C?

---

## §5.3  Walking `CreateMarket`'s access set

From `scripts/create-market/src/main.rs:118–125`, the client declaration:

```rust
let ix = Instruction {
    program_id,
    accounts: vec![
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new(market_pda, false),
        AccountMeta::new_readonly(system_program::ID, false),
    ],
    data,
};
```

Three accounts. Let us examine each from Sealevel's perspective.

**`payer` — `WRITE + SIGNER`.** The payer's lamport balance changes (rent goes out). For two CreateMarkets that share the same payer, this is the conflict point — the runtime cannot let both decrement the same balance in parallel without serialization. The same fee payer in two different transactions in the same slot is rejected for a *different* reason (duplicate transaction nonce in the same block), so in practice the conflict is moot. If you call CreateMarket from two different payers, you have no `payer`-level overlap.

**`market_pda` — `WRITE`.** This is the new account being created. The pubkey is *derived from* `(base_mint, quote_mint)` plus the program ID — see Chapter 3. Two CreateMarkets for different `(base_mint, quote_mint)` pairs will derive **different** market PDAs, so this write does not conflict between them. This is the design choice that makes openhl-core parallelism-friendly: the PDA scheme means every new market lives at its own address, never colliding with another.

**`system_program::ID` — `READ`.** System program is needed for the `create_account` CPI inside our handler. Marked read-only because we are not modifying the System program itself (you cannot — it is an executable account). All concurrent transactions that CPI to System can read-lock it together. Read locks on the same account do not block each other.

So two CreateMarkets for `(SOL, USDC)` and `(SOL, USDT)` from different payers have:

```
A.writes = {payer_A, market_SOL_USDC}
A.reads  = {system_program}
B.writes = {payer_B, market_SOL_USDT}
B.reads  = {system_program}
```

The intersections are all empty except for `system_program ∈ A.reads ∩ B.reads`, which is a read-on-read overlap — allowed. Both transactions schedule into the same slot. Parallel.

This is what "parallelism-friendly by design" means in practice. We did not write parallelism-friendly code by trying. We wrote it by giving each market its own address, which falls out of the PDA scheme we picked for unrelated reasons in Chapter 3. The Sealevel benefit is downstream of an architectural decision made for composability.

> **Exercise §5.3.** What is the read/write set of an `Initialize` (Chapter 2) instruction call? Look at `scripts/init-market/src/main.rs`. Note that Initialize also runs a `System::Assign` instruction in the same tx — count those AccountMetas too.

---

## §5.4  The Stats counter-example — singleton write contention

Now consider `BumpStats`, from `scripts/stats/src/main.rs:99–104`:

```rust
Instruction {
    program_id,
    accounts: vec![AccountMeta::new(stats_pda, false)],
    data: vec![4u8],
}
```

One account: `stats_pda`, marked `WRITE`. The `stats_pda` is derived from a fixed seed (`[STATS_SEED]`) with no per-call variation — it is the same pubkey for every BumpStats call against this program, forever. So the write set of any BumpStats transaction is `{stats_pda}`.

Two BumpStats transactions have:

```
A.writes = {stats_pda}
B.writes = {stats_pda}
```

`A.writes ∩ B.writes = {stats_pda}` — nonempty. They cannot run in parallel. The scheduler picks one, runs it, commits it, then runs the other. Throughput on BumpStats is bound by the latency of a single transaction, no matter how many cores the validator has.

This is fine for `BumpStats` itself — it is an explicit "tick the counter" call that nobody expects to be a hot path. The problem is *if you bolted Stats-writing onto an instruction that is supposed to run in parallel*. Imagine a `CreateMarketAndBumpStats` instruction that calls `CreateStats` logic at the end of `CreateMarket`. Its `AccountMeta` would be:

```
[payer (W,S), market_pda (W), system_program (R), stats_pda (W)]
```

The first three accounts are different per `(base_mint, quote_mint)` — fully parallelizable. The fourth — `stats_pda` — is *the same* across every call. Suddenly every market creation must serialize on Stats. Your beautiful PDA-per-market design now throughputs at single-transaction latency, because of one global counter.

This is the **single most common mistake** in real Solana programs: someone adds "global metrics" or "global limits" or "global rate-limit counters" to a hot-path instruction, and throughput collapses by orders of magnitude. The fix is always the same — pull the global write out — but the fix is impossible to find if you do not understand why it broke.

Run the stats client to see what the declaration looks like:

```
stats --rpc ... --program ... --init
AccountMeta declared:
  [0] <payer pubkey>           WRITE + SIGNER
  [1] <stats PDA pubkey>       WRITE
  [2] 11111111111111111111111111111111  READ
```

```
stats --rpc ... --program ...
AccountMeta declared:
  [0] <stats PDA pubkey>       WRITE
```

Two transactions, two write sets. The pubkeys are visible. The conflict is mechanical.

> **Exercise §5.4.** Send two BumpStats transactions back-to-back from the same payer. Watch their signatures and slot numbers (via `solana confirm <sig>`). They may land in the same slot or in adjacent slots — but they *cannot* be processed in parallel. Find any case where they truly were processed by the same validator in the same slot, and confirm via the runtime logs that they were processed sequentially.

---

## §5.5  Refactoring patterns — pull the global write out

You have three real options when a singleton write is creating contention.

**(1) Shard the singleton.** Replace one Stats PDA with N Stats PDAs, derived from `[STATS_SEED, &[shard_index]]`. The client picks a shard randomly (or based on some property of the call). Now the write set is `{stats_shard_K}` for some K in `0..N`, and you have N-way parallelism on the counter. To read the total, sum across shards off-chain.

This is the most common in-program fix. Costs: you give up exact "happened-before" ordering on the counter (two shards advance independently), and reading the total now requires reading N accounts.

**(2) Pre-aggregate off-chain.** Do not store the counter on-chain at all. Index the transactions that create markets via a watcher process (Geyser, RPC `getSignaturesForAddress`, etc.) and maintain the count in a database off-chain. On-chain remains parallel because no on-chain state changed.

This is the right choice when the counter is for *observability* (dashboards, analytics) rather than for *program logic*. Most "global stats" requirements fall into this bucket.

**(3) Remove the counter.** Ask whether the counter is actually load-bearing. Often someone added it "to know how many markets we have" — but the answer is `getProgramAccounts(programId, filter: discriminator == MARKET_DISCRIMINATOR).len()`, fetched on demand, no on-chain state required.

The pattern: whenever you find a writable singleton on a hot path, the question is not "how do I serialize this efficiently" but "do I actually need this account to exist on-chain at all."

For our own design, we deliberately chose to keep `BumpStats` as a separate, explicit instruction rather than integrating it into `CreateMarket`. Operators who want the counter can call it; operators who care about throughput skip it. That isolation is the point.

**What Anchor hides:** Anchor's `#[derive(Accounts)]` lets you declare an account with `#[account(mut)]` and forget about the write-set implications. Anchor never warns "you are writing a singleton in a hot-path handler." The compile-time `Accounts` struct shows up in the client's typed IDL, but the parallelism cost is invisible there too. Catching this is purely a code-review concern.

---

## §5.6  Recap + verify yourself

### Recap diagram

```
Sealevel scheduler — for each pair of pending transactions A, B:

   A.writes ────┐
                ├──► intersect? ──► YES: cannot parallelize
   B.writes ────┘                   NO:  proceed

   A.writes ────┐
                ├──► intersect? ──► YES: cannot parallelize
   B.reads  ────┘                   NO:  proceed

   A.reads  ────┐
                ├──► intersect? ──► YES: cannot parallelize
   B.writes ────┘                   NO:  proceed

   (A.reads ∩ B.reads is ignored — both readers can hold the lock)


openhl-core access sets:

   CreateMarket(base, quote)
      writes = { payer, market_PDA[base, quote] }
      reads  = { system_program }
      ↑ market_PDA varies per call → parallel across distinct (base, quote)

   BumpStats
      writes = { stats_PDA }   ← singleton, same pubkey every call
      ↑ serialized regardless of caller

   CreateStats
      writes = { payer, stats_PDA }
      reads  = { system_program }
      ↑ one-shot, contention doesn't matter
```

### Three things to verify yourself

1. **Same pubkey, every call.** Run `stats --init` (CreateStats), then `stats` (BumpStats), then `stats` again. The `stats PDA` printed should be identical across all three runs — that pubkey is the lock the scheduler keys on. It does not change.
2. **Different pubkey per CreateMarket.** Run `create-market --base-mint <A> --quote-mint <B>`. Note the `market PDA`. Re-run with `--base-mint <C> --quote-mint <D>`. The PDA should change. That is the parallelism. The first run's write set is disjoint from the second's, so the scheduler is free to run them in any order or concurrently (subject to other constraints).
3. **Read-on-read does not block.** `Bench` (Chapter 4) has an *empty* account list — no reads, no writes. Two Bench transactions could in principle run in fully parallel slots. Run two from different payers and observe their signatures land in the same or adjacent slot in `solana confirm`. Compare to two BumpStats from different payers, which land in strictly different slots.

---

## Hook into Chapter 6 — Phase B begins

You have finished Phase A. You can now allocate accounts, write programs without Anchor, derive predictable addresses, measure your CU envelope, and reason about whether two of your transactions can run in parallel. These are the runtime fundamentals — everything else in this track is built on them.

Phase B begins with Chapter 6: **CPI Internals — Vault Deposits**. We open SPL Token, write a deposit instruction that moves base-asset tokens from a user's token account into a vault token account owned by our market PDA, and walk what `invoke` and `invoke_signed` *actually* do under the hood — the stack-frame setup, the signer-privilege extension rules, the `AccountInfo` reborrowing dance. Where Chapter 3 used `invoke_signed` once for account creation, Chapter 6 uses it as the primary mechanism for talking to every other program on the chain.

By the end of Chapter 6 we will have a working SPL Token vault for our market. By the end of Phase B we will have an order book that lives inside it.
