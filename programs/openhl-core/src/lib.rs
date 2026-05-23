//! openhl-core — the openhl-solana on-chain program.
//!
//! Instructions:
//!   0  Initialize    — written for Chapter 2 (native program from scratch)
//!                     Takes ownership of a pre-allocated ad-hoc-keypair
//!                     market and writes its bytes. Requires a separate
//!                     System::Assign instruction from the client beforehand.
//!   1  CreateMarket  — written for Chapter 3 (PDAs)
//!                     One-shot: derives the market PDA from
//!                     [b"market", base_mint, quote_mint], CPIs to System
//!                     to allocate it with this program as owner, then
//!                     writes the layout. No prior Assign needed.
//!   2  Bench         — written for Chapter 4 (compute budget + heap)
//!                     Configurable workload that allocates a heap buffer
//!                     and iterates sha256 hashes, with sol_log_compute_units
//!                     bracketing each phase. Touches no accounts.
//!   3  CreateStats   — written for Chapter 5 (Sealevel parallelism)
//!                     One-shot creation of the program's singleton Stats
//!                     PDA at seeds [b"stats"]. The chapter uses Stats as
//!                     a worked example of write-shared state and why
//!                     adding it to a hot-path instruction would serialize
//!                     concurrent calls in the scheduler.
//!   4  BumpStats     — also Chapter 5
//!                     Increments market_count on the singleton Stats PDA.
//!                     Trivial body; the interesting thing is the
//!                     AccountMeta declaration (single WRITE on a singleton
//!                     pubkey), which is what Sealevel sees.
//!   5  CreateVault   — written for Chapter 6 (CPI internals)
//!                     For a given (market, mint) pair: derives the vault
//!                     token-account PDA and the vault-authority PDA,
//!                     CPIs to System::create_account (invoke_signed) to
//!                     allocate the token account, then CPIs to SPL Token
//!                     InitializeAccount3 (invoke) to set its mint+owner.
//!                     SPL Token instruction data is hand-rolled — no
//!                     spl-token crate dep, in keeping with the bytes-up
//!                     theme.
//!   6  Deposit       — also Chapter 6
//!                     Moves SPL tokens from user_token_account into
//!                     vault_token_account via SPL Token Transfer CPI.
//!                     Calls plain `invoke` because the user signs at the
//!                     outer transaction level and signer privilege
//!                     extends through to the SPL Token program.
//!
//! The entire program is one file on purpose. Splitting it into the usual
//! `instruction.rs` / `processor.rs` / `state.rs` modules buys nothing
//! before there are many instructions, and obscures the fact that a
//! Solana program is fundamentally `fn process(...)` plus an entrypoint.

#![allow(unexpected_cfgs)] // solana_program::entrypoint! gates on `target_os = "solana"`

use openhl_state::{Market, Stats, MARKET_DISCRIMINATOR, STATS_DISCRIMINATOR};
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    hash::hash as sha256,
    instruction::{AccountMeta, Instruction},
    log::sol_log_compute_units,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvar::{rent::Rent, Sysvar},
};
use solana_system_interface::{instruction as system_instruction, program as system_program};

/// PDA seed prefix for market accounts.
///
/// Full seed list: `[MARKET_SEED, base_mint.as_ref(), quote_mint.as_ref(), &[bump]]`.
/// The prefix is what makes the market address unambiguously a *market* and
/// not, say, a position account that happened to derive from the same mints.
pub const MARKET_SEED: &[u8] = b"market";

/// PDA seed for the program's singleton Stats account.
///
/// Single byte string, no per-instance suffix — there is exactly one Stats
/// PDA per program ID. This is the design choice Chapter 5 critiques: a
/// singleton write-shared account becomes a Sealevel scheduling bottleneck.
pub const STATS_SEED: &[u8] = b"stats";

