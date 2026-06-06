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
//!   7  CreateOrderBook — written for Chapter 7 (on-chain CLOB)
//!                       Creates the per-market OrderBook PDA at
//!                       [b"book", market], 2112 bytes.
//!   8  PlaceOrder      — also Chapter 7
//!                       Linear-scan for the first empty slot, write
//!                       the Order. CU envelope grows with slot index.
//!   9  CancelOrder     — also Chapter 7
//!                       Linear-scan by order_id, zero the slot.
//!  10  Match           — written for Chapter 8 (matching under CU pressure)
//!                       Takes a taker spec (side, limit_price, size,
//!                       max_fills) and walks the opposite-side resting
//!                       orders in the flat book, crossing wherever the
//!                       prices agree until size or max_fills is
//!                       exhausted. The max_fills cap is the
//!                       "pagination" response to CU pressure that
//!                       Chapter 8 walks through.
//!  11  CreateOracle    — written for Chapter 9 (oracle ingestion)
//!                       Creates the per-market Oracle PDA at
//!                       [b"oracle", market], 112 bytes.
//!  12  SetOraclePrice  — also Chapter 9
//!                       Writes price + conf + expo into the Oracle
//!                       account, stamping the current slot. Stand-in
//!                       for a real Pyth publish — auth is open here
//!                       on purpose, the chapter flags this as the
//!                       wrong production posture and what to do
//!                       instead.
//!  13  PlaceOrderChecked — also Chapter 9
//!                       PlaceOrder + (a) oracle staleness check via
//!                       Clock sysvar, (b) sanity band around the
//!                       oracle mark price. The first real risk
//!                       control in the program.
//!  14  CreateFundingState — written for Chapter 10 (funding rate)
//!                          PDA at [b"funding", market], 120 bytes.
//!  15  UpdateFunding     — also Chapter 10
//!                          Keeper-supplied rate. Computes
//!                          cumulative_funding_index += rate × elapsed_seconds.
//!                          Time-windowed accumulator pattern; the
//!                          Phase A parallelism lesson made operational.
//!  16  OpenPosition      — written for Chapter 11 (liquidation engine)
//!                          Creates per-(user, market) Position PDA, stamps
//!                          entry price from oracle, snapshots funding
//!                          index, validates initial margin.
//!  17  ClosePosition     — also Chapter 11
//!                          Settles funding, realizes price PnL into
//!                          collateral, zeros size. Account stays around
//!                          for re-open.
//!  18  Liquidate         — also Chapter 11
//!                          Permissionless. Reads oracle + funding,
//!                          computes equity, compares to maintenance
//!                          margin. If liquidatable: applies liquidation
//!                          penalty, force-closes at mark.
//!  19  CreateTradingVault — written for Chapter 12 (native vault)
//!                           PDA at [b"trading_vault", market, manager].
//!                           Empty share + asset balances, manager set.
//!  20  VaultDeposit       — also Chapter 12
//!                           Mints shares pro-rata into VaultShare PDA
//!                           at [b"vault_share", vault, owner].
//!  21  VaultWithdraw      — also Chapter 12
//!                           Burns shares, returns assets pro-rata.
//!  22  VaultUpdateNAV     — also Chapter 12
//!                           Manager-only. Sets total_assets to reflect
//!                           realized PnL since last update.
//!  23  RegisterBuilder    — written for Chapter 13 (builder codes)
//!                           Creates per-builder BuilderProfile PDA at
//!                           [b"builder", builder]. Builder declares
//!                           their self-imposed max_fee_share_bps cap.
//!  24  PlaceOrderWithBuilder — also Chapter 13
//!                           Like PlaceOrderChecked but escrows the
//!                           full protocol fee to the fee vault and
//!                           atomically credits the builder's share
//!                           to their BuilderProfile.
//!  25  ClaimBuilderFees   — also Chapter 13
//!                           Builder-only. Zeroes accumulated_fees
//!                           and CPI-signs an SPL Token Transfer of
//!                           the accumulated amount from the fee
//!                           vault to the builder's token account.
//!  26  CreateFeeVault     — also Chapter 13
//!                           One-shot bootstrap: allocates the per-
//!                           (quote_mint) fee_vault token account
//!                           owned by [b"fee_vault_auth", quote_mint].
//!
//! The entire program is one file on purpose. Splitting it into the usual
//! `instruction.rs` / `processor.rs` / `state.rs` modules buys nothing
//! before there are many instructions, and obscures the fact that a
//! Solana program is fundamentally `fn process(...)` plus an entrypoint.

#![allow(unexpected_cfgs)] // solana_program::entrypoint! gates on `target_os = "solana"`

use openhl_state::{
    side, BuilderProfile, FundingState, Market, Oracle, Order, OrderBook, Position, Stats,
    TradingVault, VaultShare, BUILDER_PROFILE_DISCRIMINATOR, FUNDING_DISCRIMINATOR,
    MARKET_DISCRIMINATOR, ORACLE_DISCRIMINATOR, ORDER_BOOK_DISCRIMINATOR, ORDER_CAPACITY,
    POSITION_DISCRIMINATOR, STATS_DISCRIMINATOR, TRADING_VAULT_DISCRIMINATOR,
    VAULT_SHARE_DISCRIMINATOR,
};
use solana_program::{
    account_info::AccountInfo,
    clock::Clock,
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

/// PDA seed prefix for the per-market order book.
///
/// Full seed list: `[BOOK_SEED, market_pubkey.as_ref(), &[bump]]`.
pub const BOOK_SEED: &[u8] = b"book";

/// PDA seed prefix for the per-market price oracle.
///
/// Full seed list: `[ORACLE_SEED, market_pubkey.as_ref(), &[bump]]`.
pub const ORACLE_SEED: &[u8] = b"oracle";

/// How many slots an oracle price may age before it is considered stale.
///
/// 25 slots ≈ 10 seconds on mainnet at the current target slot time.
/// Real perp DEXes typically tune this to the volatility of the underlying;
/// higher-vol pairs require shorter staleness windows.
pub const MAX_ORACLE_STALENESS_SLOTS: u64 = 25;

/// Order price must stay within ±SANITY_BAND_BPS basis points of the
/// oracle mark price. 2000 bps = 20%. A wide band on purpose — Chapter 9
/// flags that production deployments tune this per-market, often much
/// tighter.
pub const SANITY_BAND_BPS: u64 = 2000;

/// PDA seed prefix for the per-market funding-state account.
///
/// Full seed list: `[FUNDING_SEED, market_pubkey.as_ref(), &[bump]]`.
pub const FUNDING_SEED: &[u8] = b"funding";

/// Hard cap on the absolute value of a funding rate, in scaled 1e-9 units
/// per second. `1_000_000` = 0.001/sec = 86.4% per day = absurd but
/// catastrophic. Production caps are much tighter (e.g., 0.01% per hour).
/// We pick a loose cap so the chapter can demonstrate clamp behavior
/// without configuring per-market overrides.
pub const MAX_FUNDING_RATE_PER_SEC_ABS: i64 = 1_000_000;

/// PDA seed prefix for per-(user, market) Position accounts.
///
/// Full seed list: `[POSITION_SEED, user_pubkey.as_ref(), market_pubkey.as_ref(), &[bump]]`.
pub const POSITION_SEED: &[u8] = b"position";

/// Initial margin requirement, in basis points. `1000` = 10% of notional,
/// so positions may be opened with up to 10× leverage. Production perps
/// tune this per asset (more leverage for low-vol pairs, less for high-vol).
pub const INITIAL_MARGIN_BPS: u64 = 1000;

/// Maintenance margin requirement, in basis points. `500` = 5% of notional.
/// A position is liquidatable when equity / notional < MAINT_MARGIN_BPS / 10000.
pub const MAINT_MARGIN_BPS: u64 = 500;

/// Penalty (% of notional) taken from the position's collateral on
/// liquidation, paid to the liquidator. `100` = 1% of notional.
pub const LIQUIDATION_PENALTY_BPS: u64 = 100;

/// PDA seed prefix for the (market, manager) trading-vault aggregate.
///
/// Full seed list: `[TRADING_VAULT_SEED, market.key, manager.key, &[bump]]`.
pub const TRADING_VAULT_SEED: &[u8] = b"trading_vault";

/// PDA seed prefix for the per-(vault, owner) share ledger.
///
/// Full seed list: `[VAULT_SHARE_SEED, vault.key, owner.key, &[bump]]`.
pub const VAULT_SHARE_SEED: &[u8] = b"vault_share";

/// PDA seed prefix for per-builder fee-accrual profile.
///
/// Full seed list: `[BUILDER_PROFILE_SEED, builder.key, &[bump]]`.
pub const BUILDER_PROFILE_SEED: &[u8] = b"builder";

/// Protocol fee in basis points of notional. `10` = 0.1%. Tight by perp-DEX
/// standards (HL is ~3 bps for makers, ~5 for takers); we pick a round
/// number so the math reads clean in the chapter examples.
pub const PROTOCOL_FEE_BPS: u64 = 10;

/// Hard cap on how much of the protocol fee a builder may claim,
/// regardless of what their BuilderProfile declares. `5000` = 50%, i.e.
/// builders may keep at most half of the protocol fee, with the rest
/// retained by the protocol (in production: routed to an insurance
/// fund or treasury account).
pub const PROTOCOL_BUILDER_SHARE_CAP_BPS: u64 = 5000;

/// PDA seed prefix for the per-(quote_mint) protocol fee vault token
/// account. Every place-order variant Transfers `protocol_fee` from the
/// trader's quote token account into this PDA; ClaimBuilderFees moves
/// the builder's accumulated share out. The remainder (protocol take)
/// stays here.
///
/// Full seed list: `[FEE_VAULT_SEED, quote_mint.as_ref(), &[bump]]`.
pub const FEE_VAULT_SEED: &[u8] = b"fee_vault";

/// PDA seed prefix for the fee-vault authority. The fee_vault token
/// account is owned (in SPL Token's sense) by this PDA; the program
/// signs for it via `invoke_signed` on the claim path.
///
/// Full seed list: `[FEE_VAULT_AUTH_SEED, quote_mint.as_ref(), &[bump]]`.
pub const FEE_VAULT_AUTH_SEED: &[u8] = b"fee_vault_auth";

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
        7 => process_create_order_book(program_id, accounts, payload),
        8 => process_place_order(program_id, accounts, payload),
        9 => process_cancel_order(program_id, accounts, payload),
        10 => process_match(program_id, accounts, payload),
        11 => process_create_oracle(program_id, accounts, payload),
        12 => process_set_oracle_price(program_id, accounts, payload),
        13 => process_place_order_checked(program_id, accounts, payload),
        14 => process_create_funding_state(program_id, accounts, payload),
        15 => process_update_funding(program_id, accounts, payload),
        16 => process_open_position(program_id, accounts, payload),
        17 => process_close_position(program_id, accounts, payload),
        18 => process_liquidate(program_id, accounts, payload),
        19 => process_create_trading_vault(program_id, accounts, payload),
        20 => process_vault_deposit(program_id, accounts, payload),
        21 => process_vault_withdraw(program_id, accounts, payload),
        22 => process_vault_update_nav(program_id, accounts, payload),
        23 => process_register_builder(program_id, accounts, payload),
        24 => process_place_order_with_builder(program_id, accounts, payload),
        25 => process_claim_builder_fees(program_id, accounts, payload),
        26 => process_create_fee_vault(program_id, accounts, payload),
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

// =============================================================================
// CreateOrderBook + PlaceOrder + CancelOrder — on-chain CLOB (Chapter 7).
// =============================================================================

const CREATE_BOOK_PAYLOAD_LEN: usize = 0;
const PLACE_ORDER_PAYLOAD_LEN: usize = 1 + 8 + 8; // side u8 + price u64 + size u64
const CANCEL_ORDER_PAYLOAD_LEN: usize = 8; // order_id u64

/// Accounts:
///   0. `[WRITE, SIGNER]` payer
///   1. `[]`              market
///   2. `[WRITE]`         book           — PDA at [b"book", market]
///   3. `[]`              system_program
fn process_create_order_book(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_BOOK_PAYLOAD_LEN {
        msg!("create_order_book: payload must be empty");
        return Err(ProgramError::InvalidInstructionData);
    }

    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let book_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if market_ai.owner != program_id {
        msg!("create_order_book: market owner mismatch");
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_ai.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected_pda, bump) =
        Pubkey::find_program_address(&[BOOK_SEED, market_ai.key.as_ref()], program_id);
    if book_ai.key != &expected_pda {
        msg!(
            "create_order_book: passed book {} != derived PDA {}",
            book_ai.key,
            expected_pda
        );
        return Err(ProgramError::InvalidSeeds);
    }

    let rent = Rent::get()?.minimum_balance(OrderBook::LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        book_ai.key,
        rent,
        OrderBook::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), book_ai.clone(), system_ai.clone()],
        &[&[BOOK_SEED, market_ai.key.as_ref(), &[bump]]],
    )?;

    let mut data = book_ai.try_borrow_mut_data()?;
    let book: &mut OrderBook = bytemuck::from_bytes_mut(&mut data[..OrderBook::LEN]);
    book.discriminator = ORDER_BOOK_DISCRIMINATOR;
    book.bump = bump;
    book._pad0 = [0u8; 7];
    book.market.copy_from_slice(market_ai.key.as_ref());
    book.next_order_id = 1;
    book.active_count = 0;
    book._pad1 = [0u8; 4];
    // slots remain zeroed (size == 0 ⇒ empty slot)

    msg!("order book created (bump {})", bump);
    Ok(())
}

