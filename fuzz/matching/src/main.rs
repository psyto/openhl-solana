//! solinv harness for Chapter 8 — `Match` on the flat order book.
//!
//! Loads the real `openhl_core.so` from `target/deploy/` (`cargo build-sbf
//! --manifest-path programs/openhl-core/Cargo.toml`). One market and its
//! book, full: 32 bids of size 1. The `Match` spec is an ask whose
//! `max_fills` the fuzzer picks, with a CU cap of `MATCH_CU_CAP` (cu-dos):
//! how much can one taker make a match cost? (cu-dos restores the book
//! after each attempt, so every attempt sees the same full book.)

use crucible_fuzzer::*;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::sync::Arc;

use solinv_fuzz::{HasContext, HasInstructionSet, InstructionSpec};

const CORE_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/openhl_core.so");

const CORE_ID: Pubkey = Pubkey::new_from_array([3u8; 32]);
const SYSTEM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

// openhl-core instruction tags (programs/openhl-core/src/lib.rs).
const IX_CREATE_MARKET: u8 = 1;
const IX_CREATE_ORDER_BOOK: u8 = 7;
const IX_PLACE_ORDER: u8 = 8;
const IX_MATCH: u8 = 10;

const BID: u8 = 0;
const ASK: u8 = 1;
const ORDER_CAPACITY: usize = 32; // openhl_state::ORDER_CAPACITY

/// The CU one `Match` may use before cu-dos reports it (Chapter 8).
const MATCH_CU_CAP: u64 = 50_000;

fn book(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"book", market.as_ref()], &CORE_ID).0
}

fn order_data(side: u8, price: u64, size: u64) -> Vec<u8> {
    let mut d = vec![IX_PLACE_ORDER, side];
    d.extend_from_slice(&price.to_le_bytes());
    d.extend_from_slice(&size.to_le_bytes());
    d
}

fn match_data(side: u8, limit: u64, size: u64, max_fills: u8) -> Vec<u8> {
    let mut d = vec![IX_MATCH, side];
    d.extend_from_slice(&limit.to_le_bytes());
    d.extend_from_slice(&size.to_le_bytes());
    d.push(max_fills);
    d
}

#[derive(Clone)]
struct MatchingFixture {
    pub ctx: TestContext,
    payer: Arc<Keypair>,
    maker: Arc<Keypair>,
    taker: Arc<Keypair>,
    book: Pubkey,
    /// The last `max_fills` an action asked for; the cu-dos spec reuses it.
    max_fills: u8,
}

impl MatchingFixture {
    fn send(&mut self, ix: Instruction, signer: &Keypair, what: &str) {
        let payer = Arc::clone(&self.payer);
        let mut all: Vec<&Keypair> = vec![&*payer];
        if signer.pubkey() != payer.pubkey() {
            all.push(signer);
        }
        let outcome = self.ctx.raw_call(ix).fee_payer(&*payer).signers(&all).send().unwrap_or_else(|e| panic!("{what}: send: {e:?}"));
        // send() is Ok when the program fails; check the outcome.
        assert!(outcome.is_success(), "{what} failed: {outcome:?}");
    }

    /// Sends `ix` signed by `signer`; returns the CU used, or `None` if the
    /// call failed.
    fn cu(&mut self, ix: Instruction, signer: Arc<Keypair>) -> Option<u64> {
        let payer = Arc::clone(&self.payer);
        match self.ctx.raw_call(ix).fee_payer(&*payer).signers(&[&*payer, &*signer]).send() {
            Ok(crucible_test_context::TxOutcome::Success { compute_units, .. }) => Some(compute_units),
            _ => None,
        }
    }

    fn place(&mut self, side: u8, price: u64, size: u64) -> bool {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new_readonly(self.maker.pubkey(), true), AccountMeta::new(self.book, false)],
            data: order_data(side, price, size),
        };
        self.cu(ix, Arc::clone(&self.maker)).is_some()
    }

    fn match_ix(&self, max_fills: u8) -> Instruction {
        // An ask for as much as the book holds, at any bid price.
        Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new_readonly(self.taker.pubkey(), true), AccountMeta::new(self.book, false)],
            data: match_data(ASK, 1, u64::MAX / 2, max_fills),
        }
    }

    /// A fresh fixture whose book holds `n` bids of size 1, at prices
    /// 100..100+n.
    fn with_bids(n: usize) -> Self {
        let mut f = Self::setup_empty();
        for i in 0..n {
            assert!(f.place(BID, 100 + i as u64, 1), "place bid {i}");
        }
        f
    }

    fn setup_empty() -> Self {
        let mut ctx = TestContext::new();
        ctx.add_program(&CORE_ID, CORE_SO)
            .expect("load openhl_core.so — run cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml");
        let [payer, maker, taker] = [(); 3].map(|_| Arc::new(Keypair::new()));
        for kp in [&payer, &maker, &taker] {
            ctx.create_account().pubkey(kp.pubkey()).lamports(10_000_000_000).owner(SYSTEM_ID).create().unwrap();
        }
        let (base, quote) = (Keypair::new().pubkey(), Keypair::new().pubkey());
        let market = Pubkey::find_program_address(&[b"market", base.as_ref(), quote.as_ref()], &CORE_ID).0;
        let mut f = Self { ctx, payer: Arc::clone(&payer), maker, taker, book: book(&market), max_fills: 1 };
        let p = payer.pubkey();
        let mut d = vec![IX_CREATE_MARKET];
        d.extend_from_slice(p.as_ref());
        d.extend_from_slice(base.as_ref());
        d.extend_from_slice(quote.as_ref());
        d.extend_from_slice(&1u64.to_le_bytes());
        d.extend_from_slice(&1u64.to_le_bytes());
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new(p, true), AccountMeta::new(market, false), AccountMeta::new_readonly(SYSTEM_ID, false)],
            data: d,
        };
        f.send(ix, &payer, "create market");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![
                AccountMeta::new(p, true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(f.book, false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
            ],
            data: vec![IX_CREATE_ORDER_BOOK],
        };
        f.send(ix, &payer, "create order book");
        f
    }
}

