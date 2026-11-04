//! solinv harness for Chapters 3, 4 and 6 — `CreateMarket`, `Bench`,
//! `CreateVault` and `Deposit`.
//!
//! Loads the real `openhl_core.so` from `target/deploy/` (`cargo build-sbf
//! --manifest-path programs/openhl-core/Cargo.toml`). One market, created
//! by `CreateMarket` at its PDA, with the market's token account for the
//! quote mint (`CreateVault`). A and B each hold quote tokens. One
//! invariant test per chapter, each behind its own cargo feature:
//!
//! - `invariant_ch3`: signer-skip and account-swap on `CreateMarket` (a
//!   market address that is not the PDA, a program that is not System).
//! - `invariant_ch3_reader`: a two-step check for §3.7's exercise. It makes
//!   a second market with `Initialize` (Chapter 2) at an address that is not
//!   the PDA, and has `CreateVault` read it. This reports on the current
//!   program; the chapter asks whether that is a hole.
//! - `invariant_ch4`: cu-dos on `Bench`, with a cap of `BENCH_CU_CAP`.
//! - `invariant_ch6`: signer-skip, owner-skip and account-swap on
//!   `CreateVault` and `Deposit`.

use crucible_fuzzer::*;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::str::FromStr;
use std::sync::Arc;

use solinv_fuzz::{HasContext, HasInstructionSet, InstructionSpec};

const CORE_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/openhl_core.so");

const CORE_ID: Pubkey = Pubkey::new_from_array([3u8; 32]);
const SYSTEM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);
const TOKEN_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// openhl-core instruction tags (programs/openhl-core/src/lib.rs).
const IX_INITIALIZE: u8 = 0;
const IX_CREATE_MARKET: u8 = 1;
const IX_BENCH: u8 = 2;
const IX_CREATE_VAULT: u8 = 5;
const IX_DEPOSIT: u8 = 6;

const START_BALANCE: u64 = 1_000_000;
const MARKET_LEN: usize = 256; // openhl_state::Market::LEN

/// The CU a `Bench` call may use before cu-dos reports it (Chapter 4).
const BENCH_CU_CAP: u64 = 50_000;

fn token() -> Pubkey {
    Pubkey::from_str(TOKEN_ID).unwrap()
}
fn market_pda(base: &Pubkey, quote: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"market", base.as_ref(), quote.as_ref()], &CORE_ID).0
}
fn vault_token(market: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &CORE_ID).0
}
fn vault_auth(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault_auth", market.as_ref()], &CORE_ID).0
}

/// An SPL Token mint (82 bytes).
fn mint_data(decimals: u8) -> Vec<u8> {
    let mut d = vec![0u8; 82];
    d[36..44].copy_from_slice(&(START_BALANCE * 4).to_le_bytes()); // supply
    d[44] = decimals;
    d[45] = 1; // is_initialized
    d
}

/// An SPL Token account (165 bytes).
fn token_account_data(mint: &Pubkey, owner: &Pubkey, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1; // state = Initialized
    d
}

/// `CreateMarket` / `Initialize` payload: authority, base, quote, tick, lot.
fn market_payload(authority: &Pubkey, base: &Pubkey, quote: &Pubkey) -> Vec<u8> {
    let mut d = vec![IX_CREATE_MARKET];
    d.extend_from_slice(authority.as_ref());
    d.extend_from_slice(base.as_ref());
    d.extend_from_slice(quote.as_ref());
    d.extend_from_slice(&1u64.to_le_bytes());
    d.extend_from_slice(&1u64.to_le_bytes());
    d
}

fn bench_data(rounds: u32, heap_bytes: u32) -> Vec<u8> {
    let mut d = vec![IX_BENCH];
    d.extend_from_slice(&rounds.to_le_bytes());
    d.extend_from_slice(&heap_bytes.to_le_bytes());
    d
}

fn amount_data(tag: u8, amount: u64) -> Vec<u8> {
    let mut d = vec![tag];
    d.extend_from_slice(&amount.to_le_bytes());
    d
}