/// PDA seed prefix for vault token accounts.
///
/// Full seed list: `[VAULT_SEED, market_pubkey.as_ref(), mint_pubkey.as_ref(), &[bump]]`.
/// One vault per (market, mint), so a market can have a base-asset vault and
/// a quote-asset vault that live at distinct PDAs.
pub const VAULT_SEED: &[u8] = b"vault";

/// PDA seed prefix for the per-market vault *authority*.
///
/// Full seed list: `[VAULT_AUTH_SEED, market_pubkey.as_ref(), &[bump]]`.
/// The vault-authority PDA is the SPL Token "owner" of every vault token
/// account belonging to a given market. Withdrawals require the program to
/// sign for this PDA via `invoke_signed`.
pub const VAULT_AUTH_SEED: &[u8] = b"vault_auth";

/// SPL Token program ID. Hardcoded so we don't pull in the spl-token crate
/// just for this constant.
pub const SPL_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Size of an SPL Token Account, per spl_token::state::Account::LEN.
const TOKEN_ACCOUNT_LEN: usize = 165;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

/// Top-level instruction dispatcher.
///
/// The first byte of `instruction_data` is the instruction tag; the rest
/// is the payload. There is no Borsh, no `try_from_slice`, no
/// `#[derive(BorshDeserialize)]`. Just bytes.
pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let (tag, payload) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    match *tag {
        0 => process_initialize(program_id, accounts, payload),
        1 => process_create_market(program_id, accounts, payload),
        2 => process_bench(payload),
        3 => process_create_stats(program_id, accounts, payload),
        4 => process_bump_stats(program_id, accounts, payload),
        5 => process_create_vault(program_id, accounts, payload),
        6 => process_deposit(accounts, payload),
        unknown => {
            msg!("unknown instruction tag: {}", unknown);
            Err(ProgramError::InvalidInstructionData)
        }
    }
}

/// Initialize payload layout (manually packed, little-endian):
///
/// ```text
///   [  0.. 32)  authority    [u8; 32]
///   [ 32.. 64)  base_mint    [u8; 32]
///   [ 64.. 96)  quote_mint   [u8; 32]
///   [ 96..104)  tick_size    u64 LE
///   [104..112)  lot_size     u64 LE
/// ```
const INITIALIZE_PAYLOAD_LEN: usize = 32 + 32 + 32 + 8 + 8;

/// Accounts:
///   0. `[WRITE]` market — must already be owned by `program_id` and
///      `data.len() == Market::LEN` and `data[0..8] == [0u8; 8]`.
fn process_initialize(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != INITIALIZE_PAYLOAD_LEN {
        msg!(
            "initialize: payload must be {} bytes, got {}",
            INITIALIZE_PAYLOAD_LEN,
            payload.len()
        );
        return Err(ProgramError::InvalidInstructionData);
    }

    let market_ai = accounts
        .first()
        .ok_or(ProgramError::NotEnoughAccountKeys)?;

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

    // (2) Size check. We will cast to `&mut Market` via bytemuck; the cast
    // panics if the buffer is too small, so we explicitly reject mismatches
    // with a clean error code instead.
    if market_ai.data_len() != Market::LEN {
        msg!(
            "initialize: market data_len {} != {}",
            market_ai.data_len(),
            Market::LEN
        );
        return Err(ProgramError::InvalidAccountData);
    }

    // (3) Already-initialized check. Discriminator at offset 0 is the
    // contract; if it's set, this account is already a live Market and we
    // must not overwrite it.
    {
        let data = market_ai.try_borrow_data()?;
        if data[..8] != [0u8; 8] {
            msg!("initialize: market already initialized");
            return Err(ProgramError::AccountAlreadyInitialized);
        }
    }

    // (4) Decode the payload.
    let mut authority = [0u8; 32];
    let mut base_mint = [0u8; 32];
    let mut quote_mint = [0u8; 32];
    authority.copy_from_slice(&payload[0..32]);
    base_mint.copy_from_slice(&payload[32..64]);
    quote_mint.copy_from_slice(&payload[64..96]);
    let tick_size = u64::from_le_bytes(
        payload[96..104]
            .try_into()
            .expect("slice is 8 bytes by construction"),
    );
    let lot_size = u64::from_le_bytes(
        payload[104..112]
            .try_into()
            .expect("slice is 8 bytes by construction"),
    );

    // (5) Validate. Zero tick or lot would make every order invalid; reject
    // at the boundary rather than letting the bad state propagate.
    if tick_size == 0 || lot_size == 0 {
        msg!("initialize: tick_size and lot_size must be non-zero");
        return Err(ProgramError::InvalidInstructionData);
    }

    // (6) Write. We hold a mutable borrow of the account's data and view
    // it as a `Market` via `bytemuck::from_bytes_mut`. The pointer cast is
    // safe because `Market` is `Pod` and we verified the buffer size above.
    let mut data = market_ai.try_borrow_mut_data()?;
    let market: &mut Market = bytemuck::from_bytes_mut(&mut data[..Market::LEN]);

    market.discriminator = MARKET_DISCRIMINATOR;
    market.version = Market::VERSION;
    market.bump = 0; // not a PDA in chapter 2; PDAs arrive in chapter 3
    market._pad0 = [0u8; 6];
    market.authority = authority;
    market.base_mint = base_mint;
    market.quote_mint = quote_mint;
    market.tick_size = tick_size;
    market.lot_size = lot_size;
    market._reserved = [0u8; 128];

    msg!("market initialized");
    Ok(())
}

