# Chapter 6 — CPI Internals via Vault Deposits

> Status: draft (v0.1).
> Companion code: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_create_vault` at lines 615–728, `process_deposit` at lines 731–800), [`scripts/create-vault/src/main.rs`](../../scripts/create-vault/src/main.rs), [`scripts/deposit/src/main.rs`](../../scripts/deposit/src/main.rs).
> Tested against: solana-cpi 2.2.1, solana-program 2.3.0, SPL Token program (TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA).

---

## Phase B prologue — the architectural choice this curriculum makes

A perp DEX can be built two ways. **As programs on top of an existing chain** — you write the matching, vault, funding, and liquidation logic as smart contracts; the chain handles consensus, execution, validators, networking, and wallets. This is what Phase B builds. **As a custom L1** — you bundle the same business logic with your own consensus engine, execution layer, peer network, and RPC, and ship a binary that other validators run.

The two paths solve the same business problem (match orders, hold collateral, pay funding, liquidate the underwater) with the same on-chain primitives. They diverge sharply on everything else.

**Program-on-Solana** (this curriculum): one-command deploy, inherit existing liquidity and wallets, ride existing validator security. The codebase fits one repository; one team can ship it. Bounded by the host chain's slot time, compute budget, and validator concentration. Value capture is partial — a real fraction of transaction value flows to actors you don't control.

**Custom L1**: months of validator bootstrap, your own bridges and stablecoin economics, your own wallet integrations and listings, your own MEV policy. The codebase spans consensus + execution + networking + RPC across multiple crates. In exchange: full runtime control (block time, ordering, custom precompiles tuned to your workload) and full value capture (no validator-tier middlemen).

The L1 path becomes necessary when three conditions hold *together*:

1. You need a runtime property the host chain can't provide — sub-100ms confirmation, custom matching precompiles, deterministic MEV-free ordering, settlement at non-EVM-friendly precision.
2. Your business model depends on capturing the full transaction value rather than sharing it with the host chain's validator economy.
3. You have the team and runway to operate a chain for years, not just deploy a program.

Hyperliquid is the textbook case where all three held. For most perp-DEX projects, none of them do — and the program-on-host-chain path is dramatically cheaper to reach launch on, by orders of magnitude in calendar time, infrastructure cost, and team headcount.

This curriculum bets on the program path. Not because L1 is the wrong answer in absolute terms, but because the conditions that justify it are rare, and a project that hasn't shipped on a host chain yet hasn't earned the right to commit to a multi-year L1 build. Phase B teaches the path 99% of perp DEX projects will actually take.

---

## §6.0  Framing

Phase A built the runtime fundamentals. Phase B builds the HL primitives — and they all talk to other programs, mostly SPL Token. Every order placement moves tokens. Every settlement moves tokens. Every withdrawal moves tokens. None of this is "do something inside our program"; it is "tell SPL Token to do something on our behalf." That conversation between programs is called **cross-program invocation** (CPI), and it is the most consequential thing your program does after the owner check.

This chapter builds the most fundamental CPI we need: an SPL Token vault for a market, plus a `Deposit` instruction that moves user tokens into it. The two together exercise the full CPI mechanism:

- **CreateVault** does two consecutive CPIs — `System::create_account` (signed by the vault PDA via `invoke_signed`) and `SPL Token InitializeAccount3` (signed by no one, plain `invoke`).
- **Deposit** does one CPI — `SPL Token Transfer`, signed by the user *not* via PDA but via signer-privilege extension from the outer transaction.

By the end you will have:

1. Read `invoke` and `invoke_signed` side by side and understood that they are literally the same call with a different "do you have seeds?" parameter.
2. Walked the runtime's rule for signer privilege: every signer that appears in the outer transaction's `AccountMeta` and is then re-emitted in a CPI's `AccountInfo` is treated as a signer by the callee.
3. Built two SPL Token instructions by hand — `INITIALIZE_ACCOUNT_3` (tag 18) and `TRANSFER` (tag 3) — knowing exactly what bytes go on the wire.
4. Understood why we use `invoke_signed` to create the vault but `invoke` to deposit into it. They are *not* interchangeable — the choice is dictated by who needs to sign for the operation.

This is the chapter that turns the program from "owns its own accounts" to "talks to the rest of the chain." Everything in Phase B builds on it.

---

## §6.1  `invoke` and `invoke_signed` are the same call

Open `solana-cpi-2.2.1/src/lib.rs:137–139`:

```rust
pub fn invoke(instruction: &Instruction, account_infos: &[AccountInfo]) -> ProgramResult {
    invoke_signed(instruction, account_infos, &[])
}
```

That is the entire body of `invoke`. It calls `invoke_signed` with an empty seeds slice. The two functions are the same syscall under the hood. The only thing that varies is whether you supply PDA seeds.

The semantic distinction:

- **`invoke_signed(ix, accounts, signers_seeds)`** — "Execute this instruction, and treat the PDAs derivable from these seeds as additional signers." Used in Chapter 3 to let our program sign for the market PDA during `create_account`.
- **`invoke(ix, accounts)`** — "Execute this instruction, with no new signers from us. Whatever signers are already in `accounts` (because they signed the outer transaction) remain signers." Used when the original transaction's signer already has the authority you need.

When to use which is determined entirely by who needs to sign for the CPI's operation:

- The new account in `create_account`? That's a PDA we own. Only we can sign for it. → `invoke_signed`.
- The transfer authority on an SPL Token Transfer? That's the *user's* wallet keypair. They already signed the outer transaction. → `invoke`.
- The mint authority on `SPL Token MintTo`? Depends. If your program is the mint authority (e.g., it's a PDA we control), → `invoke_signed`. If the user is the mint authority and signed the outer tx, → `invoke`.

There is no third option. Every CPI either inherits signers from the outer transaction or extends them via PDA seeds. The runtime's job is to verify that whatever signers the CPI requires can be accounted for via one of those two mechanisms.

**What the SDK hides:** Anchor's `CpiContext::new(...)` and `CpiContext::new_with_signer(...)` are direct wrappers around `invoke` and `invoke_signed` respectively. The choice is yours; Anchor never picks for you. Pick the wrong one and the runtime fails the CPI with a signature error.

> **Exercise §6.1.** What happens if you call `invoke_signed` with a non-empty seeds slice when the CPI doesn't need any PDA signers? (Hint: it's not an error. The runtime just ignores PDA signers that don't appear in the inner instruction's required-signer list.)

---

## §6.2  Signer privilege extension

The rule that makes `Deposit` work without any PDA signing is short: **when a signer appears in your program's outer-transaction `AccountMeta` and you re-emit it in a CPI's `AccountInfo`, the callee sees that account as a signer.**

This is the runtime's "signer privilege extension." The user signs *once* at the outer transaction. Inside `process_deposit`, the `user` account's `AccountInfo` has `is_signer = true` (because the outer tx signed it). When we build the SPL Token Transfer instruction with `AccountMeta::new_readonly(*user_ai.key, true)` (signer = true) and pass `user_ai.clone()` in the `account_infos` to `invoke`, the runtime checks: "is this account marked as a signer at my level? yes. Is it marked as a signer in the CPI's account list? yes. Do they agree?" — and if yes, the callee program (SPL Token) receives an `AccountInfo` for the user with `is_signer = true`.

What this rule prevents: you cannot *promote* a non-signer into a signer through CPI. If the user did not sign the outer transaction, no amount of `AccountMeta::new(*user.key, true)` in your CPI will manufacture a signature. The runtime sees the discrepancy and rejects the CPI with `MissingRequiredSignature`.

What this rule allows: you can *propagate* signers without re-asking them to sign. The user signs once; that signature is good for every program in the chain that re-emits the user as a signer in its CPI declarations. This is how a single user signature can authorize an arbitrary sequence of SPL Token transfers, swaps, and program calls, all in one transaction.

There is one more piece. The PDA signers from `invoke_signed`'s `signers_seeds` parameter are *added* to the outer signer set for the duration of the CPI. The runtime hashes the seeds + the calling program ID, confirms the result matches the account being signed for, and treats that account as a signer for the CPI. This is the PDA's only mechanism for signing anything — they have no private key.

Both mechanisms — outer-tx propagation and PDA seeds — live in the same `invoke_signed` body. The runtime takes the union of the two when deciding what counts as a signer for the inner instruction.

> **Exercise §6.2.** In `process_deposit`, what would happen if you constructed the SPL Token Transfer instruction with `AccountMeta::new_readonly(*user_ai.key, false)` (signer = false)? Would the transfer succeed? Why or why not?

---

## §6.3  Walking CreateVault — two consecutive CPIs

CreateVault is the place where both flavors of CPI appear in the same handler. From `programs/openhl-core/src/lib.rs:680–720`.

**The PDA derivation** (lines 668–678) sets up two pubkeys: `vault_token_account` at `[VAULT_SEED, market.key, mint.key]` and `vault_authority` at `[VAULT_AUTH_SEED, market.key]`. Same find_program_address mechanic as Chapter 3. The bumps are returned and the vault bump is used below; the authority bump we don't need yet (it will matter when we add withdrawals in a later chapter and the program must sign as the vault authority).

**CPI 1 — System::create_account** at lines 689–700:

```rust
let create_ix = system_instruction::create_account(
    payer_ai.key,
    vault_ai.key,
    rent,
    TOKEN_ACCOUNT_LEN as u64,
    &SPL_TOKEN_PROGRAM_ID,
);
invoke_signed(
    &create_ix,
    &[payer_ai.clone(), vault_ai.clone(), system_ai.clone()],
    &[&[
        VAULT_SEED,
        market_ai.key.as_ref(),
        mint_ai.key.as_ref(),
        &[vault_bump],
    ]],
)?;
```

This is `invoke_signed` because the new account is a PDA we own and System requires the new account to sign. The seeds + bump are exactly what we used in the `find_program_address` call. The third argument to `create_account` is the *owner program* of the new account — we pass `SPL_TOKEN_PROGRAM_ID`, not our own program. From this point on, the vault token account is owned by SPL Token (at the Solana-runtime level), and only SPL Token can write to its data.

Note the AccountInfo array: `[payer_ai, vault_ai, system_ai]`. These are the only three accounts the System program needs to see for `create_account` (the rest of our handler's accounts — market, mint, vault_authority, token_program — aren't passed because System doesn't need them).

**CPI 2 — SPL Token InitializeAccount3** at lines 707–720:

```rust
let mut init_data = Vec::with_capacity(1 + 32);
init_data.push(spl_token_ix::INITIALIZE_ACCOUNT_3);
init_data.extend_from_slice(vault_auth_ai.key.as_ref());
let init_ix = Instruction {
    program_id: SPL_TOKEN_PROGRAM_ID,
    accounts: vec![
        AccountMeta::new(*vault_ai.key, false),
        AccountMeta::new_readonly(*mint_ai.key, false),
    ],
    data: init_data,
};
invoke(&init_ix, &[vault_ai.clone(), mint_ai.clone(), token_ai.clone()])?;
```

This is plain `invoke` because no one needs to sign. The vault account is already SPL Token-owned (we just set that up in CPI 1). InitializeAccount3 is a pure data write — it sets the mint and owner fields on the empty token account. No signature is required because the mere act of being SPL Token-owned authorizes SPL Token to write to your data.

Note the *AccountInfo* array passed to `invoke` is `[vault_ai, mint_ai, token_ai]` — but the *AccountMeta* in the instruction only mentions `vault_ai` and `mint_ai`. Why the extra `token_ai`? Because the runtime requires you to also pass the AccountInfo for the *program being invoked*, even though it's not in the instruction's account list. This is the "AccountInfo reborrowing" rule: you must pass every AccountInfo the runtime will need to set up the call, which includes the callee program itself.

Two CPIs, one handler, both kinds of signing. The pattern repeats throughout the rest of Phase B.

> **Exercise §6.3.** Remove the `token_ai.clone()` from the `invoke` call. What error does the runtime return? It is one of the most common CPI bugs in early Solana code — and one of the least googleable error messages.

---

## §6.4  Walking Deposit — user signs at the outer level

Deposit is the simpler case. From `programs/openhl-core/src/lib.rs:771–800`.

The instruction data:

```rust
let mut transfer_data = Vec::with_capacity(1 + 8);
transfer_data.push(spl_token_ix::TRANSFER);
transfer_data.extend_from_slice(&amount.to_le_bytes());
```

Nine bytes total: a one-byte tag (3 for Transfer) and an eight-byte amount in little-endian. That is the entire wire format for SPL Token Transfer.

The accounts:

```rust
let transfer_ix = Instruction {
    program_id: SPL_TOKEN_PROGRAM_ID,
    accounts: vec![
        AccountMeta::new(*user_token_ai.key, false),
        AccountMeta::new(*vault_token_ai.key, false),
        AccountMeta::new_readonly(*user_ai.key, true),
    ],
    data: transfer_data,
};
```

Three accounts: source token account (writable), destination token account (writable), authority (signer, readonly). The authority is the user's wallet, marked as signer. We don't write to the user's wallet here — the token movement is reflected in the source token account's balance field, not the wallet's lamports — so the wallet itself is read-only-but-signing.

The CPI:

```rust
invoke(
    &transfer_ix,
    &[
        user_token_ai.clone(),
        vault_token_ai.clone(),
        user_ai.clone(),
        token_ai.clone(),
    ],
)?;
```

Plain `invoke`. No seeds. The user signed at the outer transaction; that signature flows through to SPL Token via the `is_signer = true` flag on the `user_ai` `AccountInfo`. SPL Token sees a signer it accepts as the authority, and the transfer commits.

This is the canonical "program-mediated user transfer" pattern: the user authorizes the operation by signing the outer transaction, the program orchestrates whatever CPIs are needed, and the user's signer privilege flows through.

> **Exercise §6.4.** Build a Deposit transaction where the `user_token_account` and `vault_token_account` belong to different mints. What error do you get, and at which layer does it surface — our program or SPL Token?

---

## §6.5  Hand-rolling SPL Token instruction data

We deliberately did *not* import the `spl-token` crate. Instead we hand-rolled two instructions worth of bytes. Lines 594–598 of `lib.rs`:

```rust
mod spl_token_ix {
    pub const TRANSFER: u8 = 3;
    pub const INITIALIZE_ACCOUNT_3: u8 = 18;
}
```

That is the full vocabulary we needed. Two tag values. Everything else — the field encoding, the account order — comes from reading the SPL Token program's source (or its `Instruction` enum in the `spl-token` crate, which we are deliberately not depending on).

Two reasons:

1. **Pedagogy.** This chapter is about CPI bytes. Importing a builder that produces the exact bytes for you would let you finish the chapter without ever seeing them. Hand-rolling them once teaches the format permanently — every future SPL Token instruction you encounter, you can read the tag and the data without consulting docs.
2. **Binary size.** The `spl-token` crate at recent versions pulls in roughly 25 KB of dependencies (the Token enum, the error types, helper builders) when compiled to BPF. For a program that needs two instructions, that is pure overhead. Hand-rolling the data adds maybe 200 bytes.

This is a tradeoff most production programs do *not* make — they import `spl-token` for the type safety and the maintenance story. We make it here because the chapter requires it. Once you understand the bytes, the tradeoff goes the other way: pay the 25 KB and let the typed builder catch tag mistakes for you.

The general technique applies to any program you CPI to:

1. Find the instruction tag in the callee's source.
2. Find the data fields (Borsh-encoded, or hand-packed, depending on the program).
3. Find the account list in the callee's processor — usually documented as account references like `[WRITE, SIGNER]` in the variant's doc comment.
4. Build the `Instruction` struct manually.
5. `invoke` or `invoke_signed` it.

The five-step recipe works for SPL Token, the Address Lookup Table program, the Compute Budget program, the BPF Loader program, and any custom program someone built. The mechanics don't change.

**What Anchor hides:** Anchor has typed CPI wrappers for SPL Token (`anchor_spl::token::{Transfer, MintTo, ...}`) that hide the byte layout entirely. They work — but they obscure exactly what this chapter wants you to see. A native program that hand-rolls its CPIs and exposes them clearly has a much better security audit story than one that calls `anchor_spl::token::transfer(ctx, amount)` and trusts the macro.

---

## §6.6  Recap + verify yourself

### Recap diagram

```
                  invoke                            invoke_signed
                  ──────                            ─────────────
   What it is:    invoke_signed(ix, accs, &[])      invoke_signed(ix, accs, seeds)
   PDA signing:   no                                yes (program signs for derived PDAs)
   Outer signers: passed through automatically      passed through automatically
   When to use:   user / outer-tx signs the op      a PDA we own must sign


