# 第10章 — ファンディングレートの仕組み

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`FundingState`）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_create_funding_state` 1605–1680 行、`process_update_funding` 1682–1758 行）、[`scripts/funding/src/main.rs`](../../scripts/funding/src/main.rs)。

---

## §10.0  はじめに

無期限先物契約には満期がない。満期がなければ、契約の価格を原資産の spot 価格に引き戻す構造的な力もない。ファンディングはそれをやる仕組みだ: ロングとショートが定期的に、perp のマーク価格が原資産からどれだけ乖離しているかに比例する少額を互いに支払う。perp が spot より高いとき、ロングがショートに支払う（ショートの参入とロングの退出を促す）。低いときは逆。

経済は単純。エンジニアリングはそうでもない。

Solana プログラムは market 内のすべてのポジションを単一トランザクションで反復できない — 数が多すぎ、tx あたり CU バジェットが最初の数百で尽きる。壁掛け時計には頼れない（Clock sysvar の `unix_timestamp` が唯一の合法的な時間信号）。レートを正直に更新するオフチェーンプロセスは信用できない（keeper はすべて最小化すべき信用前提）。そしてポジションごとの決済額は、ファンディング間隔の間に何度ポジションが触れられようと、グローバル蓄積子と正確に一致しなければならない。

4 つの制約を同時に解くパターンは、**時間窓累積指数 (time-windowed cumulative index)** だ。本章ではこれを歩く。次の順で進める。

1. ファンディングが形式的に何を意味するか、そして累積パターンが制約からどう導かれるかを読む。
2. `FundingState` アカウントレイアウトを歩く — market ごとに 1 つの固定サイズ PDA、累積指数と現行レートを持つ。
3. `UpdateFunding` を歩く — 区分線形セグメントで指数を進める keeper クランク。
4. ポジションごとの `SettleFunding` 半分を擬似コード化する（Position は第 11 章。読み手パターンは完全に後送りするには重要すぎる）。
5. 設計を第 5 章の並列性議論に結びつける: タッチ時ファンディング決済は、**誤った**設計（シングルトン「合計」アカウント）がスループットを潰す典型ケースだ。

本章は新規 syscall は少なく、アーキテクチャの趣味は長い。コードは小さい。パターンが教えだ。

---

## §10.1  ファンディングとは形式的に何か

2 つの量が計算の錨になる。

- **マーク価格** — プログラム（あるいはチェーン全体）が原資産の現在価格と見なすもの。第 9 章のオラクル。staleness チェック付きで読む。
- **プレミアム指数 (premium index)** — perp 価格が直近の過去でマークからどれだけ乖離したかを平滑化した尺度。実務上、取引所は `(perp_mid - mark) / mark` をファンディング窓口で平均し、なんらかのクランプを加える。

ファンディングレートはおおよそプレミアム指数に比例する。

```
funding_rate ≈ k × clamp(premium_index, -max_rate, +max_rate)
```

符号規約: 正のレートはロングがショートに支払う、負はショートがロングに支払う。

長さ `T` 秒の窓に対し、サイズ `s`（ロング正、ショート負）のポジションが蓄積するファンディングは:

```
funding_owed(s, T) = funding_rate × T × s
```

quote 通貨（通常 USDC）で支払われる。ロングとショートは市場全体でゼロサム — ファンディングは**再分配**であって手数料ではない。

ここから設計上の観察が 2 つ落ちる。

1. **重要なのは積分であって瞬間レートではない。** ファンディング窓口の半分だけ存在したポジションは、半窓ぶんのファンディングを支払う。決済額はポジションの寿命にわたるレートの時間積分に依存し、ある特定瞬間のレートには依存しない。
2. **積分は market 内のすべてのポジションで同じ。** 窓の始めに開かれたか中央で開かれたかに関わらず、**レート**は market のレートで、ポジションごとのレートではない。だからポジションごとに再計算する代わりに、market 全体の単一の走行合計 — **累積ファンディング指数 (cumulative funding index)** — を保持し、各ポジションに open 時の指数スナップショットを引かせる。

これがパターンだ。本章の残りはこれを実装する。

> **演習 §10.1.** `funding_rate = 0.0001 / hour`（10 bps/hour）が 24 時間一定で、ロングサイズ 100 のポジションを全期間持っていたとする。いくらファンディングを支払った（または受け取った）か。次に最初の 12 時間が +0.0001/hour、後半 12 時間が -0.0001/hour だったとする。答えは同じか。なぜか。

---

## §10.2  `FundingState` アカウント

market あたり PDA 1 つ、120 バイト。`crates/state/src/lib.rs` から。

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct FundingState {
    pub discriminator: [u8; 8],          // 0..8
    pub bump: u8,                         // 8
    pub _pad0: [u8; 7],                   // 9..16
    pub market: [u8; 32],                 // 16..48
    pub cumulative_funding_index: i64,    // 48..56  — 1e9 でスケール
    pub last_update_ts: i64,              // 56..64  — Clock.unix_timestamp
    pub last_update_slot: u64,            // 64..72
    pub current_rate_per_sec: i64,        // 72..80  — 1e9 でスケール
    pub window_seconds: u64,              // 80..88
    pub _reserved: [u8; 32],              // 88..120
}
```

