//! Mint binding: collateral must be escrowed in the market's quote mint.
//! A position does not record its mint, and ClosePosition / Liquidate pay
//! out of the quote-mint vault (the insurance fund pins that mint). So if
//! OpenPosition took any mint, a user could escrow a worthless mint and
//! close against the quote-mint vault. Each case runs the real handler
//! natively (CPIs, the clock and rent are stubbed).

use bytemuck::Zeroable;
use openhl_core::{
    process_instruction, FUNDING_SEED, ORACLE_SEED, POSITION_SEED, SPL_TOKEN_PROGRAM_ID, VAULT_SEED,
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

/// OpenPosition (tag 16): a 10 long with 1,000 collateral at mark 100,
/// escrowed in `mint`, against a market whose data is `market_data`.
fn open(market_data: Vec<u8>, mint: Pubkey) -> ProgramResult {
    let program_id = Pubkey::new_unique();
    let p = &program_id;
    let user = Pubkey::new_unique();
    let market = Pubkey::new_unique();

    let mut o = Oracle::zeroed();
    o.discriminator = ORACLE_DISCRIMINATOR;
    o.price = 100;
    o.publish_slot = NOW_SLOT;
    let mut f = FundingState::zeroed();
    f.discriminator = FUNDING_DISCRIMINATOR;

    let mut accs = vec![
        Acc {
            signer: true,
            ..acc(user, Pubkey::default(), vec![])
        },
        acc(market, *p, market_data),
        acc(
            pda(&[POSITION_SEED, user.as_ref(), market.as_ref()], p),
            *p,
            vec![0u8; Position::LEN],
        ),
        acc(pda(&[ORACLE_SEED, market.as_ref()], p), *p, pod(o)),
        acc(pda(&[FUNDING_SEED, market.as_ref()], p), *p, pod(f)),
        acc(mint, SPL_TOKEN_PROGRAM_ID, vec![]),
        acc(Pubkey::new_unique(), SPL_TOKEN_PROGRAM_ID, vec![0u8; 165]),
        acc(
            pda(&[VAULT_SEED, market.as_ref(), mint.as_ref()], p),
            SPL_TOKEN_PROGRAM_ID,
            vec![],
        ),
        acc(system_program::ID, Pubkey::default(), vec![]),
        acc(SPL_TOKEN_PROGRAM_ID, Pubkey::default(), vec![]),
    ];
    let mut ix = vec![16u8];
    ix.extend_from_slice(&10i64.to_le_bytes());
    ix.extend_from_slice(&1_000u64.to_le_bytes());
    run(p, &mut accs, &ix)
}

fn market_with_quote(quote: &Pubkey) -> Vec<u8> {
    let mut m = Market::zeroed();
    m.discriminator = MARKET_DISCRIMINATOR;
    m.quote_mint = quote.to_bytes();
    pod(m)
}

#[test]
fn open_position_escrows_only_the_markets_quote_mint() {
    stubs();
    let quote = Pubkey::new_unique();
    assert_eq!(open(market_with_quote(&quote), quote), Ok(()));
    assert_eq!(
        open(market_with_quote(&quote), Pubkey::new_unique()),
        Err(ProgramError::InvalidAccountData)
    );
}

#[test]
fn open_position_refuses_an_account_that_is_not_a_market() {
    stubs();
    let quote = Pubkey::new_unique();
    // Program-owned but not a Market: a zeroed Market-sized account, and
    // an account of another type.
    assert_eq!(
        open(vec![0u8; Market::LEN], quote),
        Err(ProgramError::UninitializedAccount)
    );
    assert_eq!(
        open(pod(InsuranceFund::zeroed()), quote),
        Err(ProgramError::InvalidAccountData)
    );
}
