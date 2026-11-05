# 第8章 — CU 圧の下のマッチングエンジン

> 状態: ドラフト (v0.1)。
> 教材コード: [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（`process_match` 1354–1482 行）、[`scripts/match/src/main.rs`](../../scripts/match/src/main.rs)、[`fuzz/matching/src/main.rs`](../../fuzz/matching/src/main.rs)（§8.6 の solinv ハーネス）。
> 第 7 章の `OrderBook` データ構造の上に立つ。

---

## §8.0  はじめに — 本章が教えるもの

第 7 章は slab 実装をここに hook していた; その約束は独自の章（第 15 章）に移った、slab がそれに値するほど大きく育ったからだ。本章は第 7 章が出荷したフラット配列 book に対するマッチングの **CU コスト形状**についての章だ — 正確に計測され、CU 枯渇への 3 つの対応（バジェット引き上げ、`max_fills` でのページング、slab へのリファクタ）が命名され、コスト付けられる。§8.4 の slab 擬似コードは第 15 章のコードが実装する設計仕様だ。

そこで本章では次を行う。

1. フラット book 上の `Match` アルゴリズムを歩く。
2. 実ログから CU コスト形状を読み、O(fills × N) であることを示す。
3. CU 圧への 3 つの対応（バジェット引き上げ、ページング、slab リファクタ）を実演し、それぞれのコストを説明する。
4. slab の擬似コード + 図を設計仕様として提供する; 第 15 章が実装を出荷する。

---

## §8.1  フラット book 上の Match アルゴリズム

`programs/openhl-core/src/lib.rs:1354–1482` から。ハンドラはペイロードの 4 フィールドを取る。

```text
[side u8][limit_price u64 LE][size u64 LE][max_fills u8]
```

`side` は**taker** 側だ（bid の taker は ask に対して買う、ask の taker は bid に対して売る）。`limit_price` は taker が受け入れる最悪価格。`size` は取りたい総 base 単位。`max_fills` は 1 命令あたりに交差する resting maker 注文数の上限 — ページングつまみだ。

マッチングループ、1413–1472 行。

```rust
let mut fills_done: u8 = 0;
while remaining > 0 && fills_done < max_fills {
    // (a) 取りに行ける限界価格内の最良の反対側 resting 注文を線形走査で探す
    let mut best: Option<(usize, u64)> = None;
    for (i, slot) in book.slots.iter().enumerate() {
        if slot.size == 0 || slot.side != maker_side { continue; }
        let price_acceptable = match taker_side {
            side::BID => slot.price <= limit_price,
            side::ASK => slot.price >= limit_price,
            _ => unreachable!(),
        };
        if !price_acceptable { continue; }
        // ... 「現在のベストより良いか」のチェック ...
    }

    // (b) 受け入れ可能な maker がなければ停止
    let (maker_idx, fill_price) = match best { Some(b) => b, None => break };

    // (c) 交差: min(taker 残, maker 残)
    let maker = &mut book.slots[maker_idx];
    let fill_size = remaining.min(maker.size);
    maker.size -= fill_size;
    remaining -= fill_size;
    fills_done += 1;

    // (d) maker が全約定したらスロットを空ける
    if maker.size == 0 {
        *maker = <Order as bytemuck::Zeroable>::zeroed();
        book.active_count = book.active_count.saturating_sub(1);
    }
}
```

fill ごとに 5 ステップ。吸収すべき点は 2 つ。

**各 fill が完全な O(N) 走査のコストを払う。** ステップ (a) は最良の反対側注文を見つけるために book の全スロットを歩く。これは第 7 章で `OrderBook` で下した設計判断だ — bid と ask は未ソートのスロットプールを共有する。近道はない。最低 ask を見つけるには全スロットを見なければならない。

**作業は乗算的に増える。** 1 命令で `fills_done` 回の fill を行うと、総走査作業は `fills_done × ORDER_CAPACITY` になる。ORDER_CAPACITY = 32 で 10 fill 交差なら、320 スロット検査に加えて検査ごとの比較オーバーヘッド。各検査は安価（約 30 CU）なので、320 検査で走査だけで約 10 KCU。実際の書き込み作業は定数。

ハンドラは**決済**ステップを意図的に省いている。本物の取引所は各 fill に対し taker から maker へ quote トークンも動かす（SPL Token CPI 経由）。そのような CPI 1 回が約 3,000 CU。10 fill のマッチ内側で走査作業の上に 30,000 CU 載り、ログとプログラム諸経費の前にすでに約 40 KCU 最小だ。既定 200 KCU バジェットならまだ入る — しかし余裕は縮んでいるし、N = 32 は語る価値のある**最小**の book だ。

> **演習 §8.1.** book に増加価格（例: 100、101、102, ...）の ask を 10 個事前投入せよ。`match-cli --side bid --limit-price 110 --size 50 --max-fills 5` を実行する。プログラムログを辿れ: マッチャが交差する 5 つの maker はどれか、どの順序か、結果の `taker_remaining` はいくつか。

---

## §8.2  CU コスト形状を読む

半分埋まった book（resting maker 16、すべて bid、taker は ask でそれに対してマッチ）に対して、`--max-fills` を 1 から 16 まで変化させてマッチャを実行する。

| max_fills | units consumed | fill あたり限界 |
|-----------|----------------|----------------|
|   1       |    4,254       |   —            |
|   2       |    5,950       |   1,696        |
|   4       |    9,276       |   1,663        |
|   8       |   15,637       |   1,590        |
|  16       |   27,220       |   1,448        |

(`fuzz/matching` の `grid` テストで測った。実際のプログラムを LiteSVM で走らせる。LiteSVM と `solana-test-validator` 4.1.2 が同じ数を数えることは第 4 章 §4.5 で確かめた。この表の以前の版は古いツールチェーンのもので、fill あたり約 3,500 CU だった。自分で測ること。)

fill あたりの限界コストはほぼ一定で、16 maker の book で約 1.5 KCU。一部は線形走査で、fill ごとに空でも埋まっていても 32 スロットすべてを見る。一部は maker の書き込みと、fill ごとに出す `msg!` の行だ。それ以上は分けて測っていない。大事なのは、それがどう増えるかだ。

book とともに増えるが、この大きさではゆっくりだ。8、16、24、32 maker に対する 4 fill は 8,500、9,276、9,980、10,680 CU で、maker が 1 増えるごとに fill あたり約 22 CU。32 スロットの book には 32 maker までしか入らないので、ここで 1 回の `Match` がする fill は最大 32 で、`max_fills = 32` なら 57,696 CU。それより多く求めると、何も見つからない走査がもう 1 回加わり、それが最悪の場合になる。`max_fills = 255` で 58,148 CU、既定の 200K に十分収まる。バジェットが壊れるのを見るには book を大きくする。`ORDER_CAPACITY = 128`(`crates/state` にある)では、128 maker に対する 1 fill が 7,565 CU、16 fill が 80,533 なので、最初の 1 fill のあとは 1 fill ごとに約 4,900 増える。64 maker に対する 64 fill は 194,916、128 maker に対する 64 fill はもう収まらない。

これが本章が可視化するためにある CU コスト形状だ。フラットマッチャの fill あたりコストは book サイズに比例して成長する。命令あたり総コストは `O(fills × N)`。両因子とも押し上げたくなるつまみだ（呼び出しあたり fill 数を増やす、book を大きくする）。十分に押し上げればバジェットが壊れる。32 スロットでは、その「十分」は book が行ける先より遠い。

**SDK が隠していること:** Anchor のプログラムログにも同じ「consumed N of M compute units」行が含まれる。ユーザコードからではなくランタイムから来るからだ。しかし Anchor は `units_consumed` を型付きフィールドとしてどこにも公開しない — 本書のようにシミュレーション結果から `sim.value.units_consumed` で読む。

> **演習 §8.2.** book を 30/32 容量にし、`--max-fills 30` でマッチャを実行せよ。満杯の book で測ると 54,935 CU で、既定に余裕で収まる。次に `crates/state/src/lib.rs` で `ORDER_CAPACITY = 128` にしてプログラムを作り直し、book を 128 maker で満たして `--max-fills 64` で実行する。`ComputationalBudgetExceeded` で失敗するはずだ。`--cu-limit 400000` を加えて再実行する。オンチェーン確定は成功するか。`--cu-limit 1400000`（ネットワーク最大）でも成功しなくなるのは `max_fills` がいくつのときか。

---

## §8.3  CU 圧への 3 つの対応

マッチャ（あるいは任意のハンドラ）がバジェットを押し始めたとき、現実的な選択肢は 3 つある。それぞれにコストがある。

### (1) tx あたりコンピュートユニット上限を上げる

最も簡単な修正、長期的には最悪の答え。トランザクションに `ComputeBudgetInstruction::set_compute_unit_limit(N)` を前置する。N は最大 1,400,000。第 4 章から、これが単一トランザクションに対して効くことを知っている。`scripts/match/src/main.rs` から。

```rust
if let Some(limit) = cli.cu_limit {
    instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(limit));
}
instructions.push(match_ix);
```

**コスト:** 優先手数料は要求上限に比例し、実消費には比例しない。1.4M CU を要求して 100K しか使わないトランザクションも、優先手数料計算では 1.4M 上限に対して支払う。ネットワーク容量（高競合時のスロットあたり約 50 K-CU）では、大きな要求が同じスロットの他のトランザクションを締め出すこともある。

**これが正解になるとき:** 呼び出しあたり真に > 200K CU を要する命令（初期セットアップ、たまのバルク操作）で、優先手数料の希釈が許容できる場合。

### (2) `max_fills` でページングする

マッチャはすでにこれをサポートする。呼び出しあたりの作業に上限をつけ、上限を呼び出し側に晒し、クライアントに完了まで反復させる。

```text
match --max-fills 8
match --max-fills 8     # 次ページ
match --max-fills 8     # 次ページ
...
```

各呼び出しはバジェットに余裕で収まる。ネットワーク総コストは同じ（マッチング作業はどちらにせよ同じ量）だが、1 トランザクションではなく複数に分散される。

**コスト:** 複数往復。マッチャのページ間で別トランザクションが book を変更するリスク（ページ全体のアトミック性を失う — book 状態がページ N とページ N+1 の間で変わりうるし、マッチャはそれを優雅に扱う必要がある）。tx あたり手数料オーバーヘッドがページ数倍になる。

**これが正解になるとき:** クロス全体のアトミック性が要らないマッチャ（どうせスライスする HFT 風 taker）、あるいは slab リファクタ前の応急処置として。

### (3) slab にリファクタリングする

本番向け答え。フラット `[Order; N]` slots 配列を次に置き換える。

- **ノードプール**: 固定サイズのノードアリーナ、空きスロットインデックスの別フリーリスト付き。正しいスロットが見つかれば、挿入とキャンセルは O(1)。
- **価格レベルごとの FIFO**: 各価格に、FIFO 順序の注文ノード双方向リンクリスト。マッチング中のベスト価格進行は `head.next` だけ。
- **価格レベルの critbit ツリー**: 価格でキー化、ソート済み、「ベスト価格」と「新価格レベル挿入」が O(log N)。

マッチャはこうなる。

```text
remaining > 0 かつ fills_done < max_fills の間:
    level = tree.find_best_acceptable(taker_side, limit_price)   # O(log N)
    if level is None: break
    fill_size = min(remaining, level.head.size)
    level.head と交差
    if level.head.size == 0:
        level.pop_head()        # O(1)
        if level.is_empty():
            tree.remove(level)  # O(log N)
    fills_done += 1
```

価格がばらけていれば、fill あたりコストは `O(N)` ではなく `O(log N)`。N = 1024 なら fill あたり約 10 vs 約 1000 検査。ただし critbit は自己平衡しない。深さは価格のビットに従い、キーの 64 ビットまで深くなりうるので、価格を選べる maker はそこまで深くできる(第 15 章 §15.7 で測る)。現実的な book ならマッチャはバジェットに収まり、敵対的な book でも `max_fills` の上限がそこに留める。

**コスト:** 大きな実装労力。正しい critbit + ノードプール + FIFO は約 1,500 行の慎重な Rust で、強い不変条件を伴う（各ノードはツリー+キューに住むかフリーリストにあるかのいずれか、各レベルは ≥1 注文を持つかツリーから消えているかのいずれか）。監査コストはそれに比例して高い。全体が `bytemuck::Pod` 適合だが慎重さを要する: ツリーノードはポインタではなくプール内のインデックスを持つ。

**これが正解になるとき:** 非自明な流動性を持つ本番 CLOB。Phoenix と Serum はどちらも理由あってこの設計を使う。

slab 実装は第 15 章に住み、下記の設計仕様から生まれたコードを歩く。

---

## §8.4  Slab 擬似コード

最小 slab 構造、擬似 Rust で。

```rust
const POOL_CAPACITY: usize = 1024;
const TREE_CAPACITY: usize = 256;   // 木のノード数: 片側の価格水準は約 128(葉 + 内部ノード)   // 一意な価格レベル数

#[repr(C, Pod)]
struct Slab {
    discriminator: [u8; 8],
    bump: u8,
    _pad: [u8; 7],

    // ノードプール — 各スロットは生 OrderNode かフリーリストの一部
    nodes: [OrderNode; POOL_CAPACITY],
    free_head: u16,         // 最初の空ノードのインデックス、または NONE_INDEX

    // 価格レベル critbit ツリー — bid ツリーと ask ツリーは別
    bid_tree: CritbitTree,
    ask_tree: CritbitTree,
}

#[repr(C, Pod)]
struct OrderNode {
    order_id: u64,
    owner: [u8; 32],
    size: u64,
    next: u16,              // この価格レベル FIFO 内の次の OrderNode
    prev: u16,              // この価格レベル FIFO 内の前の OrderNode
    _pad: [u8; 4],
}
// ノードあたり 64 バイト、[OrderNode; 1024] = 64 KiB。他をあまり詰めなければ 64 KiB アカウントに収まる。

#[repr(C, Pod)]
struct CritbitTree {
    nodes: [TreeNode; TREE_CAPACITY],
    root: u16,
    free_head: u16,
}

#[repr(C, Pod)]
struct TreeNode {
    // critbit 風: INNER ノード（分割ビット + 2 子インデックス）か
    // LEAF ノード（価格 + FIFO キューの head/tail）のいずれか。
    tag: u8,                // 0 = leaf, 1 = inner
    // ... レイアウトは tag に依存
}
```

主要操作とコスト:

- **insert(order)**: 価格レベルを見つける critbit ウォーク（O(log N)、N = 一意価格レベル数）、レベルがなければ作る（ツリーノード 1 個確保）、`free_head` からノードを取り出す、埋める、レベルの末尾にプッシュ。総計: O(log N) ツリー作業 + O(1) プール作業。
- **best_price(side)**: 最左（ask）または最右（bid）の葉までの critbit ウォーク。O(log N)。
- **match_top(side, max_size)**: best_price → FIFO の head → 交差 → head が全約定したら head を pop してノードを `free_head` に返し、レベルが空ならツリーから削除。fill あたり O(log N)。

実装するなら吸収すべき教育上の論点。

1. **インデックスを使う、ポインタではなく。** `bytemuck::Pod` はポインタフィールドを許さない。プール配列とツリー配列への `u16` インデックスを使う。`u16` は 65k エントリで足りる — ほとんどの book には十分。
2. **`NONE_INDEX = u16::MAX` の番兵。** `Option<u16>` はだめ — 構造体を Pod 安全から外す。番兵を使う。
3. **片方向リンクスタックとしてのフリーリスト。** 空ノードの `next` フィールドが次の空ノードを指す。確保は `free_head` を pop、解放は `free_head` に push。両方とも O(1)。
4. **critbit、red-black ではない。** critbit ツリーは再バランス規則がより単純で、回転ロジックが要らない。Serum は critbit、Phoenix も critbit。パターンは踏み固められている。
5. **side ごとに 1 ツリー。** bid と ask は「ベスト」の意味論が異なる（max vs min）。2 ツリーにすれば比較子をツリーコードに通す必要がない。

slab 実装は、初めてなら 3-4 日の演習だ。1 日目はプール + フリーリスト。2 日目は critbit の insert/remove。3 日目はマッチャへの配線。4 日目はエッジケース（book 満杯、レベル満杯、all-or-nothing fill）のテスト。第 15 章がその結果の実装を歩く。

> **演習 §8.4.** プール + フリーリスト部分を組み立てよ。`Pool<OrderNode, 1024>` を書け、`alloc() -> Option<u16>` と `free(idx: u16)` メソッド付きで。10,000 個のランダムな alloc/free ペアで、プールが常に正しい `available_count` を持つことを検証せよ。これが最も正しく書きにくい単一部分だ — `free_head` の不変条件は崩しやすい。

---

## §8.5  出荷したものと十分なもの

本章はフラット book 上の動作するマッチャをページング付きで出荷した。これは次に十分だ。

- 学習用成果物: マッチャは監査可能、約 110 行の Rust、CU コスト形状が観測可能。
- 低スループット本番デプロイ: `<= 32` resting 注文と `<= 16` fill / 呼び出しの market は 200 KCU バジェットに余裕で収まる。
- 教育研究: マッチング規則を試す（本書がやったのは価格時間優先。pro-rata、時間加重 pro-rata、last-look — どれも同じハンドラ形に差し込める）。

次には十分でない。

- 数百〜数千の resting 注文を持つ book。
- 大きな taker をアトミックに処理するマッチャ。
- fill あたりレイテンシが効く HFT 風ユースケース。

そのためには slab を出荷する。§8.4 の擬似コードが設計仕様で、第 15 章がその実装だ。

---

## §8.6  まとめと自己検証

### まとめ図

```
フラット book 上の Match（本章）:

  fill ごと:    O(N) 走査 + O(1) 書き込み     → O(K × N) 総計
  ORDER_CAPACITY=32 book で K=16:           約 27 KCU(16 maker)、33 KCU(32)
  失敗モード:    32 スロットではなし(最悪の 32 fill + 空振りの走査 1 回で約 58 KCU)。
                 128 スロットでは約 40 fill を超えると CU 枯渇


Slab 上の Match（第 15 章、上で擬似コード化）:

  fill ごと:    O(log N) ツリーウォーク + O(1) FIFO 進行  →  O(K × log N) 総計
  1,024 注文の slab で K=16:               22 KCU(実測、§15.6)
  失敗モード:    プール枯渇（別 place_order 拒否で扱う）。
                 深い価格キーの上での長いマッチの CU(§15.7)


CU 圧への 3 つの対応:

  1. ComputeBudgetInstruction::set_compute_unit_limit(N)
     - 1.4M CU まで上限拡張。
     - コスト: 優先手数料は要求上限に対して、実消費に対してではない。

  2. max_fills ページング
     - 呼び出しあたり作業に上限、複数往復。
     - コスト: ページ間アトミック性喪失、ページあたり手数料。

  3. slab リファクタ
     - fill あたり O(log N)、O(N) ではない。
     - コスト: 実装 + 監査作業。
```

### 自分で検証する 3 項目

1. **コストは乗算的に成長する。** book に 8、16、24 の活性 maker を事前投入せよ。それぞれに対し `match-cli` を `--max-fills 4`（定数）で実行する。`units_consumed` を記録する。N とともに増えるが、この大きさでは fill ごとの固定部分が支配的だ。私たちの測定では 8,500、9,276、9,980。`O(K × N)` の項はある(maker が 1 増えるごとに fill あたり約 22 CU)が、効いてくるにはもっと大きな N が要る(§8.2)。
2. **ページングは正しい小計を持つ。** 満杯 32 maker book と 10 単位 taker に対し `--max-fills 16` でマッチャを実行する。`units_consumed` と post-state の `active_count` を記録する。次に同じマッチング作業を `--max-fills 8` の 2 呼び出しに分けて行う。両呼び出しの `units_consumed` 合計は 1 回の 16 呼び出しの数値に非常に近いはずだ（2 つの `process_match` セットアップオーバーヘッドのぶんだけわずかに高い）。taker を 16 すべてを取れる大きさにすると、1 呼び出しで 33,020、2 呼び出しで 18,476 + 17,072 だった。
3. **バジェット引き上げは本物。** 32 スロットの book には、既定のバジェットに収まらないほど大きなマッチが入らない。詰め込まれた 32 maker book に対する 25 fill は 47,645 だった。`ORDER_CAPACITY` を 128 に上げ(演習 §8.2 のとおり)、book を満たし、64 fill を `--cu-limit` なしで実行する。失敗するはずだ。`--cu-limit 500000` を加える。成功するはずだ。`sim.value.units_consumed` が既定 200,000 と 500,000 の間にあることを確認せよ。


### 高くつくマッチを道具に探させる

第 4 章 §4.7 では、呼び出し側がわざと値段を選ぶ `Bench` に [solinv](https://github.com/psyto/solinv) の cu-dos を走らせた。`Match` は他の人が頼る命令で、その値段を選ぶのは 2 者だ。taker が `max_fills` を選び、maker が book をどれだけ埋めるかを選ぶ。[`fuzz/matching/`](../../fuzz/matching) は book を 32 の買い注文で満たし(入る最大数。cu-dos は試すたびに book を元に戻す)、taker が 1〜255 の任意の `max_fills` を選べるようにして、1 回の `Match` に 50,000 CU の上限(`MATCH_CU_CAP`)を宣言する。第 3 章 §3.7 のとおり準備してから:

```bash
cargo build-sbf --manifest-path programs/openhl-core/Cargo.toml
cd fuzz/matching
cargo build --release --features invariant_ch8
cargo test --release --features invariant_ch8      # コストは book とともに増える
timeout -s INT 60 crucible run openhl_matching_fuzz invariant_ch8 \
    --binary-in target/release/invariant_test --timeout 45
```

> **演習 §8.6.** 走らせよ。数秒で次のような行が出るはずだ:
>
> ```
> [FUZZ_FINDING] [cu-dos:…] ix match consumed 58148 CU (cap 50000)
> ```
>
> taker が、満杯の book で上限が許す以上の fill を求めた。58,148 は §8.2 の満杯の book での最悪の場合だ。第 4 章の教訓は、上限をランタイムの限界まで上げても何も直らない、だった。ここには本当の直し方がある。最悪の場合に上限があるからだ。book は 32 スロットで、1 回の呼び出しがどれだけ働くかはプログラムが決める。§8.2 の数値から、満杯の book で 50,000 に収まる `max_fills` はいくつまでか。その答えで `process_match` に `if max_fills > N { return Err(...) }` を足し、作り直してもう一度走らせよ。
>
> 答えは測定にある。満杯の book で 26 fill は 49,147、27 fill は 50,627 なので、収まる最大は 26 だ。一部だけ埋まった book では同じ上限でもコストが低い(27 maker に 28 を求めて 48,011)。maker がなくなれば fill が止まるからで、どの上限でも満杯の book が最悪の場合になる。私たちは次のツールチェーンの値段に備えて余裕をとり 24 を使った。fuzzer は 45 秒で何も見つけず、上限から予想したとおりだった。これは §8.3 の選択肢 (2)、ページングを必須にしたものだ。もっと fill がほしい taker は呼び出しを増やす。これが成り立つのは book が 32 スロットのままの間だけだ。128 スロットでは最初の 1 fill が 7,565 CU、以後 1 fill ごとに約 4,900 なので、同じ上限で 1 呼び出しが約 119,000 になりうる。`ORDER_CAPACITY` を上げるたびに上限を下げなければならない。この結びつきが、選択肢 (3)、slab(第 15 章)を選ぶ理由だ。slab の fill あたりコストは N ではなく log N で増える(§8.4)。

---

## 第 9 章への導線

market、vault、マッチャを手にした。**まだ持っていない**のは**マーク価格**だ。マッチャは注文を互いに交差させるが、perp DEX における**価格**には真実の源が 2 つある: トレードテープ（最終約定価格、本書のマッチャが暗黙に作る）と、外部オラクル（Pyth、Switchboard）で mark を spot に固定するもの。ファンディングレート、清算トリガ、リスク数学はすべて、最終約定ではなくオラクル mark に依存する。

第 9 章では Pyth 統合を歩く: 価格アカウント レイアウト、命令ハンドラ内部で自前のデシリアライズを信頼せず読む方法、スタール価格の扱い（`slots_since_published`）、Pyth が利用不可なら Switchboard 副を使うフォールバック。章は、プログラムが外部 mark 価格を読み、それを使って `place_order` の価格が健全帯内であることを検証するところに着地する — プログラム内の最初の本物のリスク制御だ。