load-bearing なフィールドは 3 つ。

**`cumulative_funding_index: i64`** が走行合計。`UpdateFunding` が走るたびに `current_rate_per_sec × seconds_elapsed` だけ進む。レートが負（ショートがロングに支払う）にもなれるので符号付き。`1e9` でスケールしているので、表現可能な最小レートは base 名目 1 単位あたり 1 ナノ秒ぶん — 典型 perp 経済に十分な精度。

**`last_update_ts: i64`** は指数が最後に進められた Clock unix_timestamp。次の更新が `elapsed = clock.unix_timestamp - last_update_ts` を計算し、それを積分間隔として使う。Solana プログラムが 2 つのオンチェーン イベントの間にどれだけ時間が経過したかを知る唯一の方法だ。

**`current_rate_per_sec: i64`** は `last_update_ts` **以降**有効だったレート。`UpdateFunding` が走ると、まずこの先行レートを経過した窓に適用し、それから次の窓のための新レートをインストールする。これが「step 関数」の半分 — 指数は区分線形セグメントで進み、keeper 呼び出しごとに 1 セグメントだ。

他のフィールドは機械的: 標準チェックのための discriminator、PDA のための bump、追跡可能性のための market pubkey、設定されたファンディング窓口を知りたい呼び出し側のための window_seconds、forward-compat のための 32 バイト。

> **演習 §10.2.** 「0.01% per 8 hours」（Binance perp の既定）のファンディングレートを、ここで使う scaled-1e9 `current_rate_per_sec` 形式に変換せよ。算術を示せ。

---

## §10.3  `UpdateFunding` を歩く

`programs/openhl-core/src/lib.rs:1682–1758` の `process_update_funding` が唯一の変更子。検証を除いた本体:

```rust
let new_rate = new_rate_raw
    .max(-MAX_FUNDING_RATE_PER_SEC_ABS)
    .min(MAX_FUNDING_RATE_PER_SEC_ABS);

let clock = Clock::get()?;
let elapsed = clock.unix_timestamp.saturating_sub(funding.last_update_ts);
let elapsed_u = elapsed as u64;

let delta = (funding.current_rate_per_sec as i128) * (elapsed_u as i128);
let new_cumulative = (funding.cumulative_funding_index as i128).saturating_add(delta);
// ... i64 飽和クランプ ...

funding.cumulative_funding_index = new_cumulative_clamped;
funding.current_rate_per_sec = new_rate;
funding.last_update_ts = clock.unix_timestamp;
funding.last_update_slot = clock.slot;
```

操作は 4 つ。

**keeper レートをクランプ。** `MAX_FUNDING_RATE_PER_SEC_ABS = 1_000_000` scaled-1e9 単位 = 0.001 / sec ≒ 86.4% / 日。教育用に loose。本番キャップはもっと tight（おそらく 0.05% / 時、約 1.5% / 日最大）。クランプは侵害された、あるいはバグった keeper への防御 — 最悪でもレートをキャップに押し上げるだけ。

**Clock から経過時間を計算。** `Clock::get()?` がオンチェーンで何時か知る唯一の合法的方法。`unix_timestamp` は Unix epoch からの秒数を表す `i64`。ランタイムが slot ごとに更新するので、前の `UpdateFunding` と同じ slot で走る tx は `elapsed = 0` を見る — ファンディングは蓄積せず、レートが再設定されるだけだ。それで構わない。keeper スケジュールはポリシーであって不変条件ではない。

