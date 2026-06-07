# Chapter 12 — Native Vault Program (Pooled Trading)

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`TradingVault`, `VaultShare`), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_create_trading_vault` 3402–3480, `process_vault_deposit` 3494–3655, `process_vault_withdraw` 3669–3786, `process_vault_update_nav` 3793–3833), [`scripts/vault/src/main.rs`](../../scripts/vault/src/main.rs).

---

## §12.0  Framing — and a naming clarification

The word "vault" appears twice in this codebase:

- **Chapter 6's `Vault`** was an SPL Token Account — a single-user custody account where one user's collateral lived. Owner: the SPL Token program. Pure plumbing.
- **Chapter 12's `TradingVault`** is something completely different — a pooled fund where many users deposit assets and share the manager's PnL pro-rata via shares. Owner: openhl-core. Has shares, NAV, deposit/withdraw economics.

Both are reasonable uses of the word "vault." We picked the type names to disambiguate (`TradingVault` is unambiguous; `Vault` from Chapter 6 has no dedicated state struct — it's just an SPL Token Account). The instructions don't collide because of the `CreateVault` (token account) vs `CreateTradingVault` (this chapter) split.

The trading vault is the conceptual primitive that turns a perp DEX from a venue where users trade directly into a venue that also hosts *funds*. A user who doesn't want to manage positions themselves can deposit into a vault; the vault's manager runs the strategy; depositors share returns. This is the structure behind every yield vault on Solana — Drift's spot vaults, Kamino's leveraged vaults, Jupiter's perps vault, and so on.

This chapter builds the vault's accounting half — shares, deposits, withdrawals, NAV updates — *with the real SPL Token escrow wired in*. Deposit moves actual quote tokens from the depositor's account into the per-(market, mint) vault PDA (the same vault Chapter 6 created and Chapter 11 uses for position collateral). Withdraw moves them back, signed by the vault-authority PDA via `invoke_signed`. The bookkeeping (`total_shares`, `total_assets`, `VaultShare.shares`) and the actual token balance stay in sync because each handler updates both atomically.

It does not build the manager's trading half (which would be a thin wrapper instruction calling into Chapter 11's `OpenPosition` with the vault's PDA as the position owner). Adding that is mechanical once the share accounting and the escrow are in place; the chapter explains the design and leaves implementation as a homework piece.

What this chapter is actually about:

1. **The share/asset math** — how deposits and withdrawals preserve the pro-rata invariant when NAV changes. Independent of escrow; the math would be identical whether tokens moved or not.
2. **Atomic bookkeeping ↔ escrow updates** — every handler that touches `total_shares` / `total_assets` also runs the matching SPL Token CPI in the same transaction. Either both succeed or both revert. The two states cannot drift.
3. **The singleton-write reassertion** — every deposit and withdrawal mutates the same `TradingVault` account, so vault operations serialize at the scheduler. Chapter 5's antipattern shows up because we *deliberately* introduced it; we now see what mitigations look like in practice.
4. **The manager-trust model** — `VaultUpdateNAV` is the load-bearing trust assumption. How that's structured determines whether the vault is "trust the manager not to lie" or "verify NAV against on-chain state."

---

## §12.1  The two account types

From `crates/state/src/lib.rs`. Two new structs, both Pod, repr(C).

**`TradingVault`** (160 bytes): one per (market, manager) pair.

```rust
pub struct TradingVault {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub manager: [u8; 32],   // the trader; signs UpdateNAV
    pub mint: [u8; 32],      // asset mint (e.g., USDC)
    pub total_shares: u64,
    pub total_assets: u64,   // NAV — manager-reported
    pub _reserved: [u8; 32],
}
```

The two load-bearing fields are `total_shares` and `total_assets`. Their ratio is the NAV per share. They are updated *together* on every deposit and withdrawal (proportionally, preserving the per-share value) and updated *independently* by NAV updates from the manager.

`market` and `mint` are denormalized — the (market, manager) pair already implies them via the PDA's seed scheme, but storing them in-account lets readers identify the vault without re-deriving from external context. `manager` is what `VaultUpdateNAV` checks the signer against.

**`VaultShare`** (128 bytes): one per (vault, depositor) pair.

```rust
pub struct VaultShare {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub vault: [u8; 32],
    pub owner: [u8; 32],
    pub shares: u64,
    pub cost_basis: u64,     // cumulative assets deposited (for reporting)
    pub _reserved: [u8; 32],
}
```

`shares` is the depositor's share count. `cost_basis` is the cumulative quote-asset amount they've deposited — used for P&L reporting (gain = `withdraw_assets - cost_basis_share`), not for any in-program logic. The depositor signs `VaultWithdraw` to prove they own these shares.

The PDA derivations:

- TradingVault: `[b"trading_vault", market.key, manager.key]`
- VaultShare: `[b"vault_share", vault.key, owner.key]`

> **Exercise §12.1.** A user holds 200 shares of a vault with `total_shares = 1000` and `total_assets = 1500`. What fraction of the vault do they own, and what is the per-share NAV? If the manager runs a successful trade that lifts `total_assets` to 1800 (without changing `total_shares`), what is the new per-share NAV?

---

## §12.2  The share/asset math

Three operations, one invariant.

**Invariant.** For any depositor, the value they could withdraw at any moment is:

```
their_value = their_shares × total_assets / total_shares
```

A deposit must preserve this invariant for *all existing depositors*: their pre-deposit value equals their post-deposit value. A withdrawal does the same: the remaining depositors' value is unchanged. NAV updates change everyone's value by the same proportion.

**Deposit.** From `process_vault_deposit` at `programs/openhl-core/src/lib.rs:3564–3572`:

```rust
let shares_to_mint: u64 = if vault.total_shares == 0 || vault.total_assets == 0 {
    assets
} else {
    let numer = (assets as u128) * (vault.total_shares as u128);
    let s = numer / (vault.total_assets as u128);
    if s > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    }
    s as u64
};
```

Two branches. The first deposit (when `total_shares == 0`) mints shares 1:1 with assets — there's no NAV history to interpolate from. Subsequent deposits use:

```
shares_minted = assets_in × total_shares / total_assets
```

Algebraically: this is the value of `assets_in` *expressed in shares*, at the current per-share NAV. After the deposit:

```
new_total_shares = total_shares + shares_minted
new_total_assets = total_assets + assets_in
new_NAV_per_share = new_total_assets / new_total_shares
                  = (total_assets + assets_in)
                    / (total_shares + assets_in × total_shares / total_assets)
                  = total_assets × (total_assets + assets_in)
                    / (total_assets × total_shares + assets_in × total_shares)
                  = total_assets / total_shares
                  = old_NAV_per_share
