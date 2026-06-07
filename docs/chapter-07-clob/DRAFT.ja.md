# 第7章 — オンチェーン CLOB データ構造

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`Order` + `OrderBook`）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_create_order_book` 1087–1151 行、`process_place_order` 1158–1249 行、`process_cancel_order` 1256–1323 行）、[`scripts/book/src/main.rs`](../../scripts/book/src/main.rs)。

---

## §7.0  はじめに

板のない perp DEX は、データベース付きの価格フィードでしかない。板は価格発見が起きる場所、部分約定が流動性を帰属させる場所、そしてコンピュートバジェットのほぼ全てを使い切る場所だ。「この取引所はどう儲けるか」から派生するビジネス上の問いはすべて、最終的に「resting オーダーを保持するデータ構造」に行き着く。

Solana 上で課される制約は独特だ。トランザクションごとに動的に確保できない（ヒープは 32 KiB、bump アロケータは解放しない）。アカウントを任意に拡張できない（データ長は作成時固定、`realloc` は MAX_PERMITTED_DATA_INCREASE 以内のみ）。無制限ループは持てない（既定 200 KCU、最大 1.4 M）。そして組み立てたものはチェーン上の他の全プログラムから `bytemuck` キャストで読み書きできなければならない — 向こう側に何か気の利いたことをしてくれる Rust ランタイムはないからだ。

本章は Phase A の CU レクチャがいよいよ効いてくる場所だ。次の順に進める。

1. レイアウトを選ぶ — `Order` のスロット、固定容量配列を持つ `OrderBook` — そしてそれが**正しい最小の選択**であり、本番向けの選択ではないことを説明する。
2. それを 2 つの本番向け選択（slab と critbit）と明示的に比較し、本書が何をトレードしているかを特定する。
3. `place_order` と `cancel_order` を線形走査として歩き、CU ログを読んで「線形」が実際に何 CU かを確認する。
4. 板が満ちていき、命令ごとの CU がそれにつれて上がっていく様子を観察し、マッチャ章の直前で止まる — そこで問いは「この成長をどう完全に避けるか」に変わる。

本章では**マッチング**を実装しない。板は**受動的**だ — 注文が入り、order_id で注文が出る。bid と ask の交差は第 8 章。

---

## §7.1  なぜ配列、しかも正解ではない選択を

Solana 上の本物の CLOB は次の 2 つのどちらかになる。

1. **Slab** — ノードの連続アリーナ、価格レベルごとに双方向リンクの FIFO キュー、価格レベル自体は ソート済み critbit ツリーに格納（Serum、Phoenix）。place/cancel が O(log N)、ベスト bid/ask 検索が O(1)、キャンセル時にコンパクション不要。
2. **Critbit-of-orders** — 各注文が critbit ツリーの葉、キーは価格（同価格内の FIFO 順序のために副キーにタイムスタンプ）。すべてが O(log N)、slab より推論しやすい、メモリ局所性はやや劣る。

本書が組み立てるのはそのどちらでもない。**固定容量のフラット配列、線形走査**だ。`crates/state/src/lib.rs` から。

```rust
pub const ORDER_CAPACITY: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct OrderBook {
    pub discriminator: [u8; 8],
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub next_order_id: u64,
    pub active_count: u32,
    pub _pad1: [u8; 4],
    pub slots: [Order; ORDER_CAPACITY],
}
```

market あたり 2112 バイト。bid と ask は同じスロットプールを共有し、各 `Order` の `side` バイトで区別する。`size == 0` は「空スロット」を意味する。ソート順はない — スロット位置は線形走査順で「最初に見つけた空スロットが勝つ」で決まる。

これが本番向けに**誤った**選択である理由は 1 点に集約される: **ベスト bid / ベスト ask の検索が O(N)** だ。マッチングエンジンはどれも「最も高い bid は何か」「最も低い ask は何か」から始まる。本書の配列は両方の問いに答えるために 32 スロット全部の走査を強いる。slab や critbit なら O(1) か O(log N) で答える。

ではなぜこれを選ぶか。

- **コストが見える。** CU ログに線形走査が現れる。学生は `book --place` を実行し、板が満ちるにつれて CU が下がっていく様子を観察できる。
- **検証が易しい。** 配列上の `for` ループ 2 つ。崩しうる不変条件もない。ツリーの再バランスでの微妙な off-by-one もない。
- **第 7 章にはこれで十分。** 本章はデータレイアウトとコストの形についての章だ。マッチングは第 8 章で導入し、**そこで**この誤った選択が本物の問題になる — その時点で slab へのリファクタリングが許される。

正しくない選択をまず正しく組み立て、何が正しい選択になるかを理解した上でリファクタリングする、というのは妥当な教育上の順序だ。何が正しいかを理解する前に正しい選択を組み立てるのは、この授業を飛ばすことに等しい。

> **演習 §7.1.** `solana-program-2.3.0/src/system_instruction.rs:9`（廃止ノート）を見て考えよ: もし本書の `_reserved` が固定配列ではなく `Vec<u8>` だったら? なぜそれは Pod を壊すのか。（ヒント: 第 1 章のレイアウト議論を読み返せ。）

---

## §7.2  `Order` スロットと空スロット規約

`Order` は 64 バイト、Pod、repr(C)。`crates/state/src/lib.rs` から。

```rust
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Order {
    pub order_id: u64,    // 0..8
    pub price: u64,       // 8..16
    pub size: u64,        // 16..24  (size == 0 ⇒ 空スロット)
    pub owner: [u8; 32],  // 24..56
    pub side: u8,         // 56
    pub _pad: [u8; 7],    // 57..64
}
```

64 バイトは 2 の冪で、まともなアーキテクチャすべてでキャッシュラインとよく整合する。`_pad` は構造体が 8 バイト境界で終わり、`[Order; N]` が隙間なくパックされるために存在する。

**空スロット規約**は `size == 0`。新しい `OrderBook` のスロットはすべてゼロだ（System の `create_account` は常にゼロ初期化する）。`place_order` は `size == 0` の最初のスロットを探し、新しい注文をそこに書き、`active_count` を増やす。`cancel_order` は一致したスロットを `Order::zeroed()` で上書きし、`active_count` を減らす。コンパクションはしない。

なぜ別途 `is_active: bool` ではなく `size == 0` か。

- bool フィールドは 1 バイト + アラインメント維持のパディングのコストがかかる。`size` フィールドはすでに存在し、本物の注文は常に `size > 0` だ。アクティブ判定の番兵としてこのフィールドを再利用すれば、追加フィールドのコストを省ける。
- アカウント作成時のゼロ初期化により、規約は無料で機能する: 新規確保された `OrderBook` のスロットはすべて、明示的なセットアップなしに「空」になる。

コストは、プログラムが守るべき不変条件 1 行だ: `size == 0` の `Order` を絶対に書かない。両ハンドラとも、ペイロード中の `size == 0` を最初のガードで明示的に拒否する。`process_place_order`（lib.rs:1179–1182）から。

```rust
if price == 0 || size == 0 {
    msg!("place_order: price and size must be > 0");
    return Err(ProgramError::InvalidInstructionData);
}
```

これがフィールド再利用の代償だ。Anchor の `#[derive(BorshSerialize)]` アカウントには、フィールド意味論を独立させるために明示的な `is_active: bool` を持つことが多い — book あたり N スロット、M books の追加 1 バイトを払う代償で。本書ではトレードオフは再利用に有利に着地する。