/// Payload: [side u8][price u64 LE][size u64 LE]
///
/// Accounts:
///   0. `[SIGNER]` user
///   1. `[WRITE]`  book — must be owned by this program, must be initialized
fn process_place_order(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != PLACE_ORDER_PAYLOAD_LEN {
        msg!(
            "place_order: payload must be {} bytes, got {}",
            PLACE_ORDER_PAYLOAD_LEN,
            payload.len()
        );
        return Err(ProgramError::InvalidInstructionData);
    }
    let order_side = payload[0];
    let price = u64::from_le_bytes(payload[1..9].try_into().expect("8 bytes"));
    let size = u64::from_le_bytes(payload[9..17].try_into().expect("8 bytes"));

    if order_side != side::BID && order_side != side::ASK {
        msg!("place_order: invalid side byte {}", order_side);
        return Err(ProgramError::InvalidInstructionData);
    }
    if price == 0 || size == 0 {
        msg!("place_order: price and size must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    let user_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let book_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !user_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if book_ai.owner != program_id {
        msg!("place_order: book owner mismatch");
        return Err(ProgramError::IncorrectProgramId);
    }
    if book_ai.data_len() != OrderBook::LEN {
        return Err(ProgramError::InvalidAccountData);
    }

    msg!("place_order: side={} price={} size={}", order_side, price, size);
    sol_log_compute_units();

    let mut data = book_ai.try_borrow_mut_data()?;
    let book: &mut OrderBook = bytemuck::from_bytes_mut(&mut data[..OrderBook::LEN]);

    if book.discriminator != ORDER_BOOK_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }

    // Linear scan for the first empty slot. This is O(ORDER_CAPACITY) in the
    // worst case (book full of asks placed before a bid lookup). Chapter 7's
    // whole point is that this scan is *visible* in the CU log and *grows*
    // as the book fills.
    let mut chosen_slot: Option<usize> = None;
    for (i, slot) in book.slots.iter().enumerate() {
        if slot.size == 0 {
            chosen_slot = Some(i);
            break;
        }
    }
    let slot_idx = chosen_slot.ok_or_else(|| {
        msg!("place_order: book full ({} slots)", ORDER_CAPACITY);
        ProgramError::AccountDataTooSmall
    })?;

    let order_id = book.next_order_id;
    book.next_order_id = book.next_order_id.saturating_add(1);
    book.active_count = book.active_count.saturating_add(1);

    let mut owner = [0u8; 32];
    owner.copy_from_slice(user_ai.key.as_ref());

    book.slots[slot_idx] = Order {
        order_id,
        price,
        size,
        owner,
        side: order_side,
        _pad: [0u8; 7],
    };

    msg!(
        "place_order: placed order_id={} into slot {} (active={})",
        order_id,
        slot_idx,
        book.active_count
    );
    sol_log_compute_units();

    Ok(())
}

/// Payload: [order_id u64 LE]
///
/// Accounts:
///   0. `[SIGNER]` user — must match the slot's owner
///   1. `[WRITE]`  book
fn process_cancel_order(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CANCEL_ORDER_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let order_id = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));

    let user_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let book_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !user_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if book_ai.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if book_ai.data_len() != OrderBook::LEN {
        return Err(ProgramError::InvalidAccountData);
    }

    msg!("cancel_order: order_id={}", order_id);
    sol_log_compute_units();

    let mut data = book_ai.try_borrow_mut_data()?;
    let book: &mut OrderBook = bytemuck::from_bytes_mut(&mut data[..OrderBook::LEN]);

    if book.discriminator != ORDER_BOOK_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }

    // Linear scan for the order_id. Worst case is "order is in the last
    // slot" or "order not found at all" — both pay full O(ORDER_CAPACITY).
    let mut found: Option<usize> = None;
    for (i, slot) in book.slots.iter().enumerate() {
        if slot.size != 0 && slot.order_id == order_id {
            found = Some(i);
            break;
        }
    }
    let slot_idx = found.ok_or_else(|| {
        msg!("cancel_order: order_id {} not found", order_id);
        ProgramError::InvalidArgument
    })?;

    // Authorization: only the order's original owner may cancel it.
    if book.slots[slot_idx].owner != *user_ai.key.as_ref() {
        msg!("cancel_order: caller is not the order owner");
        return Err(ProgramError::IllegalOwner);
    }

    // Zero the slot. The next place_order that lands in this index will
    // overwrite it. No compaction.
    book.slots[slot_idx] = <Order as bytemuck::Zeroable>::zeroed();
    book.active_count = book.active_count.saturating_sub(1);

    msg!(
        "cancel_order: cancelled order_id={} (slot {}, active={})",
        order_id,
        slot_idx,
        book.active_count
    );
    sol_log_compute_units();

    Ok(())
}

// =============================================================================
// Match — taker crosses against the flat book (Chapter 8).
// =============================================================================
//
// Match is the simplest possible CLOB matching engine. The taker supplies:
//   - side: which side they're taking (0 = bid → matches against asks,
//                                       1 = ask → matches against bids)
//   - limit_price: the worst price the taker will accept
//   - size: total base units to take
//   - max_fills: maximum number of resting orders to cross in one
//                instruction. This is the "pagination" lever — caps the
//                worst-case CU cost so a single transaction stays under
//                the budget.
//
// The algorithm per iteration:
//   1. Linear-scan the book for the best price on the opposite side
//      that is acceptable to the taker. O(N) on every iteration —
//      this is the cost shape Chapter 8 is about.
//   2. If no acceptable maker exists, stop.
//   3. Cross: subtract the fill quantity from both sides, log the fill.
//   4. If the maker is fully filled, zero their slot.
//   5. Increment the fill counter. If we've hit max_fills, stop.
//   6. If the taker is fully filled, stop.
//
// The implementation is intentionally simple — no per-price-level FIFO,
// no maker-rebate accounting, no settlement movement. The point is to
// expose the multiplicative cost shape (O(fills * N)) and watch how
// it interacts with the per-tx CU ceiling.

const MATCH_PAYLOAD_LEN: usize = 1 + 8 + 8 + 1; // side u8 + price u64 + size u64 + max_fills u8

/// Payload: [side u8][limit_price u64 LE][size u64 LE][max_fills u8]
///
/// Accounts:
///   0. `[SIGNER]` taker — informational; this matcher does no settlement
///   1. `[WRITE]`  book  — owned by this program, must be initialized
fn process_match(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != MATCH_PAYLOAD_LEN {
        msg!(
            "match: payload must be {} bytes, got {}",
            MATCH_PAYLOAD_LEN,
            payload.len()
        );
        return Err(ProgramError::InvalidInstructionData);
    }
    let taker_side = payload[0];
    let limit_price = u64::from_le_bytes(payload[1..9].try_into().expect("8 bytes"));
    let mut remaining = u64::from_le_bytes(payload[9..17].try_into().expect("8 bytes"));
    let max_fills = payload[17];

    if taker_side != side::BID && taker_side != side::ASK {
        return Err(ProgramError::InvalidInstructionData);
    }
    if limit_price == 0 || remaining == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }
    if max_fills == 0 {
        msg!("match: max_fills must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    let taker_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let book_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    if !taker_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if book_ai.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if book_ai.data_len() != OrderBook::LEN {
        return Err(ProgramError::InvalidAccountData);
    }

    msg!(
        "match: taker_side={} limit={} size={} max_fills={}",
        taker_side,
        limit_price,
        remaining,
        max_fills
    );
    sol_log_compute_units();

    let mut data = book_ai.try_borrow_mut_data()?;
    let book: &mut OrderBook = bytemuck::from_bytes_mut(&mut data[..OrderBook::LEN]);
    if book.discriminator != ORDER_BOOK_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }

    let maker_side = if taker_side == side::BID { side::ASK } else { side::BID };

    let mut fills_done: u8 = 0;
    while remaining > 0 && fills_done < max_fills {
        // (a) Find the best opposite-side resting order whose price the
        // taker will accept. "Best" = lowest price for asks (taker = bid),
        // highest price for bids (taker = ask). Linear O(N) scan per fill.
        let mut best: Option<(usize, u64)> = None;
        for (i, slot) in book.slots.iter().enumerate() {
            if slot.size == 0 || slot.side != maker_side {
                continue;
            }
            let price_acceptable = match taker_side {
                side::BID => slot.price <= limit_price, // we'll buy at or below limit
                side::ASK => slot.price >= limit_price, // we'll sell at or above limit
                _ => unreachable!(),
            };
            if !price_acceptable {
                continue;
            }
            let is_better = match best {
                None => true,
                Some((_, p)) => match taker_side {
                    side::BID => slot.price < p, // lower ask is better
                    side::ASK => slot.price > p, // higher bid is better
                    _ => unreachable!(),
                },
            };
            if is_better {
                best = Some((i, slot.price));
            }
        }

        let (maker_idx, fill_price) = match best {
            Some(b) => b,
            None => {
                msg!("match: no acceptable maker found (book exhausted or out of price)");
                break;
            }
        };

        // (b) Cross. Take min(taker_remaining, maker_remaining).
        let maker = &mut book.slots[maker_idx];
        let fill_size = remaining.min(maker.size);

        msg!(
            "match: fill {} @ {} from maker_id={} (slot {})",
            fill_size,
            fill_price,
            maker.order_id,
            maker_idx
        );

        maker.size -= fill_size;
        remaining -= fill_size;
        fills_done += 1;

        // (c) If maker is fully filled, vacate its slot.
        if maker.size == 0 {
            *maker = <Order as bytemuck::Zeroable>::zeroed();
            book.active_count = book.active_count.saturating_sub(1);
        }
    }

    msg!(
        "match: done. fills={} taker_remaining={}",
        fills_done,
        remaining
    );
    sol_log_compute_units();

    Ok(())
}

