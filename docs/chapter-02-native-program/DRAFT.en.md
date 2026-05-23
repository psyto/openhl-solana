# Chapter 2 — Writing a Native Program Without Anchor

> Status: draft (v0.1).
> Companion code: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs), [`scripts/init-market/src/main.rs`](../../scripts/init-market/src/main.rs).
> Tested against: solana-program 2.3.0, solana-program-entrypoint 2.3.0, solana-account-info 2.3.0, solana-program-error 2.2.2, solana-system-interface 1.0.0.

---

## §2.0  Framing

At the end of Chapter 1 you had an on-chain account: 256 zero bytes, owned by the System program, with no way to change a single bit because System has no "write arbitrary data" instruction. The account was a vessel. The next step is to build the only thing that can fill it.

That thing is a Solana program — but not an Anchor program. In this chapter we write the entire program by hand. One `lib.rs`. No `#[derive(Accounts)]`. No `#[program]`. No Borsh. Just `entrypoint!`, `&[AccountInfo]`, a manual byte decode, and a `bytemuck` cast.

We will:

1. Open `solana-program-entrypoint` and see what `entrypoint!(process_instruction)` actually expands to.
2. Open `solana-account-info` and see what `AccountInfo` gives us that `Account` from Chapter 1 didn't.
3. Walk every line of `programs/openhl-core/src/lib.rs` — the dispatcher, the owner check, the bytemuck cast.
4. Walk every line of `scripts/init-market/src/main.rs` — a single client transaction with two instructions, `System::Assign` followed by `openhl-core::Initialize`.
5. Run it, hex-dump the account, and watch bytes `[0..8]` flip from `00 00 00 00 00 00 00 00` to `4d 41 52 4b 45 54 00 00` — the eight-byte payoff of building your first Solana program.
6. Enumerate exactly what `#[program]` + `#[derive(Accounts)]` would have generated for us.

By the end you will be able to read any Anchor program's expansion (`cargo expand`) and identify which generated function corresponds to which line you wrote here. The cost is paying attention to about 160 lines of Rust. The benefit is permanent.

---

## §2.1  `entrypoint!` and `process_instruction` — the program ABI

Every Solana program is a `.so` file with a single exported C function: `entrypoint`. The Solana loader calls into it with a pointer to a serialized buffer containing the program ID, the accounts, and the instruction data. The macro `entrypoint!` wraps this ABI in a Rust-friendly facade.

Open `solana-program-entrypoint-2.3.0/src/lib.rs:127–142`:

```rust
#[macro_export]
macro_rules! entrypoint {
    ($process_instruction:ident) => {
        /// # Safety
        #[no_mangle]
        pub unsafe extern "C" fn entrypoint(input: *mut u8) -> u64 {
            let (program_id, accounts, instruction_data) = unsafe { $crate::deserialize(input) };
            match $process_instruction(program_id, &accounts, instruction_data) {
                Ok(()) => $crate::SUCCESS,
                Err(error) => error.into(),
            }
        }
        $crate::custom_heap_default!();
        $crate::custom_panic_default!();
    };
}
```

Fifteen lines. That is the entire macro. It does four things:

1. Exports a `no_mangle extern "C" entrypoint` function so the loader can find it by name.
2. Calls `$crate::deserialize(input)` to unpack the loader's binary input into `(program_id, accounts, instruction_data)` — a `&Pubkey`, a `Vec<AccountInfo>`, and a `&[u8]`.
3. Forwards those three things to *your* function (the identifier passed in).
4. Converts your `Result<(), ProgramError>` back into the `u64` exit code the loader expects (`0` for success, an encoded error for failure).

That is all. There is no router, no middleware, no extension point. Whatever Rust function you name in `entrypoint!($fn)` is the single point through which the entire chain talks to your program.

Our invocation is at `programs/openhl-core/src/lib.rs:25–26`:

```rust
#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);
```

The `cfg` gate lets us conditionally drop the entrypoint when the crate is linked into a host binary (our client, our tests, anything that wants the types but not the BPF entrypoint). The `init-market` client crate enables `no-entrypoint`, so the program's types are linked in but the BPF entrypoint is not — preventing a name collision with the client's own `main`.

Our `process_instruction` follows the standard signature, `lib.rs:33–37`:

```rust
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
```

The signature is fixed by `entrypoint!`. You cannot add parameters, take different argument types, or return a different result. `ProgramResult` is just `Result<(), ProgramError>` — see `solana-program-error-2.2.2/src/lib.rs:28`:

```rust
pub type ProgramResult = std::result::Result<(), ProgramError>;
```