**経過窓に**先行**レートを適用。** これがパターンの中心。`elapsed_u` に掛けるレートは `funding.current_rate_per_sec` — **前の** `UpdateFunding` 呼び出しで設定されたレート。それから新レートをインストールする。これが指数を時間の区分線形関数にする: `t₀` から `t₁` までレート `r₀` で成長、`t₁` から `t₂` まで `r₁` で成長、以下同様。

**計算中のオーバーフローを避けるため `i128` に昇格。** 中間値 `delta = rate × elapsed` は長い間隔や大きなレートで `i64` を超えうる。`i128` で計算し、保存時に `i64` に飽和する。ポジション累積 `i64` は約 9 × 10^18 scaled-1e9 単位を保持できる — 通常レートで何世紀ぶんも入る。パニックではなく飽和することで、オーバーフローが優雅に劣化する（レート計算がキャップし、プログラムがクラッシュしない）。

**SDK が隠していること:** Anchor アカウントの `Time::now()` 風ヘルパは、**すべての**時間読み取りが Clock syscall であるという事実を覆い隠しがちだ。無料で読める「壁掛け時計」はない。`Clock::get()` 1 回が CU コスト。2 回読むハンドラ（一度 staleness 検証、一度 elapsed 計算）では、2 回呼ぶ代わりに最初の読み値をローカル変数にキャッシュできる。

> **演習 §10.3.** `UpdateFunding` を数秒間隔で 3 回連続で呼べ:
>   1. `--rate 100`（非ゼロレート）
>   2. 約 5 秒待ち、`--rate 200`
>   3. 約 5 秒待ち、`--rate 0`
>
> 各呼び出し後にファンディング状態をダンプせよ。`cumulative_funding_index` は次のようになるはず:
>   - 呼び出し 1 後: まだ 0（蓄積する先行レートがない）。
>   - 呼び出し 2 後: 約 `100 × 5 = 500`（レート 100 が約 5 秒）。
>   - 呼び出し 3 後: 約 `500 + 200 × 5 = 1500`。
>
> 正確な数値は実際の経過秒数に依る。形が教えだ。

---

## §10.4  もう半分 — ポジションごとの決済（擬似コード）

`FundingState` がグローバル指数を持つ。ポジションごとの半分は読み手側。ポジションが open されるとき、現在の指数をスナップショットする:

```rust
// open_position 時:
position.funding_snapshot_index = funding.cumulative_funding_index;
```

ポジションが後で触れられたとき（close、変更、清算、close なし決済）、差分を計算して PnL として適用する:

```rust
// 任意のポジション タッチ時:
let index_delta = funding.cumulative_funding_index - position.funding_snapshot_index;
let funding_pnl_scaled = (index_delta as i128) * (position.size as i128);
let funding_pnl = (funding_pnl_scaled / 1_000_000_000) as i64; // 1e9 スケールを戻す
position.realized_pnl += funding_pnl;
position.funding_snapshot_index = funding.cumulative_funding_index;
```

これがパターン全体。注目すべき性質は 3 つ。

1. **タッチごとに定数時間。** 反復なし。open とタッチの間に UpdateFunding 呼び出しが何回起きようと、ポジションごとの settle は固定コストの引き算と掛け算だ。
2. **ポジション間の調整なし。** ポジション A とポジション B は並列に決済できる — 別々のポジション アカウントに触れ、（単一の）`FundingState` を**読む**だけだ。第 5 章から: これは共有アカウント上の読み読みパターンで、Sealevel スケジューラは完全並列を許可する。
3. **決済は正確であって近似ではない。** 指数はレートの単調な積分なので、任意の 2 つのスナップショット間の差分は、その間隔の間に一定サイズのポジションに**正確に**蓄積したファンディングだ。ドリフトなし、1e9 スケーリングが強制するもの以外の丸め誤差なし。

実装は `process_settle_position`（第 11 章以降）に住み、ポジションに触れる他のあらゆる命令から呼ばれる。第 11 章で Position を正式に導入し、この半分を直接接続する。今のところ、擬似コードは正しく完全だ — Position が存在すれば実装は機械的になる。

> **演習 §10.4.** `cumulative_funding_index = 1500` でポジションが open される、サイズ = 100。3 更新後、指数は 1800 を読む。ファンディング PnL は? 次にもう 1 更新で指数が 1750 に進む（つまりスナップショット以降 50 **下がった**）。新しい PnL は?

---

## §10.5  クランク / keeper — UpdateFunding を何が、いつ走らせるか

`UpdateFunding` はトレーダが呼ぶものではない。keeper が呼ぶ — 唯一の仕事がスケジュールに従って `UpdateFunding` トランザクションを送って指数を前進させるオフチェーン プロセスだ。

