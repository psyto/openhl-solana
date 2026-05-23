# Chapter 11 — Position Lifecycle and Liquidation Engine

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`Position`), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (helpers + `process_open_position` 1881–1993, `process_close_position` 1995–2061, `process_liquidate` 2063–2152), [`scripts/position/src/main.rs`](../../scripts/position/src/main.rs).

---

## §11.0  Framing

This is the convergence chapter. Every other Phase A and Phase B primitive — oracle, funding, vault, matcher, parallelism — exists so this chapter can be written. A perp DEX without `OpenPosition` / `ClosePosition` / `Liquidate` is a collection of pieces; a perp DEX with them is a perp DEX.

The chapter ships three instructions, each of which integrates the SPL Token escrow path from Chapter 6 directly into the position lifecycle:

1. **`OpenPosition`** — creates a per-(user, market) Position PDA, reads the oracle for the entry price, snapshots the cumulative funding index for later settlement, validates the initial margin requirement, and **escrows the collateral** by CPI'ing an SPL Token Transfer from the user's quote token account into the market vault (the per-(market, mint) vault built in Chapter 6).
2. **`ClosePosition`** — the owner's exit. Settles funding via the snapshot pattern from Chapter 10, computes realized PnL = `size × (mark - entry)`, **transfers the realized amount back to the user** via an SPL Token CPI signed by the vault authority PDA (`invoke_signed` with `[b"vault_auth", market]` seeds), and zeros the position.
3. **`Liquidate`** — anyone's exit on someone else's underwater position. Computes equity, compares to maintenance margin, and if the position has fallen below, force-closes at the current mark. **Two SPL Token CPIs run inside the handler:** vault → liquidator for the penalty bounty, and vault → position-owner for whatever remains. Both signed by the vault authority PDA.

The collateral now lives where a real perp DEX puts it — the program's vault token account, owned by SPL Token, controlled by an `invoke_signed`-only PDA. The position record holds the *bookkeeping* (size, entry price, snapshot index); the vault holds the *money*. The two stay in sync because every state transition that touches the bookkeeping also runs the matching CPI.

One scope-honesty note remains for this chapter: **insurance fund**. When a position closes underwater (`equity < 0`), the deposited collateral is already sitting in the vault — and the program currently lets that residue absorb the loss. In production you'd route a fraction of every liquidation penalty into an `InsuranceFund` account, draw from it when underwater closes leave a shortfall, and only socialize to the LP pool once the fund is empty. We discuss this in §11.6 but don't implement it; that's a follow-up chapter on its own.

---

## §11.1  The `Position` account

One PDA per (user, market) pair. 144 bytes. From `crates/state/src/lib.rs`:

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Position {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub user: [u8; 32],
    pub market: [u8; 32],
    pub size: i64,                    // base units; signed: long > 0, short < 0
    pub entry_price: u64,             // quote per base, stamped at open
    pub collateral: u64,              // quote units posted as margin
    pub funding_snapshot_index: i64,  // FundingState.cumulative at last touch
    pub _reserved: [u8; 32],
}
```

Six load-bearing fields, plus discriminator + bump + padding.

**`size: i64`** is signed. A long position has positive size; a short has negative. `size == 0` is the "position closed" sentinel — like the empty-slot convention in Chapter 7. After a close or liquidate, the account stays around with `size = 0`, and can be reopened by issuing a fresh `OpenPosition` (which will derive the same PDA and write over the dormant state). We chose not to literally close the account (refunding rent) because the per-(user, market) PDA derivation guarantees a position-or-nothing relationship, and keeping the slot around saves a re-create CPI on reopen.

**`entry_price: u64`** is the mark price stamped from the oracle at `OpenPosition` time. It's the reference point for price PnL: `(mark - entry) × size`. We do not maintain a running entry-price for partial closes; the chapter's `ClosePosition` is all-or-nothing. Partial closes would require resetting `entry_price` to a size-weighted blend on each partial — a useful extension but not in scope.

**`collateral: u64`** is the quote-currency margin amount. Strictly positive while the position is open; can be reduced to zero by underwater close or liquidation. Cannot go negative — losses beyond collateral are socialized to the insurance fund (or, in our scope-deferred version, just lost).

**`funding_snapshot_index: i64`** is the cumulative funding index at the last touch (open, close, liquidate). The per-position settle pattern from Chapter 10 makes this the only field needed for funding accounting — the delta between `funding_now` and `funding_snapshot_index` times `size` is the funding PnL accrued since the snapshot.

The PDA derivation uses both `user` and `market` as seeds: `[b"position", user.key, market.key]`. So every (user, market) pair has exactly one position address that everyone can compute without storing a mapping anywhere. The pubkey is bound to the asset pair and the trader by the seed scheme alone.

> **Exercise §11.1.** Why does the position store both `user` and `market` *inside* the account, despite both being seeds of the PDA derivation? (Hint: think about what a third party reading the account knows vs. what they have to derive.)

---

## §11.2  Equity, notional, and the margin formulas

Before walking the handlers, fix the formulas. From `programs/openhl-core/src/lib.rs:1814–1834`:

```rust
fn compute_equity(position: &Position, mark: u64, funding_index_now: i64) -> i128 {
    let size = position.size as i128;
    let entry = position.entry_price as i128;
    let mark_i = mark as i128;
    let collateral = position.collateral as i128;

    let price_pnl = size * (mark_i - entry);

    let funding_delta = (funding_index_now as i128) - (position.funding_snapshot_index as i128);
    let funding_pnl = funding_delta * size / 1_000_000_000_i128;

    collateral + price_pnl + funding_pnl
}

