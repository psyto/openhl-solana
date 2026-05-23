# 第5章 — Sealevel 並列性とアカウントロック

> 状態: ドラフト (v0.1)。
> 教材コード: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_create_stats` 442–501 行、`process_bump_stats` 503–540 行）、[`scripts/stats/src/main.rs`](../../scripts/stats/src/main.rs)、[`scripts/create-market/src/main.rs`](../../scripts/create-market/src/main.rs)。
> 検証対象バージョン: solana-instruction 2.3.3、solana-program 2.3.0。

---

## §5.0  はじめに

Solana の看板数値 — 1 秒あたり数万トランザクション — は、VM の速度やブロック密度で買えるものではない。**トランザクションを並列に走らせる**ことで買っている。その並列実行を担うスケジューラの名前が **Sealevel** で、Sealevel がプログラムから必要としているのはトランザクションごとに 1 つの情報だ — どのアカウントを読み、どのアカウントを書くか。

その情報は、各 `Instruction` に付ける `AccountMeta` 配列から来る。ランタイムはこれを読み取り専用 / 書き込み可のロック宣言として使う。書き込みセットが交わらないトランザクション 2 つは並行実行できる。書き込み可能アカウントを共有するトランザクション 2 つは、`Mutex` で奪い合うスレッドのように直列化される。

本章ではモデルを歩く。

1. `solana-instruction` を開き、3 フィールドの `AccountMeta` 構造体を読む。Sealevel がトランザクションのデータ依存性について知る必要のあるすべては、この 3 フィールドに収まっていると理解する。
2. リーダ・ライタ ロック意味論を理解する: 同一アカウントに対し複数の `READ` ロックは共存できる。`WRITE` ロックは 1 つだけで、それ以外のすべてを排除する。
3. `CreateMarket` の `AccountMeta` 配列を歩く。書き込み対象がすべて異なる PDA（`(base_mint, quote_mint)` ペアごとに 1 つ）であることを見る。すなわち、N 個の異なるペアに対する N 個の同時 CreateMarket は、N 個の並列スロットで実行できる。
4. `BumpStats` の `AccountMeta` 配列を歩く。すべての BumpStats が**同じ**シングルトン Stats PDA を書くことを見る。よって 2 つの同時 BumpStats は、他に何をしようとも、必ず直列化する。
5. ホットパスから競合を引き剥がす設計パターンを論じる: シングルトンをシャーディングする、オフチェーンで事前集計する、カウンタを完全に取り除く。
6. この領域で Anchor が生成するもの・しないものを列挙する。

これが Foundations の最終章だ。これを終えれば、ベンチマークで速く**かつ**本番で速い Solana プログラムをゼロから書ける — 両者が乖離するのはスケジューラがボトルネックになったときだけで、それを読む方法をいま手に入れたからだ。

---

## §5.1  Sealevel が見ているもの — `AccountMeta` というロック宣言

`solana-instruction-2.3.3/src/account_meta.rs:19–32` を開こう。

```rust
#[repr(C)]
// ...
pub struct AccountMeta {
    /// An account's public key.
    pub pubkey: Pubkey,
    /// True if an `Instruction` requires a `Transaction` signature matching `pubkey`.
    pub is_signer: bool,
    /// True if the account data or metadata may be mutated during program execution.
    pub is_writable: bool,
}
```

フィールドは 3 つ。これがクライアントコードとスケジューラの間のインターフェース全体だ。pubkey がアカウントを特定し、2 つの bool がそのアカウントに対する意図を宣言する。クライアントがトランザクションを送信した瞬間から、ランタイムはこの宣言を契約として扱う。アカウントを `READ` と宣言したトランザクションがプログラム内で書き込みを試みると、確定時に `ReadonlyDataModified` で失敗する。したがってスケジューラは、並列実行可否を判定する際にこの宣言を信用できる。

61–67 行と 97–103 行の 2 つのコンストラクタが、クライアントコード上で意図を明示する。

```rust
pub fn new(pubkey: Pubkey, is_signer: bool) -> Self {
    Self { pubkey, is_signer, is_writable: true }
}

