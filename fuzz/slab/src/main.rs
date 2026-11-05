//! solinv harness for Chapter 15 — the slab, against Chapter 7/8's flat
//! book.
//!
//! Loads the real `openhl_core.so` from `target/deploy/` (`cargo build-sbf
//! --manifest-path programs/openhl-core/Cargo.toml`). One market with both
//! books: the flat `OrderBook` (32 slots) and the slab (1,024 orders,
//! critbit trees of price levels). The `grid` test measures §15.6's table.
//! The cu-dos spec is a `SlabMatch` whose `max_fills` the fuzzer picks,
//! with a cap of `SLAB_MATCH_CU_CAP`, on the slab's costliest shape: 128
//! bids at 128 prices, so every fill also removes a price level. (A side's
//! tree holds about 128 levels; more orders per level cost less per fill.)

use crucible_fuzzer::*;
use solana_account::Account;
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
const IX_CANCEL_ORDER: u8 = 9;
const IX_MATCH: u8 = 10;
const IX_CREATE_SLAB: u8 = 29;
const IX_SLAB_PLACE_ORDER: u8 = 30;
const IX_SLAB_MATCH: u8 = 31;

const BID: u8 = 0;
const ASK: u8 = 1;
const SLAB_LEN: usize = 82_000; // openhl_state::Slab::LEN
const SLAB_POOL_CAPACITY: usize = 1_024; // openhl_state::SLAB_POOL_CAPACITY

/// The CU one `SlabMatch` may use before cu-dos reports it (Chapter 15).
const SLAB_MATCH_CU_CAP: u64 = 100_000;

fn order_data(tag: u8, side: u8, price: u64, size: u64) -> Vec<u8> {
    let mut d = vec![tag, side];
    d.extend_from_slice(&price.to_le_bytes());
    d.extend_from_slice(&size.to_le_bytes());
    d
}

fn match_data(tag: u8, side: u8, limit: u64, size: u64, max_fills: u8) -> Vec<u8> {
    let mut d = order_data(tag, side, limit, size);
    d.push(max_fills);
    d
}

#[derive(Clone)]
struct SlabFixture {
    pub ctx: TestContext,
    payer: Arc<Keypair>,
    maker: Arc<Keypair>,
    taker: Arc<Keypair>,
    book: Pubkey,
    slab: Pubkey,
    /// The last `max_fills` an action asked for; the cu-dos spec reuses it.
    max_fills: u8,
}

impl SlabFixture {
    /// Sends `ix` signed by `signer`; returns the CU used, or `None` if the
    /// call failed.
    fn cu(&mut self, ix: Instruction, signer: &Keypair) -> Option<u64> {
        let payer = Arc::clone(&self.payer);
        let mut all: Vec<&Keypair> = vec![&*payer];
        if signer.pubkey() != payer.pubkey() {
            all.push(signer);
        }
        match self.ctx.raw_call(ix).fee_payer(&*payer).signers(&all).send() {
            Ok(crucible_test_context::TxOutcome::Success { compute_units, .. }) => Some(compute_units),
            _ => None,
        }
    }

