# 第11章 — ポジション ライフサイクルと清算エンジン

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`Position`）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（ヘルパ + `process_open_position` 1881–1993 行、`process_close_position` 1995–2061 行、`process_liquidate` 2063–2152 行）、[`scripts/position/src/main.rs`](../../scripts/position/src/main.rs)。

---

## §11.0  はじめに

これが収束の章だ。他のすべての Phase A と Phase B のプリミティブ — オラクル、ファンディング、vault、マッチャ、並列性 — はこの章が書けるために存在する。`OpenPosition` / `ClosePosition` / `Liquidate` のない perp DEX は piece の寄せ集めで、それらを持つ perp DEX は perp DEX だ。

本章は 3 つの命令を出荷し、いずれも第 6 章の SPL Token エスクロー パスをポジション ライフサイクルに直接統合する。

1. **`OpenPosition`** — (user, market) ごとの Position PDA を作成し、entry 価格をオラクルから読み、後の決済のために累積ファンディング指数をスナップショットし、初期証拠金要件を検証し、**担保をエスクロー**する。ユーザの quote トークンアカウントから market vault（第 6 章で組み立てた (market, mint) ごとの vault）へ SPL Token Transfer を CPI する。
2. **`ClosePosition`** — 所有者の退出。第 10 章のスナップショット パターンでファンディングを決済し、実現 PnL = `size × (mark - entry)` を計算し、**実現額を vault からユーザに戻す** — vault authority PDA が署名する SPL Token CPI（`invoke_signed` を `[b"vault_auth", market]` シードで）— その上で position をゼロ化する。
3. **`Liquidate`** — 誰でも他者の水没ポジションに対して行える退出。equity を計算し、維持証拠金と比較し、ポジションが下回っていれば現行マークで強制クローズする。**ハンドラ内で 2 つの SPL Token CPI を走らせる**: vault → 清算者にペナルティ bounty、vault → ポジション所有者に残額。両方とも vault authority PDA が署名する。

担保は本物の perp DEX が置く場所 — プログラムの vault トークンアカウント、SPL Token 所有、`invoke_signed` 経由でしか動かせない PDA が制御 — に住む。ポジション レコードは**簿記**（size、entry 価格、snapshot index）を持つ。vault は**お金**を持つ。両者は同期を保つ、すべての状態遷移が簿記と CPI を同じハンドラ内でアトミックに更新するからだ。

本章に残るスコープ正直性ノートは 1 つ: **保険基金**。ポジションが水没でクローズしたとき（`equity < 0`）、預けられた担保はすでに vault に座っている — そして本プログラムは現状その残余を損失吸収に任せる。本番では各清算ペナルティの一部を `InsuranceFund` アカウントに回し、水没クローズで不足分が発生したら基金から引き出し、基金が空になって初めて LP プールに社会化する。§11.6 で論じるが実装はしない。それ自体が後続章のテーマだ。

---

## §11.1  `Position` アカウント