// =============================================================================
// Oracle — CreateOracle + SetOraclePrice + PlaceOrderChecked (Chapter 9).
// =============================================================================
//
// Our Oracle account is a stand-in for a real Pyth price account. It carries
// the same shape (price + conf + expo + publish_slot) and behaves the same
// way under the staleness check, but is owned by our own program for ease of
// testing. Chapter 9 walks the differences carefully so the techniques
// transfer to a real Pyth integration.

const CREATE_ORACLE_PAYLOAD_LEN: usize = 0;
const SET_ORACLE_PAYLOAD_LEN: usize = 8 + 8 + 4; // price i64 + conf u64 + expo i32
const PLACE_ORDER_CHECKED_PAYLOAD_LEN: usize = 1 + 8 + 8; // same as PlaceOrder

/// Accounts:
///   0. `[WRITE, SIGNER]` payer
///   1. `[]`              market
///   2. `[WRITE]`         oracle           — PDA at [b"oracle", market]
///   3. `[]`              system_program
fn process_create_oracle(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_ORACLE_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let oracle_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if market_ai.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_ai.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected, bump) =
        Pubkey::find_program_address(&[ORACLE_SEED, market_ai.key.as_ref()], program_id);
    if oracle_ai.key != &expected {
        return Err(ProgramError::InvalidSeeds);
    }

    let rent = Rent::get()?.minimum_balance(Oracle::LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        oracle_ai.key,
        rent,
        Oracle::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), oracle_ai.clone(), system_ai.clone()],
        &[&[ORACLE_SEED, market_ai.key.as_ref(), &[bump]]],
    )?;

    let mut data = oracle_ai.try_borrow_mut_data()?;
    let oracle: &mut Oracle = bytemuck::from_bytes_mut(&mut data[..Oracle::LEN]);
    oracle.discriminator = ORACLE_DISCRIMINATOR;
    oracle.bump = bump;
    oracle._pad0 = [0u8; 7];
    oracle.market.copy_from_slice(market_ai.key.as_ref());
    oracle.price = 0;
    oracle.conf = 0;
    oracle.expo = 0;
    oracle._pad1 = [0u8; 4];
    oracle.publish_slot = 0;
    oracle._reserved = [0u8; 32];

    msg!("oracle created (bump {})", bump);
    Ok(())
}

/// Payload: [price i64 LE][conf u64 LE][expo i32 LE]
///
/// Accounts:
///   0. `[SIGNER]` publisher — open auth in this demo; production would
///                              pin to a known publisher pubkey
///   1. `[WRITE]`  oracle    — owned by this program
///
/// SECURITY NOTE: in the real world this writer must be authenticated —
/// either by checking publisher.key against a known Pyth/oracle authority,
/// or (better) by making the oracle account owned by Pyth itself and
/// reading it instead of writing it. We intentionally leave auth open
/// here so the chapter can exercise it; the auth gap is called out in
/// the chapter and as a per-instruction msg!.
fn process_set_oracle_price(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != SET_ORACLE_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let price = i64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    let conf = u64::from_le_bytes(payload[8..16].try_into().expect("8 bytes"));
    let expo = i32::from_le_bytes(payload[16..20].try_into().expect("4 bytes"));

    let publisher_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let oracle_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    if !publisher_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if oracle_ai.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if oracle_ai.data_len() != Oracle::LEN {
        return Err(ProgramError::InvalidAccountData);
    }

    msg!("set_oracle_price: open-auth publisher = {}", publisher_ai.key);

    let clock = Clock::get()?;
    let mut data = oracle_ai.try_borrow_mut_data()?;
    let oracle: &mut Oracle = bytemuck::from_bytes_mut(&mut data[..Oracle::LEN]);
    if oracle.discriminator != ORACLE_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }

    oracle.price = price;
    oracle.conf = conf;
    oracle.expo = expo;
    oracle.publish_slot = clock.slot;

    msg!(
        "set_oracle_price: price={} conf={} expo={} slot={}",
        price,
        conf,
        expo,
        clock.slot
    );
    Ok(())
}

/// Payload (same as PlaceOrder): [side u8][price u64 LE][size u64 LE]
///
/// Accounts:
///   0. `[WRITE, SIGNER]` user             — signs both outer tx and the
///                                           protocol-fee SPL Token Transfer
///   1. `[WRITE]`         book
///   2. `[]`              oracle           — must be initialized, must be fresh
///   3. `[]`              market           — owns the (book, oracle, quote_mint) chain
///   4. `[]`              mint             — quote_mint, must match market.quote_mint
///   5. `[WRITE]`         user_token       — user's quote-token account (fee source)
///   6. `[WRITE]`         fee_vault_token  — per-(quote_mint) fee vault PDA (fee sink)
///   7. `[]`              token_program    — SPL Token
fn process_place_order_checked(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != PLACE_ORDER_CHECKED_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let order_side = payload[0];
    let price = u64::from_le_bytes(payload[1..9].try_into().expect("8 bytes"));
    let size = u64::from_le_bytes(payload[9..17].try_into().expect("8 bytes"));

    if order_side != side::BID && order_side != side::ASK {
        return Err(ProgramError::InvalidInstructionData);
    }
    if price == 0 || size == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let user_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let book_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let oracle_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let user_token_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let fee_vault_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !user_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if book_ai.owner != program_id || book_ai.data_len() != OrderBook::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if oracle_ai.owner != program_id || oracle_ai.data_len() != Oracle::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if market_ai.owner != program_id || market_ai.data_len() != Market::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if mint_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if user_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("place_order_checked: user_token not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    // book and oracle are both per-market PDAs; cross-check they descend
    // from the market the caller passed. Prevents fee-vault confusion via
    // book/oracle from market_A + market account from market_B.
    let (expected_book, _) =
        Pubkey::find_program_address(&[BOOK_SEED, market_ai.key.as_ref()], program_id);
    if book_ai.key != &expected_book {
        msg!("place_order_checked: book does not descend from passed market");
        return Err(ProgramError::InvalidSeeds);
    }
    let (expected_oracle, _) =
        Pubkey::find_program_address(&[ORACLE_SEED, market_ai.key.as_ref()], program_id);
    if oracle_ai.key != &expected_oracle {
        msg!("place_order_checked: oracle does not descend from passed market");
        return Err(ProgramError::InvalidSeeds);
    }

    // Cross-check mint against market.quote_mint and derive the fee vault
    // from mint. Fee escrow always flows into the quote currency's vault.
    {
        let market_data = market_ai.try_borrow_data()?;
        let market: &Market = bytemuck::from_bytes(&market_data[..Market::LEN]);
        if market.discriminator != MARKET_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if market.quote_mint != *mint_ai.key.as_ref() {
            msg!("place_order_checked: mint != market.quote_mint");
            return Err(ProgramError::InvalidAccountData);
        }
    }
    verify_fee_vault_token_account(fee_vault_ai, mint_ai.key, program_id)?;

    // (1) Oracle staleness gauntlet, same as Chapter 9.
    let mark = read_fresh_oracle(oracle_ai, program_id)?;

    // (2) Sanity band: price must lie within ±SANITY_BAND_BPS bps of mark.
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

    // (3) Compute the protocol fee. Symmetric with PlaceOrderWithBuilder
    // (Chapter 13 §13.5): every place-order variant escrows the same fee.
    // The asymmetric alternative — where only the builder path charges a
    // token-denominated fee — would make trades through a builder cost
    // the user more in real terms than identical trades without one.
    let notional_val = (price as u128) * (size as u128);
    let protocol_fee = (notional_val * (PROTOCOL_FEE_BPS as u128) / 10_000) as u64;

    msg!(
        "place_order_checked: side={} price={} size={} mark={} protocol_fee={}",
        order_side,
        price,
        size,
        mark,
        protocol_fee
    );

    // (4) Place the order, identical to PlaceOrder (Chapter 7) from here.
    {
        let mut data = book_ai.try_borrow_mut_data()?;
        let book: &mut OrderBook = bytemuck::from_bytes_mut(&mut data[..OrderBook::LEN]);
        if book.discriminator != ORDER_BOOK_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }

        let mut chosen_slot: Option<usize> = None;
        for (i, slot) in book.slots.iter().enumerate() {
            if slot.size == 0 {
                chosen_slot = Some(i);
                break;
            }
        }
        let slot_idx = chosen_slot.ok_or(ProgramError::AccountDataTooSmall)?;

        let order_id = book.next_order_id;
        book.next_order_id = book.next_order_id.saturating_add(1);
        book.active_count = book.active_count.saturating_add(1);

        let mut owner = [0u8; 32];
        owner.copy_from_slice(user_ai.key.as_ref());

        book.slots[slot_idx] = Order {
            order_id,
            price,
            size,
            owner,
            side: order_side,
            _pad: [0u8; 7],
        };

        msg!(
            "place_order_checked: placed order_id={} into slot {}",
            order_id,
            slot_idx
        );
    }

    // (5) Escrow the protocol fee via CPI to SPL Token Transfer. Placed
    // last so a failed transfer (InsufficientFunds) reverts the whole tx
    // — including the book write above — atomically.
    spl_token_transfer_user_signed(
        user_token_ai,
        fee_vault_ai,
        user_ai,
        token_ai,
        protocol_fee,
    )?;

    Ok(())
}

