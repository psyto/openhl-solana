//! solinv harness for the Chapter 12 trading vault — `VaultDeposit` and
//! `VaultWithdraw`.
//!
//! Loads the real `openhl_core.so` from `target/deploy/` (`cargo build-sbf
//! --manifest-path programs/openhl-core/Cargo.toml`). One market holds three
//! trading vaults: K's, where depositors A and B each hold shares, A's own,
//! and C's in a junk mint. The specs send A's deposit into K's vault and
//! A's withdrawal from it, and solinv swaps in B's share account, A's share
//! account in the other vault, the other vault, or a token account B holds.
//! A two-slot set swaps C's junk withdrawal onto the real mint's token
//! account. Any swap that succeeds and changes state is a finding.

use crucible_fuzzer::*;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::str::FromStr;
use std::sync::Arc;

use solinv_core::invariants::account_swap::SwapSet;
use solinv_fuzz::{HasContext, HasInstructionSet, InstructionSpec};

const CORE_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/openhl_core.so");

const CORE_ID: Pubkey = Pubkey::new_from_array([3u8; 32]);
const SYSTEM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);
const TOKEN_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// openhl-core instruction tags (programs/openhl-core/src/lib.rs).
const IX_CREATE_TRADING_VAULT: u8 = 19;
const IX_VAULT_DEPOSIT: u8 = 20;
const IX_VAULT_WITHDRAW: u8 = 21;

const START_BALANCE: u64 = 1_000_000;
const START_DEPOSIT: u64 = 100_000;

fn token() -> Pubkey {
    Pubkey::from_str(TOKEN_ID).unwrap()
}
fn trading_vault(market: &Pubkey, manager: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"trading_vault", market.as_ref(), manager.as_ref()], &CORE_ID).0
}
fn share(vault: &Pubkey, owner: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault_share", vault.as_ref(), owner.as_ref()], &CORE_ID).0
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

fn amount_data(tag: u8, amount: u64) -> Vec<u8> {
    let mut d = vec![tag];
    d.extend_from_slice(&amount.to_le_bytes());
    d
}

#[derive(Clone)]
struct VaultFixture {
    pub ctx: TestContext,
    payer: Arc<Keypair>,
    a: Arc<Keypair>,
    b: Arc<Keypair>,
    market: Pubkey,
    mint: Pubkey,
    /// K's vault, where A and B both hold shares.
    vault: Pubkey,
    /// A's own vault in the same market.
    a_vault: Pubkey,
    a_ata: Pubkey,
    b_ata: Pubkey,
    /// C's vault in a junk mint, in the same market.
    c: Arc<Keypair>,
    junk: Pubkey,
    c_vault: Pubkey,
    c_junk: Pubkey,
    c_real: Pubkey,
}

impl VaultFixture {
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

    fn deposit_metas(&self, vault: &Pubkey, who: &Pubkey, ata: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new(*who, true),
            AccountMeta::new(*vault, false),
            AccountMeta::new(share(vault, who), false),
            AccountMeta::new_readonly(SYSTEM_ID, false),
            AccountMeta::new_readonly(self.market, false),
            AccountMeta::new_readonly(self.mint, false),
            AccountMeta::new(*ata, false),
            AccountMeta::new(vault_token(&self.market, &self.mint), false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    fn withdraw_metas(&self, vault: &Pubkey, who: &Pubkey, ata: &Pubkey) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(*who, true),
            AccountMeta::new(*vault, false),
            AccountMeta::new(share(vault, who), false),
            AccountMeta::new_readonly(self.market, false),
            AccountMeta::new_readonly(self.mint, false),
            AccountMeta::new(*ata, false),
            AccountMeta::new(vault_token(&self.market, &self.mint), false),
            AccountMeta::new_readonly(vault_auth(&self.market), false),
            AccountMeta::new_readonly(token(), false),
        ]
    }

    /// `metas` with the mint at `mint_ix` and the market's token account
    /// at `token_ix` switched to the junk mint's.
    fn junk_metas(&self, mut metas: Vec<AccountMeta>, mint_ix: usize, token_ix: usize) -> Vec<AccountMeta> {
        metas[mint_ix] = AccountMeta::new_readonly(self.junk, false);
        metas[token_ix] = AccountMeta::new(vault_token(&self.market, &self.junk), false);
        metas
    }

    fn deposit(&mut self, vault: Pubkey, who: Arc<Keypair>, ata: Pubkey, what: &str) {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: self.deposit_metas(&vault, &who.pubkey(), &ata),
            data: amount_data(IX_VAULT_DEPOSIT, START_DEPOSIT),
        };
        self.send(ix, &[&*who], what);
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
        let payer = Arc::clone(&self.payer);
        self.ctx
            .raw_call(ix)
            .fee_payer(&*payer)
            .signers(&[&*payer, &*signer])
            .send()
            .map(|o| o.is_success())
            .unwrap_or(false)
    }
}

