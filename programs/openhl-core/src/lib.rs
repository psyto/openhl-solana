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
//!
//! The entire program is one file on purpose. Splitting it into the usual
//! `instruction.rs` / `processor.rs` / `state.rs` modules buys nothing
//! before there are many instructions, and obscures the fact that a
//! Solana program is fundamentally `fn process(...)` plus an entrypoint.

#![allow(unexpected_cfgs)] // solana_program::entrypoint! gates on `target_os = "solana"`

use openhl_state::{Market, MARKET_DISCRIMINATOR};
use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
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
