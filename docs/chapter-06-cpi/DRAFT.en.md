# Chapter 6 — CPI Internals via Vault Deposits

> Status: draft (v0.1).
> Companion code: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (`process_create_vault` at lines 888–997, `process_deposit` at lines 1004–1072), [`scripts/create-vault/src/main.rs`](../../scripts/create-vault/src/main.rs), [`scripts/deposit/src/main.rs`](../../scripts/deposit/src/main.rs), [`fuzz/basics/src/main.rs`](../../fuzz/basics/src/main.rs) (the solinv harness for §6.6).
> Tested against: solana-cpi 2.2.1, solana-program 2.3.0, SPL Token program (TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA).

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

CreateVault is the place where both flavors of CPI appear in the same handler. From `programs/openhl-core/src/lib.rs:888–997`.

**The PDA derivation** (lines 928–950) sets up two pubkeys: `vault_token_account` at `[VAULT_SEED, market.key, mint.key]` and `vault_authority` at `[VAULT_AUTH_SEED, market.key]`. Same find_program_address mechanic as Chapter 3. The bumps are returned and the vault bump is used below; the authority bump we don't need yet (it will matter when we add withdrawals in a later chapter and the program must sign as the vault authority).

**CPI 1 — System::create_account** at lines 952–971:

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

**CPI 2 — SPL Token InitializeAccount3** at lines 973–993:

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

Deposit is the simpler case. From `programs/openhl-core/src/lib.rs:1004–1072`.

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
2. **AccountInfo for the callee program.** In both CPIs in CreateVault, the `accounts` slice passed to `invoke[_signed]` includes the callee program's AccountInfo — `system_ai` for CPI 1, `token_ai` for CPI 2. Omitting them is one of the most common early-Solana mistakes; the runtime returns a confusing `AccountNotFound`-flavored error. Verify by reading [`lib.rs:962, 993`](../../programs/openhl-core/src/lib.rs#L962).
3. **User signs once, used by SPL Token.** Run `deposit` against your validator. The user keypair signs the outer transaction; SPL Token receives a `Transfer` instruction with `is_signer = true` on the user; the transfer commits. There is no second signature anywhere. One signature, propagated.


### Now let a tool try the checks