pub fn new_readonly(pubkey: Pubkey, is_signer: bool) -> Self {
    Self { pubkey, is_signer, is_writable: false }
}
```

`AccountMeta::new(...)` — 書き込み可。`AccountMeta::new_readonly(...)` — 読み取りのみ。署名者ビットは read/write とは直交していて、別のランタイムチェック（要は第 2 章の所有者まわりの話）を制御する。

「特定フィールドだけ読みたい」「特定オフセットにだけ書きたい」という意図表現は存在しない。粒度はアカウント全体だ。バイトのどこかに触れる可能性がある（書き）か、あるいは閲覧するだけ（読み）か。この粗さこそがスケジューリングを安価にしている — ロックテーブルのキーは 32 バイトの pubkey であって、バイト範囲ではないからだ。

> **演習 §5.1.** `scripts/create-market/src/main.rs` の命令構築直後に `println!` を数行追加し、`CreateMarket` 命令の `AccountMeta` 配列を出力せよ。確認: payer は `WRITE + SIGNER`、market PDA は `WRITE`、system_program は `READ`。

---

## §5.2  スケジューラのリーダ・ライタ意味論

Sealevel スケジューラは、各アカウントを 1 つのリーダ・ライタ ロックとして扱う。規則は教科書どおりだ。

- **N 個のリーダ**が同じアカウントのロックを同時に保持できる。
- **1 つのライタ**が排他的に保持する — 同じアカウント上に他のリーダもライタも存在できない。
- **異なるアカウント**は独立 — 異なる pubkey のロックは互いに相互作用しない。

トランザクションがスケジューラに入ると、ランタイムはそのトランザクションの全命令の全 `AccountMeta` を収集し、重複除去し、読み取りセットと書き込みセットを形成する。トランザクション 2 つが**並列実行可能**となる必要十分条件は次のとおり。

```
(A.write_set ∩ B.write_set) == ∅
かつ (A.write_set ∩ B.read_set)  == ∅
かつ (A.read_set  ∩ B.write_set) == ∅
```

`read_set ∩ read_set` の重複はブロックしない — 読み取りと書き込みを区別する意義はそこにある。

具体的には次のようになる。

1. **異なる `(base_mint, quote_mint)` ペアに対する CreateMarket 2 つ** — 書き込みセットが交わらない（market PDA が異なる、payer が同じ自分なら共通だが）。共有 payer が唯一の競合点だが、ランタイムは同じ手数料支払者からのトランザクションを別経路で直列化する（Sealevel 規則ではない別制約）。異なる payer から呼べば完全並列。

2. **BumpStats 2 つ** — 両方ともシングルトン Stats PDA を書く。書き込みセットがその 1 つの pubkey で交わる。直列化、終わり。

3. **CreateMarket 1 つと BumpStats 1 つ** — 書き込みセットが異なる（一方は market PDA、他方は Stats を書く）。並列化する。

4. **同じ market を 2 回読む**（2 つのフロントエンドが描画するなど） — 読みで重なる、競合なし。両方走る。

ランタイムはこの判定をスロットごと、いかなるプログラムコードが走るより前に行う。プログラムはスケジューリングについて何も知らない。順番が来たら走るだけだ。

**SDK が隠していること:** `solana-sdk` も Anchor も、「このトランザクションはあのトランザクションと並列可能か」という API は公開していない。スケジューラが実行時に不透明に判定する。それを推測するには、自分の `AccountMeta` 宣言を読み、上の問いを当てる以外にない。

> **演習 §5.2.** ウォレット A からウォレット B への `Transfer` の読み/書きセットは `{A: W, B: W}`。C から D への `Transfer` の集合は `{C: W, D: W}`。並列実行できるか。A→B と B→C ではどうか。

---

## §5.3  `CreateMarket` のアクセスセットを歩く

`scripts/create-market/src/main.rs:118–125` のクライアント宣言。

```rust
let ix = Instruction {
    program_id,
    accounts: vec![
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new(market_pda, false),
        AccountMeta::new_readonly(system_program::ID, false),
    ],
    data,
};
```

アカウントは 3 つ。Sealevel の視点から 1 つずつ見ていく。

**`payer` — `WRITE + SIGNER`。** payer の lamport 残高が変わる（rent が出ていく）。同じ payer を共有する CreateMarket が 2 つあれば、ここが競合点だ — ランタイムは同じ残高を並列に減らすことを許せない。同じ手数料支払者を同じスロットの 2 つの別トランザクションで使うと、別の理由で拒否される（同一ブロック内のトランザクション nonce 重複）。よって実務上この競合は問題にならない。CreateMarket を 2 人の異なる payer から呼べば、`payer` レベルの重複はない。

**`market_pda` — `WRITE`。** 新しく作成されるアカウント。pubkey は `(base_mint, quote_mint)` とプログラム ID から**派生**される — 第 3 章を見よ。異なる `(base_mint, quote_mint)` ペアに対する 2 つの CreateMarket は**異なる** market PDA を派生するので、この書き込みは互いに競合しない。これが openhl-core を並列性に優しくしている設計上の選択だ — PDA スキームのおかげで新しい market はそれぞれ自分のアドレスに住み、別の market と衝突することがない。

**`system_program::ID` — `READ`。** System プログラムは、本ハンドラ内の `create_account` CPI のために必要だ。読み取りのみとマークされている理由は、本ハンドラが System プログラム自身を変更していないから（変更不可 — executable アカウントだ）。System に CPI する同時トランザクションすべてが System を読み取りロックして共存できる。同じアカウントへの読み取りロックは互いをブロックしない。

異なる payer からの `(SOL, USDC)` と `(SOL, USDT)` の CreateMarket 2 つを考えると、

```
A.writes = {payer_A, market_SOL_USDC}
A.reads  = {system_program}
B.writes = {payer_B, market_SOL_USDT}
B.reads  = {system_program}
```

交差はすべて空 — 例外は `system_program ∈ A.reads ∩ B.reads`、これは読み読み重複で許容される。両トランザクションは同じスロットにスケジュールされる。並列。

これが「設計上、並列性に優しい」の実態だ。並列性を狙ってコードを書いたわけではない。各 market に自分のアドレスを与えただけだ。それは第 3 章でコンポーザビリティのために選んだ PDA スキームから自然に落ちてきた結果である。Sealevel の恩恵は、別目的で下したアーキテクチャ判断の下流の利得だ。

> **演習 §5.3.** `Initialize`（第 2 章）命令の読み/書きセットはどうなるか。`scripts/init-market/src/main.rs` を見よ。Initialize は同じ tx で `System::Assign` 命令も走らせている点に注意し、それらの AccountMeta も数えること。

---

## §5.4  Stats 反例 — シングルトン書き込み競合

次に `scripts/stats/src/main.rs:99–104` の `BumpStats` を見よう。

```rust
Instruction {
    program_id,
    accounts: vec![AccountMeta::new(stats_pda, false)],
    data: vec![4u8],
}
```

アカウントは 1 つ: `stats_pda`、`WRITE` マーク。`stats_pda` は固定シード（`[STATS_SEED]`）から派生され、呼び出しごとの変化がない — このプログラムに対するすべての BumpStats 呼び出しで、永遠に同じ pubkey だ。よって BumpStats トランザクションの書き込みセットは常に `{stats_pda}` になる。

BumpStats 2 つは次のようになる。

```
A.writes = {stats_pda}
B.writes = {stats_pda}
```

`A.writes ∩ B.writes = {stats_pda}` — 空でない。並列実行できない。スケジューラは一方を選び、走らせ、確定させ、それから他方を走らせる。BumpStats のスループットは単一トランザクションのレイテンシで頭打ちになる。バリデータが何コア持っていようと関係ない。

これは `BumpStats` 自体には問題ない — 「カウンタを進める」という明示的呼び出しであり、ホットパスとして誰も期待していない。問題は、**並列に走るべき命令に Stats 書き込みを後付けで足したとき**だ。`CreateMarket` の末尾に `CreateStats` 相当のロジックを呼ぶ `CreateMarketAndBumpStats` 命令を想像しよう。その `AccountMeta` はこうなる。

```
[payer (W,S), market_pda (W), system_program (R), stats_pda (W)]
```

最初の 3 アカウントは `(base_mint, quote_mint)` ごとに異なる — 完全並列化可能だ。4 つ目 — `stats_pda` — はすべての呼び出しで**同じ**だ。突然、すべての market 作成が Stats で直列化する。market ごとに別 PDA という美しい設計が、1 つのグローバルカウンタの存在で、単一トランザクション レイテンシ相当のスループットに落ちる。

これが本物の Solana プログラムで起きる**最も頻出の失敗**だ — 誰かがホットパス命令に「グローバル統計」「グローバル上限」「グローバルレート制限カウンタ」を足し、スループットが桁違いに崩れる。直し方はいつも同じ — グローバル書き込みを引き剥がす — だが、なぜ壊れたかを理解していないと直しを見つけられない。

stats クライアントを動かして宣言の姿を見てみよう。

```
stats --rpc ... --program ... --init
AccountMeta declared:
  [0] <payer pubkey>           WRITE + SIGNER
  [1] <stats PDA pubkey>       WRITE
  [2] 11111111111111111111111111111111  READ