> **演習 §7.2.** `Order` に `_pad2: [u8; 16]` フィールドを追加し、`cargo test -p openhl-state` を再実行せよ。`order_size_is_64_bytes` テストが失敗するはずだ。次に `pub const LEN: usize = 80;` を追加して残りのコードを型整合させ、サイズテストで `OrderBook::LEN` がどうなるか観察せよ。関係性は?

---

## §7.3  `place_order` を歩く

`programs/openhl-core/src/lib.rs:1158–1249` から。ハンドラは 3 部分に分解できる。

**検証**（1163–1196 行）: ペイロードサイズ、side バイト（0 か 1）、price と size 非ゼロ、user は署名者、book の所有者が本書のプログラムと一致、book サイズが `OrderBook::LEN` と一致、book のディスクリミネータが一致。

**線形走査**、1212–1218 行。

```rust
let mut chosen_slot: Option<usize> = None;
for (i, slot) in book.slots.iter().enumerate() {
    if slot.size == 0 {
        chosen_slot = Some(i);
        break;
    }
}
```

インデックス 0 から走査、`size == 0` の最初のスロットを取り、break。最悪ケース: book が満杯。ループは 32 反復全部を回り `None` を返し、「book 満杯」分岐に入って `AccountDataTooSmall` を返す。最良ケース: スロット 0 が空（book が新規）。1 反復。

CU コストの形:

