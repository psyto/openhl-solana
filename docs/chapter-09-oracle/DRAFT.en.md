# Chapter 9 — Oracle Ingestion: Pyth Internals

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`Oracle`, plus the Pyth v1 layout constants for §9.5), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_create_oracle`, `process_set_oracle_price`, `process_place_order_checked`, `process_place_order_checked_pyth`, the `read_fresh_oracle` and `read_fresh_pyth_v1_oracle` helpers), [`scripts/oracle/src/main.rs`](../../scripts/oracle/src/main.rs).
> Reference targets: Pyth Network mainnet program (`FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH`), Switchboard On-Demand.

---

## §9.0  Framing — and a deliberate mock

A perp DEX without an external price oracle is a derivatives market whose mark price is just "wherever the last trade happened." That works fine until a stale book or a thin moment lets the last trade drift away from the spot market — at which point your liquidation engine starts triggering on prices that have nothing to do with reality, and your insurance fund pays for it. The mark price is the load-bearing input to funding rate, liquidations, margin requirements, and every other risk-side computation in the program. It cannot come from the program's own trade tape; it must come from outside.

The standard answer on Solana is **Pyth Network** (with **Switchboard** as a common secondary). Both publish per-asset price accounts that any program can read. The accounts are owned by the publisher's program, not yours — you are strictly a reader.

This chapter is about how a reader handles oracle input safely. The job has three pieces:

1. Find the price account, validate its layout, read the price + confidence + exponent.
2. Check freshness via the Clock sysvar — refuse to operate on a stale price.
3. Apply the price to a meaningful program check — here, a sanity band on `place_order`.

For the worked example, we build our own `Oracle` account type that mirrors the shape of a Pyth price feed and is owned by our program. This is a deliberate scope call. A genuine Pyth integration would import `pyth-sdk-solana`, pull the price feed account's owner check from `pyth_program_id`, and parse a price-update message that has a non-trivial v2 format. Doing that here would teach the SDK call rather than the *reading pattern*. By owning our oracle locally we control the publish moment, which makes staleness experiments trivial — and the techniques (staleness check, sanity band, defensive parse) transfer directly. The chapter calls out the production differences carefully.

---

## §9.1  Pyth in shape, in summary

A real Pyth v1 price account is a ~3 KiB struct that includes a small header (magic + version + type + size), product association, and an array of recent price observations. The fields we actually need fit in 24 bytes:

```text
price:        i64    // signed mantissa
conf:         u64    // 1-sigma confidence interval, same units
expo:         i32    // base-10 exponent (typically negative, e.g. -8 → 8 decimals)
publish_slot: u64    // slot at which this price was last updated
```

The real mark price is `price × 10^expo`. The confidence interval `conf × 10^expo` bounds how tight the price is — a wide conf means the publisher is uncertain, and many programs refuse prices with `conf > tolerance × price`.

Our `Oracle` struct at `crates/state/src/lib.rs` carries exactly this shape, plus discriminator, bump, and the market it prices:

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Oracle {
    pub discriminator: [u8; 8],   // 0..8   — ORACLE\0\0
    pub bump: u8,                  // 8
    pub _pad0: [u8; 7],            // 9..16
    pub market: [u8; 32],          // 16..48
    pub price: i64,                // 48..56
    pub conf: u64,                 // 56..64
    pub expo: i32,                 // 64..68
    pub _pad1: [u8; 4],            // 68..72
    pub publish_slot: u64,         // 72..80
    pub _reserved: [u8; 32],       // 80..112
}
```

112 bytes total, Pod, repr(C). Bytemuck-cast from the raw account data with no allocation, same trick as the OrderBook from Chapter 7.

The differences from a real Pyth integration, called out so they don't surprise you:

| Aspect | Our `Oracle` | Real Pyth |
|---|---|---|
| Account owner | openhl-core (our program) | Pyth program (`FsJ3...epH` on mainnet) |
| Owner check looks for | `program_id` (our own) | `&pyth_program::ID` |
| Account layout | this 112-byte struct | Pyth v1 PriceAccount (~3 KiB) or v2 update message |
| Update mechanism | `SetOraclePrice` instruction (our publisher) | Pyth publishers call into the Pyth program |
| Discriminator | `ORACLE\0\0` (our convention) | Pyth's magic constant + version field |
| Staleness clock | Clock sysvar `slot` (our publish_slot) | Pyth's `publish_time` + `prev_publish_time` |

