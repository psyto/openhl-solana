# 第15章 — Slab 板

> 状態: ドラフト (v0.1)。
> 教材コード: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs)（`Slab`、`OrderNode`、`CritbitTree`、`TreeNode`）、[`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs)（slab ヘルパ + `process_create_slab`、`process_slab_place_order`、`process_slab_match`）、[`scripts/slab/src/main.rs`](../../scripts/slab/src/main.rs)。

---

## §15.0  はじめに — slab が何を買い、なぜ独立章に値するか

第 7 章はフラット配列の `OrderBook` を出荷した — 32 スロット、insert で空きスロットを線形スキャン、`Match` で価格マッチを線形スキャン。第 8 章はコストを測った: `K = 10` の fill ループで `N = 32` の板に対して概ね `O(K × N)` を走り、settlement の前に板の簿記だけで ~30 KCU を費やす。これは**教育用のベースラインとして**機能する。本番の板としては機能しない — どんな現実的な深さでもスキャンが支配する。

Hyperliquid と Serum / Phoenix の答えは **slab** だ: 価格水準の上で自己バランスする木で、各葉がその価格水準のオーダの FIFO を持つ。Insert は価格水準ごとに O(log N)。Best-price は O(log N) — 求める側に向かって root から歩く。Best を pop するのは最悪 O(log N)（水準が空になり葉を木から削除する必要がある場合。葉の上の inner が冗長になり、葉と一緒に折りたたまれる）。§8.4 の擬似コードがこの章を約束した。これがそれだ。

本章は第 7 章の `CreateOrderBook` / `PlaceOrder` / `Match` に並列な、3 つの新しい命令を出荷する:

1. **`CreateSlab`**（タグ 29）— market ごとの `Slab` PDA を確保し、OrderNode プールの free list と両 critbit 木の free list を初期化する。
2. **`SlabPlaceOrder`**（タグ 30）— `PlaceOrder` と同じペイロード。critbit insert を走らせ、プールから OrderNode をアロケートし、その価格水準の FIFO につなげる。
3. **`SlabMatch`**（タグ 31）— `Match` と同じペイロード（taker side + limit price + size + max_fills cap）。maker 側の best 葉を歩き、その FIFO 先頭に当てて埋め、空になった水準を取り除き、§8.3 のページング cap を適用する。

フラット配列の `OrderBook` と slab の `Slab` はプログラム内で共存する。異なる market が異なる板を選べる; 既存の market は乱されない。本章は**新しい**パスについての章だ。

本章が**ない**もの 3 つ:

- **Match エンジン外科手術の章ではない。** slab の `SlabMatch` は第 8 章と同じ fill-loop の形を走る — 変わったのは maker 側にどう到達するか（O(N) でなく O(log N)）だけで、fill がどう会計されるかは変わらない。Settlement、手数料、builder split — どれもここには住まない。
- **すべての critbit バリアントのガイドではない。** Serum の slab は葉と inner を識別子ビット付きで同じノードプールにパックする; Phoenix はわずかに違うレイアウトを per-level FIFO 会計構造で使う。本章は §8.4 の分割 — OrderNode プール 1 つ、TreeNode プール side ごとに 1 つ — に従う。それが教育的議論を最もクリーンに保つからだ。他のレイアウトはヘッダの数百バイトとアクセスのわずかな高速化を取引する。
- **ベンチマークではない。** 第 8 章はフラット板を測った; 現実的な深さ（>100 水準）で slab をそれに対してベンチするのはフォローアップに属する。§8.4 の擬似コードは N ≈ 8 を超えるとあらゆる次元で slab が勝つと予測する。

---

## §15.1  4 つのアカウント型

`crates/state/src/lib.rs` から:

```rust
pub struct OrderNode {                          // 64 バイト
    pub order_id: u64,
    pub owner: [u8; 32],
    pub size: u64,
    pub next: u16,        // FIFO 内次（または free list 内次）
    pub prev: u16,
    pub _pad: [u8; 12],
}

pub struct TreeNode {                           // 32 バイト — タグ付き union
    pub tag: u8,          // 0 FREE、1 INNER、2 LEAF
    pub split_bit: u8,    // INNER のみ
    pub next_free: u16,   // FREE のみ
    pub left_child: u16,  // INNER のみ
    pub right_child: u16, // INNER のみ
    pub head_node: u16,   // LEAF のみ（FIFO 先頭 order_node idx）
    pub tail_node: u16,   // LEAF のみ
    pub order_count: u32, // LEAF のみ（観測性）
    pub price: u64,       // LEAF のみ
    pub _pad: [u8; 8],
}

pub struct CritbitTree {                        // 8 + 256 × 32 = 8208 バイト
    pub root: u16,                              // 空なら SLAB_NONE_INDEX
    pub free_head: u16,
    pub _pad: [u8; 4],
    pub nodes: [TreeNode; SLAB_TREE_CAPACITY],  // SLAB_TREE_CAPACITY = 256
}

pub struct Slab {                               // 計 ~82 KiB
    pub discriminator: [u8; 8],                 // SLAB\0\0\0\0
    pub bump: u8,
    pub _pad0: [u8; 7],
    pub market: [u8; 32],
    pub next_order_id: u64,
    pub active_count: u32,
    pub pool_free_head: u16,
    pub _pad1: [u8; 2],
    pub bid_tree: CritbitTree,
    pub ask_tree: CritbitTree,
    pub nodes: [OrderNode; SLAB_POOL_CAPACITY], // SLAB_POOL_CAPACITY = 1024
}
```

吸収すべき設計選択 5 つ:

**1. すべて `u16`、`Option<u16>` は使わない。** Pod 安全性のため。Sentinel `SLAB_NONE_INDEX = u16::MAX` が「インデックスなし」の代わりに立つ。Free-list 終端、空木の root、宙ぶらりんの FIFO 端、すべてこれを使う。`u16` で 65,535 エントリまでアドレス可能 — `SLAB_POOL_CAPACITY = 1024` は十分その範囲内。

**2. `tag` バイトでタグ付き union、enum は使わない。** `TreeNode` は原理的には Rust の enum（`Free`、`Inner { ... }`、`Leaf { ... }`）にできる。できない理由は enum が `Pod` でないからだ。だからどのバリアントが必要としうるすべてのフィールドを並べ、`tag` バイトがどのフィールドを読むかを消費者に告げる。使われないフィールドはゼロまたは `SLAB_NONE_INDEX` を保持する。規律のトレードは本物だ — 新たに free された node に古い `head_node` が残っているのは潜在バグ — だが zero-copy bytemuck アクセスでレイアウトは報われる。

**3. slab ごとに 2 つの木、1 つではなく。** Bid と ask の木は構造的に同一; 違いは「best price」歩行方向（max vs min）だけ。分けて保つことで、bid 側 insert が ask 状態を辿ることはなく、逆もない。これは Sealevel スタイルの並列性で効く: bid だけ動く market が共通 root を持つなら ask 木も書き、独立でありえたものを直列化してしまう。

**4. 共有 OrderNode プール 1 つ。** `Slab.nodes` は単一配列。両方の木が同じプールから FIFO を引く。代替（side ごとプール）なら同じトータルフットプリントで side ごと OrderNode 容量が倍だが、片側だけの板（片方向の動きで bids ほぼ全部のとき）が片方のプールを枯渇させ他方が空のまま、ということになる。共有することで非対称フローでも slab を使えるように保つ。

**5. アカウントサイズ = ~82 KiB。** 計算: `64 (ヘッダ) + 2 × 8208 (木) + 1024 × 64 (プール) = 82_064` バイト。これは Solana の「10 KiB」CPI realloc ソフト上限を大きく超えるが、Solana の `create_account` CPI はずっと大きい `MAX_PERMITTED_DATA_LENGTH = 10 MiB` で境界される。`CreateSlab` は `system_instruction::create_account` で全体を 1 回で確保する。Rent は market がブートストラップで払う 1 回限りのコスト。

> **演習 §15.1.** 構造体レイアウトから正確な `Slab::LEN` を計算せよ（コメントを信用するな）。`bytemuck::bytes_of(&Slab::zeroed()).len()` で検証せよ。次に、`lamports_per_byte_year = 3_480` と `years = 2`（Solana の 2 年 rent 免除）で rent をランポートで計算せよ。それが概ね `CreateSlab` が請求するもの。

---

## §15.2  プールと free list

`Slab.nodes` は 1024 OrderNode のフラット配列。任意の瞬間、ある node は「ライブ」（ある価格水準の FIFO の一部）、ある node は「free」（アロケーション待ち）。Free node は `next` フィールド経由で片方向リンクのスタックを成し、`Slab.pool_free_head` がスタックの先頭。アロケーションは `pool_free_head` を pop、デアロケーションは push する。

`programs/openhl-core/src/lib.rs` から:

```rust
fn slab_init_pool(slab: &mut Slab) {
    for i in 0..SLAB_POOL_CAPACITY {
        slab.nodes[i].next = if i + 1 < SLAB_POOL_CAPACITY {
            (i as u16) + 1
        } else {
            SLAB_NONE_INDEX
        };
        // ... 他のフィールドをゼロに ...
    }
    slab.pool_free_head = 0;
}

fn pool_alloc_node(slab: &mut Slab) -> Result<u16, ProgramError> {
    let head = slab.pool_free_head;
    if head == SLAB_NONE_INDEX {
        return Err(ProgramError::AccountDataTooSmall);
    }
    slab.pool_free_head = slab.nodes[head as usize].next;
    Ok(head)
}

fn pool_free_node(slab: &mut Slab, idx: u16) {
    let head = slab.pool_free_head;
    slab.nodes[idx as usize].next = head;
    // ... 他のフィールドをゼロに ...
    slab.pool_free_head = idx;
}
```

3 つフラグすべき細部。

**Init は配列を 1 度歩く。** ブートストラップで `O(N)`（~1k イテレーション）。そのコストは market ごとに最大 1 回走る `CreateSlab` で払う。以後すべての op は O(log N) か O(1)。

**Free 時にフィールドをゼロに保つ。** Node を free する際、リストに push するだけでなく `size`、`order_id`、`owner` をクリアする。コストは小さい（数個のストア）が、古い `owner` フィールドが将来のアロケーションに誤って漏れることを防げる。同じメモリを多くのオーダ寿命で再利用するから重要。

**失敗は `AccountDataTooSmall`。** プール枯渇時はエラーを返し、既存 node を上書きしない。`1024 nodes` は単一 market に大きい数だ。当たったら答えは (a) 古いオーダを batch cancel してノードを free するか、(b) より大きな `Slab` バリアントを出荷するかだ。本章は保守的な答えを出荷する。

2 つの per-tree プール（`bid_tree.nodes` と `ask_tree.nodes`、両方とも `[TreeNode; 256]`）は同じく動く。`tree_init` は各々を 1 度歩く; `tree_alloc` と `tree_free` は同じ pop/push パターンを `next_free` フィールド経由で従う。

> **演習 §15.2.** Free list を歩いてカウントする `available_count(slab) -> u32` ヘルパを実装せよ。Insert + cancel のシーケンスの前後でそれを走らせる; 不変条件 `live + free == SLAB_POOL_CAPACITY` が成立すべし。Free-list 破損（同じ node が 2 度リストに現れる、あるいはサイクル）は slab バグの最も難しい 1 クラス — `available_count` の instrumentation はそれを捕捉する最も安い方法だ。

---

## §15.3  Critbit insert

Critbit アルゴリズムは、各 inner node がキーの 1 つの特定のビットをテストする木を歩く。深い inner node でテストされるビットはより**下位**のもの — より上位のビットは root に近いところで決められる。

**`price` のための葉を見つけるか作る。** `critbit_find_or_create_leaf` から:

```rust
// (1) 空木 → root 葉を作る。
if tree.root == SLAB_NONE_INDEX {
    let leaf_idx = tree_alloc(tree)?;
    tree.nodes[leaf_idx] = TreeNode { tag: LEAF, price, ... };
    tree.root = leaf_idx;
    return Ok((leaf_idx, true));
}

// (2) 価格を比較するためどれか葉まで歩く。
let found_leaf = critbit_walk_to_leaf(tree, price);  // (price >> split_bit) & 1 で降りる
let found_price = tree.nodes[found_leaf].price;
if found_price == price {
    return Ok((found_leaf, false));  // 既存水準
}

// (3) 新価格 — 新 inner + 新葉を継ぎ込む。
let diff = price ^ found_price;
let shared_bit = 63 - diff.leading_zeros() as u8;  // 価格が違う最上位ビット

// Root から再び歩き、継ぎ込みポイントを探す。
let mut parent = SLAB_NONE_INDEX;
let mut parent_side = 0u8;
let mut cur = tree.root;
loop {
    let node = &tree.nodes[cur];
    if node.tag == LEAF || node.split_bit < shared_bit { break; }
    let bit = ((price >> node.split_bit) & 1) as u8;
    parent = cur;
    parent_side = bit;
    cur = if bit == 1 { node.right_child } else { node.left_child };
}

// (4) 新葉 + 新 inner をアロケート; 新 inner は `shared_bit` で分割。
let new_leaf_idx = tree_alloc(tree)?;
tree.nodes[new_leaf_idx] = TreeNode { tag: LEAF, price, ... };
let new_inner_idx = tree_alloc(tree)?;
let price_bit = ((price >> shared_bit) & 1) as u8;
let (left, right) = if price_bit == 1 { (cur, new_leaf_idx) } else { (new_leaf_idx, cur) };
tree.nodes[new_inner_idx] = TreeNode { tag: INNER, split_bit: shared_bit, left_child: left, right_child: right, ... };

// (5) 親に配線する。
if parent == SLAB_NONE_INDEX {
    tree.root = new_inner_idx;
} else if parent_side == 1 {
    tree.nodes[parent].right_child = new_inner_idx;
} else {
    tree.nodes[parent].left_child = new_inner_idx;
}
```

吸収すべきもの 5 つ。

**ステップ 2 は「*どれか*葉まで歩く」。正しい葉ではない。** Critbit の優雅さは、root から `(price >> split_bit) & 1` で歩くと**ある**葉 — `price` と最長の critical-bit prefix を共有する葉 — で終わることだ。それが `shared_bit` を見つけるために比較すべき唯一の葉。列挙不要、複数葉と比較する必要なし。

**`shared_bit = 63 - diff.leading_zeros()`。** 標準トリック: `diff` の最上位セットビットは `63 - leading_zeros` の位置（`u64` で）。`diff != 0`（等価価格ケースを除外済み）なので、`leading_zeros` は `0..=63` の範囲。

**ステップ 3 の歩きは正しい不変条件で止まる。** (a) 葉に当たるか、(b) `split_bit < shared_bit` の inner node に当たるとき継ぎ込む。両方とも意味は: 既存木はこのパス上に `shared_bit` で識別する inner を持たない、だから新 inner はここに来るべき。不変条件は維持される: `split_bit >= shared_bit` の inner では `price` と `found_price` がそのビットで一致するので、降下を続けられる。

**ステップ 4 の `price_bit` が新葉のどちら側を決める。** `price` がビット `shared_bit` をセットしているなら右へ、そうでなければ左へ。既存サブツリー（`cur`）は逆側を取る。なぜか? `shared_bit` で `price` と `found_price` が違うから — だから新 inner の反対側に属する。

**ステップ 5 は唯一の「配線」ステップ。** その前のすべてはノードをアロケートして埋めただけ。実際の木接続は親の `left_child` か `right_child` ポインタ 1 つ（または `tree.root`）。間違えやすい; テストもしやすい。

ハンドラ `process_slab_place_order` はその後、（既にアロケート & 初期化済みの）新 OrderNode を葉の FIFO に追加する。葉が新品なら（`head_node == SLAB_NONE_INDEX`）、新オーダが先頭と末尾の両方になる。既存なら、新オーダが新末尾になり、旧末尾の `next` と新ノードの `prev` がリンクされる。

> **演習 §15.3.** 価格を 3 つの異なる順序で insert せよ: `[100, 200, 150]`、`[200, 100, 150]`、`[150, 100, 200]`。出来上がる木を手で歩け（または debug print で）。3 つすべてが昇順で iterate すると `[100, 150, 200]` を生む木を作るはずだ — だが内部構造（どの inner が root か、葉がどちら側にあるか）は異なる。それでよい: critbit はその葉について順序非依存だが、内部形状については違う。

---

## §15.4  Best-price + match-and-pop

`critbit_find_best`:

```rust
fn critbit_find_best(tree: &CritbitTree, want_max: bool) -> Option<u16> {
    if tree.root == SLAB_NONE_INDEX { return None; }
    let mut cur = tree.root;
    loop {
        let node = &tree.nodes[cur];
        if node.tag == LEAF { return Some(cur); }
        cur = if want_max { node.right_child } else { node.left_child };
    }
}
```

アルゴリズム全体は「自分の側に向かって root から歩く」。Bid は最高価格を望むので常に右へ。Ask は最低を望むので常に左へ。木の不変条件 — 右側葉は `split_bit` で 1、左側葉は 0 — と、最高 split_bit から最低へ降下することの組み合わせが、これを単調に極値に到達することを保証する。

`SlabMatch` はその後走る:

```rust
while fills < max_fills && remaining > 0 {
    let (leaf_idx, head_idx, level_price) = slab_peek_best(slab, want_max)?;
    if !crosses(level_price, limit_price) { break; }

    let maker_size = slab.nodes[head_idx].size;
    let fill = remaining.min(maker_size);

    if fill == maker_size {
        slab_pop_head_of_leaf(slab, maker_side, leaf_idx)?;  // 葉を落とすかも
    } else {
        slab.nodes[head_idx].size = maker_size - fill;
    }

    remaining -= fill;
    fills += 1;
}
```

これは構造的に第 8 章のフラット板 `Match` と同じ fill-loop 形状 — taker は `remaining` を持ち、maker 側を best から歩き、`remaining == 0`、次の maker が limit を超えない、`max_fills` ページング cap が効くのいずれかまで埋める。違うのは、maker を見つけるのが O(N) でなく O(log N) で、maker を pop するのが O(1) でなく O(log N)（水準が枯れたときのトレードオフが `slab_pop_head_of_leaf` に住む）。

`crosses` 述語（上にインライン）は limit-price gate:

- **BID taker:** 高々 `limit_price` で買いたい → `level_price <= limit_price` の asks を cross。
- **ASK taker:** 少なくとも `limit_price` で売りたい → `level_price >= limit_price` の bids を cross。

Gate が cross をやめると（maker 価格が taker に有利でなくなった）、match は `remaining > 0` で戻る。Taker は望めば未充足部分を maker として post できる — それは同じ caller からの別の `SlabPlaceOrder` 呼び出し。

> **演習 §15.4.** 10 個の asks を価格 100, 101, 102, ..., 109 で insert せよ。`limit_price = 105`、`size = 25`、`max_fills = 10` で BID taker を送れ。Fills を追え: どの価格にどの順序で当たるか、taker は何回 fill した後 limit gate に当たるか、match が戻った後 `remaining` に何が残るか?

---

## §15.5  Critbit remove — 難しいやつ

水準の FIFO が空になると（`slab_pop_head_of_leaf` が最後のオーダを歩き出した）、葉は木から取り除かれねばならない。それは単に葉を free するだけではない — 直上の inner node は今や有効な子が 1 つしかなく、冗長になる。冗長な inner を超えて兄弟サブツリーを継ぎ上げる。

```rust
fn critbit_remove_leaf(tree: &mut CritbitTree, leaf_idx: u16) -> ProgramResult {
    // (1) エッジケース: 葉が root。
    if tree.root == leaf_idx {
        tree_free(tree, leaf_idx);
        tree.root = SLAB_NONE_INDEX;
        return Ok(());
    }

    // (2) 価格で歩き、親 + 祖父母を追跡する。
    let target_price = tree.nodes[leaf_idx].price;
    let mut grandparent = SLAB_NONE_INDEX;
    let mut parent = SLAB_NONE_INDEX;
    let mut gp_to_p_side = 0u8;
    let mut parent_side = 0u8;
    let mut cur = tree.root;
    loop {
        if cur == leaf_idx { break; }
        let node = &tree.nodes[cur];
        let bit = ((target_price >> node.split_bit) & 1) as u8;
        grandparent = parent;
        gp_to_p_side = parent_side;
        parent = cur;
        parent_side = bit;
        cur = if bit == 1 { node.right_child } else { node.left_child };
    }

    // (3) `parent` での兄弟は `parent_side` が取らなかった方。
    let parent_node = tree.nodes[parent];
    let sibling = if parent_side == 1 { parent_node.left_child } else { parent_node.right_child };

    // (4) `sibling` を `parent` がいた場所（祖父母の下、または root として）に配線。
    if grandparent == SLAB_NONE_INDEX {
        tree.root = sibling;
    } else if gp_to_p_side == 1 {
        tree.nodes[grandparent].right_child = sibling;
    } else {
        tree.nodes[grandparent].left_child = sibling;
    }

    tree_free(tree, parent);
    tree_free(tree, leaf_idx);
    Ok(())
}
```

これが機能する 4 つの不変条件。

**歩きは価格で、インデックスではない。** 取り除く葉は `tree.nodes` で安定したインデックスを持つが、root からそこへの**パス**はビットテストルーチンで降る。インデックスで歩くなら insert 時に親を覚える必要があり、状態が倍になる。価格で歩けば毎回 O(log N)、追加簿記なし。

**祖先 2 つが重要。** `parent` は葉の直上の inner; これが葉と一緒に削除される。`grandparent` は親の上の inner; **これ**の子ポインタが削除された親をバイパスするよう更新される必要がある。親が ROOT なら（祖父母なし）、`tree.root` を直接更新する。

**兄弟のサブツリーは保たれる。** そこに再帰せず、そのノードに触れず — 親の祖父母でのスロットを兄弟へ指すよう 1 つのポインタを書き換えるだけ。それが何だったか — 葉、inner、サブツリー全体 — そのまま 1 階層上がる。

**`tree_free` 呼び出し 2 つ。** 葉 AND 親。両方が木の free list に戻る。Remove の後の最初のアロケーションはどちらかを再利用する（おそらく親、free-list pop が LIFO だから）。

ステップ 1 の単葉 root ケースは唯一の「形が違う」分岐。それ以外は常に親がある（葉が root でないのは、最小限、この葉と少なくとも他の 1 つを識別する inner があるから）。

> **演習 §15.5.** Slab を作り、10 水準を価格 `[100, 200, 50, 150, 250, 75, 125, 175, 225, 275]` で insert せよ。木がどうなるか歩け（root、splits、葉）。次に 150 の水準をキャンセルせよ — どの inner が free され、どのサブツリーが上がるか追え。次に 250 を、次に 100 をキャンセル。3 つの remove の後、「無駄な」inner ノードはあるか?（ヒント: ないはず — remove は構築上常に親を畳む。）

---

## §15.6  3 つのハンドラ

`process_create_slab`（タグ 29）はブートストラップ。Market を検証し、`[b"slab", market]` で slab PDA を派生させ、System `create_account` 経由で `Slab::LEN` バイトをアロケートし、識別子 + bump + market を書き、両方の木で `tree_init` と OrderNode プールで `slab_init_pool` を走らせる。Init 歩きは命令の最重部分（~1k プールエントリ + 256 木エントリ × 2 side = ~1.5k イテレーション）; 以後の op は sub-log-N。

`process_slab_place_order`（タグ 30）は `[side u8][price u64 LE][size u64 LE]` を読む — `PlaceOrder` のペイロードと同一。Slab 識別子を検証し、ユーザの pubkey をコピーし、`slab_insert_order` を呼ぶ。アカウント 2 つ: `user (SIGNER)、slab (WRITE)`。第 7 章のフラット板 `process_place_order` と比較する価値がある: 同じペイロード、+0 アカウント。Slab の追加複雑性は完全にプログラム内にある; on-the-wire 命令形状は不変。これはクライアント側マイグレーションに効く: 既存のトレーディングクライアントはタグバイトとアカウント PDA 派生を変えるだけで `PlaceOrder` を `SlabPlaceOrder` に swap できる。

`process_slab_match`（タグ 31）は `[taker_side u8][price u64 LE][size u64 LE][max_fills u8]` を読む — 第 8 章の `Match` と同一。検証、上の fill ループ実行、`fills` + `remaining` をログしてページング cap を可視にする。第 8 章が導入した同じ `max_fills` セマンティクスがここでもそのまま適用される: slab は依然として CU で境界される、はるかに高い実効 N で。

CU 形状（概ね、ログオーバーヘッドについての通常の警告付き）:

| op             | フラット (ch.7/8) | slab (ch.15)  |
|----------------|---------------|---------------|
| place          | ~1 KCU        | ~3–4 KCU      |
| match (10 fill)| ~30 KCU       | ~15–20 KCU    |
| cancel         | ~1 KCU        | ~3–5 KCU      |

Slab は place ごと**より**高価だ、木歩き + node alloc が線形スキャンより多くのコードだから。Match は**より**安い、maker-find がフラット板のコストを支配するから。`Match` の crossover はおよそ `N ≈ 8` 付近 — それ以下では生 CU でフラット板が勝つ、それ以上では slab がますます大きなマージンで勝つ。

> **演習 §15.6.** `SlabCancelOrder` 命令を追加せよ。`(order_id, side, price)` を取り、特定の価格水準から特定のオーダを取り除く。ヒント: 構築したままの slab は `order_id` でオーダをインデックスしないので、正しいものを見つけるには水準の FIFO を歩く必要がある。`price` 引数が重要な理由は何か — それは木全体を歩くことに対して何を節約するか?

---

## §15.7  まとめと自己検証

### まとめ図

```
Slab 一瞥:

  Slab {
    discriminator: "SLAB\0\0\0\0"
    market: <market pubkey>
    next_order_id: u64
    active_count: u32
    pool_free_head: u16 ──┐
                          │
    bid_tree: CritbitTree {
      root: u16 ──────────┼─── nodes[] へのポインタ（または NONE_INDEX）
      free_head: u16 ─────┼─── free TreeNode スロットのスタック
      nodes: [TreeNode; 256]
              ┌──────────────────────────────┐
              │ INNER: split_bit, l_idx, r_idx
              │ LEAF:  price, head_node, tail_node, order_count
              │ FREE:  next_free
              └──────────────────────────────┘
    }
    ask_tree: CritbitTree { … 同じ形状 … }

    nodes: [OrderNode; 1024]  ◄── 共有 OrderNode プール
            ┌─────────────────────────┐
            │ order_id, owner, size,  │
            │ next, prev              │  (FIFO リンクまたは free-list リンク)
            └─────────────────────────┘
  }


価格 117、サイズ 5、BID を Insert:
  pool_alloc_node() → idx_O
  nodes[idx_O] = OrderNode{order_id, owner, size=5, next=NONE, prev=NONE}
  critbit_find_or_create_leaf(bid_tree, 117) → (idx_L, was_new)
  if was_new:
    bid_tree.nodes[idx_L].{head_node, tail_node} = idx_O
    bid_tree.nodes[idx_L].order_count = 1
  else:
    old_tail = bid_tree.nodes[idx_L].tail_node
    bid_tree.nodes[idx_L].tail_node = idx_O
    nodes[old_tail].next = idx_O
    nodes[idx_O].prev   = old_tail


BID-taker、limit=110、size=25、max_fills=3 を Match:
  critbit_find_best(ask_tree, want_max=false) 経由で best ask 葉
  FIFO 先頭を pop:
    head.size <= remaining なら: pop_head_of_leaf()（葉を取り除くかも）
    そうでないなら:               head.size を in-place で縮める
  !crosses または remaining=0 または fills==3 まで繰り返す
```

### 自分で検証する 3 項目

1. **プールが優雅に枯渇する。** 異なる価格で 1024 オーダを insert せよ。1025 番目を試せ — `pool_alloc_node` が `AccountDataTooSmall`（プールスロットなし）で失敗し、place が失敗し、`active_count` が 1024 に留まるはず。
2. **枯れた水準がその葉を綺麗に取り除く。** 価格 100 で 2 オーダを insert（同じ水準）。`SlabMatch` で両方 cross-match。Slab dump はアクティブオーダ 0 を、bid 100 の葉はなくなったことを示すはず（`describe_best` は `(empty)` を返す）。木の `free_head` は進んでいるはず（葉とその親 inner が free list に戻った — 100 が唯一の水準だった場合を除く。その場合は葉だけが戻り親は存在しなかった）。
3. **`active_count` 不変条件。** Place + match + cancel の任意のシーケンス後、`active_count` は `nodes[i].size > 0` の実際の count（ライブオーダ）に等しいはず。Slab dump が `active_count` を直接 print する; プール歩きでカウントするのは追加スクリプトパス 1 つ。ここで乖離を見つけることはどこかでカウンタが更新されなかったことを意味する — 潜在バグ。

---

## ここにない（意図的に）

これは実装章であり比較章ではない。書かなかったもの:

- 異なる N で slab vs flat の真っ向対決ベンチマーク。N ≈ 8 でのコスト crossover は本物; 正確に測るのはフォローアップ。
- Serum の slab vs Phoenix の slab vs 本書のサイドバイサイド。両方の本番 slab は本書が持たない機能を持つ（Serum のオーダブックは cancel-on-disconnect タグを保存する; Phoenix は独自の market-maker reservation スロットを持つ）、公平な比較はそれ自体の章になる。
- フラット板から slab 板への移行ガイド。2 つのアカウント型は独立 — オンチェーン変換はない。`OrderBook`（第 7 章）の上に建てた market と `Slab`（本章）の上に建てた market は同じプログラムで共存できる。

ここまで来てもっと欲しいなら、§8.4 の擬似コードの「3-4 日の練習」フレーミングは正確だった: 日々を使い、バリアントを書き、結果を測れ。