`CreateVault` has seven checks and `Deposit` four. Chapter 3 §3.7 ran [solinv](https://github.com/psyto/solinv) on `CreateMarket`, and every deleted check was backed by the runtime. These two handlers make CPIs to a second program, so there are more places a check can be backed, and one where it is not. The same harness, [`fuzz/basics/`](../../fuzz/basics), has specs for both: the payer's or user's signer flag cleared, and each slot swapped for something an attacker could pass (a "market" this program does not own, a token account as the "mint", a vault address that is not the PDA, the caller as the vault's authority, the wrong program in each program slot, B's token account as A's source). Set up as in Chapter 3 §3.7, then:

```bash
cd fuzz/basics
cargo build --release --features invariant_ch6
cargo test --release --features invariant_ch6      # A cannot spend B's tokens
timeout -s INT 60 crucible run openhl_basics_fuzz invariant_ch6 \
    --binary-in target/release/invariant_test --timeout 45
```

On the program as it is, no `[FUZZ_FINDING]` line appears.

> **Exercise §6.6.** Delete each check in turn, rebuild, run, and predict first. Here is what we measured, 45 seconds each. The last column is what refused the swapped call instead, read from its logs:
>
> | Check deleted | Caught? | What refused the call instead |
> |---|---|---|
> | CreateVault: `payer_ai.is_signer` | no | the runtime: unauthorized signer |
> | CreateVault: `market_ai.owner == program_id` | no | our vault PDA check |
> | CreateVault: `mint_ai.owner == SPL Token` | no | our vault PDA check |
> | CreateVault: `system_ai.key == System` | no | the runtime: an account required by the instruction is missing |
> | CreateVault: `token_ai.key == SPL Token` | no | the runtime: an account required by the instruction is missing |
> | CreateVault: `vault_ai.key == expected_vault` | no | the runtime: unauthorized signer |
> | CreateVault: `vault_auth_ai.key == expected_auth` | **yes**, account-swap, slot 4 | — |
> | Deposit: `user_ai.is_signer` | no | the runtime: unauthorized signer |
> | Deposit: `user_token_ai.owner == SPL Token` | no | the runtime: instruction modified data of an account it does not own |
> | Deposit: `vault_token_ai.owner == SPL Token` | no | the runtime: instruction modified data of an account it does not own |
> | Deposit: `token_ai.key == SPL Token` | no | the runtime: an account required by the instruction is missing |
>
> Before reading on, find the one row where nothing else stands behind the check.
>
> Most rows are §6.2 and §6.1 again. A program can pass on only the signatures it was given, so a missing payer or user signature fails at the CPI (§6.2). The handler builds each inner instruction with the callee's ID written in (`SPL_TOKEN_PROGRAM_ID`, `system_program::ID`), so a wrong program in the slot does not redirect the CPI; the runtime just cannot find the real callee's account (§6.1). `create_account` on an address that is not the PDA needs that address's own signature. A token account owned by some other program, with the same bytes inside, gets past SPL Token, which runs the `Transfer`; then the runtime refuses it, because SPL Token changed an account it does not own (Chapter 1). B's real token account is refused by SPL Token itself, because A is not its authority (`owner does not match`, which the test pins).
>
> Two rows are backed by our own code, and only because of how the fuzzer swaps. The vault PDA is derived from the market and the mint, so swapping either one alone makes slot 3 the wrong address. Swap them together with the accounts derived from them, a set (`account_swap::check_sets`; Chapter 12 §12.7 uses one too), and the rows split: with the mint owner check gone, SPL Token still refuses a token account as a mint (`Invalid Mint`). With the market owner check gone, the call went through and created a token account for a "market" that was someone's token account. That check is doing real work.
>
> The **yes** row is the one to remember. `vault_authority` is never created; it is only an address, which `InitializeAccount3` writes into the new token account as its owner. Nothing about the CPI depends on it being the PDA, so if the program does not check it, the caller can name themselves, and from then on every deposit into that market's vault is the caller's to move. No runtime rule can catch this, because to the runtime the address is just data.
>
> Two more to try. First, add `self.b_ata` to `Deposit`'s slot 2 alternates in `fuzz/basics/src/main.rs` (the destination) and run again: it is reported on every execution, since A can deposit into any token account. Is that a hole? Here it is not. `Deposit` records nothing, so A paying into B's account is a gift. Chapters 11 and 12 add the bookkeeping that makes the destination matter, and bind it. A finding is a call the program allowed; whether it matters is a question about what the program promised.
>
> Second, the read-time question from Chapter 3. `CreateVault` checks the market's owner, not its address. Chapter 2's `Initialize` can still make a `Market` in any account the caller creates, so a pair can have a second market that is not at its PDA. Build with `--features invariant_ch3_reader` and run: that harness makes such a market and has `CreateVault` read it (three slots swapped together: the market and the two addresses derived from it). It is reported on the current program, and the test `create_vault_accepts_a_market_not_at_its_pda` shows it directly. Is *that* a hole? Work it out before reading on.
>
> What the PDA promised was one address per `(base, quote)`. Almost nothing in the program relies on that. Every account keyed to a market (its token accounts and their authority, the book, oracle, funding state, positions, trading vaults, insurance fund) is seeded by the market's own address, so a second market is a separate island, and `CreateMarket` lets anyone choose a market's authority anyway. The exceptions are all in Chapter 13: the fee vault and its authority (one per quote mint, shared by every market) and builder profiles (one per builder). We found no way in Chapters 6–12 for a second market to reach another market's tokens; whether it can take more out of the fee vault than its users paid in is a question for Chapter 13. A client that finds "the" market by deriving its PDA is fine; a client that believes any account shaped like a market is not. If the program wanted to enforce one market per pair, `CreateVault` would re-derive the address from the market's own `base_mint`, `quote_mint` and stored `bump` (Chapter 3's "bump stored"). We tried it: with that check added, the set was refused (`InvalidSeeds`) and the fuzzer found nothing in 45 seconds.

---

## Hook into Chapter 7

You can now talk to other programs. But the program you're going to talk to most in Phase B is *yourself* — the on-chain order book. The CLOB lives inside the market account as a slab of bids and asks, and every place/cancel instruction will read and write substantial chunks of it. That data structure is the next thing to build.

Chapter 7 walks the on-chain CLOB design: critbit vs heap vs slab, zero-copy account access via `bytemuck`, the trade-off between memory locality and account size, why production programs almost always pick the slab. We will extend the `Market` struct to embed an order book of fixed capacity, add a `place_order` instruction that writes into it, and watch the CU envelope tighten as the book fills up. By the end the market will accept its first order — and the question that drove Phase A's compute-budget chapter ("how do you fit a matcher in 200K CU?") becomes urgent for the first time.