`ProgramError` itself is a 24-variant enum (`src/lib.rs:33–63`) covering every standard failure mode: `IncorrectProgramId`, `NotEnoughAccountKeys`, `InvalidAccountData`, `AccountAlreadyInitialized`, and so on. You can also raise `ProgramError::Custom(u32)` for program-defined errors with your own numeric codes.

**What Anchor hides:** Anchor's `#[program]` macro generates a function with this exact signature. Anchor's "instructions" — the handler functions you write — are NOT the entrypoint. They are functions Anchor dispatches *into*, after its own generated `process_instruction` has unpacked the instruction data, looked up the discriminator, deserialized accounts, and routed to the right handler. You don't see this code because the macro generates it. But it exists, and it has the same shape as what we just wrote.

> **Exercise §2.1.** Build the program with `cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml`. Inspect `target/deploy/openhl_core.so` with `nm` (or `objdump`) and find the exported `entrypoint` symbol. Confirm it's the only `T` (text/code) symbol with external linkage.

---

## §2.2  `AccountInfo` — what your program actually sees

In Chapter 1 we worked with `Account` — the type returned by `RpcClient::get_account`. It owned its `data: Vec<u8>`. On-chain, your program sees a different type: `AccountInfo`.

Open `solana-account-info-2.3.0/src/lib.rs:19–39`:

```rust
/// Account information
#[derive(Clone)]
#[repr(C)]
pub struct AccountInfo<'a> {
    /// Public key of the account
    pub key: &'a Pubkey,
    /// The lamports in the account.  Modifiable by programs.
    pub lamports: Rc<RefCell<&'a mut u64>>,
    /// The data held in this account.  Modifiable by programs.
    pub data: Rc<RefCell<&'a mut [u8]>>,
    /// Program that owns this account
    pub owner: &'a Pubkey,
    /// The epoch at which this account will next owe rent
    pub rent_epoch: u64,
    /// Was the transaction signed by this account's public key?
    pub is_signer: bool,
    /// Is the account writable?
    pub is_writable: bool,
    /// This account's data contains a loaded program (and is now read-only)
    pub executable: bool,
}
```

Compare this to `Account` (Chapter 1, `solana-account-2.2.1/src/lib.rs:44–56`):

| `Account` | `AccountInfo` |
|---|---|
| `lamports: u64` | `lamports: Rc<RefCell<&'a mut u64>>` |
| `data: Vec<u8>` | `data: Rc<RefCell<&'a mut [u8]>>` |
| `owner: Pubkey` | `owner: &'a Pubkey` |
| `executable: bool` | `executable: bool` |
| `rent_epoch: Epoch` | `rent_epoch: u64` |
| — | `key: &'a Pubkey` |
| — | `is_signer: bool` |
| — | `is_writable: bool` |

Two things changed; three things were added.

The **changes** are about ownership and mutability. The loader hands your program a *view* into a buffer it controls — your program does not own the lamport count or the data bytes. They are someone else's memory; you get a `Rc<RefCell<&mut _>>` borrow so multiple `AccountInfo` references (from different instructions in the same transaction) can coexist while still enforcing borrow rules at runtime. The `Rc` is because the loader hands you the same `AccountInfo` for the same pubkey across multiple instructions; the `RefCell` is because Rust's borrow checker cannot statically prove the borrow rules in this setting.

The **additions** are runtime-only information that doesn't exist for `Account`:

- **`key`** — the pubkey of the account itself. `Account` doesn't carry its own pubkey; the pubkey is the index into the on-chain account map. `AccountInfo` does carry it, because programs routinely need to compute things from it (PDA derivation, account-to-program mapping).
- **`is_signer`** — whether this account signed the *outer transaction*. The runtime sets this per-AccountInfo per-instruction.
- **`is_writable`** — whether the transaction marked this account writable. Even if your program could write to it (owner = you, size big enough), if the transaction didn't mark it writable, the write will fail at commit.

These three are how the runtime tells your program *about* the transaction it's running in. You did not put them there; the loader did.