    fn ix(&self, signer: &Pubkey, target: Pubkey, data: Vec<u8>) -> Instruction {
        Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new_readonly(*signer, true), AccountMeta::new(target, false)],
            data,
        }
    }

    fn place_flat(&mut self, side: u8, price: u64, size: u64) -> Option<u64> {
        let (maker, book) = (Arc::clone(&self.maker), self.book);
        self.cu(self.ix(&maker.pubkey(), book, order_data(IX_PLACE_ORDER, side, price, size)), &maker)
    }

    fn place_slab(&mut self, side: u8, price: u64, size: u64) -> Option<u64> {
        let (maker, slab) = (Arc::clone(&self.maker), self.slab);
        self.cu(self.ix(&maker.pubkey(), slab, order_data(IX_SLAB_PLACE_ORDER, side, price, size)), &maker)
    }

    fn match_flat(&mut self, max_fills: u8) -> Option<u64> {
        let (taker, book) = (Arc::clone(&self.taker), self.book);
        self.cu(self.ix(&taker.pubkey(), book, match_data(IX_MATCH, ASK, 1, u64::MAX / 2, max_fills)), &taker)
    }

    fn slab_match_ix(&self, max_fills: u8) -> Instruction {
        self.ix(&self.taker.pubkey(), self.slab, match_data(IX_SLAB_MATCH, ASK, 1, u64::MAX / 2, max_fills))
    }

    fn match_slab(&mut self, max_fills: u8) -> Option<u64> {
        let taker = Arc::clone(&self.taker);
        let ix = self.slab_match_ix(max_fills);
        self.cu(ix, &taker)
    }

    /// `n` size-1 bids on the slab, spread over `levels` prices.
    fn fill_slab(&mut self, n: usize, levels: usize) {
        for i in 0..n {
            assert!(self.place_slab(BID, 100 + (i % levels) as u64, 1).is_some(), "slab bid {i}");
        }
    }

    fn empty() -> Self {
        let mut ctx = TestContext::new();
        ctx.add_program(&CORE_ID, CORE_SO)
            .expect("load openhl_core.so — run cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml");
        let [payer, maker, taker] = [(); 3].map(|_| Arc::new(Keypair::new()));
        for kp in [&payer, &maker, &taker] {
            ctx.create_account().pubkey(kp.pubkey()).lamports(10_000_000_000).owner(SYSTEM_ID).create().unwrap();
        }
        let (base, quote) = (Keypair::new().pubkey(), Keypair::new().pubkey());
        let market = Pubkey::find_program_address(&[b"market", base.as_ref(), quote.as_ref()], &CORE_ID).0;
        let book = Pubkey::find_program_address(&[b"book", market.as_ref()], &CORE_ID).0;
        let slab_kp = Keypair::new();
        let mut f = Self { ctx, payer: Arc::clone(&payer), maker, taker, book, slab: slab_kp.pubkey(), max_fills: 1 };
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
        assert!(f.cu(ix, &payer).is_some(), "create market");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![
                AccountMeta::new(p, true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new(book, false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
            ],
            data: vec![IX_CREATE_ORDER_BOOK],
        };
        assert!(f.cu(ix, &payer).is_some(), "create order book");
        // The client creates the slab account (Chapter 15 §15.6); a
        // program-owned, rent-exempt, zeroed account stands in for that.
        f.ctx
            .write_account(
                &slab_kp.pubkey(),
                Account { lamports: 1_000_000_000, data: vec![0u8; SLAB_LEN], owner: CORE_ID, executable: false, rent_epoch: 0 },
            )
            .expect("write slab");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new(slab_kp.pubkey(), true), AccountMeta::new_readonly(market, false)],
            data: vec![IX_CREATE_SLAB],
        };
        assert!(f.cu(ix, &slab_kp).is_some(), "create slab");
        f
    }
}

#[fuzz_fixture]
impl SlabFixture {
    /// 128 size-1 bids, one per price level.
    pub fn setup() -> Self {
        let mut f = Self::empty();
        f.fill_slab(128, 128);
        f
    }

    pub fn action_choose_max_fills(&mut self, #[range(1..=255)] max_fills: u8) -> bool {
        self.max_fills = max_fills;
        true
    }
}

