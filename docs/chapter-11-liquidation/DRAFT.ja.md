# 第11章 — ポジション ライフサイクルと清算エンジン

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`Position`）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（ヘルパ + `process_open_position` 1881–1993 行、`process_close_position` 1995–2061 行、`process_liquidate` 2063–2152 行）、[`scripts/position/src/main.rs`](../../scripts/position/src/main.rs)。

---

## §11.0  はじめに — そして欠けた CPI

これが収束の章だ。他のすべての Phase A と Phase B のプリミティブ — オラクル、ファンディング、vault、マッチャ、並列性 — はこの章が書けるために存在する。`OpenPosition` / `ClosePosition` / `Liquidate` のない perp DEX は piece の寄せ集めで、それらを持つ perp DEX は perp DEX だ。

本章は 3 つの命令を出荷する。

1. **`OpenPosition`** — (user, market) ごとの Position PDA を作成し、entry 価格をオラクルから読み、後の決済のために累積ファンディング指数をスナップショットし、初期証拠金要件を満たす担保がユーザから掛けられているか検証する。
2. **`ClosePosition`** — 所有者の退出。第 10 章のスナップショット パターンでファンディングを決済し、実現 PnL = `size × (mark - entry)` を計算し、担保に適用し、ポジションをゼロ化する。
3. **`Liquidate`** — 誰でも他者の水没ポジションに対して行える退出。equity を計算し、維持証拠金と比較し、ポジションが下回っていれば現行マークで強制クローズし、（残担保から）ペナルティを清算者に支払う。

先にスコープに関する正直なメモを 1 つ: **担保はここでは追跡されるが、エスクローはされない**。本番では第 6 章の vault を統合する — `OpenPosition` はユーザの quote トークンアカウントから market vault へ SPL Token Transfer の CPI、close/liquidate では逆方向。本章の数学はトークンがどこに住んでいようと走らせるべき数学そのものだ。欠けているのは SPL Token CPI の配線。追加は機械的（第 6 章の `Deposit` のパターンが直接持ち越せる）だが、各ハンドラの AccountMeta 数が倍になり、本章が中心としているライフサイクル/数学の焦点を覆い隠す。

本番ではさらにクローズした担保を清算者と**保険基金 (insurance fund)** アカウントの間で分割する — ポジションが水没でクローズし、相手方を満足させる担保が不足するときに基金が不足分をカバーする。§11.6 で保険基金の役割を論じるが実装はしない。それ自体が小さな後続章のテーマだ。

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

`programs/openhl-core/src/lib.rs:1881–1993` の `process_open_position`。ハンドラは 5 部分に分解できる。

**検証**（1894–1916 行）: ペイロード サイズ、非ゼロ size と担保、user は signer、market は本書のプログラム所有、system プログラムは System プログラム。PDA 派生:

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

**PDA を確保**（1946–1960 行）: ポジション PDA のシードで署名する `System::create_account` への標準的な `invoke_signed`。`CreateMarket`、`CreateVault` などと同じパターン — 第 3 章で導入、以降のすべての章で再利用。

**レイアウトを書く**（1962–1978 行）:

```rust
position.size = size;
position.entry_price = mark;
position.collateral = collateral;
position.funding_snapshot_index = funding_snapshot;
```

データ書き込み 4 つ。`entry_price = mark` がオラクル価格をポジションの参照点として刻印する。`funding_snapshot_index = funding_snapshot` がこの瞬間のファンディング指数を捕捉する — 将来のすべての close/liquidate がこのスナップショットからの差分としてファンディング PnL を計算する。

> **演習 §11.3.** stale なオラクル（最後の `SetOraclePrice` から 25 slot 以上）に対して `OpenPosition` を試すと何が起きるか。`read_fresh_oracle` を通って失敗パスを辿れ。次に `funding --update --rate 0` と `oracle --set --price ...` を走らせてオープンを再試行せよ。

---

## §11.4  `ClosePosition` を歩く

1995–2061 行の `process_close_position`。オープンより単純 — PDA 作成なし、決済とゼロ化だけ。

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