```

The NAV per share is unchanged. The invariant holds.

**Withdrawal.** From `process_vault_withdraw` at `lib.rs:3729–3736`:

```rust
let assets_to_return: u64 = {
    let numer = (shares_to_burn as u128) * (vault.total_assets as u128);
    let a = numer / (vault.total_shares as u128);
    if a > u64::MAX as u128 {
        return Err(ProgramError::ArithmeticOverflow);
    }
    a as u64
};
```

Same math, run backward:

```
assets_returned = shares_burned × total_assets / total_shares
```

After the withdrawal:

```
new_total_shares = total_shares - shares_burned
new_total_assets = total_assets - assets_returned
new_NAV_per_share = (total_assets - shares_burned × total_assets / total_shares)
                  / (total_shares - shares_burned)
                  = ... (same algebra in reverse) ...
                  = total_assets / total_shares
                  = old_NAV_per_share
```

Withdrawal also preserves the invariant.

**NAV update.** From `process_vault_update_nav` at `lib.rs:3823–3824`:

```rust
let prev = vault.total_assets;
vault.total_assets = new_total_assets;
```

The manager writes a new `total_assets`. `total_shares` is unchanged. So per-share NAV moves from `prev / total_shares` to `new_total_assets / total_shares`. Every depositor's value moves by the same factor. This is how PnL is shared.

These three pieces — deposit, withdraw, NAV update — are the entire vault accounting model. Everything else (gates, fees, vesting, restrictions) is policy on top.

**Integer division and dust.** The `/` operations are integer division. A user depositing 7 assets into a vault with 1000 total assets and 1000 total shares would mint `7 × 1000 / 1000 = 7` shares — clean. A user depositing 7 assets into a vault with 1000 total assets and 999 total shares would mint `7 × 999 / 1000 = 6` shares (`6993 / 1000`, integer-truncated). That extra 1/1000 of a share is dust — the user paid 7 assets but received the equivalent of 6.993 shares' worth of NAV value at deposit. The dust effectively donates that 0.007 shares' worth to the remaining depositors (their per-share value ticks up slightly).

This is generally acceptable for vaults because (1) the dust is rounding-error scale, (2) it favors existing depositors over new ones, which is the conservative direction. For programs that need exact preservation (e.g., yield-bearing tokens used as collateral elsewhere), additional precision via scaled u128 share representations or fixed-point arithmetic is required. We deliberately don't add that here — it would multiply the code without adding pedagogical value.

> **Exercise §12.2.** Start with an empty vault. Deposit 100 assets (depositor A). Set NAV to 200 (price doubled). Have depositor B deposit 100 assets. How many shares does B receive? What fraction of the vault does B now own?

---

## §12.3  Walking `VaultDeposit`

`process_vault_deposit` is the most complex of the four handlers because it conditionally creates the VaultShare PDA on first deposit *and* runs an SPL Token Transfer CPI at the end. Five phases:

**Validation**: payload size, non-zero deposit, depositor is signer, vault has correct owner + size, system program is correct, **token program is SPL Token, depositor's token account is SPL Token-owned, and the vault_token_account matches the derived PDA at `[VAULT_SEED, market, mint]`**. The mint and market accounts passed in must match what's stored in the vault — caught by a `vault.mint != mint_ai.key` / `vault.market != market_ai.key` check inside the borrow scope below. Share PDA derivation is unchanged from before.

**Read vault state and compute shares to mint**: borrow vault data, cross-check the passed market/mint against vault.market/vault.mint, branch on first-deposit (1:1) vs subsequent (pro-rata).

**Update vault aggregate** (lines 3580–3587):

```rust
vault.total_shares = vault.total_shares.checked_add(shares_to_mint)?;
vault.total_assets = vault.total_assets.checked_add(assets)?;
drop(vault_data);
```

`checked_add` (not `saturating_add`): if the addition would overflow, refuse the deposit rather than silently capping. A vault that accepts deposits past `u64::MAX` shares has a different problem to solve. The explicit `drop(vault_data)` releases the mutable borrow before we touch the share account — necessary because share creation may CPI back through the vault account check path.

**Conditional create-or-update of the share account** (lines 3591–3636):

```rust
let share_exists = share_ai.owner == program_id && share_ai.data_len() == VaultShare::LEN;
if !share_exists {
    let rent = Rent::get()?.minimum_balance(VaultShare::LEN);
    let create_ix = system_instruction::create_account(...);
    invoke_signed(...)?;
    // ... write VaultShare fields ...
} else {
    // ... add to existing shares + cost_basis ...
}
```

The "exists?" check is by owner + data_len — if the account is owned by us and the right size, we assume it's a VaultShare we'd previously created. (The discriminator check happens inside the `else` branch when we cast the data.) If it's not ours, we create it via the standard `invoke_signed` + `create_account` pattern.

A user's first deposit pays the rent for their VaultShare account (small one-time cost). Subsequent deposits just increment fields. This is the conventional pattern — the alternative would be requiring the user to call a separate `CreateVaultShare` instruction first, which adds friction without benefit.

The handler must own the share account at the end regardless of which branch ran. In both branches the final state has `share.shares` reflecting the depositor's total holdings and `share.cost_basis` reflecting their cumulative deposits.

**Escrow the depositor's tokens** as the final step:

```rust
spl_token_transfer_user_signed(
    depositor_token_ai,
    vault_token_ai,
    depositor_ai,
    token_ai,
    assets,
)?;
```

Same helper from Chapter 11. The depositor signs the outer transaction; the signature flows through to SPL Token via the standard signer-privilege-extension pattern (Chapter 6 §6.2). `assets` units move from `depositor_token` into `vault_token` (the per-(market, mint) vault PDA — same vault account that holds position collateral from Chapter 11).

**Order matters.** The CPI is placed *after* the share+aggregate updates. If the transfer fails (`InsufficientFunds`, account frozen, etc.), Solana reverts the entire transaction — including the share creation, the share field updates, and the vault aggregate increment. So a failed escrow leaves the depositor with no shares minted and no vault state change. The atomicity guarantee is what makes the natural error handling correct without explicit rollback code.

> **Exercise §12.3.** A user deposits 100, then 50, then 25 in three separate transactions. The vault's NAV is constant (no UpdateNAV in between). At each step, dump the user's share account *and* the vault token account balance. The shares count should grow linearly (100 / 150 / 175), the cost_basis should be the running sum, and the vault token balance should equal the cost_basis exactly.

---

## §12.4  Walking `VaultWithdraw`

Simpler than deposit because there's nothing to create — but it does pay out tokens from the vault, which means an `invoke_signed` CPI signed by the vault-authority PDA. `process_vault_withdraw`:

**Compute assets to return** at lines 3729–3736 — the inverse of the deposit formula, as covered in §12.2.

**Authorization** at lines 3744–3747:

```rust
if share.owner != *owner_ai.key.as_ref() {
    msg!("vault_withdraw: caller is not the share owner");
    return Err(ProgramError::IllegalOwner);
}
```

Only the share's recorded owner may burn it. This is a *per-share* authorization, not vault-wide — different from the manager check in UpdateNAV. There is no "vault admin can liquidate any share" path in this design (which a real production vault might add for compliance reasons).

**Sufficient-balance check** at lines 3748–3755:

```rust
if share.shares < shares_to_burn {
    return Err(ProgramError::InsufficientFunds);
}
```

You can't burn more shares than you hold.

**Cost basis reduction** at lines 3760–3762:

```rust
let basis_reduction = (((shares_to_burn as u128) * (share.cost_basis as u128))
    / (share.shares as u128 + shares_to_burn as u128)) as u64;