```

```
stats --rpc ... --program ...
AccountMeta declared:
  [0] <stats PDA pubkey>       WRITE
```

トランザクション 2 つ、書き込みセット 2 つ。pubkey は目に見える。競合は機械的に決まる。

> **演習 §5.4.** 同じ payer から BumpStats トランザクションを 2 つ立て続けに送れ。署名とスロット番号（`solana confirm <sig>` 経由）を観察せよ。同じスロットに着地するかもしれないし、隣接スロットになるかもしれないが、**並列に処理されることはない**。両方が同じバリデータの同じスロットで処理された場面を見つけ、ランタイムログから順次処理されたことを確認せよ。

---

## §5.5  リファクタリング パターン — グローバル書き込みを引き剥がす

シングルトン書き込みが競合を生んでいるとき、現実的な選択肢は 3 つある。

**(1) シングルトンをシャーディングする。** 1 つの Stats PDA を N 個に置き換える。シードは `[STATS_SEED, &[shard_index]]`。クライアントが（無作為に、あるいは呼び出しの何らかの性質に基づいて）シャードを選ぶ。書き込みセットは `0..N` のうちのある K について `{stats_shard_K}` になり、カウンタが N 並列を得る。総計を読むには、シャードを横断してオフチェーンで集計する。

最も一般的なオンプログラム修正だ。コスト: カウンタ上の厳密な「先に起きた」順序を諦めること（2 つのシャードが独立に進む）。総計の取得には N 個のアカウント読みが必要になる。

**(2) オフチェーンで事前集計する。** カウンタをオンチェーンに置かない。market を作るトランザクションをウォッチャープロセス（Geyser、RPC `getSignaturesForAddress` ほか）で索引化し、カウントをオフチェーンの DB で保持する。オンチェーン状態が変わらないので、オンチェーンは並列のまま。

カウンタが**観測性**（ダッシュボード、分析）目的でプログラム ロジックに使われないなら、これが正解だ。「グローバル統計」要件のほとんどはここに収まる。

**(3) カウンタを取り除く。** カウンタが本当に必要か問い直す。「market が何個あるか知りたい」から誰かが足した、というのがよくある経緯だ。しかし答えは `getProgramAccounts(programId, filter: discriminator == MARKET_DISCRIMINATOR).len()` を必要時にフェッチするだけで済み、オンチェーン状態は要らない。

パターン: ホットパスに書き込み可能シングルトンを見つけたら、問いは「これをどう効率的に直列化するか」ではなく、「このアカウントをオンチェーンに置く必要が本当にあるか」だ。

本書の設計では、`BumpStats` を `CreateMarket` に統合せず、明示的に独立した命令として保つことを意図的に選んだ。カウンタが欲しいオペレータは呼ぶ。スループットを大事にしたいオペレータはスキップする。その隔離こそが要点だ。

**Anchor が隠していること:** Anchor の `#[derive(Accounts)]` を使うと、`#[account(mut)]` でアカウントを宣言して書き込みセット上の含意を忘れることができてしまう。Anchor は「ホットパスのハンドラでシングルトンを書いていますよ」とは決して警告しない。コンパイル時の `Accounts` 構造体はクライアント側の型付き IDL に出てくるが、そこにも並列性コストは見えない。これを捕まえるのは純粋にコードレビューの領分だ。

