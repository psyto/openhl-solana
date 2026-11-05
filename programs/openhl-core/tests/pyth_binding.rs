//! Pyth feed binding: `PlaceOrderCheckedPyth` (v1) and
//! `PlaceOrderCheckedPythV2` must read the feed pinned on the market, not
//! any account whose bytes look like a Pyth price. Each case runs the real
//! handler natively (CPIs and the clock are stubbed).

use bytemuck::Zeroable;
use openhl_core::{process_instruction, BOOK_SEED, FEE_VAULT_SEED, SPL_TOKEN_PROGRAM_ID};
use openhl_state::*;
use solana_program::{
    account_info::AccountInfo, clock::Clock, entrypoint::ProgramResult, instruction::Instruction,
    program_error::ProgramError, program_stubs, pubkey::Pubkey,
};

const NOW_SLOT: u64 = 1_000;
const PRICE: i64 = 100;

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
    fn sol_invoke_signed(
        &self,
        _ix: &Instruction,
        _infos: &[AccountInfo],
        _seeds: &[&[&[u8]]],
    ) -> ProgramResult {
        Ok(())
    }
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

fn pda(seeds: &[&[u8]], program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program_id).0
}

fn put_u32(d: &mut [u8], at: usize, v: u32) {
    d[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(d: &mut [u8], at: usize, v: u64) {
    d[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// A Pyth v1 `PriceAccount`'s bytes: Trading at `PRICE`, published now.
fn pyth_v1_bytes() -> Vec<u8> {
    let mut d = vec![0u8; PYTH_V1_MIN_LEN];
    put_u32(&mut d, 0, PYTH_V1_MAGIC);
    put_u32(&mut d, 4, PYTH_V1_VERSION);
    put_u32(&mut d, 8, PYTH_V1_ACCOUNT_TYPE_PRICE);
    put_u32(&mut d, PYTH_V1_OFFSET_AGG_STATUS, PYTH_V1_STATUS_TRADING);
    put_u64(&mut d, PYTH_V1_OFFSET_AGG_PRICE, PRICE as u64);
    put_u64(&mut d, PYTH_V1_OFFSET_AGG_PUB_SLOT, NOW_SLOT);
    d
}

/// A Pyth `PriceUpdateV2`'s bytes for `feed_id`. `full`: Full
/// verification, otherwise Partial with `sigs` signatures (a Full update is
/// one byte shorter before the price message).
fn pyth_v2_bytes(feed_id: [u8; 32], full: bool, sigs: u8) -> Vec<u8> {
    let mut d = vec![0u8; PYTH_V2_MIN_LEN];
    d[0..8].copy_from_slice(&PYTH_V2_PRICE_UPDATE_DISCRIMINATOR);
    let shift = if full {
        d[PYTH_V2_OFFSET_VERIFICATION_LEVEL] = PYTH_V2_VERIFICATION_FULL;
        1
    } else {
        d[PYTH_V2_OFFSET_VERIFICATION_LEVEL] = PYTH_V2_VERIFICATION_PARTIAL;
        d[PYTH_V2_OFFSET_VERIFICATION_NUM_SIGS] = sigs;
        0
    };
    let at = PYTH_V2_OFFSET_FEED_ID - shift;
    d[at..at + 32].copy_from_slice(&feed_id);
    put_u64(&mut d, PYTH_V2_OFFSET_PRICE - shift, PRICE as u64);
    put_u64(&mut d, PYTH_V2_OFFSET_POSTED_SLOT - shift, NOW_SLOT);
    d
}

/// A market with its book and fee vault, and the feeds pinned on it.
struct Setup {
    program_id: Pubkey,
    user: Pubkey,
    market: Pubkey,
    mint: Pubkey,
    authority: Pubkey,
    /// The Pyth v1 price account and its owner, and the v2 receiver + feed
    /// id, the market trusts.
    v1_feed: Pubkey,
    pyth_program: Pubkey,
    receiver: Pubkey,
    feed_id: [u8; 32],
}

impl Setup {
    fn new() -> Self {
        Self {
            program_id: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            market: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            v1_feed: Pubkey::new_unique(),
            pyth_program: Pubkey::new_unique(),
            receiver: Pubkey::new_unique(),
            feed_id: [7u8; 32],
        }
    }

    fn market_data(&self, pinned: bool) -> Vec<u8> {
        let mut m = Market::zeroed();
        m.discriminator = MARKET_DISCRIMINATOR;
        m.authority = self.authority.to_bytes();
        m.quote_mint = self.mint.to_bytes();
        if pinned {
            m.pyth_v1_price_account = self.v1_feed.to_bytes();
            m.pyth_v1_program = self.pyth_program.to_bytes();
            m.pyth_v2_receiver = self.receiver.to_bytes();
            m.pyth_v2_feed_id = self.feed_id;
        }
        bytemuck::bytes_of(&m).to_vec()
    }

    /// `PlaceOrderCheckedPyth` (tag 32) or `...V2` (tag 33) with `oracle` in
    /// slot 2, against a market that has (or has not) pinned its feeds.
    fn place(&self, tag: u8, oracle: Acc, pinned: bool) -> ProgramResult {
        let p = &self.program_id;
        let mut book = OrderBook::zeroed();
        book.discriminator = ORDER_BOOK_DISCRIMINATOR;
        let mut accs = vec![
            Acc {
                signer: true,
                ..acc(self.user, Pubkey::default(), vec![])
            },
            acc(
                pda(&[BOOK_SEED, self.market.as_ref()], p),
                *p,
                bytemuck::bytes_of(&book).to_vec(),
            ),
            oracle,
            acc(self.market, *p, self.market_data(pinned)),
            acc(self.mint, SPL_TOKEN_PROGRAM_ID, vec![]),
            acc(Pubkey::new_unique(), SPL_TOKEN_PROGRAM_ID, vec![0u8; 165]),
            acc(
                pda(&[FEE_VAULT_SEED, self.mint.as_ref()], p),
                SPL_TOKEN_PROGRAM_ID,
                vec![],
            ),
            acc(SPL_TOKEN_PROGRAM_ID, Pubkey::default(), vec![]),
        ];
        let mut ix = vec![tag, 0];
        ix.extend_from_slice(&(PRICE as u64).to_le_bytes());
        ix.extend_from_slice(&1u64.to_le_bytes());
        run(p, &mut accs, &ix)
    }
}

#[test]
fn pyth_v1_reads_only_the_pinned_price_account() {
    program_stubs::set_syscall_stubs(Box::new(Stubs));
    let s = Setup::new();
    let real = acc(s.v1_feed, s.pyth_program, pyth_v1_bytes());
    assert_eq!(s.place(32, real, true), Ok(()));
    // Same bytes, written into an account an attacker owns.
    let fake = acc(Pubkey::new_unique(), Pubkey::new_unique(), pyth_v1_bytes());
    assert_eq!(s.place(32, fake, true), Err(ProgramError::InvalidAccountData));
    // The pinned address, but owned by someone else: what a closed and
    // recreated account would look like.
    let recreated = acc(s.v1_feed, Pubkey::new_unique(), pyth_v1_bytes());
    assert_eq!(s.place(32, recreated, true), Err(ProgramError::InvalidAccountData));
    // No feed pinned yet: refuse rather than trust any account.
    let real = acc(s.v1_feed, s.pyth_program, pyth_v1_bytes());
    assert_eq!(s.place(32, real, false), Err(ProgramError::InvalidAccountData));
}

#[test]
fn pyth_v2_reads_only_the_pinned_receiver_and_feed() {
    program_stubs::set_syscall_stubs(Box::new(Stubs));
    let s = Setup::new();
    let v2 = |owner: Pubkey, feed: [u8; 32], full: bool, sigs: u8| acc(Pubkey::new_unique(), owner, pyth_v2_bytes(feed, full, sigs));
    assert_eq!(s.place(33, v2(s.receiver, s.feed_id, true, 0), true), Ok(()));
    // A PriceUpdateV2-shaped account written by some other program.
    assert_eq!(s.place(33, v2(Pubkey::new_unique(), s.feed_id, true, 0), true), Err(ProgramError::InvalidAccountData));
    // A real receiver account, but for another asset's feed.
    assert_eq!(s.place(33, v2(s.receiver, [9u8; 32], true, 0), true), Err(ProgramError::InvalidAccountData));
    // Partially verified updates, with no or some guardian signatures.
    for sigs in [0u8, 13] {
        assert_eq!(s.place(33, v2(s.receiver, s.feed_id, false, sigs), true), Err(ProgramError::InvalidAccountData));
    }
    assert_eq!(s.place(33, v2(s.receiver, s.feed_id, true, 0), false), Err(ProgramError::InvalidAccountData));
}

#[test]
fn only_the_market_authority_pins_feeds() {
    program_stubs::set_syscall_stubs(Box::new(Stubs));
    let s = Setup::new();
    let mut payload = vec![34u8];
    payload.extend_from_slice(s.v1_feed.as_ref());
    payload.extend_from_slice(s.pyth_program.as_ref());
    payload.extend_from_slice(s.receiver.as_ref());
    payload.extend_from_slice(&s.feed_id);
    for (signer, want) in [
        (Pubkey::new_unique(), Err(ProgramError::IllegalOwner)),
        (s.authority, Ok(())),
    ] {
        let mut accs = vec![
            Acc {
                signer: true,
                ..acc(signer, Pubkey::default(), vec![])
            },
            acc(s.market, s.program_id, s.market_data(false)),
        ];
        assert_eq!(run(&s.program_id, &mut accs, &payload), want);
        let m: &Market = bytemuck::from_bytes(&accs[1].data);
        let pinned = m.pyth_v1_price_account == s.v1_feed.to_bytes()
            && m.pyth_v1_program == s.pyth_program.to_bytes()
            && m.pyth_v2_receiver == s.receiver.to_bytes()
            && m.pyth_v2_feed_id == s.feed_id;
        assert_eq!(pinned, want.is_ok());
    }
}