(user, market) ペアあたり PDA 1 つ。144 バイト。`crates/state/src/lib.rs` から。

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Position {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub user: [u8; 32],
    pub market: [u8; 32],
    pub size: i64,                    // base 単位; 符号付き: ロング > 0、ショート < 0
    pub entry_price: u64,             // open 時に刻印された base あたり quote
    pub collateral: u64,              // 証拠金として掛けられた quote 単位
    pub funding_snapshot_index: i64,  // 最終タッチ時の FundingState.cumulative
    pub _reserved: [u8; 32],
}
```

load-bearing なフィールド 6 つ、加えて discriminator + bump + padding。

**`size: i64`** は符号付き。ロング ポジションは正のサイズ、ショートは負。`size == 0` は「ポジション クローズ済み」の番兵 — 第 7 章の空スロット規約と同じ。クローズや liquidate の後、アカウントは `size = 0` で残り、新たな `OpenPosition` で再オープン可能（同じ PDA を派生し、休眠状態を上書きする）。アカウントを文字通り close（rent 返金）しないことを選んだのは、(user, market) ごとの PDA 派生が「ポジションがあるかないか」の関係を保証し、スロットを残しておけば再オープン時の再作成 CPI が省けるからだ。

**`entry_price: u64`** は `OpenPosition` 時にオラクルから刻印したマーク価格。価格 PnL の参照点: `(mark - entry) × size`。部分クローズのための走行 entry 価格は維持しない。本章の `ClosePosition` は all-or-nothing。部分クローズには各部分での `entry_price` をサイズ加重平均にリセットする必要がある — 有用な拡張だがスコープ外。

**`collateral: u64`** は quote 通貨の証拠金額。ポジション オープン中は厳密に正、水没クローズや清算でゼロまで減少しうる。負にはなれない — 担保を超える損失は保険基金（あるいはスコープ繰り延べ版では単に失われる）に社会化される。

**`funding_snapshot_index: i64`** は最終タッチ（open、close、liquidate）時の累積ファンディング指数。第 10 章のポジションごとの決済パターンが、これをファンディング会計に必要な唯一のフィールドにする — `funding_now` と `funding_snapshot_index` の差にサイズを掛けたものが、スナップショット以降に蓄積したファンディング PnL だ。

PDA 派生は `user` と `market` の両方をシードに使う: `[b"position", user.key, market.key]`。だから (user, market) ペアごとに、誰もがマッピングを保存せず計算できるポジション アドレスが正確に 1 つ存在する。pubkey は資産ペアとトレーダにシード スキームだけで束縛される。

> **演習 §11.1.** ポジションが `user` と `market` の両方をアカウント**内部**にも保存する理由は何か。両方とも PDA 派生のシードであるにもかかわらず。（ヒント: アカウントを読む第三者が知っていることと派生しなければならないことを考えよ。）

---

## §11.2  equity、notional、証拠金式

ハンドラを歩く前に式を固定する。`programs/openhl-core/src/lib.rs:1814–1834` から。

```rust
fn compute_equity(position: &Position, mark: u64, funding_index_now: i64) -> i128 {
    let size = position.size as i128;
    let entry = position.entry_price as i128;
    let mark_i = mark as i128;
    let collateral = position.collateral as i128;

    let price_pnl = size * (mark_i - entry);

    let funding_delta = (funding_index_now as i128) - (position.funding_snapshot_index as i128);
    let funding_pnl = funding_delta * size / 1_000_000_000_i128;

    collateral + price_pnl + funding_pnl
}

