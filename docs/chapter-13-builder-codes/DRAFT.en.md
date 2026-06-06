# Chapter 13 — Builder Codes as a Protocol Primitive

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`BuilderProfile`), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_register_builder`, `process_place_order_with_builder`, `process_claim_builder_fees`, `process_create_fee_vault`), [`scripts/builder/src/main.rs`](../../scripts/builder/src/main.rs).

---

## §13.0  Framing — what builder codes are, and what they aren't

A **builder code** is a per-frontend identifier that gets attached to a trade. When a user opens an order through a frontend, the frontend includes its builder code in the transaction; the program then credits a configurable fraction of the trade's protocol fee to that builder's on-chain account.

Hyperliquid coined the term in its current sense. It is the protocol-native mechanism that lets a frontend monetize its order flow without having to operate the order book itself. Same family as "router fees" (Uniswap), "white-label routing" (CEXes), "introducing broker" (TradFi) — Solana's version of a long-standing distribution incentive.

Two things builder codes are **not**:

- **Referral codes.** Referral codes reward whoever introduced a new user; they typically pay once per signup or as a long-tail percentage of the referee's fees forever. Builder codes pay per *trade* and don't track who introduced whom — they reward routing, not introductions.
- **Maker/taker rebates.** Maker rebates pay the user (the limit order placer) part of their own fee back. Builder codes pay a *third party* (the frontend) a fraction of the user's fee. The user pays the same gross fee either way; what differs is who receives the split.

This chapter ships three trading instructions and one bootstrap:

1. **`RegisterBuilder`** — each builder creates a per-builder `BuilderProfile` PDA that holds their accumulated fees and their self-declared max share. Registration is required because the program needs an account to credit; no account, no fees.
2. **`PlaceOrderWithBuilder`** — the trading instruction variant that takes a builder profile as its last account. Computes the protocol fee, escrows it into the per-(quote_mint) fee vault, splits the builder's share into the profile's `accumulated_fees`, and runs the same place-order path as `PlaceOrderChecked`.
3. **`ClaimBuilderFees`** — the builder's withdraw call. Zeroes the accumulator and PDA-signs an SPL Token Transfer of the claimed amount from the fee vault into the builder's token account.
4. **`CreateFeeVault`** — one-shot bootstrap per quote mint, run once per cluster before any place-order variant. Allocates the per-(quote_mint) fee_vault token account owned by the `[b"fee_vault_auth", quote_mint]` PDA. Mirrors Chapter 6's `CreateVault`.

The chapter also extends `PlaceOrderChecked` to escrow the same protocol fee. The reason — design choice #2 in §13.5b — is symmetry: if only the builder path escrowed a fee, trades through a builder would cost the user more in real terms than identical trades without one. Production deployments resolve this by treating the protocol fee as universal across both variants from day one.

The chapter's intellectual content is three-part: the atomicity argument in §13.4 (why fee splits happen inside the trade instruction, not in a separate "claim per trade" call), the cap-stacking pattern in §13.2 (how a self-declared cap interacts with the protocol-level cap to bound fee leakage even if a builder is compromised), and the production-escrow design discussion in §13.5b (what makes builder fees structurally different from the two-party movements of Chapters 11 and 12 — and the four design choices behind our fee-vault implementation).

---

## §13.1  The `BuilderProfile` account

From `crates/state/src/lib.rs`:

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct BuilderProfile {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub builder: [u8; 32],          // pubkey of the builder
    pub max_fee_share_bps: u64,     // self-cap, clamped at register-time
    pub accumulated_fees: u64,      // claim target
    pub total_volume: u64,          // stat: base size routed through this builder
    pub _reserved: [u8; 32],
}
```

104 bytes. The four meaningful fields:

**`builder`** is the pubkey of the frontend / aggregator. Used as the only seed in the PDA derivation: `[BUILDER_PROFILE_SEED, builder.key]`. This means every Solana pubkey has at most one builder profile, derivable by anyone who knows the pubkey. The profile is owned by openhl-core (so only this program can mutate `accumulated_fees`), but anyone can *read* the public-facing fields (`builder`, `total_volume`) to verify a builder's claimed routing volume.