// =============================================================================
// Funding — CreateFundingState + UpdateFunding (Chapter 10).
// =============================================================================
//
// The funding pattern is a time-windowed accumulator. UpdateFunding is the
// only mutator: it reads (current_rate, last_update_ts) from the account,
// applies the new keeper-supplied rate over the elapsed window, and writes
// back the updated cumulative_funding_index plus the new rate and timestamp.
//
// Position settlement (the half not in this chapter) is the read-side:
// when a position is touched, you settle it by computing
//   delta = (funding.cumulative_funding_index - position.funding_snapshot_index)
//   pnl   = delta * position.size / 1e9
//   position.funding_snapshot_index = funding.cumulative_funding_index
// — a constant-time per-touch update that requires no global iteration.
// Chapter 11 introduces Position; Chapter 10 is about the accumulator.

const CREATE_FUNDING_PAYLOAD_LEN: usize = 8; // window_seconds u64
const UPDATE_FUNDING_PAYLOAD_LEN: usize = 8; // new_rate_per_sec i64

/// Accounts:
///   0. `[WRITE, SIGNER]` payer
///   1. `[]`              market
///   2. `[WRITE]`         funding          — PDA at [b"funding", market]
///   3. `[]`              system_program
fn process_create_funding_state(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_FUNDING_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let window_seconds = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    if window_seconds == 0 {
        msg!("create_funding_state: window_seconds must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let funding_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if market_ai.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_ai.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected, bump) =
        Pubkey::find_program_address(&[FUNDING_SEED, market_ai.key.as_ref()], program_id);
    if funding_ai.key != &expected {
        return Err(ProgramError::InvalidSeeds);
    }

    let rent = Rent::get()?.minimum_balance(FundingState::LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        funding_ai.key,
        rent,
        FundingState::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), funding_ai.clone(), system_ai.clone()],
        &[&[FUNDING_SEED, market_ai.key.as_ref(), &[bump]]],
    )?;

    let clock = Clock::get()?;
    let mut data = funding_ai.try_borrow_mut_data()?;
    let funding: &mut FundingState =
        bytemuck::from_bytes_mut(&mut data[..FundingState::LEN]);
    funding.discriminator = FUNDING_DISCRIMINATOR;
    funding.bump = bump;
    funding._pad0 = [0u8; 7];
    funding.market.copy_from_slice(market_ai.key.as_ref());
    funding.cumulative_funding_index = 0;
    funding.last_update_ts = clock.unix_timestamp;
    funding.last_update_slot = clock.slot;
    funding.current_rate_per_sec = 0;
    funding.window_seconds = window_seconds;
    funding._reserved = [0u8; 32];

    msg!(
        "funding state created (bump {}, window {}s)",
        bump,
        window_seconds
    );
    Ok(())
}

/// Payload: [new_rate_per_sec i64 LE]
///
/// Accounts:
///   0. `[SIGNER]` keeper — open-auth in this demo; production would pin
///   1. `[WRITE]`  funding — owned by this program
fn process_update_funding(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != UPDATE_FUNDING_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let new_rate_raw = i64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));

    let keeper_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let funding_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    if !keeper_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if funding_ai.owner != program_id || funding_ai.data_len() != FundingState::LEN {
        return Err(ProgramError::InvalidAccountData);
    }

    // Clamp the keeper's proposed rate to the program's hard cap. A keeper
    // bug or compromise should not produce arbitrarily large funding payments.
    let new_rate = new_rate_raw
        .max(-MAX_FUNDING_RATE_PER_SEC_ABS)
        .min(MAX_FUNDING_RATE_PER_SEC_ABS);
    if new_rate != new_rate_raw {
        msg!(
            "update_funding: keeper rate {} clamped to {}",
            new_rate_raw,
            new_rate
        );
    }

    let clock = Clock::get()?;
    let mut data = funding_ai.try_borrow_mut_data()?;
    let funding: &mut FundingState =
        bytemuck::from_bytes_mut(&mut data[..FundingState::LEN]);
    if funding.discriminator != FUNDING_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }

    // Accumulate funding over the elapsed window using the *prior* rate.
    // Pattern: each update commits the rate that was in effect during the
    // preceding interval, then installs the new rate for the next one.
    // This is the standard "step function" accumulator — cumulative_index
    // grows in piecewise-linear segments, one segment per UpdateFunding call.
    let elapsed = clock.unix_timestamp.saturating_sub(funding.last_update_ts);
    if elapsed < 0 {
        msg!("update_funding: clock went backwards (elapsed={})", elapsed);
        return Err(ProgramError::InvalidAccountData);
    }
    let elapsed_u = elapsed as u64;

    let delta = (funding.current_rate_per_sec as i128) * (elapsed_u as i128);
    let new_cumulative = (funding.cumulative_funding_index as i128).saturating_add(delta);
    // i64 saturation if the cumulative would overflow. Practical books take
    // years to even approach i64 range with realistic rates.
    let new_cumulative_clamped: i64 = if new_cumulative > i64::MAX as i128 {
        i64::MAX
    } else if new_cumulative < i64::MIN as i128 {
        i64::MIN
    } else {
        new_cumulative as i64
    };

    msg!(
        "update_funding: prior_rate={} elapsed={}s delta={} new_cumulative={}",
        funding.current_rate_per_sec,
        elapsed_u,
        delta,
        new_cumulative_clamped
    );

    funding.cumulative_funding_index = new_cumulative_clamped;
    funding.current_rate_per_sec = new_rate;
    funding.last_update_ts = clock.unix_timestamp;
    funding.last_update_slot = clock.slot;

    Ok(())
}

// =============================================================================
// Position lifecycle — OpenPosition + ClosePosition + Liquidate (Chapter 11).
// =============================================================================

const OPEN_POSITION_PAYLOAD_LEN: usize = 8 + 8; // size i64 + collateral u64
const CLOSE_POSITION_PAYLOAD_LEN: usize = 0;
const LIQUIDATE_PAYLOAD_LEN: usize = 0;

/// Compute equity = collateral + price PnL + funding PnL.
/// All returned in quote units, signed (negative means underwater).
fn compute_equity(position: &Position, mark: u64, funding_index_now: i64) -> i128 {
    let size = position.size as i128;
    let entry = position.entry_price as i128;
    let mark_i = mark as i128;
    let collateral = position.collateral as i128;

    let price_pnl = size * (mark_i - entry);

    let funding_delta = (funding_index_now as i128) - (position.funding_snapshot_index as i128);
    // funding pnl uses the 1e9 scaling from the FundingState index
    let funding_pnl = funding_delta * size / 1_000_000_000_i128;

    collateral + price_pnl + funding_pnl
}

/// Compute notional = abs(size) * mark.
fn notional(size: i64, mark: u64) -> u128 {
    let abs_size = (size.unsigned_abs()) as u128;
    abs_size * (mark as u128)
}

/// Read a fresh oracle mark price from an oracle account. Same staleness
/// gauntlet as Chapter 9's PlaceOrderChecked, factored out so the three
/// position handlers don't duplicate the check.
fn read_fresh_oracle(oracle_ai: &AccountInfo, program_id: &Pubkey) -> Result<u64, ProgramError> {
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

/// Read the cumulative_funding_index from a funding account.
fn read_funding_index(funding_ai: &AccountInfo, program_id: &Pubkey) -> Result<i64, ProgramError> {
    if funding_ai.owner != program_id || funding_ai.data_len() != FundingState::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    let funding_data = funding_ai.try_borrow_data()?;
    let funding: &FundingState = bytemuck::from_bytes(&funding_data[..FundingState::LEN]);
    if funding.discriminator != FUNDING_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }
    Ok(funding.cumulative_funding_index)
}

// =============================================================================
// SPL Token escrow helpers — used by Position + TradingVault handlers to
// actually move quote tokens in/out of the per-(market, mint) vault from
// Chapter 6. Without these, the position/vault accounting would be tracked
// in u64 fields but no real tokens would move.
// =============================================================================

/// Verify the passed vault_token_account matches the derived PDA at
/// `[VAULT_SEED, market, mint]` and is owned by SPL Token. Returns nothing
/// (this is a guard; the bump is rediscovered by callers that need it).
fn verify_vault_token_account(
    vault_token_ai: &AccountInfo,
    market_key: &Pubkey,
    mint_key: &Pubkey,
    program_id: &Pubkey,
) -> ProgramResult {
    let (expected, _bump) = Pubkey::find_program_address(
        &[VAULT_SEED, market_key.as_ref(), mint_key.as_ref()],
        program_id,
    );
    if vault_token_ai.key != &expected {
        msg!("vault_token_account does not match derived PDA");
        return Err(ProgramError::InvalidSeeds);
    }
    if vault_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("vault_token_account not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    Ok(())
}

/// Verify the passed vault_authority account matches the derived PDA at
/// `[VAULT_AUTH_SEED, market]`. Returns the bump for use in invoke_signed
/// transfers out of the vault.
fn verify_vault_authority(
    vault_authority_ai: &AccountInfo,
    market_key: &Pubkey,
    program_id: &Pubkey,
) -> Result<u8, ProgramError> {
    let (expected, bump) =
        Pubkey::find_program_address(&[VAULT_AUTH_SEED, market_key.as_ref()], program_id);
    if vault_authority_ai.key != &expected {
        msg!("vault_authority does not match derived PDA");
        return Err(ProgramError::InvalidSeeds);
    }
    Ok(bump)
}

/// SPL Token Transfer where the source's authority is a regular keypair
/// already signing the outer transaction. Plain `invoke` — signer privilege
/// extends through to SPL Token.
fn spl_token_transfer_user_signed<'a>(
    source: &AccountInfo<'a>,
    dest: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    token_program: &AccountInfo<'a>,
    amount: u64,
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let mut data = Vec::with_capacity(1 + 8);
    data.push(spl_token_ix::TRANSFER);
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: SPL_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*source.key, false),
            AccountMeta::new(*dest.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data,
    };
    invoke(
        &ix,
        &[
            source.clone(),
            dest.clone(),
            authority.clone(),
            token_program.clone(),
        ],
    )
}