share.cost_basis = share.cost_basis.saturating_sub(basis_reduction);
```

Proportional reduction. If a user has 100 shares with cost_basis 1000 and burns 25 shares, the cost_basis reduces by `25 × 1000 / 100 = 250`, leaving the remaining 75 shares with cost_basis 750. This keeps the per-share cost_basis flat across partial withdrawals, which is what P&L reports want.

The `as u128 + shares_to_burn as u128` in the denominator uses `share.shares` *before* the subtraction (because we haven't subtracted yet). A naive `share.shares as u128` after a `share.shares -= shares_to_burn` would compute the wrong basis.

**Update aggregates** — `total_shares -= shares_to_burn`, `total_assets -= assets_to_return`. Both `saturating_sub` defensively, though both should never underflow given the prior checks.

**Pay the assets out from the vault** as the final step — this is the new escrow CPI:

```rust
spl_token_transfer_vault_signed(
    vault_token_ai,
    owner_token_ai,
    vault_authority_ai,
    market_ai.key,
    vault_auth_bump,
    token_ai,
    assets_to_return,
)?;
```

The mechanics differ from the deposit side in one critical way: **the authority is a PDA, not the user.** The depositor's signature on the outer transaction is meaningless to SPL Token here, because the token account that's being debited (`vault_token`) is owned by `vault_authority` — a PDA the *program* controls. So the program signs the inner CPI on the vault_authority's behalf via `invoke_signed`:

```rust
let market_key_bytes = market_key.to_bytes();
let signer_seeds: &[&[u8]] = &[VAULT_AUTH_SEED, market_key_bytes.as_ref(), &[vault_auth_bump]];
invoke_signed(&ix, &[source, dest, vault_authority, token_program], &[signer_seeds])?;
```

The seeds — `[VAULT_AUTH_SEED, market, bump]` — exactly match what `create_market` derived in Chapter 6 §6.2 and what `process_create_market` recorded as `vault_auth_bump` in the Market account. The bump is passed from the caller (loaded from the market by the handler) rather than recomputed, both for compute units and to make the signing seeds match the recorded derivation exactly.

If the seeds don't form a valid PDA whose address equals `vault_authority_ai.key`, the runtime rejects the signed CPI with `ProgramError::InvalidSeeds`. So a malicious caller can't pass a fake vault_authority — only the one and only PDA that the program can sign for is accepted.

**Order matters here too.** The CPI is last. If the token transfer fails (vault_token frozen, dest closed, etc.), the share-burn and aggregate-decrement revert with it. So a failed payout leaves the depositor with their shares intact and the vault unchanged.

> **Exercise §12.4.** With the vault from §12.2's exercise, have depositor A withdraw all their shares. Dump three things before and after: (1) depositor A's share account, (2) the vault aggregate state, and (3) the vault token account balance. Verify that the tokens A receives match what depositor B's claimable value implies for the remaining shares (i.e. that B's value is unchanged).

---

## §12.5  The manager-trust problem — `VaultUpdateNAV`

`process_vault_update_nav` at lines 3793–3833 is short, but it is where the entire vault model's trust assumption lives:

```rust
if vault.manager != *manager_ai.key.as_ref() {
    msg!("vault_update_nav: caller is not the vault manager");
    return Err(ProgramError::IllegalOwner);
}