fn notional(size: i64, mark: u64) -> u128 {
    let abs_size = (size.unsigned_abs()) as u128;
    abs_size * (mark as u128)
}
```

本章が気にする量は 3 つ。

**Notional** = `|size| × mark`。現行価格でのポジションのドル（quote 通貨）価値。マーク 100 で base 5 単位のロングは notional 500 quote 単位。ロングもショートも notional は正 — 方向は PnL にとって重要、notional にとっては重要ではない。

**価格 PnL** = `size × (mark - entry)`。符号付き。ロング ポジションはマークが上がると利益（正のサイズ × 正のデルタ = 正の PnL）、ショート ポジションはマークが下がると利益（負のサイズ × 負のデルタ = 正の PnL）。算術は方向の特別扱いなしで動く、`size` が符号を運ぶからだ。

**ファンディング PnL** = `(index_now - index_snapshot) × size / 1e9`。価格 PnL と同じ形だがファンディング指数が価格の役を果たす。`/1e9` が第 10 章の `FundingState` が指数に使う 1e9 スケーリングを戻す。正のサイズのロングなら、ファンディング指数の上昇（ロングがショートに支払う）が `funding_delta × size` を正にし、式の符号が出揃うとファンディング PnL が負になる — まさに正しい意味論。

**Equity** = `collateral + price_pnl + funding_pnl`。今ポジションが指揮する quote 通貨総価値。深く水没したポジションでは equity が負になりうる。プログラムは close/liquidate でゼロにクランプする（損失は相手方に渡すのではなく社会化される）。

**維持証拠金 (Maintenance margin)** = `notional × MAINT_MARGIN_BPS / 10000`。ポジションをオープン状態に保つために必要な最小 equity。`MAINT_MARGIN_BPS = 500`（5%）で、notional 500 のポジションは清算を避けるために equity ≥ 25 quote が必要。

**初期証拠金 (Initial margin)** = `notional × INITIAL_MARGIN_BPS / 10000`。オープン時に必要な最小担保。`INITIAL_MARGIN_BPS = 1000`（10%）で、同じ notional 500 ポジションをオープンするには ≥ 50 quote の担保が必要。

IM（10%）と MM（5%）のギャップが**維持バッファ**だ — 清算される前にポジションが逆行できる距離。IM でオープンしたポジションが notional の 50% 不利に動けば equity ゼロ（担保使い切り）に達してから清算がトリガする。IM でオープンしたポジションが 5% 不利な動きでもまだ健全。IM↔MM ギャップが狭いほど資本効率は良いが清算されやすい。

> **演習 §11.2.** サイズ = 10 base 単位、entry = 100、担保 = 200（10% IM）でロング ポジションをオープンする。マーク 90、95、100、105、110 で equity を計算せよ。どのマークでポジションは清算可能か。（ファンディングは今のところ無視する。）

---

## §11.3  `OpenPosition` を歩く

`programs/openhl-core/src/lib.rs` の `process_open_position`。ハンドラは 6 部分に分解できる: 検証、オラクル/ファンディング読み、初期証拠金チェック、ポジション PDA 確保、**担保エスクロー CPI**、ポジション状態書き込み。

**検証**: ペイロード サイズ、非ゼロ size と担保、user は signer、market は本書のプログラム所有、system プログラムは System プログラム、**token プログラムは SPL Token、user_token_account は SPL Token 所有、vault_token_account は `[VAULT_SEED, market, mint]` の派生 PDA と一致**（escrow 側の新しいチェック）。ポジション PDA 派生:

```rust
let (expected, bump) = Pubkey::find_program_address(
    &[POSITION_SEED, user_ai.key.as_ref(), market_ai.key.as_ref()],
    program_id,
);
if position_ai.key != &expected {
    return Err(ProgramError::InvalidSeeds);
}
```

**外部入力を読む**（1922–1923 行）:

```rust
let mark = read_fresh_oracle(oracle_ai, program_id)?;
let funding_snapshot = read_funding_index(funding_ai, program_id)?;
```

`read_fresh_oracle`（1838–1858 行）は第 9 章の staleness ガントレットをヘルパに分離した — 同じチェック（owner + discriminator + price>0 + Clock に対する age）、3 つのポジション ハンドラで再利用。`read_funding_index`（1860–1869 行）はファンディング指数をスナップショットする簡単な読み。

**初期証拠金チェック**（1932–1942 行）:

```rust
let notional_val = notional(size, mark);
let im_required = notional_val * (INITIAL_MARGIN_BPS as u128) / 10_000;
if (collateral as u128) < im_required {
    msg!(
        "open_position: collateral {} < initial margin {} ...",
        collateral, im_required, ...
    );
    return Err(ProgramError::InvalidArgument);
}
```

担保は少なくとも notional の 10% をカバーしなければならない。価格 100 でサイズ 10 のポジション（notional = 1000）を担保 50 で要求すると、チェックは拒否する: 50 < 100（IM）。担保 100 ならちょうど IM で受理。担保 200 なら IM の上に 100 のバッファ付きで受理。

**ポジション PDA を確保**: `[POSITION_SEED, user.key, market.key, bump]` で署名する `System::create_account` への標準的な `invoke_signed`。`CreateMarket`、`CreateVault` などと同じパターン — 第 3 章で導入、以降のすべての章で再利用。

**SPL Token Transfer 経由で担保をエスクロー**:

```rust
spl_token_transfer_user_signed(
    user_token_ai,    // source — ユーザの quote アカウント
    vault_token_ai,   // destination — (market, mint) ごとの vault PDA
    user_ai,          // authority — ユーザ、外側 tx 署名者
    token_ai,         // SPL Token プログラム
    collateral,       // 量、quote base 単位
)?;
```

`spl_token_transfer_user_signed` は position セクション冒頭で抽出した 4 つの escrow ヘルパの 1 つ。第 6 章の bytes-up パターンで SPL Token Transfer 命令を手で組み立て（`[tag=3, amount_le]` データ + `[source, dest, authority]` アカウント）、プレーン `invoke` を呼ぶ。ユーザの外側トランザクション署名が、第 6 章 §6.2 の署名者特権延長を経由して SPL Token に流れる。この CPI が確定すれば、ユーザの quote 残高は `collateral` 単位減り、vault は同額増える。

順序が重要だ: ポジション PDA は転送の**前**に確保する。転送が失敗（残高不足）した場合、トランザクション全体を revert させたい — そして実際にそうなる、孤児 Position アカウントは残らない。順序を逆にすると、InsufficientFunds で半初期化 Position（rent 支払い済み、escrow なし）が残る。tx 全体のアトミシティが、自然なエラー処理を正しくする。

**ポジション状態を書く**:

```rust
position.size = size;
position.entry_price = mark;
position.collateral = collateral;
position.funding_snapshot_index = funding_snapshot;
```

データ書き込み 4 つ。`entry_price = mark` がオラクル価格をポジションの参照点として刻印する。`funding_snapshot_index = funding_snapshot` がこの瞬間のファンディング指数を捕捉する — 将来のすべての close/liquidate がこのスナップショットからの差分としてファンディング PnL を計算する。`collateral` は vault にエスクローされたものを写す。簿記と vault 残高は、同じハンドラが両方をアトミックに更新するので同期を保つ。

> **演習 §11.3.** stale なオラクル（最後の `SetOraclePrice` から 25 slot 以上）に対して `OpenPosition` を試すと何が起きるか。`read_fresh_oracle` を通って失敗パスを辿れ。次に `funding --update --rate 0` と `oracle --set --price ...` を走らせてオープンを再試行せよ。

---

## §11.4  `ClosePosition` を歩く

`process_close_position`。オープンより単純な部分（PDA 作成なし）と複雑な部分が両方ある: vault authority PDA が `invoke_signed` で署名する outbound SPL Token CPI が加わる。

**検証 + 所有者チェック**（2007–2024 行）:

```rust
if position.user != *user_ai.key.as_ref() {
    msg!("close_position: caller is not the position owner");
    return Err(ProgramError::IllegalOwner);
}
```

ポジションの所有者だけが自発的にクローズできる。Liquidate（§11.5）が他の誰でも通れる経路。user チェックは PDA 派生ではなくポジション内に保存した `user` フィールドを使う — 同じ情報、読みやすい。

**外部入力を読み equity を計算**（2026–2040 行）:

```rust
let mark = read_fresh_oracle(oracle_ai, program_id)?;
let funding_now = read_funding_index(funding_ai, program_id)?;
// ...
let equity = compute_equity(position, mark, funding_now);
```

オープンと同じオラクル + ファンディングの読みパターン。クローズで重要な計算は equity だけ — ポジションが今 quote 通貨換算で何の価値があるかを教える。

**payout を計算してポジション レコードをゼロ化**（`try_borrow_mut_data` スコープ内で、借用が CPI 前にドロップするように）:

```rust
let payout: u64;
{
    let mut data = position_ai.try_borrow_mut_data()?;
    let position: &mut Position = bytemuck::from_bytes_mut(...);
    // ... 所有者チェック、size != 0 チェック ...
    let equity = compute_equity(position, mark, funding_now);
    payout = if equity < 0 { 0 } else { equity as u64 };

    position.collateral = 0;
    position.size = 0;
    position.entry_price = 0;
    position.funding_snapshot_index = funding_now;
}
```

`position.collateral = 0` に注目 — その価値はもうポジション アカウントに保持されない。これから vault から支払われる。ポジションは純粋に「閉じた」番兵になる: size 0、entry 0、collateral 0。

**vault からユーザに支払う**、`invoke_signed` 経由:

```rust
spl_token_transfer_vault_signed(
    vault_token_ai,
    user_token_ai,
    vault_authority_ai,
    market_ai.key,
    vault_auth_bump,
    token_ai,
    payout,
)?;
```

vault authority は `[VAULT_AUTH_SEED, market]` の PDA なので、プログラムが署名する: `invoke_signed` を `[VAULT_AUTH_SEED, market_key, &[bump]]` で。vault トークン アカウントが `payout` 単位を失い、ユーザのトークン アカウントが受け取る。`payout == 0`（水没クローズ）ならヘルパは CPI をスキップ — ゼロ転送に CU を燃やす意味がない。

**水没クローズは担保を失うが、損失を相手方に渡さない。** equity = -50（損失が担保を超える）でクローズするポジションは `payout = 0` をユーザに送る。しかし元々預けた 100 単位はまだ vault に座っており、もはやポジション レコードと結びついていない。その残余が、取引の相手方への暗黙のサブシディだ。本番では InsuranceFund がこれらの残余 + 各清算ペナルティの一部から不足分を適切にカバーする。§11.6 を見よ。

> **演習 §11.4.** entry = 100、サイズ = 5、担保 = 100 でポジションをオープンする。オラクルをマーク = 80 に動かす。クローズ。期待される equity は `100 + 5 × (80 - 100) = 0`。クローズ後のユーザの quote トークン残高は、オープン前と変わらないことを確認せよ（payout = 0 — 預けた 100 は vault に入ってそこに留まる）。

---

## §11.5  `Liquidate` を歩く

`process_liquidate`。クローズとの重要な違い: **誰でも呼べる**。ハンドラは**2 つの**outbound SPL Token CPI を走らせる — vault → 清算者にペナルティ bounty、vault → ポジション所有者に残額 — 両方とも vault authority PDA が署名する。

**検証**: **清算者**は signer でなければならないが、プログラムは清算者がポジションの user と一致するかをチェック**しない**。誰でも誰のポジションに対しても liquidate を呼べる。escrow 側の追加チェック: token_program は SPL Token、`owner_token` と `liquidator_token` は両方とも SPL Token 所有、vault_token は派生 PDA と一致、vault_authority は派生 PDA と一致（bump は下の 2 つの invoke_signed 呼び出しのために捕捉）。

```rust
let liquidator_ai = accounts.first().ok_or(...)?;
// ...
if !liquidator_ai.is_signer { return Err(...); }
```

この無許可性が清算エンジンの中心。システムは小さな bounty（清算ペナルティ）を、最初に水没ポジションに気づいて清算 tx を提出した誰かに支払う。これなしには、清算はプロトコル チームが集中清算 bot を走らせることに依存する — 動くがアップタイム リスクが入る。

**ヘルス チェック**（2098–2111 行）:

```rust
let equity = compute_equity(position, mark, funding_now);
let notional_val = notional(position.size, mark);
let maint_required = (notional_val * (MAINT_MARGIN_BPS as u128) / 10_000) as i128;

