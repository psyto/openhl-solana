# Chapter 14 — Cranks, Keepers, and Off-Chain Glue

> Status: draft (v0.1).
> Companion code: none. This chapter has zero on-chain additions; its content is the off-chain operational design that surrounds the program we've spent thirteen chapters building.

---

## §14.0  Framing

A Solana DEX program is half of a Solana DEX. The other half is a coordinated set of off-chain processes — keepers, cranks, indexers, monitoring — that drive the on-chain primitives at the right cadence and surface the resulting state to users. Without them, your beautifully audited on-chain program runs once when a user triggers a transaction and then sits idle. Funding doesn't accrue. Underwater positions don't get liquidated. NAVs go stale. Frontends show last-known state from minutes ago.

This chapter walks the operational layer. Every keeper and crank this program implicitly requires, in detail enough that you can implement each one. The chapter is intentionally short on on-chain code (we add none) and long on production design patterns — failure modes, redundancy, fee economics, cadence selection — that you only learn by running these systems in anger.

Six keepers, one indexer:

1. **Funding-rate keeper** — calls `UpdateFunding` on each market periodically; computes the rate from book mid + oracle mark.
2. **Liquidator bot** — scans positions, identifies underwater ones, submits permissionless `Liquidate` calls.
3. **Vault NAV reporter** — for each vault, the manager (or their delegate) periodically calls `UpdateNAV`.
4. **Builder claim cron** — each builder periodically drains their `accumulated_fees` via `ClaimBuilderFees`.
5. **Oracle publisher** — if we ran our own oracle rather than using Pyth, this is the process pushing fresh prices via `SetOraclePrice`.
6. **Maintenance keeper** — odds and ends: closing dormant accounts, archiving filled orders, garbage collecting empty book slots.
7. **Indexer** — not a keeper but an essential read-side service: subscribes to chain state and feeds frontends, analytics, alerting.

The chapter closes Phase B and the track. After §14.7's retrospective there are no more chapters; there are no more on-chain features to add before the program is feature-complete in the sense the curriculum committed to in Chapter 0. Where you go after is your own choice — production deployment, audit prep, scaling experiments, your own perp DEX. The pieces are in your hands.

---

## §14.1  The keeper inventory

| Keeper | Trigger | Cadence | Authority | What fails without it |
|---|---|---|---|---|
| Funding-rate keeper | Per market | 1–5 min | Permissioned or permissionless with clamp | Funding stops accruing; longs and shorts no longer pay each other; mark drifts from spot |
| Liquidator bot | Per position | Per slot (sub-second scanning) | Permissionless | Underwater positions stay open; insurance fund eventually depleted |
| Vault NAV reporter | Per vault | 1 min – 1 hour | Vault manager only | Vault NAV becomes stale; deposits/withdrawals price against wrong NAV |
| Builder claim cron | Per builder | 1 hour – 1 day | Builder only | `accumulated_fees` grows; no functional impact unless it overflows u64 (extremely unlikely) |
| Oracle publisher | Per oracle | Per slot | Permissioned publisher list | Oracle ages past staleness window; `PlaceOrderChecked` / `OpenPosition` / `Liquidate` all start refusing |
| Maintenance keeper | Per program | Daily or weekly | Program admin or permissionless | Dormant data accumulates; cheap to skip but eventually pays rent on garbage |
| Indexer | Per program | Real-time | None (read-only) | Frontends show stale state; analytics break; users blind |

Two patterns visible from the table:

**Permissioned vs permissionless.** Liquidators and oracle publishers (sometimes) are permissionless — anyone can call them, and the protocol is robust against any single keeper going down. Vault NAV reporters and builder claim crons are by-definition permissioned — only the specific entity can act on their own state. Funding keepers fall in between depending on the program's clamp strictness.

**Cadence vs latency tolerance.** Liquidators must scan sub-second because the liquidation race rewards speed; an oracle stale by 25 slots (10 sec) is acceptable. Vault NAV can be a minute or an hour depending on the strategy's volatility. Builder claims can wait days. Picking the wrong cadence is one of the most common operational mistakes — too aggressive wastes fees, too lazy bleeds value.

---

## §14.2  The funding-rate keeper