/// Verify the passed fee_vault token account matches the derived PDA at
/// `[FEE_VAULT_SEED, quote_mint]` and is owned by SPL Token. Mirrors
/// `verify_vault_token_account` but keyed on quote_mint, not (market, mint).
fn verify_fee_vault_token_account(
    fee_vault_ai: &AccountInfo,
    quote_mint_key: &Pubkey,
    program_id: &Pubkey,
) -> ProgramResult {
    let (expected, _bump) =
        Pubkey::find_program_address(&[FEE_VAULT_SEED, quote_mint_key.as_ref()], program_id);
    if fee_vault_ai.key != &expected {
        msg!("fee_vault_token_account does not match derived PDA");
        return Err(ProgramError::InvalidSeeds);
    }
    if fee_vault_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("fee_vault_token_account not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    Ok(())
}

/// Verify the passed fee_vault_authority matches the derived PDA at
/// `[FEE_VAULT_AUTH_SEED, quote_mint]`. Returns the bump for use in
/// invoke_signed on the claim path.
fn verify_fee_vault_authority(
    fee_vault_auth_ai: &AccountInfo,
    quote_mint_key: &Pubkey,
    program_id: &Pubkey,
) -> Result<u8, ProgramError> {
    let (expected, bump) =
        Pubkey::find_program_address(&[FEE_VAULT_AUTH_SEED, quote_mint_key.as_ref()], program_id);
    if fee_vault_auth_ai.key != &expected {
        msg!("fee_vault_authority does not match derived PDA");
        return Err(ProgramError::InvalidSeeds);
    }
    Ok(bump)
}

/// SPL Token Transfer where the source's authority is the per-market
/// vault PDA at `[VAULT_AUTH_SEED, market]`. The program signs for the PDA
/// via `invoke_signed` with the matching seeds + bump.
fn spl_token_transfer_vault_signed<'a>(
    source: &AccountInfo<'a>,
    dest: &AccountInfo<'a>,
    vault_authority: &AccountInfo<'a>,
    market_key: &Pubkey,
    vault_auth_bump: u8,
    token_program: &AccountInfo<'a>,
    amount: u64,
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let mut data = Vec::with_capacity(1 + 8);
    data.push(spl_token_ix::TRANSFER);
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: SPL_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*source.key, false),
            AccountMeta::new(*dest.key, false),
            AccountMeta::new_readonly(*vault_authority.key, true),
        ],
        data,
    };
    invoke_signed(
        &ix,
        &[
            source.clone(),
            dest.clone(),
            vault_authority.clone(),
            token_program.clone(),
        ],
        &[&[VAULT_AUTH_SEED, market_key.as_ref(), &[vault_auth_bump]]],
    )
}

/// SPL Token Transfer where the source's authority is the per-(quote_mint)
/// fee-vault PDA at `[FEE_VAULT_AUTH_SEED, quote_mint]`. Structurally
/// identical to `spl_token_transfer_vault_signed` — different seeds. Used by
/// `ClaimBuilderFees` to move accumulated fees out of the fee vault.
fn spl_token_transfer_fee_vault_signed<'a>(
    source: &AccountInfo<'a>,
    dest: &AccountInfo<'a>,
    fee_vault_authority: &AccountInfo<'a>,
    quote_mint_key: &Pubkey,
    fee_vault_auth_bump: u8,
    token_program: &AccountInfo<'a>,
    amount: u64,
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let mut data = Vec::with_capacity(1 + 8);
    data.push(spl_token_ix::TRANSFER);
    data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: SPL_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*source.key, false),
            AccountMeta::new(*dest.key, false),
            AccountMeta::new_readonly(*fee_vault_authority.key, true),
        ],
        data,
    };
    invoke_signed(
        &ix,
        &[
            source.clone(),
            dest.clone(),
            fee_vault_authority.clone(),
            token_program.clone(),
        ],
        &[&[
            FEE_VAULT_AUTH_SEED,
            quote_mint_key.as_ref(),
            &[fee_vault_auth_bump],
        ]],
    )
}

/// Payload: [size i64 LE][collateral u64 LE]
///
/// Accounts:
///   0. `[WRITE, SIGNER]` user            — signs both outer tx and the
///                                          collateral SPL Token Transfer
///   1. `[]`              market
///   2. `[WRITE]`         position        — PDA at [b"position", user, market]
///   3. `[]`              oracle
///   4. `[]`              funding
///   5. `[]`              mint            — quote-asset SPL Mint
///   6. `[WRITE]`         user_token      — user's quote token account (SPL)
///   7. `[WRITE]`         vault_token     — PDA at [b"vault", market, mint]
///   8. `[]`              system_program
///   9. `[]`              token_program
fn process_open_position(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != OPEN_POSITION_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let size = i64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    let collateral = u64::from_le_bytes(payload[8..16].try_into().expect("8 bytes"));

    if size == 0 {
        msg!("open_position: size must be non-zero");
        return Err(ProgramError::InvalidInstructionData);
    }
    if collateral == 0 {
        msg!("open_position: collateral must be > 0");
        return Err(ProgramError::InvalidInstructionData);
    }

    let user_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let position_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let oracle_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let funding_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let user_token_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_token_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(8).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(9).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !user_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if market_ai.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_ai.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if user_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("open_position: user_token_account not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_vault_token_account(vault_token_ai, market_ai.key, mint_ai.key, program_id)?;

    // Position PDA derivation.
    let (expected, bump) = Pubkey::find_program_address(
        &[POSITION_SEED, user_ai.key.as_ref(), market_ai.key.as_ref()],
        program_id,
    );
    if position_ai.key != &expected {
        return Err(ProgramError::InvalidSeeds);
    }

    // Pull oracle mark + funding snapshot.
    let mark = read_fresh_oracle(oracle_ai, program_id)?;
    let funding_snapshot = read_funding_index(funding_ai, program_id)?;

    // Initial margin requirement check.
    let notional_val = notional(size, mark);
    let im_required = notional_val * (INITIAL_MARGIN_BPS as u128) / 10_000;
    if (collateral as u128) < im_required {
        msg!(
            "open_position: collateral {} < initial margin {} (notional {} × {} bps)",
            collateral,
            im_required,
            notional_val,
            INITIAL_MARGIN_BPS
        );
        return Err(ProgramError::InvalidArgument);
    }

    // (a) Allocate the position PDA via CPI to System. Pays rent from user.
    let rent = Rent::get()?.minimum_balance(Position::LEN);
    let create_ix = system_instruction::create_account(
        user_ai.key,
        position_ai.key,
        rent,
        Position::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[user_ai.clone(), position_ai.clone(), system_ai.clone()],
        &[&[
            POSITION_SEED,
            user_ai.key.as_ref(),
            market_ai.key.as_ref(),
            &[bump],
        ]],
    )?;

    // (b) Escrow the collateral via CPI to SPL Token. User signs the outer
    // transaction; the signature flows through to SPL Token (Chapter 6's
    // signer-privilege-extension pattern).
    spl_token_transfer_user_signed(
        user_token_ai,
        vault_token_ai,
        user_ai,
        token_ai,
        collateral,
    )?;

    // (c) Write the position record.
    let mut data = position_ai.try_borrow_mut_data()?;
    let position: &mut Position = bytemuck::from_bytes_mut(&mut data[..Position::LEN]);
    position.discriminator = POSITION_DISCRIMINATOR;
    position.bump = bump;
    position._pad0 = [0u8; 7];
    position.user.copy_from_slice(user_ai.key.as_ref());
    position.market.copy_from_slice(market_ai.key.as_ref());
    position.size = size;
    position.entry_price = mark;
    position.collateral = collateral;
    position.funding_snapshot_index = funding_snapshot;
    position._reserved = [0u8; 32];

    msg!(
        "open_position: size={} entry={} collateral={} escrowed (funding_snap={})",
        size,
        mark,
        collateral,
        funding_snapshot
    );
    Ok(())
}

/// Payload: empty.
///
/// Accounts:
///   0. `[SIGNER]` user            — must match the position's owner
///   1. `[WRITE]`  position
///   2. `[]`       oracle
///   3. `[]`       funding
///   4. `[]`       market          — needed for vault-PDA derivation
///   5. `[]`       mint            — quote-asset SPL Mint
///   6. `[WRITE]`  user_token      — destination of the payout
///   7. `[WRITE]`  vault_token     — PDA at [b"vault", market, mint]
///   8. `[]`       vault_authority — PDA at [b"vault_auth", market]
///   9. `[]`       token_program
fn process_close_position(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CLOSE_POSITION_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }

    let user_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let position_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let oracle_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let funding_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let user_token_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_token_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_authority_ai = accounts.get(8).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(9).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !user_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if position_ai.owner != program_id || position_ai.data_len() != Position::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if user_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_vault_token_account(vault_token_ai, market_ai.key, mint_ai.key, program_id)?;
    let vault_auth_bump = verify_vault_authority(vault_authority_ai, market_ai.key, program_id)?;

    let mark = read_fresh_oracle(oracle_ai, program_id)?;
    let funding_now = read_funding_index(funding_ai, program_id)?;

    // Compute realized payout inside a borrow-scope, then drop the borrow
    // before doing the CPI (the CPI may need to re-borrow vault state).
    let payout: u64;
    {
        let mut data = position_ai.try_borrow_mut_data()?;
        let position: &mut Position =
            bytemuck::from_bytes_mut(&mut data[..Position::LEN]);

        if position.discriminator != POSITION_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if position.user != *user_ai.key.as_ref() {
            msg!("close_position: caller is not the position owner");
            return Err(ProgramError::IllegalOwner);
        }
        if position.size == 0 {
            msg!("close_position: position already closed");
            return Err(ProgramError::InvalidArgument);
        }

        let equity = compute_equity(position, mark, funding_now);
        msg!(
            "close_position: size={} entry={} mark={} equity={}",
            position.size,
            position.entry_price,
            mark,
            equity
        );

        // Underwater closes pay 0 to the user. The deposited collateral
        // (in the vault) becomes a shortfall the protocol absorbs — the
        // insurance-fund hook in §11.6 is the right place to socialize it.
        payout = if equity < 0 { 0 } else { equity as u64 };

        // Close the position record. collateral=0 because the value has
        // been (or is about to be) paid out via the CPI below.
        position.collateral = 0;
        position.size = 0;
        position.entry_price = 0;
        position.funding_snapshot_index = funding_now;
    }

    // Vault → user transfer for the realized payout. Vault PDA signs via
    // invoke_signed with [b"vault_auth", market] seeds.
    spl_token_transfer_vault_signed(
        vault_token_ai,
        user_token_ai,
        vault_authority_ai,
        market_ai.key,
        vault_auth_bump,
        token_ai,
        payout,
    )?;

    msg!("close_position: closed. paid out {} to user", payout);
    Ok(())
}