**What Anchor hides:** Anchor's typed account wrappers (`Account<'info, T>`, `Signer<'info>`, `UncheckedAccount<'info>`, etc.) all hold an `AccountInfo` internally — visible as `to_account_info()`. The wrappers add type-level checks (deserialization, signer requirement, etc.) on top, but the underlying value is the same `AccountInfo`. When you see `let info = ctx.accounts.market.to_account_info();` in Anchor code, you are reaching through the abstraction to the layer we work with directly.

> **Exercise §2.2.** In our `process_initialize` (`lib.rs:79–81`), we use `accounts.first()` to get the market account. We do *not* check `is_writable`. Why not? (Hint: what error code would the runtime return if a non-writable account were passed and we tried to call `try_borrow_mut_data`?)

---

## §2.3  Owner check — the most important line in your program

We accept an account and call it a "market." How do we *know* it's actually our market and not, say, a random rent-exempt 256-byte account someone built that happens to look the right shape?

The answer — the only answer — is the **owner check**. From `programs/openhl-core/src/lib.rs:83–94`:

```rust
// (1) Owner check. The single most-skipped check in Solana programs,
// and the source of most "but I checked the pubkey!" exploits. The
// *only* thing that proves an account is one of ours is that we own
// it. If owner is something else, the bytes inside could mean anything.
if market_ai.owner != program_id {
    msg!(
        "initialize: market owner {} != program {}",
        market_ai.owner,
        program_id
    );
    return Err(ProgramError::IncorrectProgramId);
}
```

The runtime guarantee from Chapter 1: only the owner program may write to an account's `data`. The contrapositive: if `owner == program_id`, then *we* are the only program that could have written those bytes. The discriminator at offset 0 is either zero (the account exists but is uninitialized) or `MARKET_DISCRIMINATOR` (we initialized it). It cannot be anything else, because no other program would write to it.

Without this check, an attacker could:

1. Allocate a 256-byte account owned by *their* program.
2. Write whatever bytes they want to `data[0..256]`.
3. Pass that account to our `Initialize`.
4. Pass through every other check (size is 256, discriminator is whatever they set it to, payload decodes fine).
5. Our program would happily overwrite the bytes — but the *next* check of the discriminator would see whatever the attacker had set, not what we set. Worse, in later chapters when this same account is fed into `place_order`, we would trust its bytes implicitly.

Owner check is what turns "256 bytes the right shape" into "256 bytes I wrote." Skip it and you skip the security model.

The size check (`lib.rs:99–106`) comes second, but it's mechanical — `bytemuck::from_bytes_mut::<Market>(buf)` would panic if the buffer were too small, so we reject explicitly with a clean error code. The already-initialized check (`lib.rs:111–117`) comes third — if discriminator is non-zero, this account is already a live `Market` and we must not stomp it.

**What Anchor hides:** Anchor's `#[account(mut)]` constraint and the typed `Account<'info, T>` wrapper perform the owner check for you. Specifically: when Anchor deserializes `Account<'info, MyType>`, it asserts `account.owner == program_id` before returning the typed view. If the check fails, your handler is never called. This is genuinely safer than asking you to remember — but it also means many Anchor developers never internalize *why* the check exists. Read your Anchor code and find the owner check. It's there. It's just invisible.

> **Exercise §2.3.** Modify `process_initialize` to deliberately skip the owner check (comment out lines 87–94). Rebuild. Construct a transaction that passes a System-owned account (the one Chapter 1's allocator created, *without* the Assign step we'll add in §2.5) to `Initialize`. Run it. What happens? Why?

---

## §2.4  Writing data — `try_borrow_mut_data` + `bytemuck` cast

`AccountInfo::data` is a `Rc<RefCell<&'a mut [u8]>>`. To get a writable slice out, you call `try_borrow_mut_data()`. From `programs/openhl-core/src/lib.rs:147–148`:

```rust
let mut data = market_ai.try_borrow_mut_data()?;
let market: &mut Market = bytemuck::from_bytes_mut(&mut data[..Market::LEN]);
```

Two operations:

1. **`try_borrow_mut_data()`** — fallible because the `RefCell` may already be borrowed. The error case is `ProgramError::AccountBorrowFailed`. This would happen if the same `AccountInfo` were borrowed mutably elsewhere in the call stack (e.g., a CPI handler re-entering with the same account). For a leaf write like ours it never fails in practice — but using the `?` operator makes the program correct under any future use that might double-borrow.

2. **`bytemuck::from_bytes_mut::<Market>(buf)`** — a pointer cast from `&mut [u8]` to `&mut Market`. This is safe *only* because `Market` is `Pod`: all-bits-valid, no padding, `repr(C)`. We verified the buffer is exactly `Market::LEN` bytes back at the size check (§2.3), so the cast is well-defined. The `&mut [u8]` becomes a `&mut Market` view over the same bytes — no copy, no allocation, just type reinterpretation.

After the cast, we write each field by name (`lib.rs:150–159`):

```rust
market.discriminator = MARKET_DISCRIMINATOR;
market.version = Market::VERSION;
market.bump = 0;
market._pad0 = [0u8; 6];
market.authority = authority;
market.base_mint = base_mint;
market.quote_mint = quote_mint;
market.tick_size = tick_size;
market.lot_size = lot_size;
market._reserved = [0u8; 128];
```