Chapter 10's §10.5 sketched the funding keeper in pseudo-Python. The real version is more complex along three dimensions: how the rate is computed, how the keeper handles its own downtime, and how multiple keepers coordinate.

**Rate computation.** A naive keeper just samples the book mid and the oracle mark:

```python
def compute_rate(market):
    mark = read_oracle_mark(market)         # ch.9
    bid, ask = read_top_of_book(market)     # ch.7
    mid = (bid + ask) / 2
    premium = (mid - mark) / mark
    return clamp(premium * K, -MAX_RATE, +MAX_RATE)
```

This is correct in form but fragile in practice. Two improvements:

1. **TWAP the book mid** over the last few minutes, not the instantaneous spread. A keeper that submits funding based on a single transient quote-stuffing wide spread can produce nonsense rates that move users' equity meaningfully. Production keepers sample every few seconds and TWAP over 1–5 minutes.

2. **Cap the per-update change.** If the previous rate was 0.0001/sec and the current premium implies 0.001/sec, don't jump in one update — clamp the rate change to (say) 50% per update. This is rate-limiting in the control-systems sense; protects against single-keeper-call disasters from runaway feedback loops.

A real keeper structure:

```python
class FundingKeeper:
    def __init__(self, market, program, signer):
        self.market = market
        self.program = program
        self.signer = signer
        self.history = collections.deque(maxlen=300)  # 5 min at 1Hz

    def tick(self):
        # 1. sample
        mark = read_oracle_mark(self.market)
        if mark is None:                # oracle stale; skip this tick
            return
        bid, ask = read_top_of_book(self.market)
        if bid is None or ask is None:  # empty book; skip
            return
        self.history.append((time.time(), mark, bid, ask))

        # 2. compute (TWAP over history)
        if len(self.history) < 60:      # need 1 min minimum
            return
        avg_mid = mean((b+a)/2 for _, _, b, a in self.history)
        avg_mark = mean(m for _, m, _, _ in self.history)
        premium = (avg_mid - avg_mark) / avg_mark
        target_rate = clamp(premium * K, -MAX, +MAX)

        # 3. clamp per-update delta
        last_rate = read_current_funding_rate(self.market)
        delta_cap = abs(last_rate) * 0.5 + MIN_DELTA
        new_rate = clamp(target_rate, last_rate - delta_cap, last_rate + delta_cap)

        # 4. submit
        send_tx(UpdateFunding(new_rate), self.market, self.signer)

    def run(self):
        while True:
            self.tick()
            time.sleep(1)
```

**Keeper downtime.** What happens when the keeper crashes? If `MAX_ORACLE_STALENESS_SLOTS` is 25 slots (~10 sec) for the oracle, but the funding keeper is down for an hour, the funding rate set 60 minutes ago continues to apply for 60 minutes. Longs/shorts pay funding at a rate set under conditions that no longer exist.

Two defenses:

- **Heartbeat the keeper.** Operations monitoring (Grafana, PagerDuty) alerts within minutes if the keeper hasn't submitted in N minutes.
- **Bound max elapsed in the on-chain handler.** Add a check that refuses `UpdateFunding` if `clock.unix_timestamp - last_update_ts > MAX_FUNDING_ELAPSED_SECONDS`; in such cases, the keeper or a multisig has to call a "reset" instruction first. This is the "fail loud rather than fail silent" pattern — better to break the market briefly than to apply year-old funding once the keeper recovers.

**Multiple keepers.** Funding rate updates are idempotent in the sense that the *last* call's rate is what applies, but not in the sense that multiple calls in the same minute are fine — each adds CU cost and may produce different rates. Coordination patterns:

- **Single keeper, single source of truth.** Simplest. One process, one VPS, one alert.
- **Hot-standby.** Two keepers, one active. The standby promotes itself if it detects the primary hasn't submitted in N minutes. Coordination via a lock account or off-chain leader election.
- **Permissionless with clamps.** Anyone can call `UpdateFunding`; the program's clamps prevent griefing. Multiple keepers race, the first one wins, the second tx fails harmlessly (their rate read was already stale by the time they submitted). Used by some perp DEXes.