Every column on the right has a direct counterpart in the column on the left. Everything you do with our `Oracle` you do with a real Pyth account, just with different magic numbers and a different owner check.

> **Exercise §9.1.** Look up Pyth's SOL/USD price account on mainnet. Note its size (in bytes), its owner program, and the first 4 bytes of its data (the Pyth magic constant). Compare to our `Oracle`'s size, owner, and first 8 bytes.

---

## §9.2  Writing the oracle — `SetOraclePrice`

For the chapter to exercise staleness scenarios we need a way to write the oracle at a known moment. From `programs/openhl-core/src/lib.rs:1513–1571`:

```rust
fn process_set_oracle_price(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    // ... payload + account validation ...
    msg!("set_oracle_price: open-auth publisher = {}", publisher_ai.key);

    let clock = Clock::get()?;
    let mut data = oracle_ai.try_borrow_mut_data()?;
    let oracle: &mut Oracle = bytemuck::from_bytes_mut(&mut data[..Oracle::LEN]);
    // ... discriminator check ...

    oracle.price = price;
    oracle.conf = conf;
    oracle.expo = expo;
    oracle.publish_slot = clock.slot;
    // ...
}
```

Two things to absorb.

**`Clock::get()?` is a syscall.** The Clock sysvar carries `slot`, `epoch`, `unix_timestamp`, and a few related fields. It is the *only* way a program knows what slot it is currently executing in. Programs cannot read a wall clock and cannot trust the user to supply the current time. Stamping `publish_slot = clock.slot` at write time is the foundation of the staleness check we do at read time.

**The publisher check is missing on purpose.** The instruction accepts a signer but does not verify *which* signer. In production this is wrong — anyone with the program ID could write any price and trigger the sanity band to accept any limit. The fixes are:

1. **Pin to a known publisher pubkey.** Read a `pub const ORACLE_PUBLISHER: Pubkey = ...;` and check `publisher_ai.key == &ORACLE_PUBLISHER`. Simple, requires a publisher rotation procedure if the pubkey changes.
2. **Multi-publisher signature.** Store a set of acceptable publishers in the oracle account itself. Either signer must match.
3. **Hand off the account to Pyth.** Make the oracle account owned by the Pyth program, drop `SetOraclePrice` entirely. Now you cannot write the oracle yourself, which is the right architecture for production.

The chapter codes (1) and (2) as exercises and walks (3) in prose. The deliberate auth gap is so the reader can pause the price at known slots and run the staleness scenarios in §9.3.

> **Exercise §9.2.** Add a `ORACLE_PUBLISHER: Pubkey` constant to `programs/openhl-core/src/lib.rs` and an explicit check in `process_set_oracle_price` that `publisher_ai.key == &ORACLE_PUBLISHER`. Choose the constant to be your own wallet pubkey. Verify that `oracle --set ...` still works from your wallet but fails from a fresh keypair.

---

## §9.3  Reading the oracle — staleness as the foundational check

The reader pattern lives in a small helper `read_fresh_oracle` (lib.rs:1965–1987), called from `process_place_order_checked` and reused by the three position handlers Chapter 11 will add (so the gauntlet stays in one place rather than copied four times):

```rust
fn read_fresh_oracle(
    oracle_ai: &AccountInfo,
    program_id: &Pubkey,
) -> Result<u64, ProgramError> {
    if oracle_ai.owner != program_id || oracle_ai.data_len() != Oracle::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    let oracle_data = oracle_ai.try_borrow_data()?;
    let oracle: &Oracle = bytemuck::from_bytes(&oracle_data[..Oracle::LEN]);
    if oracle.discriminator != ORACLE_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }
    if oracle.price <= 0 {
        return Err(ProgramError::InvalidAccountData);
    }
    let clock = Clock::get()?;
    let age = clock.slot.saturating_sub(oracle.publish_slot);
    if age > MAX_ORACLE_STALENESS_SLOTS {
        msg!("oracle stale ({} slots, max {})", age, MAX_ORACLE_STALENESS_SLOTS);
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(oracle.price as u64)
}
```

`process_place_order_checked` calls it as `let mark = read_fresh_oracle(oracle_ai, program_id)?;` and then runs the sanity band against `mark`.