// =============================================================================
// CreateMarket — PDA-based market creation (Chapter 3).
// =============================================================================

/// Same payload layout as `Initialize`. The PDA is derived from a fixed
/// subset of the payload fields (`base_mint`, `quote_mint`), which means
/// `tick_size` and `lot_size` can change between markets on the same
/// `(base_mint, quote_mint)` pair only by also changing the seed prefix —
/// a constraint we choose, not a runtime requirement.
const CREATE_MARKET_PAYLOAD_LEN: usize = INITIALIZE_PAYLOAD_LEN;

/// Accounts:
///   0. `[WRITE, SIGNER]` payer            — funds rent + tx fee
///   1. `[WRITE]`         market           — the PDA, currently uninitialized
///   2. `[]`              system_program   — required for the create_account CPI
fn process_create_market(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_MARKET_PAYLOAD_LEN {
        msg!(
            "create_market: payload must be {} bytes, got {}",
            CREATE_MARKET_PAYLOAD_LEN,
            payload.len()
        );
        return Err(ProgramError::InvalidInstructionData);
    }

    let payer_ai = accounts
        .first()
        .ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts
        .get(1)
        .ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts
        .get(2)
        .ok_or(ProgramError::NotEnoughAccountKeys)?;

    // (1) Payer is a signer (we'll move lamports from them via the System CPI).
    if !payer_ai.is_signer {
        msg!("create_market: payer must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }

    // (2) The supplied system_program account *is* the System program.
    // Anyone could pass any account in slot 2; the only reason the CPI
    // would later succeed is that we verify it here. (Anchor's
    // `Program<'info, System>` does exactly this check.)
    if system_ai.key != &system_program::ID {
        msg!("create_market: account[2] is not the System program");
        return Err(ProgramError::IncorrectProgramId);
    }

    // (3) Decode payload (same layout as Initialize).
    let mut authority = [0u8; 32];
    let mut base_mint = [0u8; 32];
    let mut quote_mint = [0u8; 32];
    authority.copy_from_slice(&payload[0..32]);
    base_mint.copy_from_slice(&payload[32..64]);
    quote_mint.copy_from_slice(&payload[64..96]);
    let tick_size = u64::from_le_bytes(
        payload[96..104]
            .try_into()
            .expect("slice is 8 bytes by construction"),
    );
    let lot_size = u64::from_le_bytes(
        payload[104..112]
            .try_into()
            .expect("slice is 8 bytes by construction"),
    );

    if tick_size == 0 || lot_size == 0 {
        msg!("create_market: tick_size and lot_size must be non-zero");
        return Err(ProgramError::InvalidInstructionData);
    }

    // (4) Derive the expected PDA from the payload fields + program_id, and
    // verify the caller passed us the right account. This is what binds
    // a `(base_mint, quote_mint)` pair to a single, predictable address.
    //
    // If the caller passed a *different* writable account, the create_account
    // CPI below would still try to operate on it — and would fail in one of
    // two ways: either the account is already System-owned with the wrong
    // address (PDA hash mismatch when invoke_signed builds the seeds), or
    // it's a stranger's account we have no right to touch. Catching it
    // here with a clean error is much better than reading a CPI failure.
    let (expected_pda, bump) = Pubkey::find_program_address(
        &[MARKET_SEED, base_mint.as_ref(), quote_mint.as_ref()],
        program_id,
    );
    if market_ai.key != &expected_pda {
        msg!(
            "create_market: passed market {} != derived PDA {}",
            market_ai.key,
            expected_pda
        );
        return Err(ProgramError::InvalidSeeds);
    }

    // (5) Allocate via CPI to System. We sign for the PDA by passing the
    // seeds (including the bump) to invoke_signed; the runtime hashes them
    // and confirms they derive `market_ai.key`, then accepts our program
    // as the signer for that account.
    //
    // `Rent::get()` is the on-chain version of the RPC call we used in
    // Chapter 1 — same formula (data_len + 128 bytes overhead × rate × 2y),
    // computed from the sysvar instead of asked over the network.
    let rent = Rent::get()?.minimum_balance(Market::LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        market_ai.key,
        rent,
        Market::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), market_ai.clone(), system_ai.clone()],
        &[&[MARKET_SEED, base_mint.as_ref(), quote_mint.as_ref(), &[bump]]],
    )?;

    // (6) Now the account exists, is sized correctly, has zero data, and is
    // owned by us. Write the Market layout. Same logic as Initialize — we
    // could call into process_initialize here, but inlining keeps the chapter
    // self-contained and the CU savings are real.
    let mut data = market_ai.try_borrow_mut_data()?;
    let market: &mut Market = bytemuck::from_bytes_mut(&mut data[..Market::LEN]);

    market.discriminator = MARKET_DISCRIMINATOR;
    market.version = Market::VERSION;
    market.bump = bump;
    market._pad0 = [0u8; 6];
    market.authority = authority;
    market.base_mint = base_mint;
    market.quote_mint = quote_mint;
    market.tick_size = tick_size;
    market.lot_size = lot_size;
    market._reserved = [0u8; 128];

    msg!("market created at PDA (bump {})", bump);
    Ok(())
}