最小限の keeper ループ、擬似 Python で:

```python
import time
while True:
    mark = read_oracle_mark(market)            # 第 9 章
    perp_mid = read_book_mid(market)           # 第 7 章
    premium = clamp((perp_mid - mark) / mark, -MAX, +MAX)
    new_rate_per_sec = premium * RATE_SCALAR
    send_tx(UpdateFunding(new_rate_per_sec), market)
    time.sleep(60)  # market ごとに調整
```

本物の keeper が答えなければならない設計上の問いは 3 つ。

**1. どのくらいの頻度で?** 頻繁すぎると tx 手数料を浪費し指数にジッタが加わる。頻繁でなさすぎるとレートが古くなる。長い間隔の終わり近くで open されたポジションは誤ったレートを支払う。一般的な選択: 流動的市場で 60 秒ごと、低流動性で 5 分ごと。オンチェーンの `window_seconds` は**広告**窓口（手数料開示や外部ドキュメントで使われる）。**実際**の keeper cadence はポリシー。

**2. 誰が走らせるか?** パターンは 3 つ:
   - **取引所自身** — 最も単純、単一の信用前提、しかし単一障害点。
   - **許可された keeper 集合** — 複数オペレータが交代で責任を持ち、プログラムは signer をホワイトリストに対してチェックする。
   - **無許可クランク** — 誰でも呼べ、プログラムが受け入れるレートをクランプする。検閲耐性があるが、非常に慎重な境界が要る（悪意ある keeper でもレートをキャップに繰り返し押し上げられる）。
   
   本書の `process_update_funding` は教育のために任意の signer を受け入れる。本番は上記 3 つから 1 つを選ぶ。

**3. keeper が停止したら?** 停止した keeper は古いレートが長期間適用され続けることを意味する。停止中に open されたポジションは最後に公開されたレートでファンディングを支払い、それは実際のプレミアムから大きくずれる可能性がある。緩和策: 更新ごとの最大経過時間に上限を付ける（`elapsed > N 秒`の呼び出しを拒否）、手動介入で再起動する。あるいはレートのドリフトを既知の degraded モードとして受け入れる。

**Anchor が隠していること:** ここでは何も。keeper パターンは完全にプログラム作成者の選択。Anchor も他のフレームワークも「ファンディングレート」抽象を提供しない、ポリシーが domain-specific すぎて既定が決まらないからだ。

> **演習 §10.5.** 上のループをローカルバリデータに対して走らせる 30 行 Python スクリプトを書け。レートは定数（例: 100）にハードコードせよ。30 秒ごとにファンディング状態をダンプして、`cumulative_funding_index` が毎回約 3000 ずつ（100 × 30s）増えることを確認せよ。

---

## §10.6  並列性再訪 — 決済を典型ケースとして

第 5 章はシングルトン書き込み共有アンチパターンを導入した。ファンディング決済はそれが操作的になる場所だ。

1,000 個の活性ポジションを持つ perp DEX を考える。各ファンディング決済の瞬間に、2 つの設計が可能だ。

**設計 A — シングルトン「合計」アカウント。** 単一の `MarketAggregates` アカウントが `total_long_size`、`total_short_size`、走行 PnL を持つ。すべての決済がこれを増減する。すべてのポジション タッチがこのシングルトンを読み書きする。

結果: ポジションに触れるすべてのトランザクションが `MarketAggregates` 上の書き込みを共有する。そのような 2 つのトランザクションは同じ slot で走れない。スループットは単一トランザクション レイテンシに崩壊する。1,000 ポジションが 1 時間に 1 回ずつタッチされるなら、slot あたり約 1 tx = 最大 2.5 tx/sec で直列化する。プログラムはシングルスレッドのキューだ。

**設計 B — ポジションごとの決済（本章）。** シングルトンの合計アカウントはない。決済時の `FundingState` は読み取り専用 — 書き込みは `UpdateFunding` 呼び出しごとに 1 回、ポジション タッチのホットパスを外れる。ポジション A とポジション B は並列に決済する、書き込みセットが `{position_A}` と `{position_B}` で、交わらないからだ。

結果: ポジション決済はバリデータが持つコア数までスケールする。1,000 ポジションが少数の slot で決済できる。プログラムは構造的に並列に優しい。