---

## §5.6  まとめと自己検証

### まとめ図

```
Sealevel スケジューラ — 保留中のトランザクション A、B の各ペアに対し:

   A.writes ────┐
                ├──► 交差? ──► YES: 並列化不可
   B.writes ────┘            NO:  続行

   A.writes ────┐
                ├──► 交差? ──► YES: 並列化不可
   B.reads  ────┘            NO:  続行

   A.reads  ────┐
                ├──► 交差? ──► YES: 並列化不可
   B.writes ────┘            NO:  続行

   （A.reads ∩ B.reads は無視 — 両リーダがロックを保持可）


openhl-core アクセスセット:

   CreateMarket(base, quote)
      writes = { payer, market_PDA[base, quote] }
      reads  = { system_program }
      ↑ market_PDA が呼び出しごとに変わる → 異なる (base, quote) で並列

   BumpStats
      writes = { stats_PDA }   ← シングルトン、毎回同じ pubkey
      ↑ 呼び出し元に関わらず直列化

   CreateStats
      writes = { payer, stats_PDA }
      reads  = { system_program }
      ↑ 一度限り、競合は問題にならない
```

### 自分で検証する 3 項目

1. **同じ pubkey、毎回。** `stats --init`（CreateStats）を実行し、続けて `stats`（BumpStats）、もう 1 度 `stats` を実行せよ。表示される `stats PDA` は 3 回ともバイト同一のはずだ — その pubkey がスケジューラのキーになるロックだ。変化しない。
2. **CreateMarket ごとに異なる pubkey。** `create-market --base-mint <A> --quote-mint <B>` を実行する。`market PDA` を控える。`--base-mint <C> --quote-mint <D>` で再実行する。PDA は変わるはずだ。これが並列性だ。1 回目の書き込みセットは 2 回目と交わらないので、スケジューラは任意の順序、あるいは並行で走らせる自由を持つ（他の制約を除く）。
3. **読み読みはブロックしない。** `Bench`（第 4 章）はアカウントリストが**空**だ — 読みも書きも無い。Bench トランザクション 2 つは原理上、完全並列スロットで走れる。異なる payer から 2 つ送って、`solana confirm` で署名が同じか隣接するスロットに着地するのを観察せよ。異なる payer からの BumpStats 2 つと比較する。後者は厳密に異なるスロットに着地する。