impl HasContext for SlabFixture {
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

impl HasInstructionSet for SlabFixture {
    fn instructions(&self) -> Vec<InstructionSpec> {
        let ix = self.slab_match_ix(self.max_fills);
        let n = ix.accounts.len();
        vec![InstructionSpec {
            program_id: CORE_ID,
            name: "slab_match".into(),
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
            cu_budget: Some(SLAB_MATCH_CU_CAP),
            cpi_reentrancy: None,
            realloc_check: None,
            bump_seed_check: None,
            guards: vec![],
        }]
    }
}

#[invariant_test]
fn invariant_ch15(fixture: &mut SlabFixture) {
    solinv_core::invariants::cu_dos::check(fixture);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten fills cost about the same on a slab of 32 orders as of 1,024.
    #[test]
    fn slab_match_cost_barely_depends_on_depth() {
        let mut small = SlabFixture::empty();
        small.fill_slab(32, 32);
        let mut big = SlabFixture::empty();
        big.fill_slab(SLAB_POOL_CAPACITY, 128);
        let (s, b) = (small.match_slab(10).unwrap(), big.match_slab(10).unwrap());
        assert!(b < s * 3 / 2, "10 fills: {s} CU on 32 orders, {b} on 1,024");
    }

    /// Prints §15.6's numbers (`cargo test -- --ignored --nocapture grid`).
    #[test]
    #[ignore]
    fn grid() {
        // Place: into an empty book, and into one with 31 / 1,023 orders.
        let mut f = SlabFixture::empty();
        println!("GRID flat place into 0: {:?}", f.place_flat(BID, 100, 1));
        for i in 1..31 {
            f.place_flat(BID, 100 + i, 1);
        }
        println!("GRID flat place into 31: {:?}", f.place_flat(BID, 200, 1));
        for id in [0u64, 1, 31] {
            let (m, b) = (Arc::clone(&f.maker), f.book);
            let mut d = vec![IX_CANCEL_ORDER];
            d.extend_from_slice(&id.to_le_bytes());
            let ix = f.ix(&m.pubkey(), b, d);
            println!("GRID flat cancel order {id} of 32: {:?}", f.cu(ix, &m));
        }
        let mut s = SlabFixture::empty();
        println!("GRID slab place into 0: {:?}", s.place_slab(BID, 100, 1));
        println!("GRID slab place, same level: {:?}", s.place_slab(BID, 100, 1));
        s.fill_slab(126, 126);
        println!("GRID slab place, new level among 127: {:?}", s.place_slab(BID, 999, 1));
        let mut s = SlabFixture::empty();
        s.fill_slab(1_023, 128);
        println!("GRID slab place into 1,023 (128 levels): {:?}", s.place_slab(BID, 105, 1));
        // Match, 10 fills, by depth.
        let mut f = SlabFixture::empty();
        for i in 0..32 {
            f.place_flat(BID, 100 + i, 1);
        }
        println!("GRID flat match 10 on 32: {:?}", f.match_flat(10));
        for (n, levels) in [(10usize, 10usize), (32, 32), (128, 128), (1_024, 128), (1_024, 8)] {
            let mut s = SlabFixture::empty();
            s.fill_slab(n, levels);
            println!("GRID slab match 10 on {n} orders / {levels} levels: {:?}", s.match_slab(10));
        }
        // Crossover: flat vs slab at small N, 4 fills.
        for n in [4usize, 8, 16] {
            let mut f = SlabFixture::empty();
            for i in 0..n {
                f.place_flat(BID, 100 + i as u64, 1);
            }
            let mut s = SlabFixture::empty();
            s.fill_slab(n, n);
            println!("GRID n={n} 4 fills: flat {:?} slab {:?}", f.match_flat(4), s.match_slab(4));
        }
        // 64 fills by depth, and the largest max_fills under the cap.
        for (n, levels) in [(64usize, 64usize), (128, 128), (512, 128), (1_024, 128)] {
            let mut s = SlabFixture::empty();
            s.fill_slab(n, levels);
            println!("GRID slab match 64 on {n} orders / {levels} levels: {:?}", s.match_slab(64));
        }
        for mf in [68u8, 69, 70, 71] {
            let mut s = SlabFixture::empty();
            s.fill_slab(128, 128);
            println!("GRID slab match {mf} on 128 / 128 levels: {:?}", s.match_slab(mf));
        }
        for mf in [74u8, 75, 76, 77, 78] {
            let mut s = SlabFixture::empty();
            s.fill_slab(SLAB_POOL_CAPACITY, 128);
            println!("GRID slab match {mf} on 1,024 / 128 levels: {:?}", s.match_slab(mf));
        }
        let mut s = SlabFixture::empty();
        s.fill_slab(SLAB_POOL_CAPACITY, 128);
        println!("GRID slab match 16 on 1,024 / 128 levels: {:?}", s.match_slab(16));
        // Worst cases on a full slab.
        for mf in [32u8, 64, 128, 255] {
            let mut s = SlabFixture::empty();
            s.fill_slab(SLAB_POOL_CAPACITY, 128);
            println!("GRID slab match {mf} on 1,024 / 128 levels: {:?}", s.match_slab(mf));
        }
    }
}