/// Payload: empty.
///
/// Accounts:
///   0. `[SIGNER]` liquidator        — anyone; permissionless
///   1. `[WRITE]`  position
///   2. `[]`       oracle
///   3. `[]`       funding
///   4. `[]`       market            — for vault-PDA derivation
///   5. `[]`       mint              — quote-asset SPL Mint
///   6. `[WRITE]`  owner_token       — position owner's token account
///                                     (gets the remainder after penalty)
///   7. `[WRITE]`  liquidator_token  — liquidator's token account
///                                     (gets the penalty bounty)
///   8. `[WRITE]`  vault_token       — PDA at [b"vault", market, mint]
///   9. `[]`       vault_authority   — PDA at [b"vault_auth", market]
///  10. `[]`       token_program
fn process_liquidate(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != LIQUIDATE_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }

    let liquidator_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let position_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let oracle_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let funding_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let owner_token_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let liquidator_token_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_token_ai = accounts.get(8).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_authority_ai = accounts.get(9).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(10).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !liquidator_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if position_ai.owner != program_id || position_ai.data_len() != Position::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if owner_token_ai.owner != &SPL_TOKEN_PROGRAM_ID
        || liquidator_token_ai.owner != &SPL_TOKEN_PROGRAM_ID
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_vault_token_account(vault_token_ai, market_ai.key, mint_ai.key, program_id)?;
    let vault_auth_bump = verify_vault_authority(vault_authority_ai, market_ai.key, program_id)?;

    let mark = read_fresh_oracle(oracle_ai, program_id)?;
    let funding_now = read_funding_index(funding_ai, program_id)?;

    let penalty_amount: u64;
    let owner_amount: u64;
    {
        let mut data = position_ai.try_borrow_mut_data()?;
        let position: &mut Position =
            bytemuck::from_bytes_mut(&mut data[..Position::LEN]);

        if position.discriminator != POSITION_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if position.size == 0 {
            msg!("liquidate: position already closed");
            return Err(ProgramError::InvalidArgument);
        }

        let equity = compute_equity(position, mark, funding_now);
        let notional_val = notional(position.size, mark);
        let maint_required =
            (notional_val * (MAINT_MARGIN_BPS as u128) / 10_000) as i128;

        msg!(
            "liquidate: size={} mark={} equity={} maint_required={}",
            position.size,
            mark,
            equity,
            maint_required
        );

        if equity >= maint_required {
            msg!(
                "liquidate: position is healthy (equity {} >= maint {}), not liquidatable",
                equity,
                maint_required
            );
            return Err(ProgramError::InvalidArgument);
        }

        // Liquidator's penalty bounty (cap to LIQUIDATION_PENALTY_BPS of
        // notional, but never more than the equity that survives).
        let raw_penalty = (notional_val * (LIQUIDATION_PENALTY_BPS as u128) / 10_000)
            .min(i64::MAX as u128) as i128;
        let equity_positive = if equity < 0 { 0 } else { equity };
        let penalty = raw_penalty.min(equity_positive);
        let owner_remainder = (equity_positive - penalty).max(0);

        penalty_amount = penalty as u64;
        owner_amount = owner_remainder as u64;

        msg!(
            "liquidate: penalty={} (to {}), owner_remainder={} (to {})",
            penalty_amount,
            liquidator_ai.key,
            owner_amount,
            Pubkey::new_from_array(position.user)
        );

        position.collateral = 0;
        position.size = 0;
        position.entry_price = 0;
        position.funding_snapshot_index = funding_now;
    }

    // (a) Vault → liquidator transfer for the penalty.
    spl_token_transfer_vault_signed(
        vault_token_ai,
        liquidator_token_ai,
        vault_authority_ai,
        market_ai.key,
        vault_auth_bump,
        token_ai,
        penalty_amount,
    )?;

    // (b) Vault → position-owner transfer for whatever's left.
    spl_token_transfer_vault_signed(
        vault_token_ai,
        owner_token_ai,
        vault_authority_ai,
        market_ai.key,
        vault_auth_bump,
        token_ai,
        owner_amount,
    )?;

    Ok(())
}

// =============================================================================
// Trading vault — CreateTradingVault + VaultDeposit + VaultWithdraw +
//                 VaultUpdateNAV (Chapter 12).
// =============================================================================

const CREATE_TRADING_VAULT_PAYLOAD_LEN: usize = 0;
const VAULT_DEPOSIT_PAYLOAD_LEN: usize = 8; // assets u64
const VAULT_WITHDRAW_PAYLOAD_LEN: usize = 8; // shares u64
const VAULT_UPDATE_NAV_PAYLOAD_LEN: usize = 8; // new_total_assets u64

/// Accounts:
///   0. `[WRITE, SIGNER]` payer (typically == manager but doesn't have to)
///   1. `[]`              market
///   2. `[SIGNER]`        manager
///   3. `[]`              mint            — the asset mint (SPL Mint)
///   4. `[WRITE]`         vault           — PDA at [b"trading_vault", market, manager]
///   5. `[]`              system_program
fn process_create_trading_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_TRADING_VAULT_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }

    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let manager_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !manager_ai.is_signer {
        msg!("create_trading_vault: manager must sign to bind themselves to vault");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if market_ai.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    if mint_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_ai.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected, bump) = Pubkey::find_program_address(
        &[
            TRADING_VAULT_SEED,
            market_ai.key.as_ref(),
            manager_ai.key.as_ref(),
        ],
        program_id,
    );
    if vault_ai.key != &expected {
        return Err(ProgramError::InvalidSeeds);
    }

    let rent = Rent::get()?.minimum_balance(TradingVault::LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        vault_ai.key,
        rent,
        TradingVault::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), vault_ai.clone(), system_ai.clone()],
        &[&[
            TRADING_VAULT_SEED,
            market_ai.key.as_ref(),
            manager_ai.key.as_ref(),
            &[bump],
        ]],
    )?;

    let mut data = vault_ai.try_borrow_mut_data()?;
    let vault: &mut TradingVault = bytemuck::from_bytes_mut(&mut data[..TradingVault::LEN]);
    vault.discriminator = TRADING_VAULT_DISCRIMINATOR;
    vault.bump = bump;
    vault._pad0 = [0u8; 7];
    vault.market.copy_from_slice(market_ai.key.as_ref());
    vault.manager.copy_from_slice(manager_ai.key.as_ref());
    vault.mint.copy_from_slice(mint_ai.key.as_ref());
    vault.total_shares = 0;
    vault.total_assets = 0;
    vault._reserved = [0u8; 32];

    msg!("trading vault created (bump {})", bump);
    Ok(())
}