- **空の book**（active_count = 0、slot 0 が空）: 1 反復。走査に約 50 CU。
- **半分埋まった book**（active_count = 16、slots 0–15 占有）: 16 反復。約 800 CU。
- **満杯 -1**（active_count = 31、slot 31 だけが空）: 31 反復。約 1550 CU。
- **満杯**（active_count = 32、全スロット占有）: 32 反復 + エラーパス。約 1700 CU + エラーオーバーヘッド。

絶対値としてはどれも小さい — 最悪ケースでも既定 200 KCU バジェットの 1% 未満だ。しかし**形**こそが教えだ: O(N) はコストがデータとともに成長することを意味し、本物の板ではその成長がバジェットを超え得る。slab なら配置が O(log N) に留まり、1024 注文でも本書の配列の N = 32 時の 16 反復より少ない CU で済む。

**書き込み**（1224–1238 行）: `next_order_id` をインクリメント、`active_count` をインクリメント、user の pubkey を `owner` にコピー、`Order` リテラルを構築、選んだスロットに投入。スロットが決まれば全部 O(1)。

952 行（走査前）と 988 行（書き込み後）の CU ブラケットがあれば、バリデータログからコストを読める。2 つの `sol_log_compute_units` を呼んで差を取れば、この命令の「走査 + 書き込み」が実際に消費した CU が出る。

> **演習 §7.3.** book にいろんな N（たとえば 0、8、16、24、31）の注文を事前投入し、その後新しい注文を 1 つ置け。各 N について 2 つのログ呼び出しの間で消費された CU を記録せよ。プロットせよ。N に対しておおむね線形なはずだ。

---

## §7.4  `cancel_order` を歩く

`process_cancel_order`（1256–1323 行）は構造的には `place_order` と同じだ — 線形走査、次に変更 — がマッチキーと変更内容が異なる。

**走査**、1291–1297 行。

```rust
let mut found: Option<usize> = None;
for (i, slot) in book.slots.iter().enumerate() {
    if slot.size != 0 && slot.order_id == order_id {
        found = Some(i);
        break;
    }
}
```

`slot.size != 0` で空スロットを飛ばす、`slot.order_id == order_id` で ID 選択。最悪ケースは「注文が最後のスロットにある」または「見つからない」 — どちらも O(N) を払う。

**認可チェック**、1303–1307 行。

```rust
if book.slots[slot_idx].owner != *user_ai.key.as_ref() {
    msg!("cancel_order: caller is not the order owner");
    return Err(ProgramError::IllegalOwner);
}
```

注文を置いた本人だけがキャンセルできる。スロットの `owner` フィールドがキャンセル認可の仕組みだ。これはスロット単位のチェックで、book 単位ではない — 「オペレータが何でもキャンセルできる」パスは存在しない。これはオープン CLOB として適切だが、vault 管理戦略では別になる。

**ゼロ化**、1057 行。

```rust
book.slots[slot_idx] = <Order as bytemuck::Zeroable>::zeroed();
```

`Order::zeroed()` は全ゼロの `Order` を返す。`<Order as bytemuck::Zeroable>::zeroed()` の修飾構文が必要なのは、`Order::zeroed` が固有メソッドとしてスコープに入っていないからだ — `bytemuck::Zeroable` トレイトから来る。書き込まれた後、スロットの `size` は 0、次の `place_order` が空スロット走査でこの位置を見つけて利用可能だと認識する。

CU コストは `place_order` と対称: 最良ケースは「スロット 0 で一致」（1 反復）、最悪ケースは「見つからない」（N 反復 + エラー）。1029 行と 1059 行のブラケットで計測できる。

> **演習 §7.4.** 注文を 10 個置き、order_id 5 をキャンセルせよ。その後さらに 1 個置け。新しい注文はどのスロットに着地するか。各操作の前後で `book`（フラグなし）のダンプ出力からスロット配分を辿れ。

---

## §7.5  この設計が第 8 章を生き残れない理由

第 8 章ではマッチングを実装する。マッチング ループの中核は「入ってくる taker 注文ごとに、反対側で最良の resting maker を見つけ、taker が尽きるまで交差する」だ。O(log N) データ構造ならツリー検索 1 回ずつでこのループが終わる。本書のフラット配列では、ループは次のことをしなければならない。

1. taker ごとに線形走査 → taker あたり O(N)
2. マッチ試行ごとに、すべての maker を線形走査 → maker チェックあたり O(N)
3. M 個の taker が K 個の maker と交差するとき、正しいスロットを見つけるためだけのコストは O(M × N) で、トークン移動の前段階だ

