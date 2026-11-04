//! Builder fees are a bare number of quote units and anyone can create a
//! market with any quote mint, so a builder fixes its fee mint when it
//! registers: only orders quoted in that mint credit the profile, and fees
//! are claimed only from that mint's fee vault. Each case runs the real
//! handler natively (CPIs, the clock and rent are stubbed).

use bytemuck::Zeroable;
use openhl_core::{
    process_instruction, BOOK_SEED, BUILDER_PROFILE_SEED, FEE_VAULT_AUTH_SEED, FEE_VAULT_SEED,
    ORACLE_SEED, SPL_TOKEN_PROGRAM_ID,
};
use openhl_state::*;
use solana_program::{
    account_info::AccountInfo, clock::Clock, entrypoint::ProgramResult, instruction::Instruction,
    program_error::ProgramError, program_stubs, pubkey::Pubkey, rent::Rent, system_program,
};

const NOW_SLOT: u64 = 1_000;

/// Clock at NOW_SLOT, default rent; every CPI succeeds.
struct Stubs;
impl program_stubs::SyscallStubs for Stubs {
    fn sol_get_clock_sysvar(&self, var_addr: *mut u8) -> u64 {
        let clock = Clock {
            slot: NOW_SLOT,
            ..Clock::default()
        };
        unsafe { *(var_addr as *mut Clock) = clock };
        0
    }
    fn sol_get_rent_sysvar(&self, var_addr: *mut u8) -> u64 {
        unsafe { *(var_addr as *mut Rent) = Rent::default() };
        0
    }
    fn sol_invoke_signed(
        &self,
        _ix: &Instruction,
        _infos: &[AccountInfo],
        _seeds: &[&[&[u8]]],
    ) -> ProgramResult {
        Ok(())
    }
}

fn stubs() {
    program_stubs::set_syscall_stubs(Box::new(Stubs));
}

struct Acc {
    key: Pubkey,
    owner: Pubkey,
    lamports: u64,
    data: Vec<u8>,
    signer: bool,
}

fn acc(key: Pubkey, owner: Pubkey, data: Vec<u8>) -> Acc {
    Acc {
        key,
        owner,
        lamports: 1_000_000,
        data,
        signer: false,
    }
}

fn signer(key: Pubkey) -> Acc {
    Acc {
        signer: true,
        ..acc(key, Pubkey::default(), vec![])
    }
}

fn pod<T: bytemuck::Pod>(v: T) -> Vec<u8> {
    bytemuck::bytes_of(&v).to_vec()
}

fn pda(seeds: &[&[u8]], program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program_id).0
}

fn run(program_id: &Pubkey, accs: &mut [Acc], ix: &[u8]) -> ProgramResult {
    let infos: Vec<AccountInfo> = accs
        .iter_mut()
        .map(|a| {
            AccountInfo::new(
                &a.key,
                a.signer,
                true,
                &mut a.lamports,
                &mut a.data,
                &a.owner,
                false,
                0,
            )
        })
        .collect();
    process_instruction(program_id, &infos, ix)
}

fn profile(builder: &Pubkey, accumulated: u64, fee_mint: Option<&Pubkey>) -> BuilderProfile {
    let mut p = BuilderProfile::zeroed();
    p.discriminator = BUILDER_PROFILE_DISCRIMINATOR;
    p.builder = builder.to_bytes();
    p.max_fee_share_bps = 5000;
    p.accumulated_fees = accumulated;
    if let Some(m) = fee_mint {
        p.fee_mint = m.to_bytes();
    }
    p
}

/// ClaimBuilderFees (tag 25) from `claim_mint`'s fee vault.
fn claim(p: BuilderProfile, claim_mint: &Pubkey) -> ProgramResult {
    let program_id = Pubkey::new_unique();
    let pid = &program_id;
    let builder = Pubkey::new_from_array(p.builder);
    let mut accs = vec![
        signer(builder),
        acc(Pubkey::new_unique(), *pid, pod(p)),
        acc(*claim_mint, SPL_TOKEN_PROGRAM_ID, vec![]),
        acc(
            pda(&[FEE_VAULT_SEED, claim_mint.as_ref()], pid),
            SPL_TOKEN_PROGRAM_ID,
            vec![],
        ),
        acc(
            pda(&[FEE_VAULT_AUTH_SEED, claim_mint.as_ref()], pid),
            Pubkey::default(),
            vec![],
        ),
        acc(Pubkey::new_unique(), SPL_TOKEN_PROGRAM_ID, vec![0u8; 165]),
        acc(SPL_TOKEN_PROGRAM_ID, Pubkey::default(), vec![]),
    ];
    run(pid, &mut accs, &[25])
}