fn notional(size: i64, mark: u64) -> u128 {
    let abs_size = (size.unsigned_abs()) as u128;
    abs_size * (mark as u128)
}
```

Three quantities the chapter cares about.

**Notional** = `|size| × mark`. The dollar (quote-currency) value of the position at current price. A long of 5 base units at mark 100 has notional 500 quote units. Both long and short have positive notional — direction matters for PnL, not for notional.

**Price PnL** = `size × (mark - entry)`. Signed. Long positions profit when mark rises (positive size × positive delta = positive PnL); short positions profit when mark falls (negative size × negative delta = positive PnL). The arithmetic works without special-casing direction because `size` carries the sign.

**Funding PnL** = `(index_now - index_snapshot) × size / 1e9`. Same shape as price PnL but with the funding index playing the role of price. The `/1e9` un-scales the 1e9 scaling Chapter 10's `FundingState` uses for its index. For a long with positive size, a rising funding index (longs paying shorts) means positive `funding_delta × size`, which becomes negative funding PnL after the formula's signs work out — exactly the right semantics.

**Equity** = `collateral + price_pnl + funding_pnl`. The total quote-currency value the position commands right now. Equity can go negative for severely underwater positions; the program clamps to zero on close/liquidate (the loss is socialized rather than passed to a counterparty).

**Maintenance margin** = `notional × MAINT_MARGIN_BPS / 10000`. The minimum equity required to keep the position open. With `MAINT_MARGIN_BPS = 500` (5%), a 500-notional position needs equity ≥ 25 quote to avoid liquidation.

**Initial margin** = `notional × INITIAL_MARGIN_BPS / 10000`. The minimum collateral required at open. With `INITIAL_MARGIN_BPS = 1000` (10%), the same 500-notional position needs ≥ 50 quote of collateral to open.

The gap between IM (10%) and MM (5%) is the **maintenance buffer** — how far the position can move against you before you're liquidated. A position opened at IM and immediately moving 50% of its notional against you would have equity zero (collateral wiped out) before liquidation triggers; a position opened at IM with a 5% adverse move would still be healthy. The narrower the IM↔MM gap, the more capital-efficient but the easier to liquidate.

> **Exercise §11.2.** A long position is opened with size = 10 base units, entry = 100, collateral = 200 (10% IM). Compute equity at marks 90, 95, 100, 105, 110. At which marks is the position liquidatable? (Ignore funding for now.)

---

## §11.3  Walking `OpenPosition`

`process_open_position` at `programs/openhl-core/src/lib.rs`. The handler decomposes into six parts: validation, oracle/funding read, initial margin check, position PDA allocation, **collateral escrow CPI**, and the position state write.

**Validation**: payload size, non-zero size and collateral, user is signer, market is owned by us, system program is the System program, **token program is SPL Token, user_token_account is owned by SPL Token, and the vault_token_account matches the derived PDA at `[VAULT_SEED, market, mint]`** (the new escrow-side checks). PDA derivation for the position itself:

```rust
let (expected, bump) = Pubkey::find_program_address(
    &[POSITION_SEED, user_ai.key.as_ref(), market_ai.key.as_ref()],
    program_id,
);
if position_ai.key != &expected {
    return Err(ProgramError::InvalidSeeds);
}
```

**Read external inputs** (lines 1922–1923):

```rust
let mark = read_fresh_oracle(oracle_ai, program_id)?;
let funding_snapshot = read_funding_index(funding_ai, program_id)?;
```

`read_fresh_oracle` (lines 1838–1858) factors the Chapter 9 staleness gauntlet into a helper — same checks (owner + discriminator + price>0 + age vs Clock), reused across all three position handlers. `read_funding_index` (lines 1860–1869) is the simpler read used to snapshot the funding index.

**Initial margin check** (lines 1932–1942):

```rust
let notional_val = notional(size, mark);
let im_required = notional_val * (INITIAL_MARGIN_BPS as u128) / 10_000;
if (collateral as u128) < im_required {
    msg!(
        "open_position: collateral {} < initial margin {} ...",
        collateral, im_required, ...
    );
    return Err(ProgramError::InvalidArgument);
}
```

The collateral must cover at least 10% of notional. If you ask for a position of 10 at price 100 (notional = 1000) with collateral 50, the check rejects: 50 < 100 (IM). With collateral 100, accepted exactly at IM. With collateral 200, accepted with 100 of buffer above IM.

**Allocate the position PDA**: standard `invoke_signed` to `System::create_account`, signing with `[POSITION_SEED, user.key, market.key, bump]`. Same pattern as `CreateMarket`, `CreateVault`, etc. — Chapter 3 introduced it, every subsequent chapter has reused it.

**Escrow the collateral via CPI to SPL Token Transfer**:

```rust
spl_token_transfer_user_signed(
    user_token_ai,    // source — user's quote account
    vault_token_ai,   // destination — per-(market, mint) vault PDA
    user_ai,          // authority — the user, signing the outer tx
    token_ai,         // SPL Token program
    collateral,       // amount in quote base-units
)?;
```

`spl_token_transfer_user_signed` is one of the four escrow helpers factored at the top of the position section. It builds the SPL Token Transfer instruction by hand (Chapter 6's bytes-up pattern — `[tag=3, amount_le]` data + `[source, dest, authority]` accounts), then calls plain `invoke`. The user's signature on the outer transaction extends through to SPL Token via signer-privilege extension (Chapter 6 §6.2). After this CPI commits, the user's quote balance has dropped by `collateral` and the vault's has grown by the same.

The order matters: the position PDA must be allocated *before* the transfer, because if the transfer fails (insufficient funds) we want the whole transaction to revert — which it does, leaving no orphan Position account. If the order were reversed, an InsufficientFunds error on transfer would leave a half-initialized Position behind (rent paid, but no escrow). Atomicity of the whole tx is what makes the natural error handling correct.

**Write the position state**:

```rust
position.size = size;
position.entry_price = mark;
position.collateral = collateral;
position.funding_snapshot_index = funding_snapshot;
```

Four data writes. `entry_price = mark` stamps the oracle's price as the position's reference. `funding_snapshot_index = funding_snapshot` captures the funding index at this moment — every future close/liquidate computes funding PnL as the delta from this snapshot. `collateral` mirrors what's escrowed in the vault; the bookkeeping and the vault balance stay in sync because the same handler updates both atomically.

> **Exercise §11.3.** What happens if you try to `OpenPosition` against a stale oracle (more than 25 slots since the last `SetOraclePrice`)? Trace the failure path through `read_fresh_oracle`. Then run `funding --update --rate 0` and `oracle --set --price ...` and re-try the open.

---

## §11.4  Walking `ClosePosition`

`process_close_position`. Simpler than open in one dimension (no PDA creation) but more involved in another: it adds an outbound SPL Token CPI signed by the vault authority PDA via `invoke_signed`.

**Validation + owner check** (lines 2007–2024):

```rust
if position.user != *user_ai.key.as_ref() {
    msg!("close_position: caller is not the position owner");
    return Err(ProgramError::IllegalOwner);
}
```

Only the position's owner may close it voluntarily. Liquidate (§11.5) is the route for anyone else. The user check uses the `user` field stored in the position rather than the PDA derivation — same information, easier to read.

**Read external inputs + compute equity** (lines 2026–2040):

```rust
let mark = read_fresh_oracle(oracle_ai, program_id)?;
let funding_now = read_funding_index(funding_ai, program_id)?;
// ...
let equity = compute_equity(position, mark, funding_now);
```

The same oracle + funding read pattern from open. Equity is the only computation that matters at close — it tells us what the position is worth right now in quote-currency terms.

**Compute the payout + zero the position record** (inside a `try_borrow_mut_data` scope so the borrow drops before the CPI):

```rust
let payout: u64;
{
    let mut data = position_ai.try_borrow_mut_data()?;
    let position: &mut Position = bytemuck::from_bytes_mut(...);
    // ... owner check, size != 0 check ...
    let equity = compute_equity(position, mark, funding_now);
    payout = if equity < 0 { 0 } else { equity as u64 };

    position.collateral = 0;
    position.size = 0;
    position.entry_price = 0;
    position.funding_snapshot_index = funding_now;
}
```

Note `position.collateral = 0` — the value isn't held in the position account anymore; it's about to be paid out from the vault. The position becomes a pure "closed" sentinel: size 0, entry 0, collateral 0.

**Pay the user from the vault** via `invoke_signed`:

```rust
spl_token_transfer_vault_signed(
    vault_token_ai,
    user_token_ai,
    vault_authority_ai,
    market_ai.key,
    vault_auth_bump,
    token_ai,
    payout,
)?;
```

The vault authority is a PDA at `[VAULT_AUTH_SEED, market]`, so the program signs for it: `invoke_signed` with `[VAULT_AUTH_SEED, market_key, &[bump]]`. The vault token account drops `payout` units; the user's token account receives them. If `payout == 0` (underwater close), the helper skips the CPI — no point burning CU on a zero-amount transfer.

**Underwater closes lose collateral, don't pass losses on.** A position that closes with equity = -50 (loss exceeds collateral) sends `payout = 0` to the user, but the 100 units they originally deposited are still sitting in the vault — now decoupled from any position record. That residue is the implicit subsidy to whoever was on the other side of the trade. In production an InsuranceFund draws on these residues + a fraction of liquidation penalties to cover the shortfalls properly; see §11.6.

> **Exercise §11.4.** Open a position at entry = 100, size = 5, collateral = 100. Move the oracle to mark = 80. Close. The expected equity is `100 + 5 × (80 - 100) = 0`. Verify the user's quote token balance after the close is unchanged from before the open (because payout = 0 — the 100 they deposited went into the vault and stayed there).

---

## §11.5  Walking `Liquidate`

`process_liquidate`. The crucial difference from close: **anyone can call it**. The handler runs *two* outbound SPL Token CPIs — vault → liquidator for the penalty bounty, vault → position-owner for the remainder — both signed by the vault authority PDA.

**Validation**: the *liquidator* must be a signer, but the program does *not* check that the liquidator matches the position's user. Anyone can call liquidate on anyone's position. Additional escrow-side checks: token_program is SPL Token, both `owner_token` and `liquidator_token` are SPL Token-owned, vault_token matches the derived PDA, vault_authority matches the derived PDA (and the bump is captured for the two invoke_signed calls below).

```rust
let liquidator_ai = accounts.first().ok_or(...)?;
// ...
if !liquidator_ai.is_signer { return Err(...); }
```

This permissionless property is the heart of the liquidation engine. The system pays a small bounty (the liquidation penalty) to whoever first notices an underwater position and submits the liquidation tx. Without this, liquidations would depend on the protocol team running a centralized liquidator bot — which works but introduces uptime risk.

**Health check** (lines 2098–2111):

```rust
let equity = compute_equity(position, mark, funding_now);
let notional_val = notional(position.size, mark);
let maint_required = (notional_val * (MAINT_MARGIN_BPS as u128) / 10_000) as i128;