Our chapter ships permission as fully open (per §10.3 — open-auth `process_update_funding` was deliberate for testability). Production decides which of the three patterns above based on operator preference.

---

## §14.3  The liquidator bot

Chapter 11's `Liquidate` is permissionless: anyone can submit a liquidation tx against any underwater position. The economics rewards speed — first liquidator to a victim wins the penalty, so liquidator bots compete intensively on scanning latency and tx submission speed.

The liquidator's loop is structurally simple but operationally demanding:

```python
def liquidator_loop():
    while True:
        positions = scan_all_positions()
        for pos in positions:
            mark = current_mark(pos.market)
            funding = current_funding_index(pos.market)
            equity = compute_equity(pos, mark, funding)
            notional = abs(pos.size) * mark
            maint = notional * MAINT_MARGIN_BPS / 10_000
            if equity < maint:
                send_tx(Liquidate(pos.pubkey), signer)
```

Three operational problems hidden in this innocent loop:

**Scanning latency.** `scan_all_positions()` over thousands of position accounts is not cheap. RPC-based `getProgramAccounts(programId, filter: discriminator)` is slow (hundreds of ms to seconds) and not real-time. Production liquidators use Geyser plugins or RPC pubsub to receive position-account updates in real time, maintaining an in-memory mirror of every position and recomputing health on each oracle/funding tick.

**Race conditions.** Multiple bots see the same liquidatable position. The first one to land their tx wins; the rest pay fees for failed txes (the on-chain `Liquidate` rejects on already-closed positions). Bots compete by:

- **Pre-built txes.** Build the `Liquidate` tx the moment a position drops below threshold; only fetch a fresh blockhash and sign at submission time. Saves milliseconds.
- **Submitting through Jito or directly to leader RPCs.** Public RPC has measurable lag; specialized infra cuts it.
- **Priority fees.** Pay extra to land first in a contested slot. The liquidator who pays the highest priority fee that still leaves them profitable wins.

