# Chapter 1 — The Account Model from the Bytes Up

> Status: placeholder. Drafting begins after the scaffold lands.
>
> Companion code: `scripts/allocate-market/`, `crates/state/src/lib.rs`.

## §1.0  Framing

What an "account" actually is on Solana, why the model is unusual, and what the SDK hides when you call `Account::create(...)`.

## §1.1  The five fields of an account

`owner`, `lamports`, `data`, `executable`, `rent_epoch` — what each means, why they're separate, what the runtime guarantees about each.

## §1.2  Rent and rent exemption

Rent exemption as a balance threshold, not a payment. How `getMinimumBalanceForRentExemption` is calculated from `data.len()`.

## §1.3  Allocating an account from the System program

Walk through `system_instruction::create_account`. Why both the payer *and* the new account must sign. Why owner gets assigned at creation.

## §1.4  Reading the bytes

Run the companion script. Inspect the raw `account.data` against the `openhl_state::Market` layout. See what zero bytes look like.

## §1.5  What the SDK hides

Anchor's `#[account(init, ...)]` collapses §1.1–§1.4 into a macro. Show what it expands to and which decisions it makes silently (payer choice, rent calc, owner assignment, discriminator write).

## §1.6  Recap + verify yourself

- Diagram: account memory layout.
- Three things to verify: (1) `solana account <pubkey>` shows owner=System, (2) `data` is all zeros, (3) lamports == rent-exempt minimum.

## Hook into Chapter 2

The account exists, but we can't write to it — System owns it. Chapter 2 introduces our first native program so we can take ownership and write the `MARKET\0\0` discriminator.