Four checks before the price is trusted:

1. **Discriminator check** (`oracle.discriminator != ORACLE_DISCRIMINATOR`): refuses an oracle account that isn't initialized. In real Pyth this is the magic constant + version match.
2. **Price-positivity check** (`oracle.price <= 0`): refuses oracle states with non-positive prices. Real Pyth occasionally publishes `0` to signal "no good price right now" — your reader must handle that.
3. **Staleness check** (`age > MAX_ORACLE_STALENESS_SLOTS`): refuses prices older than 25 slots (~10 sec). This is the heart of the chapter. A price you cannot freshness-check is a price you cannot trust — because an attacker who can pause the publisher (or just exploit a network outage) can use a stale price to game any program that trusts it blindly.
4. **Owner check** (line 1969, `oracle_ai.owner != program_id`): refuses an account from a different program. In real Pyth this is `oracle_ai.owner == &pyth_program::ID`.

`MAX_ORACLE_STALENESS_SLOTS = 25` from `lib.rs:205`. The choice is workload-driven: 25 slots is ~10 seconds at the current target slot time. High-volatility pairs (BTC, ETH on a fast-moving day) need tighter — perhaps 10–15 slots. Stablecoin pairs can tolerate wider. The constant should ideally live on the per-market `Market` struct so each market tunes it; we keep it global for simplicity.

The borrow inside `read_fresh_oracle` is naturally scoped to the helper's body — it drops when the function returns, before the caller mutates the book. This matters because both the oracle and the book are passed as `AccountInfo`, and the runtime requires that no two mutable borrows of the same underlying account memory coexist. Even though our oracle and book are different accounts, the pattern of scoping borrows tightly is good hygiene — it prevents subtle aliasing bugs when handlers grow.

**What the SDK hides:** `pyth-sdk-solana::load_price_feed_from_account_info` does the discriminator check, the owner check, and a deserialization into a typed `PriceFeed`. It does *not* do the staleness check — that is always your job. Programs that use Pyth without an explicit staleness gate ship with one of the largest classes of oracle bugs in DeFi.

> **Exercise §9.3.** Set the oracle price at slot N (run `oracle --set --price 100 ...` and note the slot from the output). Wait 30 slots (about 12 seconds; just count slots in `solana confirm` against any tx). Now run `oracle` with no flags. The reported `age (slots)` should exceed 25. Run `place-order-checked` (assuming you wire one) — it should fail with `oracle stale`.

---

## §9.4  Using the oracle — the sanity band

A staleness-checked price is now safe to read. The first risk control we use it for: refuse `place_order` calls whose limit price drifts too far from the oracle mark.

From `process_place_order_checked` lines 1658–1671:

```rust
let band = mark.saturating_mul(SANITY_BAND_BPS) / 10_000;
let low = mark.saturating_sub(band);
let high = mark.saturating_add(band);
if price < low || price > high {
    msg!(
        "place_order_checked: price {} outside sanity band [{}, {}] (mark={})",
        price,
        low,
        high,
        mark
    );
    return Err(ProgramError::InvalidArgument);
}
```

`SANITY_BAND_BPS = 2000` (lib.rs:211) means ±20%. With `mark = 100`, an order at price 50 is rejected (below `low = 80`), an order at 95 is accepted, an order at 121 is rejected. A wide band on purpose: tighter bands cause legitimate users to fail more often during normal volatility, and ch.9 is about the *pattern*, not the calibration.

Production bands are tuned per market:

- **High-vol perps (BTC, ETH on a wild day):** 5–10% might be acceptable; wider rejects too many legitimate fat-finger-adjacent orders.
- **Mid-vol perps (SOL, AVAX):** 3–5% typical.
- **Low-vol pairs (stablecoin perps, FX):** 1–2%, sometimes tighter.

The band is the first risk control in the program because it's the simplest one that depends on external truth. Funding rate (Chapter 10) and liquidation (Chapter 11) build on the same oracle read, applying it to harder math.

**Saturating arithmetic.** `saturating_mul` and `saturating_sub` are used deliberately. If `mark = u64::MAX` (impossible in practice but theoretically) the multiplication would wrap. Saturating reduces that to a band of `[u64::MAX - band, u64::MAX]`, which fails all reasonable orders gracefully instead of producing a bizarre band that happens to be 0..something due to wraparound. Solana's program runtime panics on integer overflow (in `release` builds it wraps silently, in `debug` it panics) — explicit saturating ops are a small habit that pays off in audits.