/// PlaceOrderWithBuilder (tag 24) on a market quoted in `quote`; returns the
/// result and the profile afterwards.
fn place(p: BuilderProfile, quote: &Pubkey) -> (ProgramResult, BuilderProfile) {
    let program_id = Pubkey::new_unique();
    let pid = &program_id;
    let market = Pubkey::new_unique();
    let mut m = Market::zeroed();
    m.discriminator = MARKET_DISCRIMINATOR;
    m.quote_mint = quote.to_bytes();
    let mut book = OrderBook::zeroed();
    book.discriminator = ORDER_BOOK_DISCRIMINATOR;
    let mut o = Oracle::zeroed();
    o.discriminator = ORACLE_DISCRIMINATOR;
    o.price = 100;
    o.publish_slot = NOW_SLOT;
    let mut accs = vec![
        signer(Pubkey::new_unique()),
        acc(pda(&[BOOK_SEED, market.as_ref()], pid), *pid, pod(book)),
        acc(pda(&[ORACLE_SEED, market.as_ref()], pid), *pid, pod(o)),
        acc(market, *pid, pod(m)),
        acc(*quote, SPL_TOKEN_PROGRAM_ID, vec![]),
        acc(Pubkey::new_unique(), SPL_TOKEN_PROGRAM_ID, vec![0u8; 165]),
        acc(
            pda(&[FEE_VAULT_SEED, quote.as_ref()], pid),
            SPL_TOKEN_PROGRAM_ID,
            vec![],
        ),
        acc(SPL_TOKEN_PROGRAM_ID, Pubkey::default(), vec![]),
        acc(Pubkey::new_unique(), *pid, pod(p)),
    ];
    let mut ix = vec![24u8, side::BID];
    ix.extend_from_slice(&100u64.to_le_bytes());
    ix.extend_from_slice(&1_000u64.to_le_bytes());
    let r = run(pid, &mut accs, &ix);
    let after = *bytemuck::from_bytes::<BuilderProfile>(&accs[8].data);
    (r, after)
}

/// RegisterBuilder (tag 23) with `fee_mint`; returns the new profile.
fn register(builder: &Pubkey, fee_mint: &Pubkey) -> (ProgramResult, BuilderProfile) {
    let program_id = Pubkey::new_unique();
    let pid = &program_id;
    let mut accs = vec![
        signer(*builder),
        acc(
            pda(&[BUILDER_PROFILE_SEED, builder.as_ref()], pid),
            *pid,
            vec![0u8; BuilderProfile::LEN],
        ),
        acc(system_program::ID, Pubkey::default(), vec![]),
        acc(*fee_mint, SPL_TOKEN_PROGRAM_ID, vec![]),
    ];
    let mut ix = vec![23u8];
    ix.extend_from_slice(&3000u64.to_le_bytes());
    let r = run(pid, &mut accs, &ix);
    let after = *bytemuck::from_bytes::<BuilderProfile>(&accs[1].data);
    (r, after)
}

#[test]
fn registration_fixes_the_fee_mint() {
    stubs();
    let builder = Pubkey::new_unique();
    let usdc = Pubkey::new_unique();
    let (r, p) = register(&builder, &usdc);
    assert_eq!(r, Ok(()));
    assert_eq!(p.fee_mint, usdc.to_bytes());
}

#[test]
fn only_orders_quoted_in_the_fee_mint_credit_the_profile() {
    stubs();
    let builder = Pubkey::new_unique();
    let usdc = Pubkey::new_unique();
    let (r, after) = place(profile(&builder, 0, Some(&usdc)), &usdc);
    assert_eq!(r, Ok(()));
    assert!(after.accumulated_fees > 0);
    // A market quoted in another (say, worthless) mint is refused, so
    // nobody can credit the profile in it, or pin it to it.
    assert_eq!(
        place(profile(&builder, 0, Some(&usdc)), &Pubkey::new_unique()).0,
        Err(ProgramError::InvalidAccountData)
    );
    // A profile with no fee mint takes no credit.
    let junk = Pubkey::new_unique();
    assert_eq!(
        place(profile(&builder, 0, None), &junk).0,
        Err(ProgramError::InvalidAccountData)
    );
}

#[test]
fn fees_are_claimed_only_from_the_fee_mint() {
    stubs();
    let builder = Pubkey::new_unique();
    let junk = Pubkey::new_unique();
    let usdc = Pubkey::new_unique();
    assert_eq!(claim(profile(&builder, 500, Some(&junk)), &junk), Ok(()));
    assert_eq!(
        claim(profile(&builder, 500, Some(&junk)), &usdc),
        Err(ProgramError::InvalidAccountData)
    );
    // A profile with no fee mint (or a balance from before the fee mint
    // existed) can't be claimed from any vault.
    assert_eq!(
        claim(profile(&builder, 500, None), &usdc),
        Err(ProgramError::InvalidAccountData)
    );
}
