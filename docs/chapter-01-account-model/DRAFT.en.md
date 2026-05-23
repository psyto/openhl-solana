# Chapter 1 — The Account Model from the Bytes Up

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs), [`scripts/allocate-market/src/main.rs`](../../scripts/allocate-market/src/main.rs).
> Tested against: solana-sdk 2.3.1, solana-rent 2.2.1, solana-account 2.2.1, solana-system-interface 1.0.0.

---

## §1.0  Framing

Most Solana tutorials start with Anchor. You write a `#[derive(Accounts)]` struct, sprinkle some `#[account(init, payer = signer, space = 8 + ...)]` attributes, and a few macros later you have a working program. The macros are seductive, but they collapse the entire Solana runtime model into a single attribute line. When the macro picks the wrong default, you don't know what to look at — because you never saw what the macro was doing on your behalf.

This chapter is the antidote. We will:

1. Open `solana-account` and read the five fields of an `Account`.
2. Compute a rent-exempt balance by hand and match it against the runtime.
3. Call the System program directly to allocate a 256-byte account on a local validator.
4. Hex-dump the raw bytes and identify every byte against our own layout.
5. Enumerate exactly what `#[account(init, ...)]` would have done for us.

By the end you will be able to point at any byte in any Solana account and say what it means, where it came from, and which program is allowed to change it. That is the foundation everything else in this track builds on.

The worked example is the smallest useful version of a real artifact: an empty `Market` account for our HL-style perp DEX. It is empty because the System program owns it after creation, and System has no instruction for "write arbitrary bytes." That gap is the hook into Chapter 2.

---

## §1.1  The five fields of an account

Open `solana-account-2.2.1/src/lib.rs:44–56`:

```rust
#[repr(C)]
// ...
pub struct Account {
    /// lamports in the account
    pub lamports: u64,
    /// data held in this account
    pub data: Vec<u8>,
    /// the program that owns this account. If executable, the program that loads this account.
    pub owner: Pubkey,
    /// this account's data contains a loaded program (and is now read-only)
    pub executable: bool,
    /// the epoch at which this account will next owe rent
    pub rent_epoch: Epoch,
}
```

That's the entire definition. Five fields. There is no `slot`, no `version`, no `nonce`, no `storage_root`. The Solana runtime stores nothing else *about* an account — everything else lives *inside* the `data` field as opaque bytes.

Let's take them one at a time.

**`lamports: u64`** — the account's balance, in lamports (1 SOL = 10⁹ lamports). Any program may *increase* an account's lamports (just transfer in). Only the account's **owner** program may *decrease* them. This is a runtime invariant enforced by the loader; if your program tries to debit an account it doesn't own, the transaction fails before your code returns.

**`data: Vec<u8>`** — the account's storage. To the runtime, this is opaque: just bytes, length capped at 10 MB. Programs interpret these bytes however they like (Anchor with Borsh, our code with `bytemuck`, raw shaders with whatever they please). Only the owner program may write to `data`. Other programs can read it.