**Profitability.** A liquidation pays `notional × LIQUIDATION_PENALTY_BPS / 10000 = notional × 0.01` (Chapter 11's value). A liquidator needs revenue > (tx cost + RPC cost + infra cost + opportunity cost of the capital tied up running the bot). At $0.001 per Solana tx, a successful liquidation of a $1000 notional position pays $10 — comfortably profitable. A $10 notional position pays $0.10, which is below most operational thresholds — small underwater positions get less competition, may sit underwater longer, and (if collateral has already gone to zero) are net-negative to liquidate. Production designs sometimes add a per-position minimum size to avoid this.

A modern Solana liquidator architecture:

```
   Validator Geyser plugin / RPC pubsub
              │
              ▼
   Local position mirror (in-memory)
              │
              ▼
   Health computer (triggered on oracle/funding updates)
              │
              ▼
   Liquidation candidate queue (sorted by expected revenue)
              │
              ▼
   Tx builder + submitter (Jito MEV searcher or direct leader RPC)
```

Multiple competing liquidators run essentially identical stacks. Differentiation is in latency, priority-fee tuning, and the small algorithmic edges (e.g., predicting which positions will become liquidatable next oracle tick, pre-building their txes).

---

## §14.4  The vault NAV reporter

Chapter 12's `UpdateNAV` is manager-only and trusted. The keeper is therefore a process the manager runs, with two things to get right: cadence and accuracy.

**Cadence.** Too frequent and depositors see NAV bouncing on noise (the manager hasn't actually changed positions, but the keeper updates anyway because the underlying oracle moved). Too lazy and deposits/withdrawals price against multi-minute-stale NAVs, giving the late mover a free option on price movement.

Typical patterns:

- **High-frequency vaults (HFT, market making):** every minute. The cadence approximates real-time enough that the deposit/withdraw timing arbitrage is negligible.
- **Mid-frequency vaults (trend following, momentum):** every 5–15 minutes. Enough for the manager's positions to actually change.
- **Slow vaults (yield aggregators, basis trades):** every hour or per epoch boundary.

A subtle design choice: should the keeper update NAV *only when it changes meaningfully*, or always? "Always" gives depositors predictable cadence and visible "yes, the manager is still reporting." "Only when meaningful" saves tx fees. Most production vaults pick a hybrid: always update at least once per N hours (heartbeat), update sooner if the change exceeds some threshold.

**Accuracy.** The manager computes the NAV off-chain — sum of vault's open position equities (using `compute_equity` from Chapter 11), plus cash collateral, minus any pending fees. This computation must match what the on-chain handler would compute if given the same inputs, or depositors see drift between the reported NAV and what they'd actually receive.

Three risk areas:

- **Funding accrual drift.** If `UpdateFunding` hasn't been called since the last NAV report, the position's funding PnL is computed against a stale index. Production keepers call `UpdateFunding` for the relevant markets *before* calling `UpdateNAV` to ensure the NAV reflects accrued funding accurately.
- **Oracle staleness drift.** If the oracle hasn't been updated, the mark used in `compute_equity` is stale. Same fix: refresh the oracle before NAV update.
- **Unrealized vs realized.** A vault holding open positions has unrealized PnL that depends on mark price. A vault that mostly closed positions has realized PnL sitting in cash. The keeper should compute both correctly and not double-count partial closes.

This is the part where, in production, the manager often *outsources* the keeper to a service (Squads multi-sig + automated NAV scripts, or a vault-management platform like Lulo or Kamino's vault SDK). Doing it yourself requires owning the operational reliability problem — keeper outages = stale NAV = unhappy depositors.

---

## §14.5  Builder claim cron

The simplest keeper. A builder's `accumulated_fees` grows monotonically until they call `ClaimBuilderFees`. The keeper is a cron job:

```python
def claim_loop():
    while True:
        profile = read_builder_profile(my_pubkey)
        if profile.accumulated_fees >= CLAIM_THRESHOLD:
            send_tx(ClaimBuilderFees(), my_pubkey)
        time.sleep(CLAIM_CHECK_INTERVAL)
```

Two parameters and one boilerplate.

**`CLAIM_THRESHOLD`.** Don't claim every single fee, even though each is small. A claim tx costs ~$0.001 in Solana fees; if your accumulated fee is $0.005, you've blown 20% of it on a claim. Set the threshold high enough that claim cost is < a few % of claimed amount — typically several dollars' worth of accumulated fee.

**`CLAIM_CHECK_INTERVAL`.** Hourly is generous. There's no urgency in claiming — your fees can't be stolen, can't be inflated away (no inflation; all u64 quote units), only sit there. Some builders claim daily, others weekly, others monthly.

**Boilerplate.** Set up monitoring so a stuck claim job (e.g., wallet ran out of SOL for tx fees) doesn't silently let fees accumulate forever. Trivial in operational terms but easy to forget.

This is the lowest-stakes keeper in the system. We mention it primarily for completeness — but it's also a good first keeper to write if you're new to operating Solana infrastructure, because the failure mode (a few unclaimed dollars) is gentle.

---

## §14.6  Off-chain indexer

Not strictly a keeper, but essential. An indexer subscribes to chain state, processes it, and exposes the result to frontends, analytics tools, and alerting.

Three architectural choices.

**(1) Geyser plugin.** Geyser is Solana's validator-side streaming interface. A Geyser plugin runs inside a validator and gets every account change, transaction, and slot event in real time, sub-millisecond latency from chain commit. Pros: lowest latency, complete data. Cons: requires running your own validator (or partnering with a node operator who runs the plugin for you), operational complexity, hardware costs.

Production indexers for large DEXes almost always use Geyser. Helius, Triton, and other Solana-RPC providers offer Geyser-as-a-service to avoid the validator-running burden.

**(2) RPC pubsub.** Subscribe to account changes via the WebSocket-based RPC pubsub interface (`accountSubscribe`, `programSubscribe`, `logsSubscribe`). Pros: simple to set up, no infrastructure beyond a WebSocket client. Cons: latency is higher (~hundreds of ms), connections drop, some events can be missed during reconnect.

Fine for mid-stakes use cases: a frontend that updates user-facing state every few seconds, an analytics service computing daily volumes. Insufficient for high-frequency uses (liquidator bots, market-making bots).

**(3) RPC polling.** `getProgramAccounts` + `getTransaction` in a loop. The fallback when pubsub isn't an option (development, debugging, simple bots). Pros: maximally simple. Cons: high latency, expensive in RPC calls, scales badly with many accounts.

A typical production architecture for this program:

```
   Geyser stream (account changes + tx logs)
        │
        ▼
   Event dispatcher (route by program ID, account discriminator)
        │
        ├─► Position mirror (for liquidator bots)
        │
        ├─► Vault NAV cache (for frontend)
        │
        ├─► Trade history DB (for analytics)
        │
        ├─► Alert engine (oracle staleness, vault NAV staleness,
        │                 builder fee accumulation, ...)
        │
        └─► WebSocket fan-out (for frontends subscribing to user events)
```

The indexer is the load-bearing piece of the off-chain stack — every keeper above implicitly depends on having fast, correct state queries against the chain, and the indexer is what provides them.

What to compute off-chain rather than on-chain (Chapter 5's lesson, restated for the indexer specifically):

- **Total volume traded per market.** Off-chain. Cheap to compute from tx logs.
- **Total open interest.** Off-chain. Sum of all Position accounts' notionals.
- **Per-market activity, top-of-book history, fill price tape.** Off-chain.
- **NAV per share, historical PnL of a vault.** Off-chain. Computed from `UpdateNAV` log events.
- **Builder volume rankings.** Off-chain. From BuilderProfile reads.

The on-chain program holds *only* what it needs to enforce its own invariants. Everything that's nice to have but not part of the trust boundary lives in the indexer.

---

## §14.7  Closing — what we built, and what to do with it

Across fourteen chapters, we built:

**Phase A (foundations, ch.1–5):** Account model, native programs, PDAs, compute budget, Sealevel parallelism. A complete "Solana from scratch, no Anchor" curriculum that stands on its own — a learner who finishes Phase A can write a real Solana program and debug a real Solana program, without ever touching a framework.

**Phase B (HL primitives, ch.6–14):** SPL Token vaults via CPI, on-chain order books, matching engines, oracle integration, funding rate accumulators, position lifecycle with liquidation, pooled trading vaults, builder codes, and the off-chain infrastructure that runs the result. A working — if scope-deferred — perp DEX, with every design choice annotated and every honesty note about what's deferred written into the chapter.

Things explicitly deferred (so they're easy to find later):

- **Slab-based order book.** Ch.8 ships the flat-array matcher with pagination and pseudocodes the slab in §8.4. Implementing it is well-scoped homework.
- **Real Pyth integration.** Ch.9 uses a mock; the §9.5 migration table is the one-page diff to switch to real Pyth.

Things you'd add for a real production deployment:

- An audit. Three months of security review minimum.
- A multisig admin authority on all the constants this chapter referenced (margin BPS, fee BPS, caps).
- Per-market configuration on the constants currently global to the program.
- A governance program if the project is meant to be community-controlled rather than admin-controlled.
- The full keeper stack from this chapter, with monitoring, alerting, runbooks.
- A frontend and an indexer.
- Legal review (which jurisdictions can operate, can list, can be users).

Things to do with the curriculum:

- **Use it.** The companion code is MIT-licensed. Fork it, deploy it, extend it. Drop the educational variants (Bench, Stats) and ship the actual primitives.
- **Teach it.** The chapters were designed for a SolDojo internals track. Run a workshop, build a course, write follow-ups. The pedagogical framework (validated chapter format, deliberate scope-deferral with explicit honesty notes, two-language coverage) generalizes to other Solana subjects.
- **Critique it.** Every chapter has design choices that another designer would make differently. Build your own version, then write up your divergences as a counter-curriculum.

The pieces are in your hands.

---

## What's not here (intentionally)

This is a closing chapter, not a roadmap. We didn't write:

- A "what to read next" reading list. The Phase A and Phase B chapters cite Solana docs, Pyth docs, and source code where relevant; that's the canonical further reading.
- A pitch for any particular Solana ecosystem direction. Where the chain goes from here is for the people building on it to decide; this track teaches the substrate, not the strategy.
- A "thank you" or "see you next time." Curriculum endings are for readers, not writers. If you got something out of these fourteen chapters, the next valuable thing you do is ship something with it. Go.