ORDER_CAPACITY = 32、M = 10、K = 5 で、マッチ命令あたり約 1600 配列走査になる。純粋な走査だけで約 80,000 CU。既定バジェットは 200,000。

第 8 章で実際にトークンを動かす必要があるとき（SPL Token への CPI、それ自体がトランスファあたり約 3,000 CU）、既定バジェットに収まらない。次のいずれかが必要になる。

- slab にリファクタリングする — 本番向け答え
- `ComputeBudgetInstruction::set_compute_unit_limit` で CU 上限を上げる — 絆創膏
- 命令あたりのマッチを N 交差に制限し、マッチャを複数回呼ぶようにする — 回避策

第 8 章ではこの 3 つを探索し、実際にスケールするのは slab だけだと説明する; 第 15 章が slab の実装を出荷する。今のところ、フラット配列は正しく、遅く、その遅さが目に見える。その可視性こそが、リファクタリングが何を買うかを理解する前提条件だ。

**Anchor が隠していること:** Anchor の `#[account(zero_copy)]` 属性は型付きフィールドアクセスで bytemuck キャストアカウントを利用可能にする。正しいデータ構造を選ぶことについては何もしない — その判断はフレームワークに関わらず常にあなたのものだ。素朴な `Vec<Order>` book レイアウトの Anchor プログラムは、本書のものと同じ速さで CU バジェットを溶かす。

---

## §7.6  まとめと自己検証

### まとめ図

```
OrderBook アカウント（2112 バイト、openhl-core 所有）:

  ┌──────── ヘッダ（64 バイト） ────────────────────────────────┐
  │ discriminator   bump  _pad0  market  next_order_id  active │
  │                                       (u64)         (u32)  │
  └────────────────────────────────────────────────────────────┘
  ┌──────── slots [Order; 32]（2048 バイト） ─────────────────┐
  │ [0]  Order or 空                                            │
  │ [1]  Order or 空                                            │
  │ ...                                                         │
  │ [31] Order or 空                                            │
  └────────────────────────────────────────────────────────────┘

  各 Order（64 バイト）:
    order_id (u64) | price (u64) | size (u64, 0 = 空)
    owner ([u8;32]) | side (u8, 0=bid 1=ask) | _pad ([u8;7])

place_order(side, price, size):
  検証 → slots[0..32] を size==0 で走査 → 書き込み → 完了
  コスト: O(active_count + 1)

cancel_order(order_id):
  検証 → slots[0..32] を order_id で走査 → 所有者チェック → ゼロ化
  コスト: 最悪 O(N)、最良 O(matched_position)
```

### 自分で検証する 3 項目

1. **線形コスト。** `book --init` を実行し、30 注文置き、31 番目を置け。ハンドラ開始時の読み値と書き込み後の読み値の間の `sol_log_compute_units` 差分を比較せよ。31 番目の配置の走査部分は、1 番目の配置の約 30 倍のコストになるはずだ。一定値（検証 / CPI / ログのオーバーヘッド）を引いて走査コスト自体を分離せよ。
2. **空スロット規約。** 5 注文置き、order 3 をキャンセルし、6 つ目を置け。6 つ目の注文はスロット index 5 ではなく、スロット index 2（キャンセルで空いた）に着地するはずだ。「最初の空スロットが勝つ」規則がキャンセルで開いた穴を埋める。
3. **Pod レイアウト不変条件は強制される。** `cargo test -p openhl-state` は 3 つの Order/OrderBook レイアウトテストを走らせる。`ORDER_CAPACITY` を 33 に変更し再コンパイルせよ。`order_book_size_matches_layout` テストは `2112 → 2176` を示すはずだ。安定した予測可能なバイト数こそ、bytemuck がこの構造体で機能する理由だ。

---

## 第 8 章への導線

板を持った。それを満たせる。order_id で注文を取り出せる。**まだできない**のは、bid を ask に対して**交差**させることだ。taker 注文が到着し、反対側で最良価格を走査し、maker に対して約定し、どちらかが尽きるまで繰り返す。これがマッチングであり、perp DEX における単一の最も CU を食う操作だ。

第 8 章では `Match`（または `Take`、流儀次第）を実装する — taker 注文を取り、resting book を歩き、約定を生む命令だ。本章のフラット配列レイアウトが本物の負荷に耐えられない理由を正確に確認し、置き換えとして動作する slab 実装を書き、CU 差を計測する。マッチャは、Phase A のすべての制約 — CU バジェット、ヒープ規律、並列性 — が単一の設計問題に収束する場所だ。