let prev = vault.total_assets;
vault.total_assets = new_total_assets;
```

The manager signs a transaction that updates `total_assets` to whatever number they choose. There is no on-chain verification that this number reflects the manager's actual trading PnL. **The depositors trust the manager.**

Three patterns to harden this in production:

**(1) Compute NAV on-chain from referenced state.** Instead of accepting `new_total_assets` as a payload, the handler reads the vault's open Position accounts, sums their equity (using the same `compute_equity` from Chapter 11), and writes the result. Now the manager can't lie — `total_assets` is mechanically derived. Cost: a lot more accounts referenced per UpdateNAV call (one per position), pushing into CU and account-list limits.

**(2) Allow withdrawals at oracle prices, not stated NAV.** Withdrawals compute the assets they're owed based on a transparent on-chain rule (e.g., NAV-by-formula, not NAV-by-manager-report). Manager NAV reports become advisory metadata, not the basis for redemption.

**(3) Two-step NAV updates with delay.** Manager proposes a new NAV; the change applies after some delay (e.g., 1 hour); during that delay, depositors who think the manager is reporting falsely can withdraw at the *old* NAV. This is the trust-but-verify pattern used by some Curve/Yearn vaults.

Our chapter ships pattern (0) — no verification, manager is trusted. This is fine for educational and small-deployment vaults but is the right place to start a security audit when productizing.

The reason pattern (1) is so attractive in theory and so rarely implemented in practice: summing position equity across N positions requires loading N accounts, and N can be hundreds for a real vault. The transaction limit of ~64 accounts and the CU budget put hard limits on how many positions can be aggregated in one transaction. Production vaults either restrict themselves to a small number of concurrent positions or batch NAV updates across multiple transactions.

> **Exercise §12.5.** Walk through what would happen if the manager set `total_assets` to `u64::MAX` (a malicious update). What's the immediate effect on existing depositors? On new depositors? What's the eventual outcome when somebody tries to withdraw?

---

## §12.6  Singleton-write reassertion — the Chapter 5 antipattern, redux

`TradingVault` is the canonical singleton-write-shared account this codebase has had since Chapter 5's `Stats` warning. Every deposit, every withdrawal, every NAV update writes the same `(total_shares, total_assets)` pair. Two concurrent deposits from different users cannot run in parallel — they both write to the vault aggregate, and Sealevel serializes them.

How bad is this? With a 1-second deposit latency, the vault accepts one deposit per slot, ~2.5 deposits/sec maximum. For a vault with thousands of depositors moving capital between strategies, this is the binding constraint on user experience.

Three mitigations, all real, all used in production by different vaults:

**(1) Off-chain deposit queue.** Deposits are written off-chain to a queue (Redis, a database). A periodic on-chain "batch settle" instruction processes N deposits in one transaction, paying the singleton-write cost once for many users. Tradeoff: deposits aren't atomic anymore — users see "pending" status, then "confirmed" minutes later. Most institutional vaults work this way; it's the "you wait, but at 4 PM ET you're in the strategy" pattern.

**(2) Shard the vault.** Have N independent `TradingVault` accounts, each with its own (total_shares, total_assets). Deposits route to a shard based on the depositor's pubkey hash. Reads aggregate across all shards. This breaks the singleton — N shards mean N-way parallel deposits. Tradeoff: NAV updates now require N transactions, and rebalances across shards become a thing. Real-world example: large Curve/Yearn vaults sometimes shard for exactly this reason.

**(3) Per-deposit accumulator.** Instead of updating the singleton on every deposit, individual deposit "tickets" are written to per-user accounts, and a periodic "checkpoint" call rolls them into the singleton. Looks like option 1 but stays on-chain — the queue is the set of unsettled ticket accounts. Tradeoff: settlement complexity, slight delay between deposit and share issuance.

Our chapter ships option (0) — vanilla synchronous deposits. This is fine for low-throughput vaults (< 10 deposits/sec) and pedagogically clearest. The production path through (1) or (3) is well-trodden and not in scope for this chapter; the framing is "you can see why you'd want it now."

The deeper lesson, restating Chapter 5's: **every singleton write is a future scaling bottleneck.** When you find yourself reaching for a "totals" or "aggregate" account, ask whether you can express the same semantics without one. Sometimes the answer is yes (per-position settlement from Chapter 10 — no aggregate needed); sometimes the answer is no and you need the mitigation patterns above. The point is to make the trade consciously rather than discover it under load.

---

## §12.7  Recap + verify yourself

### Recap diagram

```
Vault lifecycle:

  manager ──► CreateTradingVault ──► TradingVault{shares=0, assets=0}
                                            │
   user A deposits 100 ──► VaultDeposit ──► │
                                            ▼
                          TradingVault{shares=100, assets=100} (1:1 first)
                          VaultShare_A{shares=100, basis=100}
                                            │
   manager trades ──► UpdateNAV(200) ────►  │
                                            ▼
                          TradingVault{shares=100, assets=200} (NAV ×2)
                                            │
   user B deposits 100 ──► VaultDeposit ──► │  shares = 100 × 100 / 200 = 50
                                            ▼
                          TradingVault{shares=150, assets=300}
                          VaultShare_B{shares=50, basis=100}

   A withdraws all ──► VaultWithdraw(100) ──► assets_out = 100 × 300 / 150 = 200
                                            ▼
                          TradingVault{shares=50, assets=100}
                          VaultShare_A{shares=0, basis=0}
                          VaultShare_B{shares=50, basis=100} (unchanged)