**PnL を実現**（2046–2058 行）:

```rust
let new_collateral = if equity < 0 { 0 } else { equity as u64 };
position.collateral = new_collateral;
position.size = 0;
position.entry_price = 0;
position.funding_snapshot_index = funding_now;
```

書き込み 4 つ:

1. **担保が equity になる**（水没ならゼロ）。利益のクローズは担保を増やし、損失のクローズは縮め、水没のクローズはゼロにする。本番ではこの担保が SPL Token CPI で vault から ユーザに戻る。ここではポジション アカウントに座ったまま、まだ実装されていない後続「withdraw」命令が返すのを待つ。
2. **Size = 0** がポジションをクローズ済みとマークする。PDA は確保されたまま、再オープンが同じ PDA を派生して休眠状態を上書きする。
3. **Entry price = 0** は家事 — 休眠状態を認識可能に保つ（ゼロ化された `entry_price` はオープン ポジションでは不可能）。
4. **ファンディング スナップショット = 現行** で、次回再オープンが古い差分を再適用するのではなく新鮮なスナップショットから始まる。

**水没クローズは担保を失うが、損失を回さない。** equity = -50（損失が担保を超える）でクローズするポジションは担保を 0 にしてそこで止まる。相手方（元のトレードの反対側にいた誰か — 暗黙にはマッチャ / 板）は通知も補填も受けない。これが本物の perp DEX が保険基金を tap する場所だ: 「ポジションが 50 単位の不足でクローズした、保険基金がカバーする、相手側は全額支払われる」。§11.6 を見よ。

> **演習 §11.4.** entry = 100、サイズ = 5、担保 = 100 でポジションをオープンする。オラクルをマーク = 80 に動かす。クローズ。期待される equity は `100 + 5 × (80 - 100) = 0`。ポジション ポスト状態で `collateral = 0` を確認せよ。

---

## §11.5  `Liquidate` を歩く

2063–2152 行の `process_liquidate`。クローズとの重要な違い: **誰でも呼べる**。

**検証**（2076–2091 行）: **清算者**は signer でなければならないが、プログラムは清算者がポジションの user と一致するかをチェック**しない**。誰でも誰のポジションに対しても liquidate を呼べる。

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

**ペナルティを適用し強制クローズ**（2117–2147 行）:

```rust
let liquidation_penalty = ((notional_val * (LIQUIDATION_PENALTY_BPS as u128) / 10_000)
    .min(i64::MAX as u128)) as i128;

let mut realized = if equity < 0 { 0 } else { equity };
realized = (realized - liquidation_penalty).max(0);
let new_collateral = realized as u64;

position.collateral = new_collateral;
position.size = 0;
position.entry_price = 0;
position.funding_snapshot_index = funding_now;
```

ペナルティはポジションの残った equity から取る。`LIQUIDATION_PENALTY_BPS = 100` = notional の 1%。残担保（ペナルティ後）はポジション アカウントに残る。本番ではペナルティ額が SPL Token CPI で vault から清算者のトークン アカウントへ転送される。

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

## §11.6  欠けたピース — 保険基金と SPL Token 配線

本章が明示的に実装しないこと 2 つ、本番での役割を明示しておく。

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

**SPL Token エスクロー。** 本章で「担保」と言及するすべての操作は今のところ簿記のみ。本番では:

- `OpenPosition` は `collateral` 額分のユーザの quote トークン アカウントから market vault へ SPL Token Transfer を CPI。
- `ClosePosition` は逆方向を CPI、`new_collateral` をユーザに返す。
- `Liquidate` はペナルティを清算者に、残りをユーザ（あるいは保険基金）に CPI。

追加は機械的: 第 6 章の `Deposit` パターンがそのまま持ち越せる。ここにない理由は、追加が各ハンドラの AccountMeta 数を 3 倍にし（user_token_account、vault_token_account、token_program がそれぞれに必要）、本章が中心としているライフサイクル/数学の焦点を覆い隠すからだ。繰り延べ章（Ch.11b と呼ぼう）は数学に一切触れずに vault CPI を追加する。

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