#[fuzz_fixture]
impl VaultFixture {
    pub fn setup() -> Self {
        let mut ctx = TestContext::new();
        ctx.add_program(&CORE_ID, CORE_SO)
            .expect("load openhl_core.so — run cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml");

        let payer = Arc::new(Keypair::new());
        let a = Arc::new(Keypair::new());
        let b = Arc::new(Keypair::new());
        let k = Arc::new(Keypair::new());
        let c = Arc::new(Keypair::new());
        for kp in [&payer, &a, &b, &k, &c] {
            ctx.create_account()
                .pubkey(kp.pubkey())
                .lamports(10_000_000_000)
                .owner(SYSTEM_ID)
                .create()
                .unwrap();
        }

        let market = Keypair::new().pubkey();
        let mint = Keypair::new().pubkey();
        let mut f = Self {
            ctx,
            payer: Arc::clone(&payer),
            a: Arc::clone(&a),
            b: Arc::clone(&b),
            market,
            mint,
            vault: trading_vault(&market, &k.pubkey()),
            a_vault: trading_vault(&market, &a.pubkey()),
            a_ata: Keypair::new().pubkey(),
            b_ata: Keypair::new().pubkey(),
            c: Arc::clone(&c),
            junk: Keypair::new().pubkey(),
            c_vault: trading_vault(&market, &c.pubkey()),
            c_junk: Keypair::new().pubkey(),
            c_real: Keypair::new().pubkey(),
        };

        // The vault handlers read only the market's key and owner, so the
        // market account is written directly instead of via CreateMarket.
        f.put(market, CORE_ID, vec![0u8; 8]);
        f.put(mint, token(), mint_data(6));
        f.put(vault_token(&market, &mint), token(), token_account_data(&mint, &vault_auth(&market), 0));
        let (aa, ba) = (f.a_ata, f.b_ata);
        f.put(aa, token(), token_account_data(&mint, &a.pubkey(), START_BALANCE));
        f.put(ba, token(), token_account_data(&mint, &b.pubkey(), START_BALANCE));

        // K's vault and A's vault, both in this market.
        for (manager, vault) in [(&k, f.vault), (&a, f.a_vault)] {
            let ix = Instruction {
                program_id: CORE_ID,
                accounts: vec![
                    AccountMeta::new(payer.pubkey(), true),
                    AccountMeta::new_readonly(market, false),
                    AccountMeta::new_readonly(manager.pubkey(), true),
                    AccountMeta::new_readonly(mint, false),
                    AccountMeta::new(vault, false),
                    AccountMeta::new_readonly(SYSTEM_ID, false),
                ],
                data: vec![IX_CREATE_TRADING_VAULT],
            };
            f.send(ix, &[&**manager], "create_trading_vault");
        }

        // A and B hold shares in K's vault; A also in its own.
        let (v, av) = (f.vault, f.a_vault);
        f.deposit(v, Arc::clone(&a), aa, "deposit A into K's vault");
        f.deposit(v, Arc::clone(&b), ba, "deposit B into K's vault");
        f.deposit(av, Arc::clone(&a), aa, "deposit A into A's vault");

        // C: a junk mint, the market's token account for it (CreateVault,
        // tag 5, lets anyone create one), a vault in it, and a deposit.
        let (junk, cv, cj, cr) = (f.junk, f.c_vault, f.c_junk, f.c_real);
        f.put(junk, token(), mint_data(6));
        f.put(vault_token(&market, &junk), token(), token_account_data(&junk, &vault_auth(&market), 0));
        f.put(cj, token(), token_account_data(&junk, &c.pubkey(), START_BALANCE));
        f.put(cr, token(), token_account_data(&mint, &c.pubkey(), 0));
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new_readonly(c.pubkey(), true),
                AccountMeta::new_readonly(junk, false),
                AccountMeta::new(cv, false),
                AccountMeta::new_readonly(SYSTEM_ID, false),
            ],
            data: vec![IX_CREATE_TRADING_VAULT],
        };
        f.send(ix, &[&*c], "C opens a junk-mint vault");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.junk_metas(f.deposit_metas(&cv, &c.pubkey(), &cj), 5, 7),
            data: amount_data(IX_VAULT_DEPOSIT, 3 * START_DEPOSIT),
        };
        f.send(ix, &[&*c], "C deposits junk");
        f
    }

    pub fn action_deposit(&mut self, #[range(1..10_000)] amount: u64) -> bool {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: self.deposit_metas(&self.vault, &self.a.pubkey(), &self.a_ata),
            data: amount_data(IX_VAULT_DEPOSIT, amount),
        };
        self.send_ok(ix, Arc::clone(&self.a))
    }

    pub fn action_withdraw(&mut self, #[range(1..10_000)] shares: u64) -> bool {
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: self.withdraw_metas(&self.vault, &self.a.pubkey(), &self.a_ata),
            data: amount_data(IX_VAULT_WITHDRAW, shares),
        };
        self.send_ok(ix, Arc::clone(&self.a))
    }
}