これが報われる第 5 章の教訓だ。設計 B を選ぶことは選択の瞬間には**最適化のように感じない** — ただ「グローバル合計を必要としないなら保存するな」と感じるだけだ。スループットで報われる理由は Sealevel の読み書きセット スケジューリングであり、データレイアウトを設計するときに直接見えないからだ。

「グローバルカウンタ」を誘惑する場面ならどこでも同じパターンが当てはまる:
- 取引総ボリューム? 保存するな。オフチェーンでトランザクションを索引化する。
- 徴収手数料総額? 手数料をカウンタではなく手数料受信トークンアカウントに蓄積させる。
- ポジション総数? `getProgramAccounts(programId, filter: discriminator == POSITION).len()`、オフチェーン。

ルールを証明する例外: **プログラム ロジックに load-bearing なもの**（ファンディング指数自体、保険基金残高、オラクル staleness チェック）は真に書き込み共有アカウントを要する。それらには直列化を受け入れ、その周りを設計する（keeper のみが書く、短いクリティカル セクション、可能なところでシャーディング）。それ以外すべてには、グローバル カウンタを拒否する。

---

## §10.7  まとめと自己検証

### まとめ図

```
時間:           t0 ───────── t1 ───────── t2 ───────── t3 ──── 今
keeper 呼び出し: UpdateFunding   UpdateFunding   UpdateFunding
設定レート:      r0              r1              r2
経過:           Δ0              Δ1              Δ2

時間に対する cumulative_funding_index:

  idx(t1) = idx(t0) + r0 × (t1 - t0)
  idx(t2) = idx(t1) + r1 × (t2 - t1)
  idx(t3) = idx(t2) + r2 × (t3 - t2)

ポジションごとの settle（第 11 章で実装）:

  position.funding_pnl_delta
    = (idx_now - position.funding_snapshot_index) × position.size / 1e9
  position.funding_snapshot_index = idx_now


Sealevel スケジューリングへの影響:

  UpdateFunding はシングルトン FundingState を書く — keeper 間で直列。
  SettleFunding はポジションごとのアカウントを書く — ポジション間で並列。
  結果: 決済は N コアまでスケール、レート更新は market ごとに分に 1 回、
  何もゲートしない。
```

### 自分で検証する 3 項目

1. **蓄積子は区分線形。** UpdateFunding 呼び出しを既知の秒数間隔で異なるレートで 3 回走らせよ。各呼び出し後の累積指数は `prior_index + prior_rate × elapsed_seconds` と正確に一致するはずだ（1e9 スケーリングの整数除算内で）。
2. **クランプが強制される。** `--rate 5000000`（キャップを大きく超える）を試せ。[`lib.rs:1703–1705`](../../programs/openhl-core/src/lib.rs#L1703) のクランプが `MAX_FUNDING_RATE_PER_SEC_ABS = 1_000_000` に縮め、`clamped to` ログ行が見えるはずだ。
3. **slot vs 時間の区別が重要。** `UpdateFunding --rate 100` を走らせ、即座に別の `UpdateFunding --rate 200`（同じ slot）を走らせよ。2 回目は `elapsed=0s` を報告し、指数は進まないはずだ。10 秒待って 3 回目を `--rate 0` で走らせる — 今度は `elapsed≈10` が見え、指数は `200 × 10` だけ進むはずだ。

---

## 第 11 章への導線

market、vault、マッチャ、オラクル、ファンディング蓄積子を持つようになった。**まだ持っていない**のは**ポジション**だ。プログラム内の他のすべてのプリミティブは、ポジションでないアカウントに対して動作する（板、ファンディング状態）か、ポジションがどこかに存在することを仮定する（第 6 章の deposit は資金を vault に移すがポジションを open しない。第 10 章のタッチ時決済パターンは決済するものがまだないので不完全）。

第 11 章は `Position` アカウントを導入する: ユーザ×market ごと、サイズ + entry 価格 + ファンディング スナップショット + 証拠金残高を持つ。`OpenPosition`、`ClosePosition`、`Liquidate` を追加する — 清算エンジンが、他のすべての Phase A と Phase B プリミティブが収束する典型ユースケースだ。清算はオラクルを読み（staleness チェック付き）、ファンディング指数を読み（決済する）、証拠金をポジションサイズに対してチェックし、マッチャ（あるいはその slab 親戚）を呼んでポジションを close し、ユーザの vault を掃き出す。第 11 章はプログラムが完全な意味で perp DEX になる場所だ、プリミティブの寄せ集めではなく。