---

## Phase B prologue — 本カリキュラムが行うアーキテクチャ選択

Phase A を終えた。Phase B にコミットする前に、何を**選ばないか**を明示しておく価値がある — これから続く章群は別のカリキュラムなら別の判断を下しえたアーキテクチャ選択を、暗黙のうちに行っているからだ。

Perp DEX は 2 通りに作れる。**既存チェーン上のプログラム** — マッチング、vault、ファンディング、清算ロジックをスマートコントラクトとして書き、コンセンサス・実行・バリデータ・ネットワーキング・ウォレットはチェーンに任せる。これが Phase B が組み立てる路線だ。**独自 L1** — 同じビジネスロジックを独自のコンセンサスエンジン・実行層・P2P ネットワーク・RPC と一緒にパッケージし、他のバリデータが走らせるバイナリを出荷する。

両者は同じビジネス問題（注文マッチ、担保保持、ファンディング支払い、水没ポジションの清算）を同じオンチェーン プリミティブで解く。それ以外のすべてが大きく分岐する。

**Solana 上のプログラム路線**（本カリキュラム）: 1 コマンドで deploy、既存流動性とウォレットを継承、既存バリデータのセキュリティに乗る。コードベースは 1 リポジトリに収まり、1 チームで出荷できる。ホストチェーンの slot 時間、コンピュートバジェット、バリデータ集中度に縛られる。値捕捉は部分的 — トランザクション価値の実質的な一部が自分の制御外のアクターに流れる。

