# Chapter 11 — Position Lifecycle and Liquidation Engine

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`Position`, `InsuranceFund`), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (helpers + `process_open_position`, `process_close_position`, `process_liquidate`, `process_create_insurance_fund`, `process_insurance_fund_deposit`), [`scripts/position/src/main.rs`](../../scripts/position/src/main.rs).

---

## §11.0  Framing

This is the convergence chapter. Every other Phase A and Phase B primitive — oracle, funding, vault, matcher, parallelism — exists so this chapter can be written. A perp DEX without `OpenPosition` / `ClosePosition` / `Liquidate` is a collection of pieces; a perp DEX with them is a perp DEX.

The chapter ships three instructions, each of which integrates the SPL Token escrow path from Chapter 6 directly into the position lifecycle:

1. **`OpenPosition`** — creates a per-(user, market) Position PDA, reads the oracle for the entry price, snapshots the cumulative funding index for later settlement, validates the initial margin requirement, and **escrows the collateral** by CPI'ing an SPL Token Transfer from the user's quote token account into the market vault (the per-(market, mint) vault built in Chapter 6).
2. **`ClosePosition`** — the owner's exit. Settles funding via the snapshot pattern from Chapter 10, computes realized PnL = `size × (mark - entry)`, **transfers the realized amount back to the user** via an SPL Token CPI signed by the vault authority PDA (`invoke_signed` with `[b"vault_auth", market]` seeds), and zeros the position.
3. **`Liquidate`** — anyone's exit on someone else's underwater position. Computes equity, compares to maintenance margin, and if the position has fallen below, force-closes at the current mark. **Two SPL Token CPIs run inside the handler:** vault → liquidator for the penalty bounty, and vault → position-owner for whatever remains. Both signed by the vault authority PDA.

The collateral now lives where a real perp DEX puts it — the program's vault token account, owned by SPL Token, controlled by an `invoke_signed`-only PDA. The position record holds the *bookkeeping* (size, entry price, snapshot index); the vault holds the *money*. The two stay in sync because every state transition that touches the bookkeeping also runs the matching CPI.

The chapter also ships the §11.6 **insurance fund**: a per-market `InsuranceFund` PDA + dedicated token account that receives a configurable slice (`INSURANCE_FUND_PENALTY_SHARE_BPS = 5000`, i.e. half) of every liquidation penalty and drains to cover underwater-close shortfalls. The fund's token account is authority-shared with the position vault (`[b"vault_auth", market]`), so no new signer PDA is introduced. Bootstrap via `CreateInsuranceFund` once per market; permissionless top-up via `InsuranceFundDeposit`.

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

**`collateral: u64`** is the quote-currency margin amount. Strictly positive while the position is open; can be reduced to zero by underwater close or liquidation. Cannot go negative — losses beyond collateral are absorbed by the per-market insurance fund up to its balance (§11.6); the residual once the fund is empty falls on whoever holds the other side of the trade (no autodeleverage logic in this chapter — that branch is its own architectural problem and is the only insurance-fund piece §11.6 still defers).

**`funding_snapshot_index: i64`** is the cumulative funding index at the last touch (open, close, liquidate). The per-position settle pattern from Chapter 10 makes this the only field needed for funding accounting — the delta between `funding_now` and `funding_snapshot_index` times `size` is the funding PnL accrued since the snapshot.

The PDA derivation uses both `user` and `market` as seeds: `[b"position", user.key, market.key]`. So every (user, market) pair has exactly one position address that everyone can compute without storing a mapping anywhere. The pubkey is bound to the asset pair and the trader by the seed scheme alone.

> **Exercise §11.1.** Why does the position store both `user` and `market` *inside* the account, despite both being seeds of the PDA derivation? (Hint: think about what a third party reading the account knows vs. what they have to derive.)

---

## §11.2  Equity, notional, and the margin formulas

Before walking the handlers, fix the formulas. From `programs/openhl-core/src/lib.rs:2329–2342`:

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

**Read external inputs** (lines 2911–2912):

```rust
let mark = read_fresh_oracle(oracle_ai, program_id)?;
let funding_snapshot = read_funding_index(funding_ai, program_id)?;
```

`read_fresh_oracle` (lines 2353–2372) factors the Chapter 9 staleness gauntlet into a helper — same checks (owner + discriminator + price>0 + age vs Clock), reused across all three position handlers. `read_funding_index` (lines 2597–2607) is the simpler read used to snapshot the funding index.

**Initial margin check** (lines 2915–2926):

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

`process_close_position`. Simpler than open in one dimension (no PDA creation) but more involved in another: it runs two outbound SPL Token CPIs signed by the vault authority PDA via `invoke_signed` — the user payout, and (on underwater close) the insurance fund's shortfall drain. The handler takes 12 accounts; the last two are the insurance fund state and its token account (§11.6).

**Validation + owner check** (lines 3052–3062):

```rust
if position.user != *user_ai.key.as_ref() {
    msg!("close_position: caller is not the position owner");
    return Err(ProgramError::IllegalOwner);
}
```