if equity >= maint_required {
    msg!(
        "liquidate: position is healthy (equity {} >= maint {}), not liquidatable",
        equity, maint_required
    );
    return Err(ProgramError::InvalidArgument);
}
```

If `equity >= maintenance_margin`, the position is fine and the call is rejected. The liquidator just paid tx fees for nothing — a small disincentive to spam-call liquidate against healthy positions. (Production protocols sometimes refund tx fees when this happens, or simply expect liquidators to do their own off-chain health check before submitting.)

**Apply penalty + force-close + run two CPIs**:

```rust
// Inside a borrow scope (so position data ref drops before CPIs):
let equity = compute_equity(position, mark, funding_now);
let raw_penalty = (notional_val * LIQUIDATION_PENALTY_BPS as u128 / 10_000) as i128;
let equity_positive = if equity < 0 { 0 } else { equity };
let penalty = raw_penalty.min(equity_positive);             // cap at available equity
let owner_remainder = (equity_positive - penalty).max(0);

penalty_amount = penalty as u64;
owner_amount = owner_remainder as u64;

position.collateral = 0;
position.size = 0;
position.entry_price = 0;
position.funding_snapshot_index = funding_now;

// ─── outside the borrow scope ───
// CPI 1: vault → liquidator
spl_token_transfer_vault_signed(vault_token_ai, liquidator_token_ai, ..., penalty_amount)?;
// CPI 2: vault → position owner
spl_token_transfer_vault_signed(vault_token_ai, owner_token_ai, ..., owner_amount)?;
```

The penalty is capped at the equity that survives (you can't pay a 50-unit bounty out of a position with 10 units of equity remaining). The two CPIs are sequential, both `invoke_signed` with the same vault-authority seeds. Either both succeed and the position is fully wound down, or the whole transaction reverts — atomicity is what keeps the books consistent.

The penalty serves two purposes:

1. **Liquidator incentive.** Running a liquidator bot has costs (RPC bandwidth, gas, monitoring infra). The penalty is the bounty that makes the work economically viable.
2. **User disincentive.** Approaching the liquidation threshold becomes costly even if you'd ultimately survive (e.g., the price reverses immediately after liquidation). Users are pushed to maintain higher buffers above MM than the IM↔MM gap suggests.

The Liquidate handler does NOT verify *why* the position is underwater. It could be price movement (mark moved against you), funding accumulation (rate compounded over time), or both. The equity calculation includes both contributions, and the maintenance check is on equity vs notional regardless of the cause. This is correct: liquidation triggers on insolvency, not on root cause.

> **Exercise §11.5.** Build the textbook "death spiral" scenario:
>   1. Open a long at size = 10, entry = 100, collateral = 100 (right at IM).
>   2. Move the oracle mark to 95 (price drop). Check `equity` and `maint_required` — is the position liquidatable? The drop costs 10 × (95 − 100) = -50, so equity = 50, maint = 10×95×0.05 = 47.5. Still healthy.
>   3. Move to 94. Equity = 40, maint = 47. *Now* liquidatable.
>   4. Submit Liquidate from a *different* keypair. Confirm the position closes and the penalty is applied.

---

## §11.6  The missing piece — insurance fund

One thing this chapter still does not implement, with its production role called out.

**Insurance fund.** A separate `InsuranceFund` account per market holds a pool of quote-currency that covers underwater-close shortfalls. The pattern:

```text
when ClosePosition / Liquidate computes equity < 0:
    shortfall = -equity
    if insurance_fund.balance >= shortfall:
        insurance_fund.balance -= shortfall
        # counterparty made whole, life continues
    else:
        # auto-deleverage or socialized loss — bigger architectural question