Singleton-write reassertion (Ch.5 redux):

  All four instructions WRITE the TradingVault account:
    deposit / withdraw / update_nav / create

  ⇒ Sealevel serializes every vault operation against every other.
  ⇒ Throughput cap: ~1 op per slot per vault.

  Mitigations (not implemented here):
    - off-chain deposit queue → batch settle
    - shard the vault across N independent aggregates
    - per-deposit accumulator + periodic checkpoint
```

### Three things to verify yourself

1. **NAV preservation across deposits.** Start with empty vault. Have A deposit 100. Have B deposit 100 immediately (no UpdateNAV between). The vault should have `total_shares = 200, total_assets = 200`, and A's and B's per-share NAV must both equal 1.0. Now have C deposit 100. Same per-share NAV: 1.0.
2. **NAV update changes everyone's value uniformly.** From the above state, run `vault --update-nav --total-assets 600` (3× value). A's `claimable_assets = A.shares × total_assets / total_shares = 100 × 600 / 300 = 200`. B and C the same. All three depositors share the 3× gain proportionally.
3. **Withdrawal at non-1:1 NAV.** From the above (each depositor has 100 shares worth 200 assets each), have A withdraw all 100 shares. They receive 200 assets. The vault now has `total_shares = 200, total_assets = 400`. B and C each still own 100 shares (50% of vault), worth 200 assets each — unchanged by A's exit.

---

## Hook into Chapter 13

You now have a vault that can pool depositor capital and distribute PnL pro-rata. What you don't have is **a mechanism for the vault to actually trade**. The manager's NAV updates in §12.5 are claims, not verified actions — there is no instruction that says "vault manager opens a position using the vault's assets." Adding that is the natural next step in the Phase B integration arc: a manager-signed `VaultOpenPosition` that creates a Position owned by the vault PDA (using `invoke_signed` with the vault's seeds), drawing collateral from the vault's tracked assets.

Chapter 13 builds builder codes — the protocol-native referral / fee-share mechanism that lets a trading frontend collect a slice of fees from users routed through it. Builder codes touch every fee-bearing instruction in the program (place_order, deposits in the production-escrow path, liquidations) and add a fee_recipient account to each transaction's AccountMeta. The chapter explores how the fee split happens atomically with the underlying action (no separate "claim fees" call required) and how the builder-code structure encodes the distribution incentives that make Solana DEX frontends viable as standalone businesses.