Only the position's owner may close it voluntarily. Liquidate (§11.5) is the route for anyone else. The user check uses the `user` field stored in the position rather than the PDA derivation — same information, easier to read.

**Read external inputs + compute equity** (lines 3040–3072):

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

**Underwater closes drain the insurance fund.** A position that closes with equity = -50 sends `payout = 0` to the user, and the handler additionally drains `min(50, fund.balance)` from the insurance fund's token account into the vault. The drain is the bookkeeping side of "the protocol covered the shortfall instead of the residue silently subsidizing the counterparty" — see §11.6 for the full design including the cap behavior when the fund runs dry.

> **Exercise §11.4.** Open a position at entry = 100, size = 5, collateral = 100. Move the oracle to mark = 80. Close. The expected equity is `100 + 5 × (80 - 100) = 0`. Verify the user's quote token balance after the close is unchanged from before the open (because payout = 0 — the 100 they deposited went into the vault and stayed there).

---

## §11.5  Walking `Liquidate`

`process_liquidate`. The crucial difference from close: **anyone can call it**. The handler runs *four* outbound SPL Token CPIs — vault → liquidator (the liquidator's slice of the penalty), vault → insurance fund (the fund's slice of the penalty), vault → position-owner (the remainder), and fund → vault (the shortfall drain when equity < 0) — all signed by the same vault authority PDA. The handler takes 13 accounts; the last two are the InsuranceFund state and its token account (§11.6).

**Validation**: the *liquidator* must be a signer, but the program does *not* check that the liquidator matches the position's user. Anyone can call liquidate on anyone's position. Additional escrow-side checks: token_program is SPL Token, both `owner_token` and `liquidator_token` are SPL Token-owned, vault_token matches the derived PDA, vault_authority matches the derived PDA (and the bump is captured for the two invoke_signed calls below).

```rust
let liquidator_ai = accounts.first().ok_or(...)?;
// ...
if !liquidator_ai.is_signer { return Err(...); }
```

This permissionless property is the heart of the liquidation engine. The system pays a small bounty (the liquidation penalty) to whoever first notices an underwater position and submits the liquidation tx. Without this, liquidations would depend on the protocol team running a centralized liquidator bot — which works but introduces uptime risk.

**Health check** (lines 3233–3253):

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

**Apply penalty (split between liquidator and fund) + force-close + run four CPIs**:

```rust
// Inside a borrow scope (so position data ref drops before CPIs):
let equity = compute_equity(position, mark, funding_now);
let raw_penalty = (notional_val * LIQUIDATION_PENALTY_BPS as u128 / 10_000) as i128;
let equity_positive = if equity < 0 { 0 } else { equity };
let penalty = raw_penalty.min(equity_positive);             // cap at available equity
let owner_remainder = (equity_positive - penalty).max(0);

// Split the penalty per INSURANCE_FUND_PENALTY_SHARE_BPS (= 5000 → 50/50).
let fund_slice = penalty * INSURANCE_FUND_PENALTY_SHARE_BPS / 10_000;
let liquidator_slice = penalty - fund_slice;

position.collateral = 0;
position.size = 0;
position.entry_price = 0;
position.funding_snapshot_index = funding_now;
// ─── borrow ends ───

// Fund state updates (own borrow scope):
//   fund.balance         += fund_slice;
//   fund.total_deposits  += fund_slice;
//   shortfall_drain      = if equity < 0 { min(-equity, fund.balance) } else { 0 };
//   fund.balance         -= shortfall_drain;
//   fund.total_drawdowns += shortfall_drain;

// Then four Token Transfers, all invoke_signed with vault_authority seeds:
spl_token_transfer_vault_signed(vault_token_ai, liquidator_token_ai, ..., liquidator_slice)?;
spl_token_transfer_vault_signed(vault_token_ai, fund_token_ai,       ..., fund_slice)?;
spl_token_transfer_vault_signed(vault_token_ai, owner_token_ai,      ..., owner_remainder)?;
spl_token_transfer_vault_signed(fund_token_ai,  vault_token_ai,      ..., shortfall_drain)?;
```