These writes happen *in place* in the account's data buffer. There is no "save" call. When `process_instruction` returns `Ok(())`, the loader sees the modified bytes and commits them to the ledger as part of the transaction.

Notice the explicit `_pad0` and `_reserved` zero-writes. They're not required (the bytes were already zero from System's allocator, and bytemuck doesn't add padding because we did) — but writing them anyway makes the code robust if the account were ever reused with non-zero padding from earlier state. For a brand-new account this is paranoia. For a `realloc`'d account it would matter.

**What Anchor hides:** Anchor's typed wrapper exposes `account.fieldname = value;` directly, no `try_borrow_mut_data` call needed. The wrapper holds the `RefMut` internally and flushes back to the account on `Drop`. It also writes the 8-byte discriminator for you on `init` — at the cost of every account having Anchor's own discriminator format (the first 8 bytes of `sha256("account:TypeName")`) rather than something human-readable like our `MARKET\0\0`.

> **Exercise §2.4.** Change `market.discriminator = MARKET_DISCRIMINATOR;` to write a single byte at offset 0 instead (e.g., `data[0] = 0x42;`). What error code do you get when you next run `init-market`? Why is it that error and not a corruption?

---

## §2.5  The client side — `Assign` + `Initialize` in one transaction

The account Chapter 1 created is owned by System. Our program can't write to it yet — the owner check would fail. To take ownership we need a `System::Assign` instruction, signed by the market keypair itself.

From `scripts/init-market/src/main.rs:117–146`:

```rust
// (1) System::Assign — transfer ownership to our program.
let assign_ix = system_instruction::assign(&market.pubkey(), &program_id);

// (2) openhl-core::Initialize — see programs/openhl-core/src/lib.rs.
let mut init_data = Vec::with_capacity(1 + 32 + 32 + 32 + 8 + 8);
init_data.push(0u8); // tag = Initialize
init_data.extend_from_slice(authority.as_ref());
init_data.extend_from_slice(base_mint.as_ref());
init_data.extend_from_slice(quote_mint.as_ref());
init_data.extend_from_slice(&cli.tick_size.to_le_bytes());
init_data.extend_from_slice(&cli.lot_size.to_le_bytes());

let init_ix = Instruction {
    program_id,
    accounts: vec![AccountMeta::new(market.pubkey(), false)],
    data: init_data,
};

let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
let tx = Transaction::new_signed_with_payer(
    &[assign_ix, init_ix],
    Some(&payer.pubkey()),
    &[&payer, &market],
    blockhash,
);
```

Three things worth pausing on.

**Why `Assign` needs the market to sign.** Open `solana-system-interface-1.0.0/src/instruction.rs:621–628`:

```rust
pub fn assign(pubkey: &Pubkey, owner: &Pubkey) -> Instruction {
    let account_metas = vec![AccountMeta::new(*pubkey, true)];
    // ...
}
```

The `true` on `AccountMeta::new(*pubkey, true)` means **signer required**. Just like `CreateAccount` in Chapter 1, the runtime demands proof that whoever currently controls this account (the keypair that owns the pubkey) consents to the ownership change. Otherwise anyone could "steal" any rent-exempt System-owned account by reassigning it to a program they control.

**Why the two instructions are in one transaction.** Transactions are atomic: either all instructions commit or none do. By bundling `[Assign, Initialize]` in a single transaction, there is no observable state where openhl-core owns an uninitialized 256-zero-byte market. From the outside the account flips directly from "System-owned + zero data" to "openhl-core-owned + initialized data." This matters because in later chapters, the same atomicity will protect us from partial-update bugs where someone reads a half-initialized account.