/// Payload: [assets u64 LE]
///
/// Accounts:
///   0. `[WRITE, SIGNER]` depositor (also pays rent for share account if new)
///   1. `[WRITE]`         vault            — TradingVault PDA
///   2. `[WRITE]`         share            — VaultShare PDA at [b"vault_share", vault, depositor]
///   3. `[]`              system_program
///   4. `[]`              market           — must match vault.market
///   5. `[]`              mint             — must match vault.mint
///   6. `[WRITE]`         depositor_token  — depositor's SPL Token account (source)
///   7. `[WRITE]`         vault_token      — per-(market, mint) vault PDA (destination)
///   8. `[]`              token_program
fn process_vault_deposit(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != VAULT_DEPOSIT_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let assets = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    if assets == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let depositor_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let share_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let depositor_token_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_token_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(8).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !depositor_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if vault_ai.owner != program_id || vault_ai.data_len() != TradingVault::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if system_ai.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if depositor_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("vault_deposit: depositor_token not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_vault_token_account(vault_token_ai, market_ai.key, mint_ai.key, program_id)?;

    let (expected_share, share_bump) = Pubkey::find_program_address(
        &[VAULT_SHARE_SEED, vault_ai.key.as_ref(), depositor_ai.key.as_ref()],
        program_id,
    );
    if share_ai.key != &expected_share {
        return Err(ProgramError::InvalidSeeds);
    }

    // (a) Read current vault state for share math + cross-check the
    //     market/mint the caller passed against the vault's record.
    let shares_to_mint: u64;
    {
        let mut vault_data = vault_ai.try_borrow_mut_data()?;
        let vault: &mut TradingVault =
            bytemuck::from_bytes_mut(&mut vault_data[..TradingVault::LEN]);
        if vault.discriminator != TRADING_VAULT_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if vault.market != *market_ai.key.as_ref() {
            msg!("vault_deposit: market account does not match vault.market");
            return Err(ProgramError::InvalidAccountData);
        }
        if vault.mint != *mint_ai.key.as_ref() {
            msg!("vault_deposit: mint account does not match vault.mint");
            return Err(ProgramError::InvalidAccountData);
        }

        // (b) Compute shares to mint. First deposit: 1:1. Subsequent:
        //     shares = assets * total_shares / total_assets.
        shares_to_mint = if vault.total_shares == 0 || vault.total_assets == 0 {
            assets
        } else {
            let numer = (assets as u128) * (vault.total_shares as u128);
            let s = numer / (vault.total_assets as u128);
            if s > u64::MAX as u128 {
                return Err(ProgramError::ArithmeticOverflow);
            }
            s as u64
        };
        if shares_to_mint == 0 {
            msg!("vault_deposit: deposit too small relative to NAV (would mint 0 shares)");
            return Err(ProgramError::InvalidArgument);
        }

        // (c) Update vault aggregate.
        vault.total_shares = vault
            .total_shares
            .checked_add(shares_to_mint)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        vault.total_assets = vault
            .total_assets
            .checked_add(assets)
            .ok_or(ProgramError::ArithmeticOverflow)?;
    }

    // (d) Create the share account if it doesn't exist yet, then update.
    let share_exists = share_ai.owner == program_id && share_ai.data_len() == VaultShare::LEN;
    if !share_exists {
        let rent = Rent::get()?.minimum_balance(VaultShare::LEN);
        let create_ix = system_instruction::create_account(
            depositor_ai.key,
            share_ai.key,
            rent,
            VaultShare::LEN as u64,
            program_id,
        );
        invoke_signed(
            &create_ix,
            &[depositor_ai.clone(), share_ai.clone(), system_ai.clone()],
            &[&[
                VAULT_SHARE_SEED,
                vault_ai.key.as_ref(),
                depositor_ai.key.as_ref(),
                &[share_bump],
            ]],
        )?;

        let mut share_data = share_ai.try_borrow_mut_data()?;
        let share: &mut VaultShare = bytemuck::from_bytes_mut(&mut share_data[..VaultShare::LEN]);
        share.discriminator = VAULT_SHARE_DISCRIMINATOR;
        share.bump = share_bump;
        share._pad0 = [0u8; 7];
        share.vault.copy_from_slice(vault_ai.key.as_ref());
        share.owner.copy_from_slice(depositor_ai.key.as_ref());
        share.shares = shares_to_mint;
        share.cost_basis = assets;
        share._reserved = [0u8; 32];
    } else {
        let mut share_data = share_ai.try_borrow_mut_data()?;
        let share: &mut VaultShare = bytemuck::from_bytes_mut(&mut share_data[..VaultShare::LEN]);
        if share.discriminator != VAULT_SHARE_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        share.shares = share
            .shares
            .checked_add(shares_to_mint)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        share.cost_basis = share
            .cost_basis
            .checked_add(assets)
            .ok_or(ProgramError::ArithmeticOverflow)?;
    }

    // (e) Escrow the depositor's tokens into the vault. Last step so an
    //     InsufficientFunds (or any SPL Token error) reverts the whole tx,
    //     including the share + aggregate updates done above.
    spl_token_transfer_user_signed(
        depositor_token_ai,
        vault_token_ai,
        depositor_ai,
        token_ai,
        assets,
    )?;

    msg!(
        "vault_deposit: deposited {} assets escrowed, minted {} shares",
        assets,
        shares_to_mint
    );
    Ok(())
}

/// Payload: [shares u64 LE]
///
/// Accounts:
///   0. `[SIGNER]` owner            — depositor whose shares are being burned
///   1. `[WRITE]`  vault
///   2. `[WRITE]`  share
///   3. `[]`       market           — must match vault.market
///   4. `[]`       mint             — must match vault.mint
///   5. `[WRITE]`  owner_token      — destination of the asset withdrawal
///   6. `[WRITE]`  vault_token      — per-(market, mint) vault PDA
///   7. `[]`       vault_authority  — PDA at [b"vault_auth", market]
///   8. `[]`       token_program
fn process_vault_withdraw(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != VAULT_WITHDRAW_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let shares_to_burn = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    if shares_to_burn == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let owner_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let share_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let owner_token_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_token_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_authority_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(8).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !owner_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if vault_ai.owner != program_id || vault_ai.data_len() != TradingVault::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if share_ai.owner != program_id || share_ai.data_len() != VaultShare::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if owner_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_vault_token_account(vault_token_ai, market_ai.key, mint_ai.key, program_id)?;
    let vault_auth_bump = verify_vault_authority(vault_authority_ai, market_ai.key, program_id)?;

    let assets_to_return: u64;
    {
        // Compute assets to return: shares_to_burn * total_assets / total_shares.
        let mut vault_data = vault_ai.try_borrow_mut_data()?;
        let vault: &mut TradingVault =
            bytemuck::from_bytes_mut(&mut vault_data[..TradingVault::LEN]);
        if vault.discriminator != TRADING_VAULT_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if vault.total_shares == 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        if vault.market != *market_ai.key.as_ref() {
            return Err(ProgramError::InvalidAccountData);
        }
        if vault.mint != *mint_ai.key.as_ref() {
            return Err(ProgramError::InvalidAccountData);
        }

        assets_to_return = {
            let numer = (shares_to_burn as u128) * (vault.total_assets as u128);
            let a = numer / (vault.total_shares as u128);
            if a > u64::MAX as u128 {
                return Err(ProgramError::ArithmeticOverflow);
            }
            a as u64
        };

        let mut share_data = share_ai.try_borrow_mut_data()?;
        let share: &mut VaultShare =
            bytemuck::from_bytes_mut(&mut share_data[..VaultShare::LEN]);
        if share.discriminator != VAULT_SHARE_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if share.owner != *owner_ai.key.as_ref() {
            msg!("vault_withdraw: caller is not the share owner");
            return Err(ProgramError::IllegalOwner);
        }
        if share.shares < shares_to_burn {
            msg!(
                "vault_withdraw: insufficient shares ({} < {})",
                share.shares,
                shares_to_burn
            );
            return Err(ProgramError::InsufficientFunds);
        }

        share.shares -= shares_to_burn;
        // Reduce cost basis proportionally so partial withdrawals don't
        // overstate gains on subsequent reports.
        let basis_reduction = (((shares_to_burn as u128) * (share.cost_basis as u128))
            / (share.shares as u128 + shares_to_burn as u128)) as u64;
        share.cost_basis = share.cost_basis.saturating_sub(basis_reduction);

        vault.total_shares -= shares_to_burn;
        vault.total_assets = vault.total_assets.saturating_sub(assets_to_return);
    }

    // Vault → owner SPL Token Transfer. Vault authority PDA signs via
    // invoke_signed with [VAULT_AUTH_SEED, market] seeds.
    spl_token_transfer_vault_signed(
        vault_token_ai,
        owner_token_ai,
        vault_authority_ai,
        market_ai.key,
        vault_auth_bump,
        token_ai,
        assets_to_return,
    )?;

    msg!(
        "vault_withdraw: burned {} shares, returned {} assets",
        shares_to_burn,
        assets_to_return
    );
    Ok(())
}

/// Payload: [new_total_assets u64 LE]
///
/// Accounts:
///   0. `[SIGNER]` manager
///   1. `[WRITE]`  vault
fn process_vault_update_nav(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != VAULT_UPDATE_NAV_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let new_total_assets = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));

    let manager_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let vault_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !manager_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if vault_ai.owner != program_id || vault_ai.data_len() != TradingVault::LEN {
        return Err(ProgramError::InvalidAccountData);
    }

    let mut vault_data = vault_ai.try_borrow_mut_data()?;
    let vault: &mut TradingVault = bytemuck::from_bytes_mut(&mut vault_data[..TradingVault::LEN]);
    if vault.discriminator != TRADING_VAULT_DISCRIMINATOR {
        return Err(ProgramError::UninitializedAccount);
    }
    if vault.manager != *manager_ai.key.as_ref() {
        msg!("vault_update_nav: caller is not the vault manager");
        return Err(ProgramError::IllegalOwner);
    }

    let prev = vault.total_assets;
    vault.total_assets = new_total_assets;

    msg!(
        "vault_update_nav: total_assets {} -> {} (shares unchanged at {})",
        prev,
        new_total_assets,
        vault.total_shares
    );
    Ok(())
}

// =============================================================================
// Builder codes — RegisterBuilder + PlaceOrderWithBuilder + ClaimBuilderFees
// (Chapter 13).
// =============================================================================
//
// A "builder" is a frontend / aggregator / market-maker that routes orders
// to this program. In exchange, the program credits them a configurable
// fraction of the protocol fee earned on those orders.

const REGISTER_BUILDER_PAYLOAD_LEN: usize = 8; // max_fee_share_bps u64
const PLACE_ORDER_WITH_BUILDER_PAYLOAD_LEN: usize = 1 + 8 + 8; // same as PlaceOrder
const CLAIM_BUILDER_FEES_PAYLOAD_LEN: usize = 0;

/// Payload: [max_fee_share_bps u64 LE]
///
/// Accounts:
///   0. `[WRITE, SIGNER]` payer        — also implicitly the builder
///   1. `[WRITE]`         builder_profile — PDA at [b"builder", payer]
///   2. `[]`              system_program
fn process_register_builder(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != REGISTER_BUILDER_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut max_share = u64::from_le_bytes(payload[0..8].try_into().expect("8 bytes"));
    if max_share > PROTOCOL_BUILDER_SHARE_CAP_BPS {
        msg!(
            "register_builder: requested {} bps clamped to protocol cap {} bps",
            max_share,
            PROTOCOL_BUILDER_SHARE_CAP_BPS
        );
        max_share = PROTOCOL_BUILDER_SHARE_CAP_BPS;
    }

    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let profile_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if system_ai.key != &system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected, bump) =
        Pubkey::find_program_address(&[BUILDER_PROFILE_SEED, payer_ai.key.as_ref()], program_id);
    if profile_ai.key != &expected {
        return Err(ProgramError::InvalidSeeds);
    }

    let rent = Rent::get()?.minimum_balance(BuilderProfile::LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        profile_ai.key,
        rent,
        BuilderProfile::LEN as u64,
        program_id,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), profile_ai.clone(), system_ai.clone()],
        &[&[BUILDER_PROFILE_SEED, payer_ai.key.as_ref(), &[bump]]],
    )?;

    let mut data = profile_ai.try_borrow_mut_data()?;
    let profile: &mut BuilderProfile =
        bytemuck::from_bytes_mut(&mut data[..BuilderProfile::LEN]);
    profile.discriminator = BUILDER_PROFILE_DISCRIMINATOR;
    profile.bump = bump;
    profile._pad0 = [0u8; 7];
    profile.builder.copy_from_slice(payer_ai.key.as_ref());
    profile.max_fee_share_bps = max_share;
    profile.accumulated_fees = 0;
    profile.total_volume = 0;
    profile._reserved = [0u8; 32];

    msg!(
        "register_builder: registered {} with max_fee_share_bps={}",
        payer_ai.key,
        max_share
    );
    Ok(())
}