if equity >= maint_required {
    msg!(
        "liquidate: position is healthy (equity {} >= maint {}), not liquidatable",
        equity, maint_required
    );
    return Err(ProgramError::InvalidArgument);
}
```

`equity >= maintenance_margin` ならポジションは健全で呼び出しは拒否される。清算者は無意味に tx 手数料を払った — 健全ポジションに対する liquidate のスパム呼び出しを小さく抑制する。（本番プロトコルは時にこういうとき tx 手数料を返金するか、清算者が提出前にオフチェーン ヘルス チェックをやることを期待する。）

**ペナルティ適用 + 強制クローズ + 2 つの CPI**:

```rust
// 借用スコープ内（ポジション データ参照が CPI 前にドロップするように）:
let equity = compute_equity(position, mark, funding_now);
let raw_penalty = (notional_val * LIQUIDATION_PENALTY_BPS as u128 / 10_000) as i128;
let equity_positive = if equity < 0 { 0 } else { equity };
let penalty = raw_penalty.min(equity_positive);           // 利用可能 equity で上限
let owner_remainder = (equity_positive - penalty).max(0);

penalty_amount = penalty as u64;
owner_amount = owner_remainder as u64;

position.collateral = 0;
position.size = 0;
position.entry_price = 0;
position.funding_snapshot_index = funding_now;