The penalty is capped at the equity that survives (you can't pay a 50-unit bounty out of a position with 10 units of equity remaining). The four CPIs are sequential, all `invoke_signed` with the same vault-authority seeds. Either every Transfer succeeds and the position + fund state are fully wound, or the whole transaction reverts. The shortfall drain only fires (non-zero) on the equity < 0 path; on a healthy liquidation it's a 0-amount Transfer that the helper short-circuits.

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

## §11.6  Insurance fund — penalty split, shortfall drain

The vanilla escrow path leaves one honesty problem: when a position closes underwater (`equity < 0`), the user's deposited collateral is already in the vault, the user gets 0, and the residue implicitly subsidizes the counterparty. There's no accounting of "the protocol absorbed this loss" — the vault is just quieter than it should be. The insurance fund is the bookkeeping piece that fixes this.

### The state

Two PDAs per market:

```rust
// crates/state/src/lib.rs
pub struct InsuranceFund {
    pub discriminator: [u8; 8],   // INSFUND\0
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub mint: [u8; 32],           // quote_mint (matches market.quote_mint)
    pub balance: u64,             // mirrors the token-account balance
    pub total_deposits: u64,      // observability
    pub total_drawdowns: u64,     // observability
    pub _reserved: [u8; 32],
}
```

```text
fund state PDA  : [b"insurance_fund",       market]      — program-owned
fund token PDA  : [b"insurance_fund_token", market, mint] — SPL Token-owned,
                                                             authority = vault_authority
                                                             (the same PDA Liquidate
                                                              already signs as)
```

Reusing `vault_authority` is the choice that keeps the implementation small. The position vault and the fund vault are different token accounts but share one signer PDA — so `Liquidate` already knows how to sign `vault → fund` or `fund → vault` Transfers without introducing a new authority surface. The on-chain `balance` counter is redundant with the SPL Token account's lamport-side balance, but it gives scripts and indexers cheap access to the running total without parsing SPL Token bytes, and it lets the chapter's "verify yourself" tests assert on it directly.

### Penalty split

`Liquidate` now splits the penalty between the liquidator and the fund:

```rust
let fund_slice = penalty * INSURANCE_FUND_PENALTY_SHARE_BPS / 10_000;  // = penalty / 2
let liquidator_slice = penalty - fund_slice;
```

With `INSURANCE_FUND_PENALTY_SHARE_BPS = 5000`, half the penalty goes to the fund as a deposit, the rest to the liquidator as their bounty. Three Token Transfers fire per liquidate now instead of two — vault → liquidator (liquidator slice), vault → fund (fund slice), vault → owner (remainder).

In a healthy liquidation (equity > 0, position only underwater on margin), the liquidator still gets paid enough to make liquidation worth their CU + tx-fee cost. With `LIQUIDATION_PENALTY_BPS = 100` and the 50/50 split, the liquidator's effective bounty drops from 1% of notional to 0.5%. For our chapter values that's fine; production tuning is a function of typical position size, liquidator infrastructure costs, and how aggressively the protocol wants to grow the fund.

### Shortfall drain

`ClosePosition` and `Liquidate` both run the same drain logic at the end of their handler:

```rust
let shortfall_drain = if equity < 0 {
    let shortfall = (-equity).min(u64::MAX as i128) as u64;
    shortfall.min(fund.balance)        // never drain more than the fund holds
} else {
    0
};
fund.balance -= shortfall_drain;
fund.total_drawdowns += shortfall_drain;
// ... then: SPL Token Transfer fund_token → vault_token (shortfall_drain)
```

The Transfer goes *into* the vault, not to the user. The user still receives `payout = if equity < 0 { 0 } else { equity }`. The drain is bookkeeping for the protocol — it represents the fund covering the loss the vault would otherwise have absorbed silently. The Transfer is signed by `vault_authority`, the same PDA that signs the vault → user payout, so the same `invoke_signed` call path is reused for both.

When `fund.balance < shortfall` the drain caps at `fund.balance`. The uncovered remainder is a *socialized loss*: it stays in the vault as residue and falls implicitly on whoever holds the other side of the trade. Production exchanges resolve this with **autodeleverage** (force-close the most profitable counterparty positions until the loss is fully absorbed) or **explicit socialization** (mark all open positions in the same direction down). Our chapter ships the simpler path of "let it stay residue, document the cap" — autodeleverage is its own architectural problem.

### CreateInsuranceFund + InsuranceFundDeposit

`CreateInsuranceFund` is the one-shot bootstrap, run once per market. It mirrors `CreateVault` from Chapter 6 — System `create_account` with PDA signing for both the state account and the token account, plus an `InitializeAccount3` CPI to set the fund-token's owner to `vault_authority`.

`InsuranceFundDeposit` is permissionless. Anyone with quote tokens can credit the fund — most commonly the protocol team for initial seed, but in practice donations or admin top-ups are valid uses too. The handler runs in the standard "update counters in borrow scope, drop borrow, Transfer at the end" pattern, with `total_deposits` advancing alongside `balance`.

### Two account additions to ClosePosition / Liquidate

Both handlers grew two accounts at the end of their list:

```
N-2.  [WRITE]  insurance_fund        — InsuranceFund state PDA
N-1.  [WRITE]  insurance_fund_token  — fund's SPL token account
```

`ClosePosition` is now 12 accounts; `Liquidate` is 13. The fund-state writability is for the counter updates; the fund-token writability is for the penalty-credit Transfer and (on underwater close) the shortfall drain.

> **Exercise §11.6.** Open a position, push the oracle hard against it until equity falls below `maint`, but stop *before* equity goes negative. Liquidate. Observe in the dump: the fund's `total_deposits` should grow by `fund_slice`. Now reduce the oracle further to push equity below zero on the same market with a fresh position. Liquidate again — `total_drawdowns` should grow by `min(-equity, fund.balance)`. When does the fund stop being able to absorb shortfalls, and what would you ship at that point?

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