// =============================================================================
// Bench — instrumented workload for measuring compute units (Chapter 4).
// =============================================================================

/// Payload layout (8 bytes, little-endian):
///   [0..4)  rounds      u32   — number of sha256 iterations
///   [4..8)  heap_bytes  u32   — Vec<u8> allocation size, exercises the
///                               default 32 KiB bump allocator
const BENCH_PAYLOAD_LEN: usize = 8;

/// Accounts: none. Bench is a pure-compute instruction that touches no state.
///
/// What this exists to demonstrate:
///   1. CU is a real, observable, per-syscall cost. The `sol_log_compute_units`
///      brackets around each phase let you read the cost of allocation vs.
///      hashing vs. epilogue in the validator log.
///   2. The default heap is 32 KiB and uses a *bump* allocator that never
///      frees. Drop the Vec, and the bytes stay claimed until the program
///      exits. Allocate beyond 32 KiB and the allocator returns null — Rust's
///      global-alloc handler then aborts the program.
///   3. Linear loops over non-trivial work blow the default 200 KCU budget
///      fast. Raising it requires a ComputeBudgetInstruction in the same
///      transaction.
fn process_bench(payload: &[u8]) -> ProgramResult {
    if payload.len() != BENCH_PAYLOAD_LEN {
        msg!(
            "bench: payload must be {} bytes, got {}",
            BENCH_PAYLOAD_LEN,
            payload.len()
        );
        return Err(ProgramError::InvalidInstructionData);
    }

    let rounds = u32::from_le_bytes(payload[0..4].try_into().expect("4 bytes"));
    let heap_bytes = u32::from_le_bytes(payload[4..8].try_into().expect("4 bytes"));

    msg!("bench: start (rounds={}, heap_bytes={})", rounds, heap_bytes);
    sol_log_compute_units();

    // Phase A — heap allocation. The Vec lives until the end of the function,
    // when it's dropped — but the bump allocator's `dealloc` is a no-op
    // (see solana_program_entrypoint::BumpAllocator), so the bytes stay
    // reserved until the program exits.
    let mut buf = vec![0u8; heap_bytes as usize];
    msg!("bench: after heap alloc ({} bytes)", buf.len());
    sol_log_compute_units();

    // Phase B — hash loop. Each iteration hashes the buffer and stuffs the
    // 32-byte digest back into the front, ensuring no compiler can DCE the
    // computation. sha256 is a syscall on BPF, so this CU cost is meaningful
    // and stable across runs.
    for i in 0..rounds {
        let digest = sha256(&buf);
        let bytes = digest.to_bytes();
        // Re-feed the digest into the buffer head so the next hash sees
        // different input. Use index arithmetic to also exercise bounds checks.
        let copy_len = bytes.len().min(buf.len());
        buf[..copy_len].copy_from_slice(&bytes[..copy_len]);
        // Stir the loop counter in too, so equivalent-size buffers don't
        // produce identical digests.
        if !buf.is_empty() {
            buf[0] ^= i as u8;
        }
    }

    msg!("bench: after {} hash rounds", rounds);
    sol_log_compute_units();

    Ok(())
}