**`max_fee_share_bps`** is the builder's self-declared cap on what fraction of the protocol fee they will take. A builder that registers with `max_fee_share_bps = 2000` is publicly committing to take at most 20% of the protocol fee on any order routed through them. This is a credibility signal — a builder advertising "20% to us, 80% to protocol" vs. one advertising "50/50" tells users something about how the frontend monetizes. Lower self-cap → more user-friendly fee split → potentially more flow.

**`accumulated_fees`** is the running total of fees credited to the builder, awaiting claim. Incremented by every `PlaceOrderWithBuilder` that routes through this profile. Reset to zero by `ClaimBuilderFees`. The builder accumulates inside the program account and withdraws in batches — much cheaper than claiming once per trade.

**`total_volume`** is observability — base size routed through the builder. Used by builders to prove their flow to potential partners (or by users to vet a builder's track record). Not used by the program for any logic.

Two design constraints worth noting:

- **One profile per builder.** The PDA derivation makes the (builder pubkey) → (profile pubkey) mapping bijective. A builder cannot have two profiles with different fee splits per market; if they want to A/B test fee splits they use two different builder wallets.
- **Fee accumulation in quote units.** `accumulated_fees` is denominated in whatever quote currency the protocol fee is taken in. With multiple quote currencies (USDC and USDT, say) the design would need per-(builder, mint) profiles, not just per-builder. We deliberately keep it single-quote here since the rest of the program is also single-quote.

> **Exercise §13.1.** A frontend registers with `max_fee_share_bps = 3000`. Three trades route through them: notional 1000, 2500, 700. With `PROTOCOL_FEE_BPS = 10` (0.1%), what is the total `accumulated_fees` after all three trades?

---

## §13.2  Two caps, stacked

Builder codes need *two* fee-share caps, not one, because two parties have different incentives:

**Protocol cap (`PROTOCOL_BUILDER_SHARE_CAP_BPS = 5000`)** — the maximum fraction of any protocol fee that *any* builder may keep. Hard-coded into the program. With our value of 5000 bps, the protocol guarantees that at least 50% of every protocol fee stays with the protocol regardless of builder configuration.

**Builder self-cap (`BuilderProfile.max_fee_share_bps`)** — the maximum fraction this *particular* builder will keep. Self-declared at registration; user-visible.

The effective share on any trade is `min(builder.max_fee_share_bps, PROTOCOL_BUILDER_SHARE_CAP_BPS)`. From `programs/openhl-core/src/lib.rs:2742–2748`:

```rust
share_bps = profile.max_fee_share_bps;
if share_bps > PROTOCOL_BUILDER_SHARE_CAP_BPS {
    // Defensive: registered profile shouldn't exceed cap, but the
    // cap could have been lowered since the builder registered.
    share_bps = PROTOCOL_BUILDER_SHARE_CAP_BPS;
}
```

The protocol cap clamps at register-time (the user can't request more than the cap), but it also re-clamps at trade-time — because the cap could conceivably be lowered between registration and trade. (Our chapter ships a constant; a real program might make `PROTOCOL_BUILDER_SHARE_CAP_BPS` a governance-tunable value living on a config account, which is exactly where the defensive re-clamp matters.)

The two-cap structure is the core safety property. Three failure modes it handles:

1. **Malicious builder.** A builder who somehow declares `max_fee_share_bps = 10000` (100% of fee) at registration is clamped to `PROTOCOL_BUILDER_SHARE_CAP_BPS` immediately. The protocol always keeps its floor.
2. **User mistake.** A user who routes through an unfamiliar builder still knows the *maximum* possible fee leak before the trade — they can read both caps from on-chain state. No surprise fees.
3. **Compromised builder.** If a builder's wallet is compromised and the attacker tries to inflate their share, they can't go above the registered cap (immutable after register) without re-registering — which would create a new PDA at a different address, breaking the existing flow.

Production builder programs often add a *third* cap — a per-market or per-asset cap that lets the protocol charge different fee splits in different markets. Our chapter omits this for clarity; the pattern extends naturally.

> **Exercise §13.2.** A builder is registered with `max_fee_share_bps = 8000`. Run through the cap stacking: what's their effective share with our `PROTOCOL_BUILDER_SHARE_CAP_BPS = 5000`? What if a governance vote drops the protocol cap to 3000 *after* the builder registered — what happens on the next trade through their profile?

---

## §13.3  Walking `PlaceOrderWithBuilder`

`process_place_order_with_builder` in `programs/openhl-core/src/lib.rs`. The handler is a strict superset of `PlaceOrderChecked` — same eight prefix accounts (`user`, `book`, `oracle`, `market`, `mint`, `user_token`, `fee_vault_token`, `token_program`), one extra `builder_profile` account at the end, same fee math, same place logic, plus the builder credit step.

**Validation + sanity band**: identical to `PlaceOrderChecked`. Both handlers cross-check the (book, oracle, mint) chain against the passed market, run the oracle staleness gauntlet, and apply the price sanity band.

**Fee computation + builder credit**:

```rust
let notional_val = (price as u128) * (size as u128);
let protocol_fee = (notional_val * (PROTOCOL_FEE_BPS as u128) / 10_000) as u64;

// ... read builder profile, compute share_bps clamped to caps ...

let builder_share = ((protocol_fee as u128) * (share_bps as u128) / 10_000) as u64;
profile.accumulated_fees = profile.accumulated_fees.checked_add(builder_share)?;
new_volume = profile.total_volume.checked_add(size)?;
profile.total_volume = new_volume;
```

Three numbers fall out:

- `notional_val = price × size` — the trade's gross value in quote-unit-scaled form.
- `protocol_fee = notional × 10 / 10_000` — 0.1% of notional, in quote units.
- `builder_share = protocol_fee × share_bps / 10_000` — fraction of the protocol fee, in quote units.

`builder_share` is added to `profile.accumulated_fees` with `checked_add` (overflow refuses the trade rather than silently capping — the builder can claim periodically to keep the running total under u64::MAX). `total_volume` advances by the trade size for observability.

**Order placement**: identical to `PlaceOrderChecked` from §9.4. Linear-scan for an empty slot, write the order, increment counters.

**Fee escrow**: the handler ends with a CPI to SPL Token Transfer (`spl_token_transfer_user_signed`) moving the *full* `protocol_fee` from the user's quote-token account into the per-(quote_mint) fee vault. The `(protocol_fee - builder_share)` remainder stays in the fee vault as the protocol's take; `ClaimBuilderFees` later releases `accumulated_fees` from the same vault to each builder. Placed last so a Transfer failure (InsufficientFunds, frozen account) reverts both the order write and the builder credit atomically — same order-matters argument as Chapter 11's collateral escrow.

The CU cost of `PlaceOrderWithBuilder` is `PlaceOrderChecked + ~600 CU` for the builder profile borrow. The fee math and the fee-escrow Transfer are already in `PlaceOrderChecked`; the marginal cost over the no-builder path is just the builder credit.

> **Exercise §13.3.** What happens if `PlaceOrderWithBuilder` is called with `price × size` so large that `notional × PROTOCOL_FEE_BPS` overflows `u128`? Trace the failure path. Why is `u128` the right precision for this calculation rather than `u64`?

---

## §13.4  The atomicity argument

`PlaceOrderWithBuilder` does the fee split *inside the same instruction* as the order placement. There is no "after each trade, builder calls a separate `RecordFee(trade_id, amount)` instruction" pattern. The atomicity is load-bearing for three reasons:

**1. Settlement honesty.** If the fee split happened in a separate transaction, the user could pay the trade fee at slot N and the builder could fail to receive their share at slot N+1 (their account was closed, the cap changed, etc.). Atomicity means: either the trade commits with the split applied, or neither happens. No "I paid the fee but the builder didn't get credit" failure mode.

**2. CU efficiency.** A separate "record fee" instruction would cost another transaction's worth of fees, network round-trip, and CU overhead — for every single trade. With thousands of trades per day per builder, that adds up to material cost. Inline accumulation pays once per trade, claim pays once per N trades, total cost is amortized.

**3. Scheduling.** A separate fee-recording instruction would require the builder profile to be a writable account in *every* trade transaction even when split == 0. Sealevel would then serialize all trades on the same builder's profile (Chapter 5's antipattern). With the split inside the trade, *only trades routed through that builder* touch the profile — so two builders' flows can be processed in parallel even if both involve the same market.

The split between **accrual** (atomic with trade, inside `PlaceOrderWithBuilder`) and **claim** (batch, separate `ClaimBuilderFees` call) is also load-bearing. Claim is the expensive operation: it needs to move actual tokens (in production), which means an SPL Token CPI, which means signer setup and account validation. Doing that per-trade would be wasteful. By accumulating in the profile and letting builders claim periodically (hourly, daily, whatever), the per-trade cost stays minimal.

This accrue-batch-claim pattern is identical to ERC-20 dividend distributions in Ethereum-land — same problem, same solution, different runtime.

> **Exercise §13.4.** Sketch the alternative design where every trade emits a separate `RecordFee` instruction that the builder must process. Count the writes against the BuilderProfile account per second under load (say, 10 trades/sec routed through one builder). Compare to our design. Which one Sealevel-serializes more aggressively?

---

## §13.5  `RegisterBuilder`, `ClaimBuilderFees`, `CreateFeeVault`

`RegisterBuilder` is the standard PDA-creation pattern from Chapter 3, with one twist: the requested `max_fee_share_bps` is clamped at registration time:

```rust
if max_share > PROTOCOL_BUILDER_SHARE_CAP_BPS {
    msg!(
        "register_builder: requested {} bps clamped to protocol cap {} bps",
        max_share,
        PROTOCOL_BUILDER_SHARE_CAP_BPS
    );
    max_share = PROTOCOL_BUILDER_SHARE_CAP_BPS;
}
```

The clamp is silent — the registration succeeds, just with a reduced share. Logging the clamp lets builders verify their effective cap from program logs.

`ClaimBuilderFees` is two steps — zero the accumulator inside a borrow scope, then PDA-sign a Transfer out of the fee vault:

```rust
if profile.builder != *builder_ai.key.as_ref() {
    return Err(ProgramError::IllegalOwner);
}
let claimed = profile.accumulated_fees;
profile.accumulated_fees = 0;
// ... borrow scope ends ...

spl_token_transfer_fee_vault_signed(
    fee_vault_ai,
    builder_token_ai,
    fee_vault_auth_ai,
    mint_ai.key,
    fee_vault_auth_bump,
    token_ai,
    claimed,
)?;
```

Authorization (only the builder can claim their own fees), zero the accumulator, release the borrow, PDA-sign the Transfer. The Transfer helper is structurally identical to §12.4's `spl_token_transfer_vault_signed` — same `invoke_signed` pattern, same `InvalidSeeds` protection — with the per-(quote_mint) `[FEE_VAULT_AUTH_SEED, quote_mint]` seeds instead of per-(market) `[VAULT_AUTH_SEED, market]`. Order matters: zero the accumulator *before* the Transfer so a transfer-failure tx revert restores the accumulator (atomic), and so a successful Transfer cannot be replayed against a still-funded accumulator.

A real production claim also typically supports partial withdrawals (`claim --amount N` rather than always-everything), accumulator timeouts (fees idle for >N days are forfeited to the protocol), and per-token claims when the protocol supports multiple quote currencies. None of these change the fundamental shape; they're policy decisions on top.

`CreateFeeVault` is the one-shot bootstrap. Mirrors `process_create_vault` from Chapter 6 verbatim, with two differences: the seeds are `[b"fee_vault", quote_mint]` / `[b"fee_vault_auth", quote_mint]` instead of `[b"vault", market, mint]` / `[b"vault_auth", market]`, and only one is needed per quote currency (not per market). The handler does the standard two-CPI dance — System `create_account` with PDA signing, then SPL Token `InitializeAccount3` setting the authority PDA as the owner — and is run once before any place-order variant.

> **Exercise §13.5.** Modify `process_claim_builder_fees` to accept a `partial_amount: Option<u64>` in the payload. If `Some(n)`, claim `min(n, accumulated_fees)`; if `None`, claim all. Why is the partial-withdraw pattern useful for builders even though the total they can withdraw is the same?

### §13.5b  Production-escrow design notes — the four choices behind the implementation

Chapters 11 and 12 added real SPL Token escrow to position collateral and vault deposits with a single matching helper apiece. This chapter's escrow needed four design choices instead, because builder fees are a *structurally different* escrow problem than the two-party token movements those chapters needed. The implementation in `process_place_order_checked`, `process_place_order_with_builder`, and `process_claim_builder_fees` is shaped by all four; understanding them is what makes the code read as a sequence of forced moves rather than arbitrary choices.

**1. Two parties vs three.** Position collateral and vault deposits are two-party movements: user ↔ vault, signed by one side, with the entire economic decision (how much, signed by whom) encoded in the instruction. Builder fees are a *three-party split*: the user pays a single protocol fee, of which one fraction goes to the protocol and another to the builder. Splitting one user payment across two recipients atomically — and crediting the right fraction to each — does not fit `spl_token_transfer_user_signed` and `spl_token_transfer_vault_signed` alone. The implementation introduces a *fee-vault* token account (one per quote mint) owned by a PDA at `[b"fee_vault_auth", quote_mint]`. Every place-order variant (a) Transfers the *full* `protocol_fee` from the user's token account into the fee vault, then (b) credits `builder_share` to the builder's accumulator (in the builder variant). The `(protocol_fee - builder_share)` remainder stays in the fee vault as the protocol's take.

**2. The `PlaceOrderChecked` asymmetry.** Adding fee escrow only to `PlaceOrderWithBuilder` would create a perverse incentive: the no-builder path would charge no token-denominated fee, so trades through a builder would cost the user more in real terms than identical trades without one. The chapter resolves this by making *all* place-order variants escrow the protocol fee — which is why `process_place_order_checked` grew the same `(market, mint, user_token, fee_vault_token, token_program)` prefix that `PlaceOrderWithBuilder` carries. Both variants now escrow the same fee; the builder variant additionally credits the builder share. Production deployments make this choice from day one rather than as a follow-up because the asymmetric design is a one-way migration trap — once users have signed transactions that assume an account shape, you can't quietly add slots later.

**3. Multi-quote support.** With one quote currency (our case) the fee vault is a single account per cluster, and the per-builder accumulator is a single u64. With multiple quote currencies the design needs per-(quote_mint) fee vaults *and* per-(builder, quote_mint) accumulators — meaning `BuilderProfile` either grows a map field (not Pod-friendly) or splits into many one-per-quote profile PDAs. §13.1 mentioned the single-quote constraint as deliberate; here is where it pays off architecturally. Note that the PDA seeds we picked (`[b"fee_vault", quote_mint]` rather than `[b"fee_vault"]`) already support per-mint vaults; only the per-(builder, quote_mint) accumulator side would need restructuring to go multi-quote.

**4. Claim-side authority.** With the fee vault in place, `ClaimBuilderFees` is a PDA-signed Transfer from `fee_vault` to `builder_token`, signed by the fee-vault authority PDA — structurally identical to §12.4's `VaultWithdraw`, same `invoke_signed` pattern, same `InvalidSeeds` protection, different seeds. This is the piece that was always going to be a "mechanical extension" of work the prior chapters already did; it lands as the new `spl_token_transfer_fee_vault_signed` helper next to its per-(market) cousin.

The mechanism the chapter teaches — accrual atomic with trade, claim as a separate batch operation, two-cap safety — survives all four design choices unchanged. Choices 1 and 2 dictate where Transfers land; choice 4 dictates how the claim signs; choice 3 is what we get for free by keeping the seeds mint-scoped.

> **Exercise §13.5b (design).** Compare the `accounts: vec![...]` declarations in `scripts/builder/src/main.rs` for `--place-with-builder`, `--claim`, and the original Chapter 12 `--withdraw` (in `scripts/vault/src/main.rs`). Identify three structural similarities and three structural differences. Trace each difference back to one of the four design choices above. Hint: the seed-derivation pattern is identical; the authority side is identical; the asymmetries are in account count, signer role, and PDA-derivation seeds.

---

## §13.6  Recap + verify yourself

### Recap diagram

```
Builder lifecycle:

  0) Cluster bootstrap (once per quote mint)
     CreateFeeVault(quote_mint)
     ──► fee_vault token account at [b"fee_vault", quote_mint]
         owned by [b"fee_vault_auth", quote_mint] PDA


  1) Builder registers
     RegisterBuilder(max_fee_share_bps=2000)
     ──► BuilderProfile{ builder, max_share=2000, fees=0, vol=0 }


  2) User trades through builder
     PlaceOrderWithBuilder(side, price, size)
     accounts: [user(S,W), book(W), oracle(R), market(R), mint(R),
                user_token(W), fee_vault(W), token_program(R),
                builder_profile(W)]

       notional       = price × size
       protocol_fee   = notional × 10 / 10000    (PROTOCOL_FEE_BPS)
       share_bps      = min(profile.max, 5000)    (PROTOCOL cap)
       builder_share  = protocol_fee × share_bps / 10000

     atomic:
       order placed in book                       ─┐
       profile.accumulated_fees += builder_share   ├─ same tx, same slot
       profile.total_volume     += size            │
       SPL Token Transfer:                         │
         user_token → fee_vault   protocol_fee     ─┘


  3) Builder claims periodically
     ClaimBuilderFees(empty)
     accounts: [builder(S), profile(W), mint(R), fee_vault(W),
                fee_vault_auth(R), builder_token(W), token_program(R)]
     ──► profile.accumulated_fees = 0
     ──► invoke_signed SPL Token Transfer:
           fee_vault → builder_token  (claimed amount)
           signer seeds: [b"fee_vault_auth", quote_mint, &[bump]]


Two-cap safety:

  effective_share = min( builder.max_fee_share_bps
                       , PROTOCOL_BUILDER_SHARE_CAP_BPS
                       )

  ↑ builder commits to maximum self-cap (user-visible)
  ↑ protocol commits to maximum cap-of-caps (hard-coded / governance)

  Floor on protocol take = (10000 - 5000) × PROTOCOL_FEE_BPS / 10000
                         = 5 bps of notional, always retained
```

### Three things to verify yourself

Prerequisites: bootstrap the fee vault for the quote mint (`builder --create-fee-vault --mint <quote_mint>`) and ensure your trader has a funded quote-token account before any place-order variant; otherwise the fee Transfer reverts.

1. **The cap stacks correctly.** Register a builder with `--max-share-bps 9999`. The handler should log the clamp, and the dump should show `max_fee_share_bps = 5000` (our `PROTOCOL_BUILDER_SHARE_CAP_BPS`). Now route a trade through them — the builder's share should be exactly 50% of the protocol fee.
2. **Atomicity holds under failure.** Use a simulated failure: try `PlaceOrderWithBuilder` with a stale oracle (so the order placement will fail). The simulation should fail with `oracle stale` *and* the builder profile's `accumulated_fees` should be unchanged, the fee vault's balance should be unchanged, and the trader's quote-token account should be unchanged after the failed sim (the entire transaction reverts). The split doesn't happen unless the trade does.
3. **Volume accrues per trade.** Place 5 orders through the same builder with sizes 10, 20, 30, 40, 50. The `total_volume` should be exactly 150 (10+20+30+40+50). If `accumulated_fees` doesn't add up to `(notional_total × PROTOCOL_FEE_BPS × share_bps / 10000 / 10000)`, there's an off-by-one in the math — chase it down. Then run `--claim --builder-token-account <pubkey>`; the builder's quote-token account balance should grow by exactly `accumulated_fees`, and the dump should show `accumulated_fees = 0`.

---

## Hook into Chapter 14

You now have a perp DEX program that handles every primitive a production deployment needs: accounts, programs, PDAs, CPI, compute, parallelism, vaults, an order book, a matcher, an oracle reader, funding, positions, liquidations, pooled trading vaults, and builder codes. What you do *not* have is the off-chain plumbing that keeps it running.

Chapter 14 — Cranks, Keepers, and Off-Chain Glue — closes Phase B and the track. We will walk through every keeper this program implicitly requires: the funding-rate keeper (Chapter 10's hook), the liquidator bots (Chapter 11's permissionless `Liquidate`), the vault NAV reporter (Chapter 12's `UpdateNAV` cadence), the builder claim cron (Chapter 13's accumulator), the oracle publisher (Chapter 9's `SetOraclePrice` if we ran it ourselves rather than using Pyth), the matching-engine cranker (if we'd built async matching), and the off-chain indexer that feeds frontends. The chapter has zero new on-chain code — its content is design patterns for off-chain processes, fee economics, redundancy and failover, and the architectural reality that "Solana DEX" is half on-chain program and half coordinated off-chain infrastructure.
