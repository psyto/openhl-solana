//! CreateSlab (tag 29) only initializes a Slab account the client created in
//! the same transaction (the ~82 KB Slab is too big for a CPI create). Each
//! case runs the real handler natively and shows which accounts it refuses.

use bytemuck::Zeroable;
use openhl_core::process_instruction;
use openhl_state::*;
use solana_program::{
    account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
    program_stubs, pubkey::Pubkey, rent::Rent,
};

/// Rent sysvar = the default rent; nothing else is called.
struct Stubs;
impl program_stubs::SyscallStubs for Stubs {
    fn sol_get_rent_sysvar(&self, var_addr: *mut u8) -> u64 {
        unsafe { *(var_addr as *mut Rent) = Rent::default() };
        0
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
    writable: bool,
}

fn run(program_id: &Pubkey, accs: &mut [Acc], ix: &[u8]) -> ProgramResult {
    let infos: Vec<AccountInfo> = accs
        .iter_mut()
        .map(|a| {
            AccountInfo::new(
                &a.key,
                a.signer,
                a.writable,
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

/// [slab, market]: a fresh, rent-exempt, program-owned slab signed by its
/// keypair, and a real market.
fn accounts(program_id: &Pubkey) -> Vec<Acc> {
    let mut m = Market::zeroed();
    m.discriminator = MARKET_DISCRIMINATOR;
    vec![
        Acc {
            key: Pubkey::new_unique(),
            owner: *program_id,
            lamports: Rent::default().minimum_balance(Slab::LEN),
            data: vec![0u8; Slab::LEN],
            signer: true,
            writable: true,
        },
        Acc {
            key: Pubkey::new_unique(),
            owner: *program_id,
            lamports: 1_000_000,
            data: bytemuck::bytes_of(&m).to_vec(),
            signer: false,
            writable: false,
        },
    ]
}

const CREATE: [u8; 1] = [29];

fn place_bid(user: Pubkey, slab: &mut Acc, program_id: &Pubkey) -> ProgramResult {
    let mut ix = vec![30u8, side::BID];
    ix.extend_from_slice(&100u64.to_le_bytes());
    ix.extend_from_slice(&5u64.to_le_bytes());
    let mut user = Acc {
        key: user,
        owner: Pubkey::default(),
        lamports: 0,
        data: vec![],
        signer: true,
        writable: false,
    };
    let infos = vec![
        AccountInfo::new(
            &user.key,
            true,
            false,
            &mut user.lamports,
            &mut user.data,
            &user.owner,
            false,
            0,
        ),
        AccountInfo::new(
            &slab.key,
            false,
            true,
            &mut slab.lamports,
            &mut slab.data,
            &slab.owner,
            false,
            0,
        ),
    ];
    process_instruction(program_id, &infos, &ix)
}

fn slab(a: &Acc) -> &Slab {
    bytemuck::from_bytes(&a.data[..Slab::LEN])
}

#[test]
fn initializes_a_client_created_slab() {
    stubs();
    let program_id = Pubkey::new_unique();
    let mut accs = accounts(&program_id);
    assert_eq!(run(&program_id, &mut accs, &CREATE), Ok(()));
    let s = slab(&accs[0]);
    assert_eq!(s.discriminator, SLAB_DISCRIMINATOR);
    assert_eq!(s.market, accs[1].key.to_bytes());
    assert_eq!(s.active_count, 0);
}

#[test]
fn refuses_to_reinitialize_a_live_slab() {
    stubs();
    let program_id = Pubkey::new_unique();
    let mut accs = accounts(&program_id);
    assert_eq!(run(&program_id, &mut accs, &CREATE), Ok(()));
    assert_eq!(
        place_bid(Pubkey::new_unique(), &mut accs[0], &program_id),
        Ok(())
    );
    assert_eq!(slab(&accs[0]).active_count, 1);

    assert_eq!(
        run(&program_id, &mut accs, &CREATE),
        Err(ProgramError::AccountAlreadyInitialized)
    );
    assert_eq!(slab(&accs[0]).active_count, 1, "resting order survives");
}

#[test]
fn refuses_a_slab_owned_by_another_program() {
    stubs();
    let program_id = Pubkey::new_unique();
    let mut accs = accounts(&program_id);
    accs[0].owner = Pubkey::new_unique();
    assert_eq!(
        run(&program_id, &mut accs, &CREATE),
        Err(ProgramError::IncorrectProgramId)
    );
}

#[test]
fn refuses_a_slab_of_the_wrong_size() {
    stubs();
    let program_id = Pubkey::new_unique();
    let mut accs = accounts(&program_id);
    accs[0].data = vec![0u8; Slab::LEN + 1];
    assert_eq!(
        run(&program_id, &mut accs, &CREATE),
        Err(ProgramError::InvalidAccountData)
    );
}

#[test]
fn refuses_a_slab_that_did_not_sign() {
    stubs();
    let program_id = Pubkey::new_unique();
    let mut accs = accounts(&program_id);
    accs[0].signer = false;
    assert_eq!(
        run(&program_id, &mut accs, &CREATE),
        Err(ProgramError::MissingRequiredSignature)
    );
    assert_eq!(slab(&accs[0]).discriminator, [0u8; 8], "left uninitialized");
}

#[test]
fn refuses_a_read_only_slab() {
    stubs();
    let program_id = Pubkey::new_unique();
    let mut accs = accounts(&program_id);
    accs[0].writable = false;
    assert_eq!(
        run(&program_id, &mut accs, &CREATE),
        Err(ProgramError::InvalidAccountData)
    );
}

#[test]
fn refuses_a_slab_that_is_not_rent_exempt() {
    stubs();
    let program_id = Pubkey::new_unique();
    let mut accs = accounts(&program_id);
    accs[0].lamports -= 1;
    assert_eq!(
        run(&program_id, &mut accs, &CREATE),
        Err(ProgramError::AccountNotRentExempt)
    );
}

#[test]
fn refuses_an_account_that_is_not_a_market() {
    stubs();
    let program_id = Pubkey::new_unique();

    let mut foreign = accounts(&program_id);
    foreign[1].owner = Pubkey::new_unique();
    assert_eq!(
        run(&program_id, &mut foreign, &CREATE),
        Err(ProgramError::InvalidAccountData)
    );

    let mut not_market = accounts(&program_id);
    not_market[1].data[..8].copy_from_slice(&SLAB_DISCRIMINATOR);
    assert_eq!(
        run(&program_id, &mut not_market, &CREATE),
        Err(ProgramError::InvalidAccountData)
    );
}