#[derive(Clone)]
struct BasicsFixture {
    pub ctx: TestContext,
    payer: Arc<Keypair>,
    a: Arc<Keypair>,
    base: Pubkey,
    /// The quote mint, a real SPL Token mint.
    quote: Pubkey,
    /// The market at its PDA, `[b"market", base, quote]`.
    market: Pubkey,
    a_ata: Pubkey,
    b_ata: Pubkey,
    /// A second base mint with no market yet, for the `CreateMarket` spec.
    base2: Pubkey,
    /// A second real mint with no market token account yet, for the
    /// `CreateVault` spec.
    mint2: Pubkey,
    /// The last `Bench` arguments an action sent; the cu-dos spec reuses them.
    bench: (u32, u32),
    /// An address that is not a PDA of this program.
    stranger: Pubkey,
}

impl BasicsFixture {
    fn send(&mut self, ix: Instruction, signers: &[&Keypair], what: &str) {
        let payer = Arc::clone(&self.payer);
        let mut all: Vec<&Keypair> = vec![&*payer];
        all.extend(signers.iter().copied().filter(|k| k.pubkey() != payer.pubkey()));
        let outcome = self
            .ctx
            .raw_call(ix)
            .fee_payer(&*payer)
            .signers(&all)
            .send()
            .unwrap_or_else(|e| panic!("{what}: send: {e:?}"));
        // send() is Ok when the program fails; check the outcome.
        assert!(outcome.is_success(), "{what} failed: {outcome:?}");
    }

    fn put(&mut self, key: Pubkey, owner: Pubkey, data: Vec<u8>) {
        self.ctx
            .write_account(&key, Account { lamports: 10_000_000, data, owner, executable: false, rent_epoch: 0 })
            .expect("write account");
    }

    fn create_market_metas(&self, payer: &Pubkey, market: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*market, false),
            AccountMeta::new_readonly(SYSTEM_ID, false),
        ]
    }

    fn create_vault_metas(&self, payer: &Pubkey, market: &Pubkey, mint: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_token(market, mint), false),
            AccountMeta::new_readonly(vault_auth(market), false),
            AccountMeta::new_readonly(SYSTEM_ID, false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    fn deposit_metas(&self, who: &Pubkey, from: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(*who, true),
            AccountMeta::new(*from, false),
            AccountMeta::new(vault_token(&self.market, &self.quote), false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    /// Sends `ix` and returns whether it succeeded, with the outcome's
    /// Debug text (it includes the program logs) to assert on.
    fn send_logged(&mut self, ix: Instruction, signer: Arc<Keypair>) -> (bool, String) {
        let payer = Arc::clone(&self.payer);
        match self.ctx.raw_call(ix).fee_payer(&*payer).signers(&[&*payer, &*signer]).send() {
            Ok(o) => (o.is_success(), format!("{o:?}")),
            Err(e) => (false, format!("{e:?}")),
        }
    }

    fn send_ok(&mut self, ix: Instruction, signer: Arc<Keypair>) -> bool {
        self.send_logged(ix, signer).0
    }

    /// A second market for the same pair, made by `Initialize` (Chapter 2)
    /// in an account the caller created, at an address that is not the PDA.
    /// Made once; later calls return the same address.
    fn stranger_market(&mut self) -> Pubkey {
        let key = Pubkey::new_from_array([7u8; 32]);
        if !self.ctx.account_exists(&key) {
            self.put(key, CORE_ID, vec![0u8; MARKET_LEN]);
            let mut data = market_payload(&self.a.pubkey(), &self.base, &self.quote);
            data[0] = IX_INITIALIZE;
            let ix = Instruction { program_id: CORE_ID, accounts: vec![AccountMeta::new(key, false)], data };
            self.send(ix, &[], "initialize a market at a non-PDA address");
        }
        key
    }
}

#[fuzz_fixture]
impl BasicsFixture {
    pub fn setup() -> Self {
        let mut ctx = TestContext::new();
        ctx.add_program(&CORE_ID, CORE_SO)
            .expect("load openhl_core.so — run cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml");

        let payer = Arc::new(Keypair::new());
        let a = Arc::new(Keypair::new());
        let b = Arc::new(Keypair::new());
        for kp in [&payer, &a, &b] {
            ctx.create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(SYSTEM_ID)
                .create()
                .unwrap();
        }

        let base = Keypair::new().pubkey();
        let quote = Keypair::new().pubkey();
        let mut f = Self {
            ctx,
            payer: Arc::clone(&payer),
            a: Arc::clone(&a),
            base,
            quote,
            market: market_pda(&base, &quote),
            a_ata: Keypair::new().pubkey(),
            b_ata: Keypair::new().pubkey(),
            base2: Keypair::new().pubkey(),
            mint2: Keypair::new().pubkey(),
            bench: (0, 0),
            stranger: Keypair::new().pubkey(),
        };

        let (market, aa, ba, mint2) = (f.market, f.a_ata, f.b_ata, f.mint2);
        f.put(quote, token(), mint_data(6));
        f.put(mint2, token(), mint_data(6));
        f.put(aa, token(), token_account_data(&quote, &a.pubkey(), START_BALANCE));
        f.put(ba, token(), token_account_data(&quote, &b.pubkey(), START_BALANCE));

        // The market at its PDA (Chapter 3), then its quote token account
        // (Chapter 6).
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.create_market_metas(&payer.pubkey(), &market),
            data: market_payload(&payer.pubkey(), &base, &quote),
        };
        f.send(ix, &[], "create_market");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.create_vault_metas(&payer.pubkey(), &market, &quote),
            data: vec![IX_CREATE_VAULT],
        };
        f.send(ix, &[], "create_vault");
        f
    }

    pub fn action_bench(&mut self, #[range(0..2_000)] rounds: u32, #[range(0..4_096)] heap_bytes: u32) -> bool {
        self.bench = (rounds, heap_bytes);
        let ix = Instruction { program_id: CORE_ID, accounts: vec![], data: bench_data(rounds, heap_bytes) };
        self.send_ok(ix, Arc::clone(&self.payer))
    }

    pub fn action_deposit(&mut self, #[range(1..10_000)] amount: u64) -> bool {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: self.deposit_metas(&self.a.pubkey(), &self.a_ata),
            data: amount_data(IX_DEPOSIT, amount),
        };
        self.send_ok(ix, Arc::clone(&self.a))
    }
}