#[fuzz_fixture]
impl MatchingFixture {
    /// A full book: 32 bids of size 1.
    pub fn setup() -> Self {
        Self::with_bids(ORDER_CAPACITY)
    }

    pub fn action_choose_max_fills(&mut self, #[range(1..=255)] max_fills: u8) -> bool {
        self.max_fills = max_fills;
        true
    }
}

impl HasContext for MatchingFixture {
    fn ctx(&self) -> &TestContext {
        &self.ctx
    }
    fn ctx_mut(&mut self) -> &mut TestContext {
        &mut self.ctx
    }
    fn program_ids(&self) -> Vec<Pubkey> {
        vec![CORE_ID]
    }
    fn fee_payer(&self) -> Arc<Keypair> {
        Arc::clone(&self.payer)
    }
}

impl HasInstructionSet for MatchingFixture {
    fn instructions(&self) -> Vec<InstructionSpec> {
        let ix = self.match_ix(self.max_fills);
        let n = ix.accounts.len();
        vec![InstructionSpec {
            program_id: CORE_ID,
            name: "match".into(),
            accounts: ix.accounts,
            signer_indices: vec![0],
            optional_signer_indices: vec![],
            expected_owners: vec![None; n],
            expected_discriminators: vec![None; n],
            expected_pda_seeds: vec![None; n],
            creates_indices: vec![],
            swap_alternates: vec![vec![]; n],
            data_sample: ix.data,
            signers: vec![Arc::clone(&self.taker)],
            state_invariants: vec![],
            cu_budget: Some(MATCH_CU_CAP),
            cpi_reentrancy: None,
            realloc_check: None,
            bump_seed_check: None,
            guards: vec![],
        }]
    }
}

#[invariant_test]
fn invariant_ch8(fixture: &mut MatchingFixture) {
    solinv_core::invariants::cu_dos::check(fixture);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fill costs more on a fuller book: the scan is over every slot.
    #[test]
    fn match_cost_grows_with_book_and_fills() {
        let one = MatchingFixture::with_bids(8).cu_of(4);
        let three = MatchingFixture::with_bids(24).cu_of(4);
        assert!(three > one, "4 fills: {one} CU on 8 bids, {three} on 24");
    }

    /// Prints the numbers in §8.2 and §8.6
    /// (`cargo test -- --ignored --nocapture grid`).
    #[test]
    #[ignore]
    fn grid() {
        for max_fills in [1u8, 2, 4, 8, 16] {
            println!("GRID book=16 max_fills={max_fills} cu={:?}", MatchingFixture::with_bids(16).try_cu(max_fills));
        }
        for n in [8usize, 16, 24, 32] {
            println!("GRID book={n} max_fills=4 cu={:?}", MatchingFixture::with_bids(n).try_cu(4));
        }
        for max_fills in [16u8, 25, 30, 32] {
            println!("GRID book=32 max_fills={max_fills} cu={:?}", MatchingFixture::with_bids(32).try_cu(max_fills));
        }
        if std::env::var("BIG").is_ok() {
            for (n, mf) in [(128usize, 1u8), (128, 16), (128, 64), (128, 128), (64, 64)] {
                println!("GRID128 book={n} max_fills={mf} cu={:?}", MatchingFixture::with_bids(n).try_cu(mf));
            }
        }
        for max_fills in [25u8, 26, 27, 28, 33, 255] {
            println!("GRID book=32 max_fills={max_fills} cu={:?}", MatchingFixture::with_bids(32).try_cu(max_fills));
        }
        for (n, mf) in [(24usize, 25u8), (25, 26), (26, 27), (27, 28)] {
            println!("GRID book={n} max_fills={mf} cu={:?}", MatchingFixture::with_bids(n).try_cu(mf));
        }
        // Two pages of 8 against one call of 16, on a full book.
        let mut f = MatchingFixture::with_bids(32);
        let a = f.try_cu(8);
        let b = f.try_cu(8);
        println!("GRID book=32 two pages of 8: {a:?} + {b:?}");
    }

    impl MatchingFixture {
        fn try_cu(&mut self, max_fills: u8) -> Option<u64> {
            let ix = self.match_ix(max_fills);
            self.cu(ix, Arc::clone(&self.taker))
        }
        fn cu_of(mut self, max_fills: u8) -> u64 {
            self.try_cu(max_fills).expect("match failed")
        }
    }
}