Per-CPI privilege resolution:
    For each AccountMeta in the inner instruction with is_signer = true:
       Is the same pubkey marked is_signer in the *outer* AccountMeta?   ──► YES → signer ok
       OR did we pass seeds that derive this pubkey via PDA?              ──► YES → signer ok
       OTHERWISE                                                          ──► reject CPI


openhl-core CPI map:

    CreateVault
       CPI 1: System::create_account
          invoke_signed, vault PDA seeds
       CPI 2: SPL Token InitializeAccount3
          invoke (no signing needed — SPL Token owns the new account)

    Deposit
       CPI 1: SPL Token Transfer
          invoke (user signed at outer tx, privilege flows through)
```

### Three things to verify yourself

1. **`invoke` is `invoke_signed` with no seeds.** Open `solana-cpi-2.2.1/src/lib.rs:137` and read the three-line body. Internalize that the only difference between the two is "did we pass PDA seeds." Everything else is the same syscall.
2. **AccountInfo for the callee program.** In both CPIs in CreateVault, the `accounts` slice passed to `invoke[_signed]` includes the callee program's AccountInfo — `system_ai` for CPI 1, `token_ai` for CPI 2. Omitting them is one of the most common early-Solana mistakes; the runtime returns a confusing `AccountNotFound`-flavored error. Verify by reading [`lib.rs:691, 720`](../../programs/openhl-core/src/lib.rs#L691).
3. **User signs once, used by SPL Token.** Run `deposit` against your validator. The user keypair signs the outer transaction; SPL Token receives a `Transfer` instruction with `is_signer = true` on the user; the transfer commits. There is no second signature anywhere. One signature, propagated.

---

## Hook into Chapter 7

You can now talk to other programs. But the program you're going to talk to most in Phase B is *yourself* — the on-chain order book. The CLOB lives inside the market account as a slab of bids and asks, and every place/cancel instruction will read and write substantial chunks of it. That data structure is the next thing to build.

Chapter 7 walks the on-chain CLOB design: critbit vs heap vs slab, zero-copy account access via `bytemuck`, the trade-off between memory locality and account size, why production programs almost always pick the slab. We will extend the `Market` struct to embed an order book of fixed capacity, add a `place_order` instruction that writes into it, and watch the CU envelope tighten as the book fills up. By the end the market will accept its first order — and the question that drove Phase A's compute-budget chapter ("how do you fit a matcher in 200K CU?") becomes urgent for the first time.