> **Exercise §9.4.** Set the oracle price to 100. Try to place an order at price 90 (inside band), 75 (outside band — below low at 80), 120 (just inside — high is 120 since mark*0.2=20). Trace each. Then change `SANITY_BAND_BPS` to 500 (5%) and re-test the same prices.

---

## §9.5  Production Pyth v1 — implemented bytes-up

The mock `Oracle` is built so the chapter can pause publishes at known slots. Production reads from a Pyth account that publishers populate independently. The migration is localized — same staleness gauntlet, same sanity band, different parser — and the chapter ships it as a peer instruction `PlaceOrderCheckedPyth` (tag 32) so you can compare the two paths side by side. The Pyth v1 layout is well-defined enough that we parse it bytes-up, with no `pyth-sdk-solana` dependency — the bytes-up posture from Chapter 1 carries straight through.

### The Pyth v1 PriceAccount layout

Every Pyth v1 price account begins with a magic constant + version + account-type discriminator, then a header of EMA bookkeeping, then the aggregate `agg` block we actually trust. The six fields we read:

```text
offset | size | field                       | what we check
-------+------+-----------------------------+---------------------------------
   0   |  4   | magic (u32)                 | == 0xa1b2c3d4
   4   |  4   | ver   (u32)                 | == 2 (Pyth v1)
   8   |  4   | atype (u32)                 | == 3 (PriceAccount)
  20   |  4   | expo  (i32)                 | logged; not applied (see below)
 208   |  8   | agg.price (i64)             | > 0
 216   |  8   | agg.conf  (u64)             | logged
 224   |  4   | agg.status (u32)            | == 1 (Trading)
 232   |  8   | agg.pub_slot (u64)          | clock.slot - pub_slot <= MAX_ORACLE_STALENESS_SLOTS
```