impl HasContext for VaultFixture {
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

impl HasInstructionSet for VaultFixture {
    fn instructions(&self) -> Vec<InstructionSpec> {
        let a = self.a.pubkey();
        let b = self.b.pubkey();
        let t = Some(token());
        let c = Some(CORE_ID);
        let deposit = InstructionSpec {
            program_id: CORE_ID,
            name: "vault_deposit".into(),
            accounts: self.deposit_metas(&self.vault, &a, &self.a_ata),
            signer_indices: vec![0],
            optional_signer_indices: vec![],
            expected_owners: vec![None, c, c, None, None, None, t, t, None],
            expected_discriminators: vec![None; 9],
            expected_pda_seeds: vec![None; 9],
            creates_indices: vec![],
            swap_alternates: vec![
                vec![],
                vec![self.a_vault],              // A's vault, with the share of K's
                vec![share(&self.vault, &b)],   // credit B's shares
                vec![],
                vec![],
                vec![],
                vec![],
                vec![self.b_ata],                // "vault" account B holds
                vec![],
            ],
            data_sample: amount_data(IX_VAULT_DEPOSIT, 10),
            signers: vec![Arc::clone(&self.a)],
            state_invariants: vec![],
            cu_budget: None,
            cpi_reentrancy: None,
            realloc_check: None,
            bump_seed_check: None,
            guards: vec![],
        };
        let withdraw = InstructionSpec {
            program_id: CORE_ID,
            name: "vault_withdraw".into(),
            accounts: self.withdraw_metas(&self.vault, &a, &self.a_ata),
            signer_indices: vec![0],
            optional_signer_indices: vec![],
            expected_owners: vec![None, c, c, None, None, t, t, None, None],
            expected_discriminators: vec![None; 9],
            expected_pda_seeds: vec![None; 9],
            creates_indices: vec![],
            swap_alternates: vec![
                vec![],
                vec![self.a_vault],                  // burn against A's vault instead
                vec![
                    share(&self.vault, &b),          // burn B's shares
                    share(&self.a_vault, &a),        // A's shares in the other vault
                ],
                vec![],
                vec![],
                // The destination is not swapped: paying one's own
                // withdrawal to someone else's account harms no one.
                vec![],
                vec![self.b_ata],                    // "vault" account B holds
                vec![],
                vec![],
            ],
            data_sample: amount_data(IX_VAULT_WITHDRAW, 10),
            signers: vec![Arc::clone(&self.a)],
            state_invariants: vec![],
            cu_budget: None,
            cpi_reentrancy: None,
            realloc_check: None,
            bump_seed_check: None,
            guards: vec![],
        };
        // C's own withdrawal from its junk vault; check_sets below swaps
        // its source and destination for the real mint's together.
        let junk_withdraw = InstructionSpec {
            name: "vault_withdraw_junk".into(),
            accounts: self.junk_metas(self.withdraw_metas(&self.c_vault, &self.c.pubkey(), &self.c_junk), 4, 6),
            swap_alternates: vec![vec![]; 9],
            signers: vec![Arc::clone(&self.c)],
            ..withdraw.clone()
        };
        vec![deposit, withdraw, junk_withdraw]
    }
}

#[invariant_test]
fn invariant_vault(fixture: &mut VaultFixture) {
    solinv_core::invariants::signer_skip::check(fixture);
    solinv_core::invariants::owner_skip::check(fixture);
    solinv_core::invariants::account_swap::check(fixture);
    // Withdraw junk, paid from the real mint's token account to C's own
    // real-mint account: two slots that only work together.
    let real_token = vault_token(&fixture.market, &fixture.mint);
    let sets = [SwapSet { ix: "vault_withdraw_junk".into(), swaps: vec![(5, fixture.c_real), (6, real_token)] }];
    solinv_core::invariants::account_swap::check_sets(fixture, &sets);
}

#[cfg(test)]
mod tests {
    use super::*;

    const IX_VAULT_UPDATE_NAV: u8 = 22;