// =============================================================================
// CreateStats + BumpStats — singleton write-shared state (Chapter 5).
// =============================================================================
//
// The Stats PDA exists per program ID (single account, derived from
// `[STATS_SEED]` + program_id). It deliberately serves as a worked
// counter-example to parallelism: any instruction that writes to Stats
// joins the same Sealevel lock queue, regardless of which market the
// caller is otherwise touching. Chapter 5 walks the AccountMeta of these
// instructions and explains why a singleton write-shared account is the
// fastest path to single-threading a Solana program.

const CREATE_STATS_PAYLOAD_LEN: usize = 0;
const BUMP_STATS_PAYLOAD_LEN: usize = 0;

/// Accounts:
///   0. `[WRITE, SIGNER]` payer
///   1. `[WRITE]`         stats           — singleton Stats PDA
///   2. `[]`              system_program
fn process_create_stats(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_STATS_PAYLOAD_LEN {
        msg!("create_stats: payload must be empty");
        return Err(ProgramError::InvalidInstructionData);
    }

    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let stats_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        msg!("create_stats: payer must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if system_ai.key != &system_program::ID {
        msg!("create_stats: account[2] is not the System program");
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected_pda, bump) = Pubkey::find_program_address(&[STATS_SEED], program_id);
    if stats_ai.key != &expected_pda {
        msg!(
            "create_stats: passed stats {} != derived PDA {}",
            stats_ai.key,
            expected_pda
        );
        return Err(ProgramError::InvalidSeeds);
    }

    let rent = Rent::get()?.minimum_balance(Stats::LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        stats_ai.key,
        rent,
        Stats::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), stats_ai.clone(), system_ai.clone()],
        &[&[STATS_SEED, &[bump]]],
    )?;

    let mut data = stats_ai.try_borrow_mut_data()?;
    let stats: &mut Stats = bytemuck::from_bytes_mut(&mut data[..Stats::LEN]);
    stats.discriminator = STATS_DISCRIMINATOR;
    stats.bump = bump;
    stats._pad0 = [0u8; 7];
    stats.market_count = 0;
    stats._reserved = [0u8; 32];

    msg!("stats created at PDA (bump {})", bump);
    Ok(())
}