```

The insurance fund is funded by a fraction of liquidation penalties (e.g., 50% to liquidator, 50% to insurance fund), exchange fees, and sometimes by exchange equity at launch. Without an insurance fund, every losing position with insufficient collateral imposes a hidden loss on whoever was on the other side — usually the LP pool or the rest of the book.

In our current escrowed handlers, the residue of an underwater close stays in the vault — physically, the user's original deposit is still there, just not associated with any active position. That residue is implicitly subsidizing the counterparty. An insurance fund would route those leftovers properly: a fraction of each liquidation penalty into the fund at withdrawal time, a draw from the fund whenever an underwater close would otherwise leave a vault residue. The accounting is a small chapter on its own (15th in the track if added) — the math is simple, the wiring touches `Liquidate` and `ClosePosition`, and the new `InsuranceFund` PDA is the only state addition.

---

## §11.7  Recap + verify yourself

### Recap diagram

```
Position lifecycle:

  ┌──────────────────────────────────────────────────────────────────┐
  │                                                                  │
  │            ┌────────────────┐                                    │
  │   user ──► │ OpenPosition   │   reads: oracle(mark) + funding    │
  │            │                │   writes: position (size, entry,   │
  │            └───────┬────────┘            collateral, snapshot)   │
  │                    │                                             │
  │                    ▼                                             │
  │            ┌────────────────┐                                    │
  │            │   live state   │   (mark moves, funding accrues)    │
  │            └───┬────────┬───┘                                    │
  │                │        │                                        │
  │   owner-only   │        │   permissionless                       │
  │                ▼        ▼                                        │
  │     ┌──────────────┐  ┌──────────────┐                           │
  │     │ ClosePosition│  │  Liquidate   │   reads: oracle + funding │
  │     │ (settle PnL) │  │ (penalty +   │   writes: position        │
  │     │              │  │  force-close)│       (collateral, size=0)│
  │     └──────────────┘  └──────────────┘                           │
  │                                                                  │
  └──────────────────────────────────────────────────────────────────┘