// ─── 借用スコープ外 ───
// CPI 1: vault → 清算者
spl_token_transfer_vault_signed(vault_token_ai, liquidator_token_ai, ..., penalty_amount)?;
// CPI 2: vault → ポジション所有者
spl_token_transfer_vault_signed(vault_token_ai, owner_token_ai, ..., owner_amount)?;
```

ペナルティは生き残った equity で上限を取る（残 equity が 10 単位のポジションから 50 単位の bounty を払うことはできない）。2 つの CPI は順次、両方とも同じ vault-authority シードで `invoke_signed`。両方が成功してポジションが完全に巻き戻るか、トランザクション全体が revert する — アトミシティが帳簿を整合させる。

ペナルティは 2 つの目的を持つ。

1. **清算者インセンティブ。** 清算 bot の運営にはコストがある（RPC 帯域、gas、監視インフラ）。ペナルティが作業を経済的に成立させる bounty。
2. **ユーザ ディスインセンティブ。** 清算閾値に近づくのは、最終的に生き残るとしても（例: 清算直後に価格が反転）コストになる。ユーザは IM↔MM ギャップが示唆する以上のバッファを MM の上に維持するよう押される。

Liquidate ハンドラはポジションが**なぜ**水没したかを検証**しない**。価格の動き（マークが逆行）、ファンディング蓄積（時間とともに複利化したレート）、あるいは両方でありうる。equity 計算は両方の寄与を含み、維持チェックは原因に関わらず equity vs notional だ。これが正しい: 清算は資不足でトリガするのであって、根本原因でトリガするのではない。

> **演習 §11.5.** 教科書的な「death spiral」シナリオを組み立てよ:
>   1. サイズ = 10、entry = 100、担保 = 100（IM ちょうど）でロングをオープン。
>   2. オラクル マークを 95 に動かす（価格下落）。`equity` と `maint_required` をチェック — ポジションは清算可能か。下落のコストは 10 × (95 − 100) = -50、なので equity = 50、maint = 10×95×0.05 = 47.5。まだ健全。
>   3. 94 に動かす。equity = 40、maint = 47。**今度は**清算可能。
>   4. **別の**鍵ペアから Liquidate を提出する。ポジションがクローズしペナルティが適用されることを確認。

---

## §11.6  欠けたピース — 保険基金

本章が今もなお実装しないこと 1 つ、本番での役割を明示しておく。

**保険基金。** market ごとの別 `InsuranceFund` アカウントが、水没クローズの不足分をカバーする quote 通貨プールを持つ。パターン:

```text
ClosePosition / Liquidate が equity < 0 を計算したとき:
    shortfall = -equity
    if insurance_fund.balance >= shortfall:
        insurance_fund.balance -= shortfall
        # 相手方は補填され、人生は続く
    else:
        # 自動レバ削減か社会化損失 — より大きなアーキテクチャ問題