impl HasContext for BasicsFixture {
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

/// A spec with every optional detector off; each chapter's specs fill in
/// what they test.
fn spec(name: &str, accounts: Vec<AccountMeta>, data: Vec<u8>, signer: Arc<Keypair>) -> InstructionSpec {
    let n = accounts.len();
    InstructionSpec {
        program_id: CORE_ID,
        name: name.into(),
        accounts,
        signer_indices: vec![0],
        optional_signer_indices: vec![],
        expected_owners: vec![None; n],
        expected_discriminators: vec![None; n],
        expected_pda_seeds: vec![None; n],
        creates_indices: vec![],
        swap_alternates: vec![vec![]; n],
        data_sample: data,
        signers: vec![signer],
        state_invariants: vec![],
        cu_budget: None,
        cpi_reentrancy: None,
        realloc_check: None,
        bump_seed_check: None,
        guards: vec![],
    }
}

impl HasInstructionSet for BasicsFixture {
    fn instructions(&self) -> Vec<InstructionSpec> {
        let a = self.a.pubkey();
        let t = Some(token());
        let c = Some(CORE_ID);

        // Chapter 3: a market nobody has created yet, at its PDA.
        let market2 = market_pda(&self.base2, &self.quote);
        let mut create_market = spec(
            "create_market",
            self.create_market_metas(&a, &market2),
            market_payload(&a, &self.base2, &self.quote),
            Arc::clone(&self.a),
        );
        create_market.creates_indices = vec![1];
        create_market.swap_alternates = vec![
            vec![],
            vec![self.stranger], // a market that is not at the PDA
            vec![token()],       // a program that is not System
        ];

        // Chapter 4: whatever Bench arguments the last action sent.
        let mut bench = spec("bench", vec![], bench_data(self.bench.0, self.bench.1), Arc::clone(&self.payer));
        bench.signer_indices = vec![];
        bench.signers = vec![];
        bench.cu_budget = Some(BENCH_CU_CAP);

        // Chapter 6: CreateVault for a mint the market has no account for
        // yet, reading the market at slot 1.
        let mut create_vault = spec(
            "create_vault",
            self.create_vault_metas(&a, &self.market, &self.mint2),
            vec![IX_CREATE_VAULT],
            Arc::clone(&self.a),
        );
        create_vault.creates_indices = vec![3];
        create_vault.expected_owners = vec![None, c, t, None, None, None, None];
        create_vault.swap_alternates = vec![
            vec![],
            vec![self.a_ata],    // a "market" this program does not own
            vec![self.b_ata],    // a "mint" that is a token account
            vec![self.stranger], // a vault that is not at the PDA
            vec![a],             // the caller as the vault's authority
            vec![token()],       // a program that is not System
            vec![SYSTEM_ID],     // a program that is not SPL Token
        ];

        // Chapter 6: A's deposit into the market's quote token account.
        let mut deposit = spec(
            "deposit",
            self.deposit_metas(&a, &self.a_ata),
            amount_data(IX_DEPOSIT, 10),
            Arc::clone(&self.a),
        );
        deposit.expected_owners = vec![None, t, t, None];
        deposit.swap_alternates = vec![
            vec![],
            vec![self.b_ata],  // spend B's tokens with A's signature
            vec![],            // §6.6's exercise adds the destination here
            vec![SYSTEM_ID],   // a program that is not SPL Token
        ];

        vec![create_market, bench, create_vault, deposit]
    }
}

#[invariant_test]
fn invariant_ch3(fixture: &mut BasicsFixture) {
    solinv_core::invariants::signer_skip::check(fixture);
    solinv_core::invariants::account_swap::check(fixture);
}

/// §3.7's exercise: a market made by `Initialize` at an address that is
/// not the PDA, read by `CreateVault`. Three slots change together: the
/// market and the two accounts derived from it.
#[invariant_test]
fn invariant_ch3_reader(fixture: &mut BasicsFixture) {
    use solinv_core::invariants::account_swap::SwapSet;
    let other = fixture.stranger_market();
    let mint2 = fixture.mint2;
    let sets = [SwapSet {
        ix: "create_vault".into(),
        swaps: vec![(1, other), (3, vault_token(&other, &mint2)), (4, vault_auth(&other))],
    }];
    solinv_core::invariants::account_swap::check_sets(fixture, &sets);
}

#[invariant_test]
fn invariant_ch4(fixture: &mut BasicsFixture) {
    solinv_core::invariants::cu_dos::check(fixture);
}

#[invariant_test]
fn invariant_ch6(fixture: &mut BasicsFixture) {
    solinv_core::invariants::signer_skip::check(fixture);
    solinv_core::invariants::owner_skip::check(fixture);
    solinv_core::invariants::account_swap::check(fixture);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chapter 4: the CU a Bench call uses grows with `rounds`, which the
    /// caller picks.
    #[test]
    fn bench_cu_grows_with_rounds() {
        let mut f = BasicsFixture::setup();
        let mut used = vec![];
        for rounds in [0u32, 10, 100] {
            let ix = Instruction { program_id: CORE_ID, accounts: vec![], data: bench_data(rounds, 256) };
            let payer = Arc::clone(&f.payer);
            let out = f.ctx.raw_call(ix).fee_payer(&*payer).signers(&[&*payer]).send().expect("send");
            let cu = match out {
                crucible_test_context::TxOutcome::Success { compute_units, .. } => compute_units,
                other => panic!("bench {rounds} failed: {other:?}"),
            };
            used.push((rounds, cu));
        }
        println!("bench CU (rounds, CU): {used:?}");
        assert!(used[2].1 > used[1].1 && used[1].1 > used[0].1, "{used:?}");
    }

    /// §3.7: CreateVault takes a market made by Initialize at an address
    /// that is not the PDA, for a pair that already has a PDA market.
    #[test]
    fn create_vault_accepts_a_market_not_at_its_pda() {
        let mut f = BasicsFixture::setup();
        let other = f.stranger_market();
        assert_ne!(other, f.market);
        let (a, mint2) = (Arc::clone(&f.a), f.mint2);
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.create_vault_metas(&a.pubkey(), &other, &mint2),
            data: vec![IX_CREATE_VAULT],
        };
        let (ok, out) = f.send_logged(ix, a);
        assert!(ok, "refused: {out}");
        assert!(f.ctx.account_exists(&vault_token(&other, &mint2)));
    }