Math:
  notional       = |size| × mark
  initial_margin = notional × INITIAL_MARGIN_BPS / 10000   (10%)
  maint_margin   = notional × MAINT_MARGIN_BPS / 10000     (5%)
  price_pnl      = size × (mark − entry_price)
  funding_pnl    = (index_now − snapshot_index) × size / 1e9
  equity         = collateral + price_pnl + funding_pnl
  liquidatable   = equity < maint_margin
```

### Three things to verify yourself

1. **The IM↔MM gap is the buffer.** Open a position right at IM (collateral = 10% of notional). Without any oracle move, check via the dump output that the position is healthy (equity ≈ collateral, well above MM). Move the oracle to where the IM gap is consumed (5% adverse). Now equity ≈ MM — still not liquidatable. One more bp of adverse move and `Liquidate` succeeds.
2. **Funding accrual can flip the answer.** Open a position with collateral right at MM. Don't touch the oracle. Let funding accumulate against you via `funding --update --rate 100`, wait a minute, `funding --update --rate 100`. Check the position's computed equity in the dump — funding PnL has dragged it below MM even though the price hasn't moved. `Liquidate` will succeed.
3. **Closing returns collateral; liquidating doesn't.** Open at IM, close immediately (no price move, no funding). `position.collateral` ≈ original. Open at IM again, let it fall to MM, get liquidated by a separate keypair. `position.collateral` after liquidate = equity − penalty ≈ much less. The penalty is the daylight between "exit cleanly" and "let yourself get liquidated."

---

## Hook into Chapter 12

You now have a perp DEX whose positions can be opened, closed, and force-liquidated. The unit of throughput is now bigger: a single `OpenPosition` involves 6 accounts, a `Liquidate` involves 4, and the supporting reads (oracle + funding) add a few more. The accounts touched form a write-set graph — and how that graph is laid out determines what Sealevel can run in parallel and what serializes.

Chapter 12 builds the **native vault program** — the dedicated wrapper account that aggregates user collateral into a fund that's traded as a whole. Vault depositors share PnL; the vault manager places trades on their behalf using the Phase B primitives we've built. The vault accounts form a different write-set graph than per-position trading: every deposit touches the vault aggregate, every trade touches positions owned by the vault. We'll see how the singleton-write-shared antipattern from Chapter 5 reasserts itself (the vault total *is* a singleton), and the design moves the architecture has to make to keep throughput sane.