/// Accounts:
///   0. `[WRITE]` stats — the singleton Stats PDA
fn process_bump_stats(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != BUMP_STATS_PAYLOAD_LEN {
        msg!("bump_stats: payload must be empty");
        return Err(ProgramError::InvalidInstructionData);
    }

    let stats_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;

    if stats_ai.owner != program_id {
        msg!("bump_stats: stats owner mismatch");
        return Err(ProgramError::IncorrectProgramId);
    }
    if stats_ai.data_len() != Stats::LEN {
        msg!(
            "bump_stats: stats data_len {} != {}",
            stats_ai.data_len(),
            Stats::LEN
        );
        return Err(ProgramError::InvalidAccountData);
    }

    let mut data = stats_ai.try_borrow_mut_data()?;
    let stats: &mut Stats = bytemuck::from_bytes_mut(&mut data[..Stats::LEN]);

    if stats.discriminator != STATS_DISCRIMINATOR {
        msg!("bump_stats: stats account not initialized");
        return Err(ProgramError::UninitializedAccount);
    }

    stats.market_count = stats.market_count.saturating_add(1);
    msg!("stats.market_count: {}", stats.market_count);
    Ok(())
}

// =============================================================================
// CreateVault + Deposit — SPL Token CPI mechanics (Chapter 6).
// =============================================================================
//
// The SPL Token instructions are constructed by hand rather than imported
// from the spl-token crate. Two reasons:
//   1. Chapter 6 teaches the bytes that go on the wire — the instruction
//      tag, the field encoding, the AccountMeta order. Importing a builder
//      hides exactly the thing the chapter is about.
//   2. spl-token at its current version pulls in a chunk of code we do not
//      need for two CPI calls, and inflates the .so binary by ~25 KB.

/// SPL Token instruction tags (subset used by this program).
mod spl_token_ix {
    pub const TRANSFER: u8 = 3;
    pub const INITIALIZE_ACCOUNT_3: u8 = 18;
}

const CREATE_VAULT_PAYLOAD_LEN: usize = 0;
const DEPOSIT_PAYLOAD_LEN: usize = 8; // amount: u64 LE