    /// Not a check: sends each spec once with each single-slot swap and with
    /// the signer flag cleared, and prints how it ended. Used to explain the
    /// "no" rows in §3.7 and §6.6 (`cargo test -- --ignored --nocapture probe`).
    #[test]
    #[ignore]
    fn probe() {
        let f0 = BasicsFixture::setup();
        for spec in f0.instructions() {
            let mut variants: Vec<(String, Vec<AccountMeta>)> = vec![];
            for &i in &spec.signer_indices {
                let mut m = spec.accounts.clone();
                m[i].is_signer = false;
                variants.push((format!("signer {i} off"), m));
            }
            for (i, alts) in spec.swap_alternates.iter().enumerate() {
                for alt in alts {
                    let mut m = spec.accounts.clone();
                    m[i].pubkey = *alt;
                    variants.push((format!("slot {i} swapped"), m));
                }
            }
            if spec.name == "create_vault" {
                // The market or the mint swapped together with the accounts
                // derived from it.
                let (m, n) = (f0.a_ata, f0.b_ata);
                let mut v = spec.accounts.clone();
                v[1].pubkey = m;
                v[3].pubkey = vault_token(&m, &f0.mint2);
                v[4].pubkey = vault_auth(&m);
                variants.push(("market set".into(), v));
                let mut v = spec.accounts.clone();
                v[2].pubkey = n;
                v[3].pubkey = vault_token(&f0.market, &n);
                variants.push(("mint set".into(), v));
            }
            if spec.name == "deposit" {
                // owner-skip's shape: a copy of the token account owned by
                // another program, in the source or the destination.
                for i in [1usize, 2] {
                    variants.push((format!("slot {i} owner-swapped"), spec.accounts.clone()));
                }
            }
            for (what, metas) in variants {
                let mut f = f0.clone();
                let mut metas = metas;
                if let Some(i) = what.strip_prefix("slot ").and_then(|w| w.strip_suffix(" owner-swapped")) {
                    let i: usize = i.parse().unwrap();
                    let real = f.ctx.get_account(&metas[i].pubkey).expect("account");
                    let copy = Keypair::new().pubkey();
                    f.put(copy, CORE_ID, real.data.clone());
                    metas[i].pubkey = copy;
                }
                let ix = Instruction { program_id: CORE_ID, accounts: metas, data: spec.data_sample.clone() };
                let (ok, out) = f.send_logged(ix, Arc::clone(&f.a));
                let lines: Vec<&str> = out.split("\", \"").collect();
                let pick = |p: &dyn Fn(&str) -> bool| lines.iter().rev().find(|l| p(l)).copied().unwrap_or("-");
                let log = pick(&|l| l.starts_with("Program log:"));
                let fail = pick(&|l| l.contains(" failed: "));
                let fail = fail.split(" failed: ").last().unwrap_or("-");
                println!("PROBE {} | {what} | ok={ok} | {} | {}", spec.name, log.chars().take(110).collect::<String>(), fail.split('"').next().unwrap_or(""));
            }
        }
    }