**独自 L1 路線**: バリデータ集合の bootstrap に数ヶ月、自前のブリッジとステーブルコイン経済、自前のウォレット統合と上場交渉、自前の MEV ポリシー。コードベースは consensus + execution + networking + RPC にまたがって複数 crate に分散する。対価として、フルなランタイム制御（block time、順序、自前の負荷向け precompile）とフル値捕捉（バリデータ中間業者なし）が得られる。

L1 路線が必要になるのは、次の 3 条件が**揃って**成り立つときだ:

1. ホストチェーンが提供できないランタイム性質が必要 — sub-100ms 確認、独自マッチング precompile、決定論的な MEV-free 順序、EVM 互換でない精度での決済。
2. ビジネスモデルがトランザクション価値の完全捕捉に依存しており、ホストチェーンのバリデータ経済と分け合うわけにいかない。
3. チームと runway がチェーンを数年運営できる規模 — プログラム 1 つを deploy するのとは別次元の運用責任。

Hyperliquid は 3 条件すべてが揃った教科書的ケースだ。perp DEX プロジェクトの大半はどれも揃わない。そしてホストチェーン上のプログラム路線は、ローンチまでに掛かる時間・インフラ コスト・チーム規模のいずれにおいても、桁違いに安価だ。

本カリキュラムはプログラム路線に賭ける。L1 が絶対的に間違いだからではなく、L1 を正当化する条件が稀であり、ホストチェーン上でまだ出荷していないプロジェクトが多年がかりの L1 開発にコミットする資格をまだ得ていないからだ。Phase B は perp DEX プロジェクトの 99% が実際に取る路線を教える。

---

## 第 6 章への導線 — Phase B 開始

Phase A を終えた。アカウントを確保でき、Anchor なしでプログラムを書け、予測可能なアドレスを派生し、CU 包絡線を計測し、2 つのトランザクションが並列実行可能かを推論できるようになった。これらがランタイム基礎であり、本トラックのそれ以後すべてはこの上に立つ。

Phase B は第 6 章 — **CPI 内部 — Vault Deposits** — から始まる。SPL Token を開き、ユーザのトークンアカウントから本書の market PDA が所有する vault トークンアカウントへ base 資産トークンを移す deposit 命令を書き、`invoke` と `invoke_signed` が**実際に**フード下で何をしているかを歩く — スタックフレームのセットアップ、署名者特権の拡張規則、`AccountInfo` の再借用ダンス。第 3 章では `invoke_signed` をアカウント作成のために 1 度だけ使ったが、第 6 章ではチェーン上の他の全プログラムと話す主たる仕組みとして使う。

第 6 章が終わったとき、market のための動作する SPL Token vault を手にしている。Phase B が終わったとき、その中に住む板を手にしている。