/// Accounts:
///   0. `[WRITE, SIGNER]` payer
///   1. `[]`              market           — must be owned by program_id
///   2. `[]`              mint             — SPL Mint, must be owned by Token
///   3. `[WRITE]`         vault_token_acct — new SPL Token Account at PDA
///                                            [b"vault", market.key, mint.key]
///   4. `[]`              vault_authority  — PDA at
///                                            [b"vault_auth", market.key];
///                                            becomes the SPL Token "owner"
///                                            of vault_token_acct
///   5. `[]`              system_program
///   6. `[]`              token_program    — must be SPL_TOKEN_PROGRAM_ID
fn process_create_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_VAULT_PAYLOAD_LEN {
        msg!("create_vault: payload must be empty");
        return Err(ProgramError::InvalidInstructionData);
    }

    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_auth_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        msg!("create_vault: payer must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }

    if market_ai.owner != program_id {
        msg!("create_vault: market not owned by this program");
        return Err(ProgramError::IncorrectProgramId);
    }
    if mint_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("create_vault: mint not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_ai.key != &system_program::ID {
        msg!("create_vault: account[5] is not the System program");
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        msg!("create_vault: account[6] is not the SPL Token program");
        return Err(ProgramError::IncorrectProgramId);
    }

    // Derive both PDAs and validate the caller passed the right accounts.
    let (expected_vault, vault_bump) = Pubkey::find_program_address(
        &[VAULT_SEED, market_ai.key.as_ref(), mint_ai.key.as_ref()],
        program_id,
    );
    if vault_ai.key != &expected_vault {
        msg!(
            "create_vault: passed vault {} != derived PDA {}",
            vault_ai.key,
            expected_vault
        );
        return Err(ProgramError::InvalidSeeds);
    }
    let (expected_auth, _auth_bump) =
        Pubkey::find_program_address(&[VAULT_AUTH_SEED, market_ai.key.as_ref()], program_id);
    if vault_auth_ai.key != &expected_auth {
        msg!(
            "create_vault: passed vault_authority {} != derived PDA {}",
            vault_auth_ai.key,
            expected_auth
        );
        return Err(ProgramError::InvalidSeeds);
    }

    // (1) System CPI: allocate the token account, owned by SPL Token Program.
    // The new account is the vault PDA, so we sign with its seeds + bump.
    let rent = Rent::get()?.minimum_balance(TOKEN_ACCOUNT_LEN);
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

    // (2) SPL Token CPI: InitializeAccount3. Instruction layout:
    //   data:     [tag = 18][owner: 32 bytes Pubkey]
    //   accounts: 0 = `[WRITE]` account to init
    //             1 = `[]`      mint
    //
    // Plain `invoke` — no PDA signing needed. The new account is now owned
    // by SPL Token at the Solana-runtime level, and InitializeAccount3 is a
    // pure data write that requires no signatures (the program tag itself is
    // the authorization).
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

    msg!("vault created (vault bump {})", vault_bump);
    Ok(())
}

/// Accounts:
///   0. `[SIGNER]` user            — SPL Token authority on user_token_acct
///   1. `[WRITE]`  user_token_acct — source
///   2. `[WRITE]`  vault_token_acct — destination, owned by SPL Token
///   3. `[]`       token_program   — must be SPL_TOKEN_PROGRAM_ID
fn process_deposit(accounts: &[AccountInfo], payload: &[u8]) -> ProgramResult {
    if payload.len() != DEPOSIT_PAYLOAD_LEN {
        msg!("deposit: payload must be {} bytes (amount u64 LE)", DEPOSIT_PAYLOAD_LEN);
        return Err(ProgramError::InvalidInstructionData);
    }
    let amount = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    if amount == 0 {
        msg!("deposit: amount must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    let user_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let user_token_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_token_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !user_ai.is_signer {
        msg!("deposit: user must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if user_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("deposit: user_token_acct not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    if vault_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("deposit: vault_token_acct not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        msg!("deposit: account[3] is not the SPL Token program");
        return Err(ProgramError::IncorrectProgramId);
    }

    // SPL Token Transfer. Instruction layout:
    //   data:     [tag = 3][amount: u64 LE]
    //   accounts: 0 = `[WRITE]`  source
    //             1 = `[WRITE]`  destination
    //             2 = `[SIGNER]` authority
    //
    // Plain `invoke`. The user is a signer at the outer tx level; the
    // runtime carries that signer privilege through to the SPL Token program
    // because the user appears as a signer in this program's AccountMeta
    // for this instruction. SPL Token sees `user.is_signer == true` in the
    // AccountInfo it receives, which is what authorizes the transfer.
    let mut transfer_data = Vec::with_capacity(1 + 8);
    transfer_data.push(spl_token_ix::TRANSFER);
    transfer_data.extend_from_slice(&amount.to_le_bytes());
    let transfer_ix = Instruction {
        program_id: SPL_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*user_token_ai.key, false),
            AccountMeta::new(*vault_token_ai.key, false),
            AccountMeta::new_readonly(*user_ai.key, true),
        ],
        data: transfer_data,
    };
    invoke(
        &transfer_ix,
        &[
            user_token_ai.clone(),
            vault_token_ai.clone(),
            user_ai.clone(),
            token_ai.clone(),
        ],
    )?;

    msg!("deposit: transferred {} units", amount);
    Ok(())
}