    /// Prints Bench CU over a grid of arguments, for the numbers in §4.7
    /// (`cargo test -- --ignored --nocapture bench_grid`).
    #[test]
    #[ignore]
    fn bench_grid() {
        let f0 = BasicsFixture::setup();
        for heap in [0u32, 256, 1024, 4095] {
            for rounds in [0u32, 1, 10, 100, 150] {
                let mut f = f0.clone();
                let ix = Instruction { program_id: CORE_ID, accounts: vec![], data: bench_data(rounds, heap) };
                let payer = Arc::clone(&f.payer);
                let out = f.ctx.raw_call(ix).fee_payer(&*payer).signers(&[&*payer]).send().expect("send");
                let cu = match out {
                    crucible_test_context::TxOutcome::Success { compute_units, .. } => compute_units.to_string(),
                    _ => "over 200k".into(),
                };
                println!("GRID heap={heap} rounds={rounds} cu={cu}");
            }
        }
    }

    /// Chapter 6: A cannot spend B's tokens; SPL Token checks the authority.
    #[test]
    fn deposit_refuses_another_users_source() {
        let mut f = BasicsFixture::setup();
        let (a, b_ata) = (Arc::clone(&f.a), f.b_ata);
        let ix = Instruction { program_id: CORE_ID, accounts: f.deposit_metas(&a.pubkey(), &b_ata), data: amount_data(IX_DEPOSIT, 10) };
        let (ok, out) = f.send_logged(ix, a);
        assert!(!ok, "A spent B's tokens");
        assert!(out.contains("owner does not match"), "refused for another reason: {out}");
    }
}