**`owner: Pubkey`** — the public key of the program allowed to mutate `lamports` (downward) and `data`. For a wallet, this is the System program. For an SPL Token account, the SPL Token program. For our `Market` account in Chapter 2 onward, our own program. The owner is set once at creation (via System's `CreateAccount` or `Assign`) and changes only by an explicit `Assign` instruction issued by the current owner.

**`executable: bool`** — true if `data` contains a loaded BPF program, false otherwise. Once `true`, the account becomes read-only and can no longer be written to, ever. This is how immutability works in Solana: an account flips to executable, and the runtime refuses every subsequent write.

**`rent_epoch: Epoch`** — historical baggage. In early Solana, the runtime periodically charged rent against accounts based on size; this field tracked when the next charge was due. Rent collection was effectively disabled in favor of strict rent-exemption (see §1.2), so this field exists but no longer means much in practice. You will see it in `solana account <pubkey>` output as a number that doesn't change.

**What the SDK hides:** When you write Anchor and declare `pub user_data: Account<'info, UserData>`, you receive a typed view that *parses* the `data` field for you. The other four fields — `lamports`, `owner`, `executable`, `rent_epoch` — are still there, accessible via the underlying `AccountInfo`, but the type hints you toward only one of them. Most developers go their entire Anchor career without explicitly touching `owner`, despite owner-checking being one of the most common security bugs in Solana programs.

> **Exercise §1.1.** Pick any SPL token account address (your own USDC, for example), then run:
> ```
> solana account <pubkey> --output json
> ```
> Identify each of the five fields in the JSON output. What is `owner`? What does `data` look like base64-encoded? Is `executable` what you'd expect?

---

## §1.2  Rent and rent exemption

Open `solana-rent-2.2.1/src/lib.rs:32–45`:

```rust
#[repr(C)]
// ...
pub struct Rent {
    /// Rental rate in lamports/byte-year.
    pub lamports_per_byte_year: u64,

    /// Amount of time (in years) a balance must include rent for the account to
    /// be rent exempt.
    pub exemption_threshold: f64,

    /// The percentage of collected rent that is burned.
    pub burn_percent: u8,
}
```

And the formula itself, `lib.rs:93–97`:

```rust
pub fn minimum_balance(&self, data_len: usize) -> u64 {
    let bytes = data_len as u64;
    (((ACCOUNT_STORAGE_OVERHEAD + bytes) * self.lamports_per_byte_year) as f64
        * self.exemption_threshold) as u64
}
```

The rule: an account is **rent-exempt** if its lamport balance is at least enough to cover two years of rent at the current rate. If you try to create an account below this threshold, the transaction fails. If you reduce a live account's balance below the threshold (e.g. transferring lamports out), the same.

The `ACCOUNT_STORAGE_OVERHEAD` constant at `lib.rs:70` is the key surprise:

```rust
pub const ACCOUNT_STORAGE_OVERHEAD: u64 = 128;
```

Every account, no matter how small its `data` field, is *billed* for 128 extra bytes. This covers the runtime's own bookkeeping — the metadata fields from §1.1, indexing overhead, and so on. So a "zero-byte" account costs the same rent as a 128-byte account.

For our `Market` (256 bytes of data), the calculation:

```
minimum_balance = (128 + 256) × lamports_per_byte_year × 2.0
                = 384 × 3480 × 2
                = 2,672,640 lamports
                ≈ 0.00267 SOL
```

(`lamports_per_byte_year` defaults to ~3480 — see `lib.rs:54`, derived from "$0.01 per megabyte day".)

Our script asks the RPC for this number rather than computing it locally:

```rust
// scripts/allocate-market/src/main.rs:56–58
let rent_lamports = client
    .get_minimum_balance_for_rent_exemption(Market::LEN)
    .context("fetch rent-exempt minimum")?;
```

This is a round-trip to the validator. Why? Because `Rent` is a *sysvar* — its values are not hard-coded into your binary, they live on-chain and can (in principle) be changed by a future runtime update. Asking the RPC is the only way to get the value that *this* cluster will enforce.

**What the SDK hides:** Anchor's `#[account(init, ..., space = 8 + 248)]` reads the `space` argument, calls `Rent::get()?.minimum_balance(space)` *inside the program* (no RPC needed, because the sysvar is accessible on-chain), and uses that as the lamport amount for `create_account`. The `8` you always see in `space = 8 + ...` is Anchor's own discriminator overhead — eight extra bytes prepended to every account so Anchor can identify the type at runtime. Our `Market` already has its own 8-byte discriminator at offset 0, so we are paying the same 8-byte tax, just visibly.

> **Exercise §1.2.** Compute `minimum_balance` by hand for a 0-byte account, a 256-byte account, and a 10,000-byte account (the upper limit Anchor encourages). Then verify against the cluster:
> ```
> solana rent <bytes>
> ```
> Where does the 128 of overhead actually live? (Hint: read the field-name comment at `lib.rs:67–70`.)

---

## §1.3  Allocating an account from the System program

The System program (`11111111111111111111111111111111`) is the only program that can bring an account into existence. It owns every wallet, it is the only thing that can move lamports around freely, and it is the source of every other program's first account.

Open `solana-system-interface-1.0.0/src/instruction.rs:80–95`:

```rust
pub enum SystemInstruction {
    /// Create a new account
    ///
    /// # Account references
    ///   0. `[WRITE, SIGNER]` Funding account
    ///   1. `[WRITE, SIGNER]` New account
    CreateAccount {
        /// Number of lamports to transfer to the new account
        lamports: u64,
        /// Number of bytes of memory to allocate
        space: u64,
        /// Address of program that will own the new account
        owner: Pubkey,
    },
    // ...
}
```

`CreateAccount` does three things in one syscall:

1. **Transfer** `lamports` from the funding account to the new account.
2. **Allocate** `space` bytes for the new account's `data` field.
3. **Assign** `owner` as the new account's owner.

The doc comment at `instruction.rs:9–12` spells out the same decomposition:

> Account creation typically involves three steps: `allocate` space, `transfer` lamports for rent, `assign` to its owning program. The `create_account` function does all three at once.

The constructor we call is at `instruction.rs:406–426`:

```rust
pub fn create_account(
    from_pubkey: &Pubkey,
    to_pubkey: &Pubkey,
    lamports: u64,
    space: u64,
    owner: &Pubkey,
) -> Instruction {
    let account_metas = vec![
        AccountMeta::new(*from_pubkey, true),
        AccountMeta::new(*to_pubkey, true),
    ];
    // ...
}
```

Note the `true` on both `AccountMeta::new` calls — that flag means **signer required**. Both the funding account and the new account must sign the transaction. This is jarring the first time you see it: why does an account that doesn't exist yet need to sign?

The answer is grief protection. If only the payer had to sign, anyone could pay 0.003 SOL to create an account at *your* address, set its owner to a program *they* control, and bind that address to junk before you ever got there. By requiring the new account to sign, the runtime forces a proof that *you* control the private key for the new address. The lamports come from the payer; the signature on the new account comes from whoever owns the keypair that will identify it.

Our script does this at `scripts/allocate-market/src/main.rs:79–85`:

```rust
let blockhash = client.get_latest_blockhash().context("fetch blockhash")?;
let tx = Transaction::new_signed_with_payer(
    &[create_ix],
    Some(&payer.pubkey()),
    &[&payer, &market],
    blockhash,
);
```

`&[&payer, &market]` — two signers. The `market` is a `Keypair::new()` (line 52), generated locally; we hold its private key just long enough to sign this one transaction. After that we never use it again — the account is identified by its public key, and from this transaction forward, only the **owner program** (System, for now) can change anything about it.

In Chapter 3 we will replace the random `Keypair::new()` with a Program-Derived Address (PDA), at which point the new-account signature is provided by the program itself via `invoke_signed`. Same model, different signer.

**What the SDK hides:** Anchor's `#[account(init, payer = payer, space = ...)]` expands (roughly) to:

```rust
// 1. Compute the rent-exempt minimum at runtime via Rent::get().
let rent = Rent::get()?.minimum_balance(space);

// 2. Construct the System CreateAccount instruction.
let ix = system_instruction::create_account(
    payer.key,
    account.key,
    rent,
    space as u64,
    program_id,                   // <-- our program, not System!
);

// 3. Invoke via CPI with `invoke_signed` if `account` is a PDA, else `invoke`.
invoke_signed(&ix, &[payer, account, system_program], &[seeds_with_bump])?;

// 4. Write Anchor's 8-byte discriminator to data[0..8].
account.try_borrow_mut_data()?[..8].copy_from_slice(&MyType::DISCRIMINATOR);

// 5. Zero-init the rest of data (because Anchor's account type is repr(C) Pod-like).
```

Five steps. Behind a single attribute. None of them are wrong — Anchor's choices are reasonable defaults — but every one of them is a decision you didn't make.

> **Exercise §1.3.** The new account is allocated owned by the program that called `create_account`. In our script, what owner does the new account end up with? Look at `main.rs:69` and `main.rs:71–77`. Why is that the right choice for Chapter 1 specifically?

---

## §1.4  Reading the bytes

The script's last act is to fetch the account back and dump it. Here is the dump table from `main.rs:113–125`:

```rust
fn dump_market_bytes(data: &[u8]) {
    let regions: &[(usize, usize, &str)] = &[
        (0, 8, "discriminator      [u8; 8]    expected: MARKET\\0\\0"),
        (8, 1, "version            u8"),
        (9, 1, "bump               u8"),
        (10, 6, "_pad0              [u8; 6]"),
        (16, 32, "authority          [u8; 32]"),
        (48, 32, "base_mint          [u8; 32]"),
        (80, 32, "quote_mint         [u8; 32]"),
        (112, 8, "tick_size          u64"),
        (120, 8, "lot_size           u64"),
        (128, 128, "_reserved          [u8; 128]"),
    ];
    // ...
}
```

These offsets are *not* invented — they come straight from the `Market` struct at `crates/state/src/lib.rs:43–56`:

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Market {
    pub discriminator: [u8; 8],   // 0..8
    pub version: u8,              // 8
    pub bump: u8,                 // 9
    pub _pad0: [u8; 6],           // 10..16
    pub authority: [u8; 32],      // 16..48
    pub base_mint: [u8; 32],      // 48..80
    pub quote_mint: [u8; 32],     // 80..112
    pub tick_size: u64,           // 112..120
    pub lot_size: u64,            // 120..128
    pub _reserved: [u8; 128],     // 128..256
}
```

Two things are worth pausing on.

**The `_pad0: [u8; 6]` field.** After `bump: u8` at offset 9, the next field is `authority: [u8; 32]`. A `[u8; 32]` has alignment 1, but our struct is `#[repr(C)]` and we want `tick_size: u64` further down to land on an 8-byte boundary so future code can read it as a `u64` without an unaligned access. The padding makes the next 8-byte-aligned offset land at 16 instead of 10. We declare it explicitly rather than letting the compiler insert hidden padding — because hidden padding would break `bytemuck::Pod`, which requires every byte of the struct to be initialized and accessible.

**`Pubkey` stored as `[u8; 32]`.** The doc comment at `crates/state/src/lib.rs:8–15` explains this: `solana_program::pubkey::Pubkey` does not implement `bytemuck::Pod` upstream, so to keep the layout `Pod`-safe we store the raw 32 bytes. This is also pedagogically honest — a `Pubkey` *is* 32 bytes; the type alias just gives them a name. Chapter 2's program will convert at the boundary.

When you run the script against a fresh `solana-test-validator`, expected output looks like:

```
rpc:            http://127.0.0.1:8899
payer:          7c5...QJZ
market pubkey:  E2k...A9M
space:          256 bytes
rent lamports:  2672640  (0.002673 SOL)

create_account signature: 5xH...t8N

account metadata:
  owner:        11111111111111111111111111111111
  lamports:     2672640
  executable:   false
  rent_epoch:   18446744073709551615
  data length:  256

account data (raw bytes, annotated against openhl_state::Market):

  0x0000  discriminator      [u8; 8]    expected: MARKET\0\0
          00 00 00 00 00 00 00 00
  0x0008  version            u8
          00
  0x0009  bump               u8
          00
  0x000a  _pad0              [u8; 6]
          00 00 00 00 00 00
  0x0010  authority          [u8; 32]
          00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
          00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
  ... (all zeros for 256 bytes)
```

A few things to stare at:

- `owner: 11111111111111111111111111111111` — the all-ones-in-base58 address is the System program. We asked for it explicitly at `main.rs:69`.
- `lamports: 2672640` — exactly the rent-exempt minimum we computed in §1.2. No more, no less.
- `executable: false` — this is a data account, not a program.
- `rent_epoch: 18446744073709551615` — that's `u64::MAX`. The runtime marks rent-exempt accounts with this sentinel value, effectively saying "never charge this one." This is the modern legacy of the rent system.
- `data: [0u8; 256]` — every byte zero. We never wrote anything; System program does not let anyone write to data fields it owns. The bytes are zero because the allocator zero-initializes them.

The discriminator at offset 0 is `00 00 00 00 00 00 00 00`. It is **not** `4d 41 52 4b 45 54 00 00` ("MARKET\0\0"). We allocated the right number of bytes, but no one has written `MARKET_DISCRIMINATOR` into them. That's the Chapter 2 job.

**What the SDK hides:** When you fetch an account through Anchor's typed `Account<'info, T>` interface, the framework reads `data[0..8]` and *compares* it against `T::DISCRIMINATOR`. If they don't match, you get an error before your code sees the account at all. This is invaluable safety — but it also means an account in the state we just created (correct size, all zeros) would fail Anchor's discriminator check and be invisible to typed code. The raw bytes are still there. Anchor just refuses to look.

> **Exercise §1.4.** Run the script against `solana-test-validator`. Open another terminal and run `solana account <market_pubkey>` for the market pubkey the script printed. Compare the output to the script's. They should agree on every field. Find one piece of information the `solana account` command shows that the script doesn't print, and one piece of information the script shows that `solana account` doesn't.

---

## §1.5  What `#[account(init, ...)]` actually does

We've now seen everything Anchor's most common attribute would have done for us. Pulling it together, here is the literal correspondence:

| Anchor does | Spelled out as |
|---|---|
| reads `space = N` from the attribute | `let space = N;` |
| calls `Rent::get()?.minimum_balance(space)` | §1.2 — our script uses the RPC variant `get_minimum_balance_for_rent_exemption` |
| constructs `system_instruction::create_account` | §1.3 — `main.rs:71–77` |
| invokes via `invoke_signed` (with PDA seeds) or `invoke` | omitted in our script (we sign client-side instead — chapter 2 introduces CPI) |
| writes `T::DISCRIMINATOR` to `data[0..8]` | omitted — bytes stay zero, as §1.4 shows |
| sets the new account's owner to the program ID | we set it to `system_program::ID` instead, on purpose |
| binds the typed account view to a Rust struct | we use raw `account.data: Vec<u8>` and a separate `Market` struct |

The macro is doing real work. It is not "just sugar." It picks defaults at every step — payer choice, rent calc, owner = program ID, discriminator = type ID, layout = Borsh-ish — that are right *most* of the time. When they are wrong (cross-program ownership, custom discriminators, byte-exact layouts for ZK verifiers), you need to know which line of the expansion is the one to override.

Three concentric layers, from inside out:

1. **`solana-program` syscalls** — the runtime ABI. `sol_invoke`, `sol_log`, `create_program_address`. Closest to the metal. You will rarely call these directly, but every higher abstraction eventually does.
2. **`solana-sdk` wrappers** — `Transaction::new_signed_with_payer`, `Account`, `Rent`, instruction constructors. Ergonomic, typed, no magic.
3. **`anchor-lang` macros** — `#[program]`, `#[derive(Accounts)]`, `#[account(...)]`. Maximum ergonomics, opinionated defaults, generated boilerplate. The deepest abstraction.

This track teaches the bottom two. If you understand them, you can debug the third when its defaults betray you.

---

## §1.6  Recap + verify yourself

### Recap diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                       Solana Account                            │
│                                                                 │
│   lamports     : u64       ← only owner program may decrement   │
│   data         : Vec<u8>   ← only owner program may write       │
│   owner        : Pubkey    ← set at creation, changed by Assign │
│   executable   : bool      ← one-way: false → true → forever ro │
│   rent_epoch   : u64       ← legacy; u64::MAX = rent-exempt     │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘

Account creation (System program):

  payer ──signs──┐
                 ▼
          CreateAccount { lamports, space, owner }
                 │
                 ▼
  new ──signs────┤  ← signature proves keypair control
                 ▼
       runtime: allocate `space` bytes of zeros,
                transfer `lamports` from payer to new,
                set new.owner = `owner`
```

### Three things to verify yourself

1. **Owner check.** After running the script, run `solana account <market_pubkey>`. Confirm the `Owner` line is `11111111111111111111111111111111` (System). Our script asks for this at [`main.rs:69`](../../scripts/allocate-market/src/main.rs#L69) and the runtime obeys.
2. **Rent exemption math.** Run `solana rent 256`. The output should match what the script printed for `rent lamports`. Both come from the same formula at [`solana-rent-2.2.1/src/lib.rs:93`](#) — one via local computation, one via RPC.
3. **Layout offsets.** Open [`crates/state/src/lib.rs:43–56`](../../crates/state/src/lib.rs#L43) and add up the field sizes by hand. Confirm `quote_mint` starts at byte 80 and `_reserved` ends at byte 256. Run `cargo test -p openhl-state` to have the compiler confirm it too — the `market_size_is_256_bytes` test at [`lib.rs:67–70`](../../crates/state/src/lib.rs#L67) would fail if any field changed size.

---

## Hook into Chapter 2

You now have an account on-chain. You know exactly what its five fields contain. You know that the data field is 256 zero bytes, that the System program owns those bytes, and that — because no System instruction writes arbitrary data — those bytes will stay zero forever unless ownership changes.

To take ownership, we need a Solana program of our own. Not an Anchor program: a program built from `entrypoint!`, `&[AccountInfo]`, and a hand-written instruction dispatcher. Chapter 2 builds that program, deploys it to the same validator, and uses it to (a) take ownership of the account and (b) write the `MARKET_DISCRIMINATOR` bytes at offset 0.

When the script for Chapter 2 finishes, the same hex dump will start with `4d 41 52 4b 45 54 00 00` instead of `00 00 00 00 00 00 00 00`. That eight-byte change is the entire visible result of building your first program — and the invisible result is that you will understand what every Anchor program does the moment it deserializes its accounts.