**Why `AccountMeta::new(market.pubkey(), false)` for the init instruction.** The market account *is* writable (we'll mutate its data) — but it doesn't need to sign the *Initialize* instruction. The signature requirement is per-instruction, not per-transaction. Assign needs the market keypair signature (the System program enforces this); Initialize does not (our program enforces only an owner check, not a signer check). Different security models, different `is_signer` flags.

The `&[&payer, &market]` at the bottom lists transaction-level signers. The transaction collects them once; each instruction's `AccountMeta` then declares which of those signers are required for that specific instruction.

> **Exercise §2.5.** Run `init-market` against `solana-test-validator` (after deploying `openhl_core.so`). Re-run `init-market` against the *same* market account. What error do you get on the second run? Trace it back: which check in `process_initialize` rejects you?

---

## §2.6  What `#[program]` and `#[derive(Accounts)]` actually generate

In Chapter 1's §1.5 we walked through `#[account(init, ...)]`. This chapter the equivalent expansion is larger — the entire `#[program]` + `#[derive(Accounts)]` pair. Pulling it together:

| Anchor does | We spelled it out as |
|---|---|
| generates the `entrypoint!` invocation | `lib.rs:25–26` |
| generates a `process_instruction` that decodes the 8-byte discriminator | `lib.rs:38–48` (we use a 1-byte tag) |
| generates per-handler dispatch (one match arm per `#[program]` fn) | `lib.rs:42–48` |
| deserializes accounts into the typed `Accounts` struct, enforcing each constraint (`#[account(mut)]`, `#[account(signer)]`, etc.) | `lib.rs:79–117` (owner check, size check, already-init check) |
| asserts `account.owner == program_id` for every `Account<'info, T>` | `lib.rs:87–94` |
| deserializes instruction data into the handler's argument struct via Borsh | `lib.rs:119–135` (manual byte decode) |
| calls your handler function with the typed args | `lib.rs:65–162` (our handler is `process_initialize`) |
| serializes the modified `Account<'info, T>` back into the account data on `Drop` | `lib.rs:147–159` (we write in place) |
| converts any returned `Result<(), Error>` into the loader's `u64` exit code | inherited from `entrypoint!` itself |

Eight responsibilities. Anchor handles all of them via macro generation; we handled them in ~130 lines of Rust. Neither approach is wrong. The point is that **every one of those responsibilities exists** — the macro hides them, but they don't go away.

When an Anchor program misbehaves — wrong account passed, signer not enforced, discriminator collision, unexpected serialization layout — you debug it by mentally re-tracing this list and asking which step went wrong. Knowing the list is the difference between debugging Anchor confidently and guessing.

---

## §2.7  Recap + verify yourself

### Recap diagram

```
Transaction
┌────────────────────────────────────────────────────────────────┐
│                                                                │
│  Instruction[0]: System::Assign                                │
│    accounts: [market (WRITE, SIGNER)]                          │
│    data:     SystemInstruction::Assign { owner: program_id }   │
│    effect:   market.owner: System → openhl_core                │
│                                                                │
│  Instruction[1]: openhl_core::Initialize                       │
│    accounts: [market (WRITE)]                                  │
│    data:     [tag=0, authority, base_mint, quote_mint,         │
│               tick_size, lot_size]                             │
│    effect:   market.data[0..256] = initialized Market layout   │
│                                                                │
│  signers: [payer, market]                                      │
│                                                                │
└────────────────────────────────────────────────────────────────┘
                              │
                              ▼
   atomic commit: either both instructions apply, or neither
```

### Three things to verify yourself

1. **Discriminator flipped.** After running `init-market`, the hex dump should start with `4d 41 52 4b 45 54 00 00` (= "MARKET\0\0"). Run `init-market` and confirm. Compare against Chapter 1's all-zero output — these eight bytes are the entire visible result of writing a Solana program.
2. **Owner changed.** Run `solana account <market_pubkey>` after `init-market`. The `Owner` line should now show your deployed `openhl-core` program ID, not `11111111111111111111111111111111`. The `Assign` instruction did this; the `Initialize` instruction depended on it.
3. **Re-run rejected.** Run `init-market` a second time against the same market. The transaction should fail. Trace the on-chain logs (`solana logs --include-failed`) and find the `initialize: market already initialized` message from [`lib.rs:114`](../../programs/openhl-core/src/lib.rs#L114). The already-initialized check at [`lib.rs:111–117`](../../programs/openhl-core/src/lib.rs#L111) is what rejected you.

---

## Hook into Chapter 3

You can now create accounts and own them. But you cannot yet *create accounts whose addresses are derived from your program* — every account in Chapters 1 and 2 was identified by an ad-hoc keypair generated client-side. That works for single-account demos and breaks for everything else: how do you find the market account for a given `(base_mint, quote_mint)` pair next time without storing the keypair somewhere off-chain? How does a user's position account stay tied to their wallet without you tracking the mapping in a database?

The answer is Program-Derived Addresses (PDAs) — pubkeys mathematically derived from seeds + your program ID, with no corresponding private key. Chapter 3 walks the derivation by hand, shows how `invoke_signed` lets your program "sign" for a PDA it owns, and replaces the `Keypair::new()` from Chapter 1 with a `find_program_address(&[b"market", base_mint.as_ref(), quote_mint.as_ref()], program_id)` derivation.

When Chapter 3 finishes, the same market will live at a *predictable* address. Any client that knows the base and quote mints can recompute it without external state — which is what makes Solana programs composable.