    /// A manages its own vault in the same market, so it can mint itself
    /// shares there for almost nothing (report NAV 1, then deposit 2), and
    /// then burn them against K's vault, whose tokens sit in the same
    /// per-(market, mint) token account.
    #[test]
    fn withdraw_refuses_a_share_from_another_vault() {
        let mut f = VaultFixture::setup();
        let (a, mint, market) = (Arc::clone(&f.a), f.mint, f.market);
        let (vault, a_vault, a_ata) = (f.vault, f.a_vault, f.a_ata);

        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new_readonly(a.pubkey(), true), AccountMeta::new(a_vault, false)],
            data: amount_data(IX_VAULT_UPDATE_NAV, 1),
        };
        f.send(ix, &[&*a], "A reports NAV 1 for its own vault");
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.deposit_metas(&a_vault, &a.pubkey(), &a_ata),
            data: amount_data(IX_VAULT_DEPOSIT, 2),
        };
        f.send(ix, &[&*a], "A deposits 2 into its own vault");

        let before = f.ctx.token_balance(&a_ata);
        // K's vault, but the share account of A's vault.
        let mut accounts = f.withdraw_metas(&vault, &a.pubkey(), &a_ata);
        accounts[2] = AccountMeta::new(share(&a_vault, &a.pubkey()), false);
        let ix = Instruction { program_id: CORE_ID, accounts, data: amount_data(IX_VAULT_WITHDRAW, 2 * START_DEPOSIT) };
        let (ok, out) = f.send_logged(ix, Arc::clone(&a));
        let taken = f.ctx.token_balance(&a_ata) - before;
        let left = f.ctx.token_balance(&vault_token(&market, &mint));
        assert!(!ok, "withdraw from K's vault with A's other share succeeded: A took {taken}, {left} left in the token account");
        assert!(out.contains("share belongs to another vault"), "refused for another reason: {out}");
    }

    /// CreateVault (tag 5) lets anyone create the market's token account for
    /// any mint, and the vault authority PDA is the owner of all of them. So
    /// C opens a vault in a junk mint, deposits junk, and withdraws naming
    /// the real mint's token account as the source and its own real-mint
    /// account as the destination. Two accounts change together; the fuzzer
    /// reaches it only through the `check_sets` pair in `invariant_vault`.
    #[test]
    fn withdraw_refuses_the_token_account_of_another_mint() {
        let mut f = VaultFixture::setup();
        let (market, mint) = (f.market, f.mint);
        let (c, c_vault, c_real) = (Arc::clone(&f.c), f.c_vault, f.c_real);
        let mut accounts = f.withdraw_metas(&c_vault, &c.pubkey(), &c_real);
        accounts[4] = AccountMeta::new_readonly(f.junk, false);
        // accounts[6] stays the real mint's token account.
        let ix = Instruction { program_id: CORE_ID, accounts, data: amount_data(IX_VAULT_WITHDRAW, 3 * START_DEPOSIT) };
        let (ok, out) = f.send_logged(ix, Arc::clone(&c));
        let taken = f.ctx.token_balance(&c_real);
        let left = f.ctx.token_balance(&vault_token(&market, &mint));
        assert!(!ok, "C swapped junk for {taken} real tokens; {left} left in the token account");
        assert!(out.contains("vault_token_account does not match derived PDA"), "refused for another reason: {out}");
    }

    /// Not fixed: a vault's manager sets its NAV, and every vault in the
    /// market pays out of one token account, so a manager who over-reports
    /// NAV withdraws other vaults' deposits. Closing this needs a token
    /// account per vault (a design change, not a missing check).
    #[test]
    #[ignore = "known hole: manager-reported NAV draws on the market's shared token account"]
    fn manager_cannot_withdraw_more_than_its_vault_holds() {
        let mut f = VaultFixture::setup();
        let (a, mint, market) = (Arc::clone(&f.a), f.mint, f.market);
        let (a_vault, a_ata) = (f.a_vault, f.a_ata);

        let ix = Instruction {
            program_id: CORE_ID,
            accounts: vec![AccountMeta::new_readonly(a.pubkey(), true), AccountMeta::new(a_vault, false)],
            data: amount_data(IX_VAULT_UPDATE_NAV, 3 * START_DEPOSIT),
        };
        f.send(ix, &[&*a], "A reports its own vault's NAV at 3x what it holds");
        let before = f.ctx.token_balance(&a_ata);
        let ix = Instruction {
            program_id: CORE_ID,
            accounts: f.withdraw_metas(&a_vault, &a.pubkey(), &a_ata),
            data: amount_data(IX_VAULT_WITHDRAW, START_DEPOSIT),
        };
        let ok = f.send_ok(ix, Arc::clone(&a));
        let taken = f.ctx.token_balance(&a_ata) - before;
        let left = f.ctx.token_balance(&vault_token(&market, &mint));
        assert!(
            !ok || taken <= START_DEPOSIT,
            "A put {START_DEPOSIT} into its own vault and took {taken} out; {left} left in the token account"
        );
    }
}