```

保険基金は清算ペナルティの一部（例: 50% 清算者、50% 保険基金）、取引手数料、ときに立ち上げ時の取引所エクイティで資金供給される。保険基金なしには、担保不足のすべての損失ポジションが、相手方 — 通常 LP プールか板の残り — に隠れた損失を課す。

本書の現行のエスクロー版ハンドラでは、水没クローズの残余は vault に留まる — 物理的には、ユーザの当初預け入れがまだそこにあるが、アクティブなポジションには結びついていない。その残余が暗黙のうちに相手方にサブシディしている。保険基金はこの残余を適切に経路化する: 引き出し時に各清算ペナルティの一部を基金に回し、水没クローズで vault に残余が出るたびに基金から引き出す。会計はそれ自体が小さな章だ（本トラックに加えるなら 15 章目）— 数学は単純、配線は `Liquidate` と `ClosePosition` を触り、新規 `InsuranceFund` PDA が唯一の state 追加だ。

---

## §11.7  まとめと自己検証

### まとめ図

```
ポジション ライフサイクル:

  ┌──────────────────────────────────────────────────────────────────┐
  │                                                                  │
  │            ┌────────────────┐                                    │
  │   user ──► │ OpenPosition   │   読み: oracle(mark) + funding      │
  │            │                │   書き: position (size, entry,      │
  │            └───────┬────────┘            collateral, snapshot)   │
  │                    │                                             │
  │                    ▼                                             │
  │            ┌────────────────┐                                    │
  │            │   live state   │   (マーク動く、ファンディング蓄積)   │
  │            └───┬────────┬───┘                                    │
  │                │        │                                        │
  │   所有者のみ    │        │   無許可                                │
  │                ▼        ▼                                        │
  │     ┌──────────────┐  ┌──────────────┐                           │
  │     │ ClosePosition│  │  Liquidate   │   読み: oracle + funding   │
  │     │ (PnL 決済)    │  │ (ペナルティ + │   書き: position           │
  │     │              │  │  強制クローズ)│       (collateral, size=0) │
  │     └──────────────┘  └──────────────┘                           │
  │                                                                  │
  └──────────────────────────────────────────────────────────────────┘