(`agg` is the publisher network's aggregated price for the slot. Pyth has up to 32 publishers; the on-chain aggregation runs every slot and `agg.status` reflects whether they agreed.)

Offsets are exported as `PYTH_V1_OFFSET_*` constants from `crates/state/src/lib.rs`. We deliberately don't mirror the full ~3 KiB `PriceAccount` as a Pod struct — the bulk of the bytes are per-publisher `comp[]` entries we don't read. Hardcoding the six offsets is fewer bytes of code, easier to audit, and matches the chapter's "the byte layout *is* the contract" stance.

### `read_fresh_pyth_v1_oracle`

The helper, in skeleton:

```rust
fn read_fresh_pyth_v1_oracle(oracle_ai: &AccountInfo) -> Result<u64, ProgramError> {
    let data = oracle_ai.try_borrow_data()?;
    if data.len() < PYTH_V1_MIN_LEN { return Err(InvalidAccountData); }

    // (1) Header gauntlet.
    if u32::from_le_bytes(data[0..4]) != PYTH_V1_MAGIC      { return Err(...); }
    if u32::from_le_bytes(data[4..8]) != PYTH_V1_VERSION    { return Err(...); }
    if u32::from_le_bytes(data[8..12]) != PYTH_V1_ACCOUNT_TYPE_PRICE { return Err(...); }

    // (2) Aggregate status.
    let status = u32::from_le_bytes(data[PYTH_V1_OFFSET_AGG_STATUS..]);
    if status != PYTH_V1_STATUS_TRADING { return Err(InvalidAccountData); }

    // (3) Staleness — agg.pub_slot vs Clock.slot.
    let pub_slot = u64::from_le_bytes(data[PYTH_V1_OFFSET_AGG_PUB_SLOT..]);
    let age = Clock::get()?.slot.saturating_sub(pub_slot);
    if age > MAX_ORACLE_STALENESS_SLOTS { return Err(InvalidAccountData); }

    // (4) Read price + log expo/conf for observability.
    let price_i = i64::from_le_bytes(data[PYTH_V1_OFFSET_AGG_PRICE..]);
    if price_i <= 0 { return Err(InvalidAccountData); }
    Ok(price_i as u64)
}
```

The check order mirrors the mock path: structural validation first (magic / version / atype), then the status check (the Pyth analog of our mock's discriminator+initialization check), then staleness, then the price sanity. Any failure returns `InvalidAccountData` so callers can handle a stale or non-Trading Pyth feed the same way they handle a stale mock.

Four design decisions worth flagging:

**1. No owner check on the Pyth account.** The mock path requires `oracle_ai.owner == program_id`; we deliberately *don't* require `oracle_ai.owner == &pyth_program::ID` here. Hardcoding the Pyth program ID would force a mainnet-vs-devnet choice up front (`FsJ3...epH` for mainnet, `gSbE...92s` for devnet), and the magic+version+atype gauntlet already structurally guarantees the account is a Pyth v1 PriceAccount. A production deployment would either (a) pin the expected Pyth program ID on the `Market` struct, or (b) pin the expected Pyth account *pubkey* on the `Market` — option (b) is tighter because it prevents a caller from swapping in a Pyth feed for the wrong asset.

**2. `expo` is read and logged, not applied.** Pyth prices come as `(mantissa, expo)` pairs — the real price is `mantissa × 10^expo`. For USD pairs `expo` is typically `-8`. The mock side has no expo (our `Oracle.price` is a u64 in whatever scale the market uses), so to keep the two paths interoperable in our chapter we treat the mantissa as the price and log `expo` for observability. A production deployment would normalize: either pin a single supported `expo` and reject anything else, or translate the price into the program's fixed-point convention on the fly. This is the most likely place to write a real bug in the migration; the chapter calls it out so you can put a TODO there yourself.

**3. We refuse `price <= 0` even when status == Trading.** Pyth occasionally publishes zero — a publisher-network corner case where the aggregate is non-negative but degenerate. The mock path refuses the same; the Pyth path matches.

**4. `agg.status != Trading` is a hard refusal, not a fallback.** A real production reader with a Switchboard fallback would try the other oracle here rather than returning an error. We don't ship the fallback because the *worked example* is the v1 single-oracle migration; the fallback pattern (try-A-then-B with disagreement tolerance) is straightforward to add on top — the §9.5 closing notes lay it out.

### `PlaceOrderCheckedPyth` (tag 32)

The peer instruction. Same payload as `PlaceOrderChecked` (`[side u8][price u64 LE][size u64 LE]`), same eight prefix accounts modulo two changes:

- Slot 2 is a Pyth v1 PriceAccount, not our mock `Oracle`. (The oracle-PDA-from-market cross-check is dropped; Pyth accounts aren't ours to PDA-derive.)
- The oracle-side gauntlet calls `read_fresh_pyth_v1_oracle` instead of `read_fresh_oracle`.

Everything downstream — sanity-band, fee escrow, book write — is identical to the mock path. Both handlers share the same `SANITY_BAND_BPS`, `PROTOCOL_FEE_BPS`, and the same `spl_token_transfer_user_signed` Transfer at the end. The migration's surface is exactly what §9.5 promised: localized.

### The v1 / v2 caveat

The "v1" in `read_fresh_pyth_v1_oracle` matters. Pyth's v1 model — publishers writing a static PriceAccount each slot, readers parsing it — is being deprecated in favor of v2's pull-oracle architecture, where publishers sign update messages off-chain that any caller can verify (via Wormhole VAA verification) and apply to a price-cache account. v2 is structurally different: not a layout change but a model change. Reading "real Pyth" in 2025 increasingly means v2, which requires cryptographic verification we don't ship here.

What you'd actually do for v2:

1. Add a `VerifyPythUpdate` instruction that takes a Pyth update message + the Wormhole guardian set, verifies the signatures, and writes the verified price into a per-market price-cache PDA owned by your program.
2. Trading instructions then read from that cache PDA (using the same mock-shape gauntlet — the cache is structurally a mock `Oracle` you populate at update-time).
3. The trading critical path stays bytes-up; only the update path involves the SDK.

That's a separate chapter — the Wormhole verification step is its own machinery, and parking it inside ch.9 would blow the chapter's scope. The v1 reader here is the smaller of the two paths and is what Phoenix and most existing Solana CLOBs still rely on; ship that, ship the v2 path later if you need it.

### The Switchboard fallback

A single oracle is a single point of failure. Pyth has been down. Switchboard has been down. Both at the same time has happened (rarely). Programs that protect downside trust *both* and refuse to operate when neither is fresh. The wiring is mechanical:

1. The transaction's `AccountMeta` array includes both oracle accounts.
2. The handler reads each, doing the full validation pattern (discriminator/magic + status + price-positive + staleness).
3. If either passes, use it. If both fail, refuse the call.

Programs that do this also typically *compare* the two when both are fresh — refuse the call if they disagree by more than some tolerance (e.g., 50 bps). A 50-bp disagreement between Pyth and Switchboard usually means one of them is wrong, and a program that just picks the cheaper price for the user has been gamed.

> **Exercise §9.5.** Pick a real Pyth v1 mainnet PriceAccount (SOL/USD is `H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG` at the time of writing — verify via Pyth's price-feed-ids page). Use the local validator's `clone` feature to mirror that account into your test cluster, then invoke `PlaceOrderCheckedPyth` against it. Compare the logged `expo` to the SOL/USD product (should be `-8`). Confirm the staleness gauntlet by replaying a stale slot of the account from an archive.

---

## §9.6  Recap + verify yourself

### Recap diagram

```
External truth → program safety:

  Pyth/Switchboard publisher          Our Oracle account
   ┌──────────────────┐                ┌───────────────────────┐
   │ writes price+conf│ ──── owns ───► │  Pyth program (mainnet)│
   │  every few slots │                │  openhl-core (ours)    │
   └──────────────────┘                └───────────────────────┘
                                                │  reads
                                                ▼
                                   ┌─────────────────────────────┐
                                   │ place_order_checked         │
                                   │  - load oracle account      │
                                   │  - owner check              │
                                   │  - discriminator check      │
                                   │  - price > 0                │
                                   │  - staleness via Clock      │
                                   │  - sanity band ±X bps       │
                                   │  - then run normal place    │
                                   └─────────────────────────────┘

Required checks before trusting a price:

  ┌────────────────────────┬──────────────────────────────────┐
  │ Check                  │ Where                            │
  ├────────────────────────┼──────────────────────────────────┤
  │ owner == oracle program│ before any data read             │
  │ discriminator matches  │ first 8 bytes of data            │
  │ price > 0              │ rejects "no price" signal        │
  │ slot age <= MAX        │ requires Clock sysvar            │
  │ (optional) conf small  │ rejects wide / uncertain prices  │
  │ sanity band            │ applies the price to user input  │
  └────────────────────────┴──────────────────────────────────┘
```

### Three things to verify yourself

1. **The discriminator check matters.** Create a market PDA, then construct a transaction that calls `place_order_checked` passing the market PDA in the oracle slot. The discriminator check in `read_fresh_oracle` at [`lib.rs:1974`](../../programs/openhl-core/src/lib.rs#L1974) should fail with `UninitializedAccount`. Without this check, the code would `bytemuck::from_bytes` into garbage data and use a nonsensical `price`.
2. **Staleness is the security gate.** Set the oracle, wait 30+ slots, try to place an order at any price inside the band. It should fail with `oracle stale`. This is the most commonly forgotten check; it is also the one that has caused the most oracle exploits in the wild.
3. **The band's edge is exact.** With `SANITY_BAND_BPS = 2000` and `mark = 100`, an order at exactly 80 should be *accepted* (the check is `< low`, not `<= low`). An order at exactly 79 should be rejected. Confirm by running both. The edge case for a `<=` vs `<` slip is a single bp; for tighter bands at higher prices, the dollar value of the difference can be significant.

---

## Hook into Chapter 10

You now have a mark price. The next thing a perp DEX does with that mark is *funding rate*. Funding is the mechanism by which long and short positions periodically exchange payments to keep the perp's price tethered to spot — formally, `funding_rate ≈ k × (perp_premium_index - mark_price) / mark_price`, where `perp_premium_index` is some accumulator over recent fill prices and `mark_price` is what we just learned to read. The rate is paid every funding window (1 hour on most venues, 8 hours on some), and the program must accumulate per-position settlements continuously without an unbounded loop.

Chapter 10 walks the time-windowed accumulator pattern, the Clock sysvar's `unix_timestamp` field for funding deadlines, and the crank/keeper pattern for getting "every position pays funding at this slot" done without a single transaction touching all positions. This is where Phase A's parallelism lesson (Chapter 5) starts to dictate the data layout: funding settlement is the canonical case where a singleton "totals" account becomes a bottleneck, and we use the sharding pattern from §5.5 to avoid it.