/// Payload (same as PlaceOrderChecked): [side u8][price u64 LE][size u64 LE]
///
/// Accounts: same prefix as PlaceOrderChecked, plus the builder profile.
///   0. `[WRITE, SIGNER]` user
///   1. `[WRITE]`         book
///   2. `[]`              oracle
///   3. `[]`              market
///   4. `[]`              mint            — quote_mint
///   5. `[WRITE]`         user_token      — fee source
///   6. `[WRITE]`         fee_vault_token — fee sink
///   7. `[]`              token_program
///   8. `[WRITE]`         builder_profile — credited with builder fee share
fn process_place_order_with_builder(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != PLACE_ORDER_WITH_BUILDER_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let order_side = payload[0];
    let price = u64::from_le_bytes(payload[1..9].try_into().expect("8 bytes"));
    let size = u64::from_le_bytes(payload[9..17].try_into().expect("8 bytes"));

    if order_side != side::BID && order_side != side::ASK {
        return Err(ProgramError::InvalidInstructionData);
    }
    if price == 0 || size == 0 {
        return Err(ProgramError::InvalidInstructionData);
    }

    let user_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let book_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let oracle_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let market_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let user_token_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let fee_vault_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let builder_profile_ai = accounts.get(8).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !user_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if book_ai.owner != program_id || book_ai.data_len() != OrderBook::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if market_ai.owner != program_id || market_ai.data_len() != Market::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if mint_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if user_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("place_order_with_builder: user_token not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if builder_profile_ai.owner != program_id
        || builder_profile_ai.data_len() != BuilderProfile::LEN
    {
        return Err(ProgramError::InvalidAccountData);
    }

    // Cross-check the (book, oracle, mint) chain against the passed market,
    // same gauntlet as PlaceOrderChecked.
    let (expected_book, _) =
        Pubkey::find_program_address(&[BOOK_SEED, market_ai.key.as_ref()], program_id);
    if book_ai.key != &expected_book {
        msg!("place_order_with_builder: book does not descend from passed market");
        return Err(ProgramError::InvalidSeeds);
    }
    let (expected_oracle, _) =
        Pubkey::find_program_address(&[ORACLE_SEED, market_ai.key.as_ref()], program_id);
    if oracle_ai.key != &expected_oracle {
        msg!("place_order_with_builder: oracle does not descend from passed market");
        return Err(ProgramError::InvalidSeeds);
    }
    {
        let market_data = market_ai.try_borrow_data()?;
        let market: &Market = bytemuck::from_bytes(&market_data[..Market::LEN]);
        if market.discriminator != MARKET_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if market.quote_mint != *mint_ai.key.as_ref() {
            msg!("place_order_with_builder: mint != market.quote_mint");
            return Err(ProgramError::InvalidAccountData);
        }
    }
    verify_fee_vault_token_account(fee_vault_ai, mint_ai.key, program_id)?;

    // (1) Same oracle staleness + sanity-band gauntlet as PlaceOrderChecked.
    let mark = read_fresh_oracle(oracle_ai, program_id)?;
    let band = mark.saturating_mul(SANITY_BAND_BPS) / 10_000;
    let low = mark.saturating_sub(band);
    let high = mark.saturating_add(band);
    if price < low || price > high {
        return Err(ProgramError::InvalidArgument);
    }

    // (2) Compute fees.
    //
    // Notional = price × size (in the smallest quote units our integer
    // math can express). Protocol fee = notional × PROTOCOL_FEE_BPS / 10000.
    // Builder share = protocol_fee × min(builder.max_fee_share_bps,
    //                                    PROTOCOL_BUILDER_SHARE_CAP_BPS) / 10000.
    let notional_val = (price as u128) * (size as u128);
    let protocol_fee = (notional_val * (PROTOCOL_FEE_BPS as u128) / 10_000) as u64;

    let mut share_bps;
    let mut builder_pubkey = [0u8; 32];
    let new_volume: u64;
    {
        let mut profile_data = builder_profile_ai.try_borrow_mut_data()?;
        let profile: &mut BuilderProfile =
            bytemuck::from_bytes_mut(&mut profile_data[..BuilderProfile::LEN]);
        if profile.discriminator != BUILDER_PROFILE_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        share_bps = profile.max_fee_share_bps;
        if share_bps > PROTOCOL_BUILDER_SHARE_CAP_BPS {
            // Defensive: registered profile shouldn't exceed cap, but the
            // cap could have been lowered since the builder registered.
            share_bps = PROTOCOL_BUILDER_SHARE_CAP_BPS;
        }
        let builder_share =
            ((protocol_fee as u128) * (share_bps as u128) / 10_000) as u64;
        profile.accumulated_fees = profile
            .accumulated_fees
            .checked_add(builder_share)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        new_volume = profile
            .total_volume
            .checked_add(size)
            .ok_or(ProgramError::ArithmeticOverflow)?;
        profile.total_volume = new_volume;
        builder_pubkey.copy_from_slice(&profile.builder);

        msg!(
            "place_order_with_builder: notional={} protocol_fee={} builder_share={} ({} bps to {})",
            notional_val,
            protocol_fee,
            builder_share,
            share_bps,
            Pubkey::new_from_array(builder_pubkey)
        );
    }

    // (3) Place the order — same write logic as PlaceOrder / PlaceOrderChecked.
    {
        let mut data = book_ai.try_borrow_mut_data()?;
        let book: &mut OrderBook = bytemuck::from_bytes_mut(&mut data[..OrderBook::LEN]);
        if book.discriminator != ORDER_BOOK_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }

        let mut chosen_slot: Option<usize> = None;
        for (i, slot) in book.slots.iter().enumerate() {
            if slot.size == 0 {
                chosen_slot = Some(i);
                break;
            }
        }
        let slot_idx = chosen_slot.ok_or(ProgramError::AccountDataTooSmall)?;

        let order_id = book.next_order_id;
        book.next_order_id = book.next_order_id.saturating_add(1);
        book.active_count = book.active_count.saturating_add(1);

        let mut owner = [0u8; 32];
        owner.copy_from_slice(user_ai.key.as_ref());

        book.slots[slot_idx] = Order {
            order_id,
            price,
            size,
            owner,
            side: order_side,
            _pad: [0u8; 7],
        };

        msg!(
            "place_order_with_builder: placed order_id={} into slot {}",
            order_id,
            slot_idx
        );
    }

    // (4) Escrow the full protocol fee to the fee vault. The
    // (protocol_fee - builder_share) remainder stays here as the protocol's
    // take; the builder's share is later released to them via
    // ClaimBuilderFees. Placed last so a transfer failure reverts both
    // the builder credit and the order write atomically.
    spl_token_transfer_user_signed(
        user_token_ai,
        fee_vault_ai,
        user_ai,
        token_ai,
        protocol_fee,
    )?;

    let _ = (share_bps, builder_pubkey, new_volume); // log fields kept for the chapter walkthrough
    Ok(())
}

/// Payload: empty.
///
/// Accounts:
///   0. `[SIGNER]` builder
///   1. `[WRITE]`  builder_profile
///   2. `[]`       mint                — quote_mint (used to derive fee vault PDAs)
///   3. `[WRITE]`  fee_vault_token     — source (per-(quote_mint) PDA)
///   4. `[]`       fee_vault_authority — signer PDA via invoke_signed
///   5. `[WRITE]`  builder_token       — builder's quote-token account (destination)
///   6. `[]`       token_program
fn process_claim_builder_fees(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CLAIM_BUILDER_FEES_PAYLOAD_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }

    let builder_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let profile_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let fee_vault_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let fee_vault_auth_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let builder_token_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(6).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !builder_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if profile_ai.owner != program_id || profile_ai.data_len() != BuilderProfile::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    if mint_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if builder_token_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("claim_builder_fees: builder_token not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    verify_fee_vault_token_account(fee_vault_ai, mint_ai.key, program_id)?;
    let fee_vault_auth_bump = verify_fee_vault_authority(fee_vault_auth_ai, mint_ai.key, program_id)?;

    let claimed: u64;
    {
        let mut data = profile_ai.try_borrow_mut_data()?;
        let profile: &mut BuilderProfile =
            bytemuck::from_bytes_mut(&mut data[..BuilderProfile::LEN]);
        if profile.discriminator != BUILDER_PROFILE_DISCRIMINATOR {
            return Err(ProgramError::UninitializedAccount);
        }
        if profile.builder != *builder_ai.key.as_ref() {
            msg!("claim_builder_fees: caller is not the registered builder");
            return Err(ProgramError::IllegalOwner);
        }

        claimed = profile.accumulated_fees;
        profile.accumulated_fees = 0;
    }

    msg!(
        "claim_builder_fees: builder {} claiming {} units from fee vault",
        builder_ai.key,
        claimed
    );

    // PDA-signed Transfer from fee_vault → builder_token. Structurally
    // identical to §12.4's VaultWithdraw — same invoke_signed pattern,
    // same InvalidSeeds protection, different seeds (per-quote_mint rather
    // than per-market).
    spl_token_transfer_fee_vault_signed(
        fee_vault_ai,
        builder_token_ai,
        fee_vault_auth_ai,
        mint_ai.key,
        fee_vault_auth_bump,
        token_ai,
        claimed,
    )?;

    Ok(())
}

const CREATE_FEE_VAULT_PAYLOAD_LEN: usize = 0;

/// Allocate the per-(quote_mint) fee_vault token account and initialize
/// it under the fee_vault_authority PDA. Mirrors `process_create_vault`
/// (Chapter 6) but keyed on quote_mint rather than (market, mint). One
/// fee vault per quote currency; ours is single-quote so one call.
///
/// Payload: empty.
///
/// Accounts:
///   0. `[WRITE, SIGNER]` payer
///   1. `[]`              quote_mint            — owned by SPL Token
///   2. `[WRITE]`         fee_vault             — PDA at [b"fee_vault", quote_mint]
///   3. `[]`              fee_vault_authority   — PDA at [b"fee_vault_auth", quote_mint]
///   4. `[]`              system_program
///   5. `[]`              spl_token_program
fn process_create_fee_vault(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    payload: &[u8],
) -> ProgramResult {
    if payload.len() != CREATE_FEE_VAULT_PAYLOAD_LEN {
        msg!("create_fee_vault: payload must be empty");
        return Err(ProgramError::InvalidInstructionData);
    }

    let payer_ai = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint_ai = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let fee_vault_ai = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let fee_vault_auth_ai = accounts.get(3).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let system_ai = accounts.get(4).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let token_ai = accounts.get(5).ok_or(ProgramError::NotEnoughAccountKeys)?;

    if !payer_ai.is_signer {
        msg!("create_fee_vault: payer must sign");
        return Err(ProgramError::MissingRequiredSignature);
    }
    if mint_ai.owner != &SPL_TOKEN_PROGRAM_ID {
        msg!("create_fee_vault: mint not owned by SPL Token");
        return Err(ProgramError::IncorrectProgramId);
    }
    if system_ai.key != &system_program::ID {
        msg!("create_fee_vault: account[4] is not the System program");
        return Err(ProgramError::IncorrectProgramId);
    }
    if token_ai.key != &SPL_TOKEN_PROGRAM_ID {
        msg!("create_fee_vault: account[5] is not the SPL Token program");
        return Err(ProgramError::IncorrectProgramId);
    }

    let (expected_vault, fee_vault_bump) =
        Pubkey::find_program_address(&[FEE_VAULT_SEED, mint_ai.key.as_ref()], program_id);
    if fee_vault_ai.key != &expected_vault {
        msg!(
            "create_fee_vault: passed fee_vault {} != derived PDA {}",
            fee_vault_ai.key,
            expected_vault
        );
        return Err(ProgramError::InvalidSeeds);
    }
    let (expected_auth, _auth_bump) =
        Pubkey::find_program_address(&[FEE_VAULT_AUTH_SEED, mint_ai.key.as_ref()], program_id);
    if fee_vault_auth_ai.key != &expected_auth {
        msg!(
            "create_fee_vault: passed fee_vault_authority {} != derived PDA {}",
            fee_vault_auth_ai.key,
            expected_auth
        );
        return Err(ProgramError::InvalidSeeds);
    }

    // (1) System CPI: allocate the token account, owned by SPL Token Program.
    // The new account is the fee_vault PDA, so we sign with its seeds + bump.
    let rent = Rent::get()?.minimum_balance(TOKEN_ACCOUNT_LEN);
    let create_ix = system_instruction::create_account(
        payer_ai.key,
        fee_vault_ai.key,
        rent,
        TOKEN_ACCOUNT_LEN as u64,
        &SPL_TOKEN_PROGRAM_ID,
    );
    invoke_signed(
        &create_ix,
        &[payer_ai.clone(), fee_vault_ai.clone(), system_ai.clone()],
        &[&[FEE_VAULT_SEED, mint_ai.key.as_ref(), &[fee_vault_bump]]],
    )?;

    // (2) SPL Token CPI: InitializeAccount3 with fee_vault_authority as owner.
    let mut init_data = Vec::with_capacity(1 + 32);
    init_data.push(spl_token_ix::INITIALIZE_ACCOUNT_3);
    init_data.extend_from_slice(fee_vault_auth_ai.key.as_ref());
    let init_ix = Instruction {
        program_id: SPL_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*fee_vault_ai.key, false),
            AccountMeta::new_readonly(*mint_ai.key, false),
        ],
        data: init_data,
    };
    invoke(
        &init_ix,
        &[fee_vault_ai.clone(), mint_ai.clone(), token_ai.clone()],
    )?;

    msg!("fee vault created (fee_vault bump {})", fee_vault_bump);
    Ok(())
}