数学:
  notional       = |size| × mark
  initial_margin = notional × INITIAL_MARGIN_BPS / 10000   (10%)
  maint_margin   = notional × MAINT_MARGIN_BPS / 10000     (5%)
  price_pnl      = size × (mark − entry_price)
  funding_pnl    = (index_now − snapshot_index) × size / 1e9
  equity         = collateral + price_pnl + funding_pnl
  liquidatable   = equity < maint_margin
```

### 自分で検証する 3 項目

1. **IM↔MM ギャップがバッファ。** IM ちょうど（担保 = notional の 10%）でポジションをオープン。オラクルを動かさずに、ダンプ出力からポジションが健全（equity ≈ collateral、MM をしっかり上回る）であることを確認。IM ギャップが消費される程度（5% 逆行）にオラクルを動かす。これで equity ≈ MM — まだ清算可能ではない。さらに 1 bp の逆行で `Liquidate` が成功する。
2. **ファンディング蓄積で答えが反転しうる。** 担保ちょうど MM でポジションをオープン。オラクルに触れない。`funding --update --rate 100` で逆方向にファンディングを蓄積させ、1 分待ち、`funding --update --rate 100`。ダンプでポジションの計算済み equity をチェック — 価格は動いていなくてもファンディング PnL が equity を MM 以下に引きずる。`Liquidate` が成功する。
3. **クローズは担保を返すが、清算は返さない。** IM でオープン、即座にクローズ（価格動かず、ファンディングなし）。`position.collateral` ≈ オリジナル。再び IM でオープン、MM まで落ちて別の鍵ペアに清算される。清算後の `position.collateral` = equity − ペナルティ ≈ かなり少ない。ペナルティが「綺麗に退出する」vs「清算される」の差。

---

## 第 12 章への導線

ポジションがオープン、クローズ、強制清算できる perp DEX を持つようになった。スループットの単位はこれで大きくなる: 単一の `OpenPosition` は 6 アカウントを巻き込み、`Liquidate` は 4、補助の読み（オラクル + ファンディング）がさらにいくつか加わる。触れるアカウントは書き込みセット グラフを形成し — そのグラフがどう配置されるかが Sealevel が並列に走らせられるものと直列化するものを決定する。

第 12 章では**ネイティブ vault プログラム**を組み立てる — ユーザ担保を全体として取引される基金に集約する専用ラッパ アカウント。Vault 預金者は PnL を共有する。vault 管理者は本書の Phase B プリミティブを使って彼らのために取引する。Vault アカウントはポジションごとの取引とは異なる書き込みセット グラフを形成する: すべての deposit が vault 合計に触れ、すべての取引が vault が所有するポジションに触れる。第 5 章のシングルトン書き込み共有アンチパターンが再び立ち上がる（vault 合計は**まさに**シングルトン）のを見て、アーキテクチャがスループットを正気に保つために打つべき設計手を見る。
