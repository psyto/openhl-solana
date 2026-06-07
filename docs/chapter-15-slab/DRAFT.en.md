# Chapter 15 — Slab Order Book

> Status: draft (v0.1).
> Companion code: [`crates/state/src/lib.rs`](../../crates/state/src/lib.rs) (`Slab`, `OrderNode`, `CritbitTree`, `TreeNode`), [`programs/openhl-core/src/lib.rs`](../../programs/openhl-core/src/lib.rs) (slab helpers + `process_create_slab`, `process_slab_place_order`, `process_slab_match`), [`scripts/slab/src/main.rs`](../../scripts/slab/src/main.rs).

---

## §15.0  Framing — what the slab buys, and why it deserved its own chapter

Chapter 7 shipped a flat-array `OrderBook` — 32 slots, linear-scan for an empty slot on insert, linear-scan for a price-match in `Match`. Chapter 8 measured the cost: a `K = 10` fill loop against a `N = 32` book runs ~`O(K × N)` and spends ~30 KCU just on book bookkeeping before any settlement runs. That works *as a teaching baseline*. It does not work as a production book — the scan dominates for any realistic depth.

The Hyperliquid and Serum / Phoenix answer is a **slab**: a self-balancing tree over price levels, with each leaf holding a FIFO of orders at that price. Insert is O(log N) by price level. Best-price is O(log N) — walk root toward the side you want. Pop-the-best is O(log N) in the worst case (a level empties and the leaf has to be removed; an inner node above it goes redundant and collapses with it). The §8.4 pseudocode promised this chapter; here it is.

The chapter ships three new instructions, parallel to Chapter 7's `CreateOrderBook` / `PlaceOrder` / `Match`:

1. **`CreateSlab`** (tag 29) — allocates the per-market `Slab` PDA and initializes the OrderNode pool's free list plus both critbit trees' free lists.
2. **`SlabPlaceOrder`** (tag 30) — same payload as `PlaceOrder`. Runs the critbit insert, allocates an OrderNode from the pool, threads it onto the price level's FIFO.
3. **`SlabMatch`** (tag 31) — same payload as `Match` (taker side + limit price + size + max_fills cap). Walks the best leaf on the maker side, fills against its FIFO head, removes drained levels, applies the §8.3 pagination cap.

The flat-array `OrderBook` and the slab `Slab` coexist in the program. Different markets can choose different books; existing markets aren't disturbed. The chapter is about the *new* path.

Three things this chapter is *not* about:

- **It is not a Match-engine surgery chapter.** The slab's `SlabMatch` runs the same fill-loop shape as Chapter 8 — the change is in how the maker side is reached (O(log N) instead of O(N)), not in how a fill is accounted. Settlement, fees, builder splits — none of that lives here.
- **It is not a guide to every critbit variant.** Serum's slab packs leaves and inners into the same node pool with discriminator bits; Phoenix uses a slightly different layout with a separate per-level FIFO accounting struct. Ours follows §8.4's split — one OrderNode pool, one TreeNode pool per side — because the split makes the pedagogical argument cleanest. Other layouts trade a few hundred bytes of header for slightly faster access.
- **It is not a benchmark.** Chapter 8 measured the flat book; benchmarking the slab against it under realistic depth (>100 levels) belongs in a follow-up. The pseudocode in §8.4 predicts the slab wins on every dimension above N ≈ 8.

---

## §15.1  The four account types

From `crates/state/src/lib.rs`:

```rust
pub struct OrderNode {                          // 64 bytes
    pub order_id: u64,
    pub owner: [u8; 32],
    pub size: u64,
    pub next: u16,        // next in FIFO (or in free list)
    pub prev: u16,
    pub _pad: [u8; 12],
}

pub struct TreeNode {                           // 32 bytes — tagged union
    pub tag: u8,          // 0 FREE, 1 INNER, 2 LEAF
    pub split_bit: u8,    // INNER only
    pub next_free: u16,   // FREE only
    pub left_child: u16,  // INNER only
    pub right_child: u16, // INNER only
    pub head_node: u16,   // LEAF only (FIFO head order_node idx)
    pub tail_node: u16,   // LEAF only
    pub order_count: u32, // LEAF only (observability)
    pub price: u64,       // LEAF only
    pub _pad: [u8; 8],
}

pub struct CritbitTree {                        // 8 + 256 × 32 = 8208 bytes
    pub root: u16,                              // SLAB_NONE_INDEX if empty
    pub free_head: u16,
    pub _pad: [u8; 4],
    pub nodes: [TreeNode; SLAB_TREE_CAPACITY],  // SLAB_TREE_CAPACITY = 256
}

pub struct Slab {                               // ~82 KiB total
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

Five design choices to absorb:

**1. `u16` everywhere, never `Option<u16>`.** Pod safety. The sentinel `SLAB_NONE_INDEX = u16::MAX` stands in for "no index." Free-list terminators, empty-tree roots, and dangling FIFO endpoints all use it. With `u16` we can address up to 65,535 entries — `SLAB_POOL_CAPACITY = 1024` is well within that.

**2. Tagged union via `tag` byte, no enum.** `TreeNode` could in principle be a Rust enum (`Free`, `Inner { ... }`, `Leaf { ... }`). It can't, because enums aren't `Pod`. So we lay out all the fields any variant might need, and the `tag` byte tells consumers which fields to read. Unused fields hold zero / `SLAB_NONE_INDEX`. The discipline trade is real — a stale `head_node` in a freshly-freed node is a latent bug — but the layout pays off in zero-copy bytemuck access.

**3. Two trees per slab, not one.** Bid and ask trees are structurally identical; the only thing that differs is the "best price" walk direction (max vs min). Keeping them separate means the bid-side insert never traverses ask state and vice versa, which matters for Sealevel-style parallelism: a market with bids-only churn would still write the ask tree if they shared a root, serializing what could be independent.

**4. One shared OrderNode pool.** `Slab.nodes` is a single array. Both trees draw FIFOs from the same pool. The alternative (per-side pool) would double the OrderNode capacity per side for the same total footprint, but it would also mean that a one-sided book (e.g., almost all bids during a one-way move) would exhaust one pool while the other sat empty. Sharing keeps the slab usable across asymmetric flow.

**5. Account size = ~82 KiB.** Computed: `64 (header) + 2 × 8208 (trees) + 1024 × 64 (pool) = 82_064` bytes. That's well over Solana's "10 KiB" CPI realloc soft limit, but Solana's `create_account` CPI is bounded by the much larger `MAX_PERMITTED_DATA_LENGTH = 10 MiB`. `CreateSlab` allocates the whole thing in one shot via `system_instruction::create_account`. The rent is a one-time cost the market pays at bootstrap.

> **Exercise §15.1.** Compute the exact `Slab::LEN` from the struct layout (don't trust the comment). Verify with `bytemuck::bytes_of(&Slab::zeroed()).len()`. Now compute the rent in lamports at `lamports_per_byte_year = 3_480` and `years = 2` (Solana's two-year rent exemption). That's roughly what `CreateSlab` charges.

---

## §15.2  The pool + free list

`Slab.nodes` is a flat array of 1024 OrderNodes. At any moment some are "live" (part of a FIFO at some price level), some are "free" (waiting to be allocated). Free nodes form a singly-linked stack via their `next` field; `Slab.pool_free_head` is the top of the stack. Allocation pops `pool_free_head`; deallocation pushes.

From `programs/openhl-core/src/lib.rs`:

```rust
fn slab_init_pool(slab: &mut Slab) {
    for i in 0..SLAB_POOL_CAPACITY {
        slab.nodes[i].next = if i + 1 < SLAB_POOL_CAPACITY {
            (i as u16) + 1
        } else {
            SLAB_NONE_INDEX
        };
        // ... zero the other fields ...
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
    // ... zero the other fields ...
    slab.pool_free_head = idx;
}
```

Three details worth flagging.

**The init walks the array once.** It costs `O(N)` at bootstrap (~1k iterations). That cost is paid in `CreateSlab` which runs at most once per market. Every subsequent op is O(log N) or O(1).

**Free fields stay zeroed.** When a node is freed, we don't just push it on the list — we also clear `size`, `order_id`, and `owner`. The cost is small (a few stores), and it means a stale `owner` field can't leak into a future allocation by mistake. This matters because we're reusing the same memory for many orders' lifetimes.

**Failure is `AccountDataTooSmall`.** When the pool exhausts, we return an error rather than overwriting some existing node. `1024 nodes` is large for a single market; if you hit it, the answer is to either (a) batch-cancel old orders to free nodes, or (b) ship a larger `Slab` variant. The chapter ships the conservative answer.

The two per-tree pools (`bid_tree.nodes` and `ask_tree.nodes`, both `[TreeNode; 256]`) work identically. `tree_init` walks each one once; `tree_alloc` and `tree_free` follow the same pop/push pattern via the `next_free` field.

> **Exercise §15.2.** Implement an `available_count(slab) -> u32` helper that walks the free list and counts. Run it before and after a sequence of inserts + cancels; the invariant `live + free == SLAB_POOL_CAPACITY` should hold. Free-list corruption (a node appearing twice on the list, or a cycle) is the single hardest class of slab bug — instrumenting `available_count` is the cheapest way to catch it.

---

## §15.3  Critbit insert

The critbit algorithm walks a tree where each inner node tests one specific bit of the key. The bit tested at a deeper inner node is *less* significant — more significant bits are decided closer to the root.

**Find or create a leaf for `price`.** From `critbit_find_or_create_leaf`:

```rust
// (1) Empty tree → create root leaf.
if tree.root == SLAB_NONE_INDEX {
    let leaf_idx = tree_alloc(tree)?;
    tree.nodes[leaf_idx] = TreeNode { tag: LEAF, price, ... };
    tree.root = leaf_idx;
    return Ok((leaf_idx, true));
}

// (2) Walk to any leaf to compare prices.
let found_leaf = critbit_walk_to_leaf(tree, price);  // descend by (price >> split_bit) & 1
let found_price = tree.nodes[found_leaf].price;
if found_price == price {
    return Ok((found_leaf, false));  // existing level
}

// (3) New price — splice in a new inner + new leaf.
let diff = price ^ found_price;
let shared_bit = 63 - diff.leading_zeros() as u8;  // highest bit where prices differ

// Walk again from root looking for the spot to splice.
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

// (4) Allocate new leaf + new inner; the new inner splits at `shared_bit`.
let new_leaf_idx = tree_alloc(tree)?;
tree.nodes[new_leaf_idx] = TreeNode { tag: LEAF, price, ... };
let new_inner_idx = tree_alloc(tree)?;
let price_bit = ((price >> shared_bit) & 1) as u8;
let (left, right) = if price_bit == 1 { (cur, new_leaf_idx) } else { (new_leaf_idx, cur) };
tree.nodes[new_inner_idx] = TreeNode { tag: INNER, split_bit: shared_bit, left_child: left, right_child: right, ... };

// (5) Wire into parent.
if parent == SLAB_NONE_INDEX {
    tree.root = new_inner_idx;
} else if parent_side == 1 {
    tree.nodes[parent].right_child = new_inner_idx;
} else {
    tree.nodes[parent].left_child = new_inner_idx;
}
```

Five things to absorb.

**Step 2 is "walk to *any* leaf." Not the right one.** Critbit's elegance is that walking by `(price >> split_bit) & 1` from root ends at *some* leaf — the one that shares the longest critical-bit prefix with `price`. That's the only leaf we need to compare against to find `shared_bit`. We don't have to enumerate; we don't have to compare against multiple leaves.

**`shared_bit = 63 - diff.leading_zeros()`.** Standard trick: the highest set bit of `diff` is at position `63 - leading_zeros` (for `u64`). Since `diff != 0` (we ruled out the equal-prices case), `leading_zeros` is in `0..=63`.

**Step 3's walk stops at the right invariant.** We splice when we hit (a) a leaf, or (b) an inner node whose `split_bit < shared_bit`. Both cases mean: the existing tree doesn't have an inner discriminating at `shared_bit` on this path, so the new inner should go here. The invariant is maintained: at any inner with `split_bit >= shared_bit`, both `price` and `found_price` would agree at that bit, so we can keep descending.

**Step 4's `price_bit` decides which side the new leaf goes on.** If `price` has bit `shared_bit` set, it goes right; otherwise left. The existing subtree (`cur`) takes the other side. Why? Because at `shared_bit`, `price` and `found_price` differ — so they belong on opposite sides of the new inner.

**Step 5 is the only "wiring" step.** Everything before just allocated and filled in nodes. The actual tree connection is one parent's `left_child` or `right_child` pointer (or `tree.root`). Easy to get wrong; easy to test for.

The handler `process_slab_place_order` then attaches the new OrderNode (already allocated and initialized) to the leaf's FIFO. If the leaf is brand new (`head_node == SLAB_NONE_INDEX`), the new order becomes both head and tail. If it's existing, the new order becomes the new tail; the old tail's `next` and the new node's `prev` are linked.

> **Exercise §15.3.** Insert prices in three different orders: `[100, 200, 150]`, `[200, 100, 150]`, `[150, 100, 200]`. Walk the resulting trees by hand (or with a debug print). All three should produce trees that yield `[100, 150, 200]` when iterated in ascending order — but the internal structure (which inner is the root, which sides leaves are on) will differ. That's fine: critbit is *order-independent* in its leaves, not its internal shape.

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

The whole algorithm is "walk root toward your side." For bids we want the highest price, so we always go right. For asks we want the lowest, so we always go left. The tree's invariant — right-side leaves have a 1 at `split_bit`, left-side have a 0 — combined with descending from highest split_bit to lowest, guarantees this monotonically arrives at the extremum.

`SlabMatch` then runs:

```rust
while fills < max_fills && remaining > 0 {
    let (leaf_idx, head_idx, level_price) = slab_peek_best(slab, want_max)?;
    if !crosses(level_price, limit_price) { break; }

    let maker_size = slab.nodes[head_idx].size;
    let fill = remaining.min(maker_size);

    if fill == maker_size {
        slab_pop_head_of_leaf(slab, maker_side, leaf_idx)?;  // may drop the leaf
    } else {
        slab.nodes[head_idx].size = maker_size - fill;
    }

    remaining -= fill;
    fills += 1;
}
```

This is structurally the same fill-loop shape as Chapter 8's flat-book `Match` — taker has a `remaining`, walk the maker side from best, fill until either `remaining == 0`, the next maker doesn't cross the limit, or the `max_fills` pagination cap kicks in. What's different is that finding the maker is O(log N) instead of O(N), and popping the maker is O(log N) instead of O(1) (the trade-off lives in `slab_pop_head_of_leaf` when the level drains).

The `crosses` predicate (inline above) is the limit-price gate:

- **BID taker:** willing to buy at most `limit_price` → cross asks where `level_price <= limit_price`.
- **ASK taker:** willing to sell at least `limit_price` → cross bids where `level_price >= limit_price`.

When the gate stops crossing (the maker's price is no longer favorable to the taker), the match returns with `remaining > 0`. The taker can then post the unfilled portion as a maker if they want — that's a separate `SlabPlaceOrder` call from the same caller.

> **Exercise §15.4.** Insert ten asks at prices 100, 101, 102, ..., 109. Send a BID taker with `limit_price = 105`, `size = 25`, `max_fills = 10`. Trace the fills: which prices are hit, in what order, how many fills does the taker take before hitting the limit gate, what's left of `remaining` after the match returns?

---

## §15.5  Critbit remove — the hard one

When a level's FIFO empties (`slab_pop_head_of_leaf` walked the last order off), the leaf has to come out of the tree. That's not just freeing the leaf — the inner node directly above it now has only one valid child, which makes it redundant. We splice the sibling subtree up past the redundant inner.

```rust
fn critbit_remove_leaf(tree: &mut CritbitTree, leaf_idx: u16) -> ProgramResult {
    // (1) Edge case: leaf is the root.
    if tree.root == leaf_idx {
        tree_free(tree, leaf_idx);
        tree.root = SLAB_NONE_INDEX;
        return Ok(());
    }

    // (2) Walk by price, tracking parent + grandparent.
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

    // (3) The sibling at `parent` is the side `parent_side` did NOT take.
    let parent_node = tree.nodes[parent];
    let sibling = if parent_side == 1 { parent_node.left_child } else { parent_node.right_child };

    // (4) Wire `sibling` up where `parent` used to live (under grandparent, or as root).
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

Four invariants making this work.

**The walk is by price, not by index.** The leaf we're removing has a stable index in `tree.nodes`, but the *path* to it from root descends by the bit-test routine. Walking by index would require remembering parents during insert, doubling state. Walking by price is O(log N) every time, no extra bookkeeping.

**Two ancestors matter.** `parent` is the inner directly above the leaf; that's the one being deleted along with the leaf. `grandparent` is the inner above the parent; *that* is the one whose child pointer needs updating to bypass the deleted parent. If parent IS the root (no grandparent), we update `tree.root` directly.

**The sibling's subtree is preserved.** We don't recurse into it, we don't touch its nodes — we just rewire one pointer (parent's slot in grandparent) to point at the sibling. Whatever it was — a leaf, an inner, a whole subtree — moves up one level intact.

**Two `tree_free` calls.** The leaf AND the parent. Both go back on the tree's free list. The first allocation after a remove will reuse one of them (probably the parent, since free-list pops are LIFO).

The single-leaf root case in step 1 is the only "shape-different" branch. For everything else there's always a parent (the leaf isn't the root because at minimum we have an inner discriminating between this leaf and at least one other).

> **Exercise §15.5.** Build a slab, insert ten levels at prices `[100, 200, 50, 150, 250, 75, 125, 175, 225, 275]`. Walk what the tree looks like (root, splits, leaves). Now cancel the level at 150 — trace which inner gets freed and which subtree moves up. Now cancel at 250. Now at 100. After three removes, are there any "wasted" inner nodes? (Hint: there shouldn't be — the remove always collapses the parent, by construction.)

---

## §15.6  The three handlers

`process_create_slab` (tag 29) is the bootstrap. It validates the market, derives the slab PDA at `[b"slab", market]`, allocates `Slab::LEN` bytes via System `create_account`, writes the discriminator + bump + market, and runs `tree_init` on both trees plus `slab_init_pool` on the OrderNode pool. The init walk is the heaviest part of the instruction (~1k pool entries + 256 tree entries × 2 sides = ~1.5k iterations); subsequent ops are sub-log-N.

`process_slab_place_order` (tag 30) reads `[side u8][price u64 LE][size u64 LE]` — identical to `PlaceOrder`'s payload. Validates the slab discriminator, copies the user's pubkey, calls `slab_insert_order`. Two accounts: `user (SIGNER), slab (WRITE)`. Worth comparing to ch.7's flat-book `process_place_order`: same payload, +0 accounts. The slab's added complexity is entirely inside the program; the on-the-wire instruction shape is unchanged. That matters for client-side migration: existing trading clients can swap `PlaceOrder` for `SlabPlaceOrder` by changing the tag byte and the account PDA derivation.

`process_slab_match` (tag 31) reads `[taker_side u8][price u64 LE][size u64 LE][max_fills u8]` — identical to ch.8's `Match`. Validates, runs the fill loop above, logs `fills` + `remaining` so the pagination cap is visible. The same `max_fills` semantics ch.8 introduced apply here unchanged: the slab is still bounded by CU, just at a much higher effective N.

The CU shapes (rough envelope, with all the usual caveats about logging overhead):

| op             | flat (ch.7/8) | slab (ch.15)  |
|----------------|---------------|---------------|
| place          | ~1 KCU        | ~3–4 KCU      |
| match (10 fill)| ~30 KCU       | ~15–20 KCU    |
| cancel         | ~1 KCU        | ~3–5 KCU      |

The slab is *more* expensive per place because the tree walk + node alloc is more code than a linear scan. It's *less* expensive on match because the maker-find dominates the flat book's cost. The crossover for `Match` is around `N ≈ 8` — below that, the flat book wins on raw CU; above, the slab wins by an ever-increasing margin.

> **Exercise §15.6.** Add a `SlabCancelOrder` instruction that takes `(order_id, side, price)` and removes a specific order from a specific price level. Hint: the slab as built doesn't index orders by `order_id`, so you'll need to walk the level's FIFO to find the right one. Why is the `price` argument important — what does it save you over walking the whole tree?

---

## §15.7  Recap + verify yourself

### Recap diagram

```
A slab at a glance:

  Slab {
    discriminator: "SLAB\0\0\0\0"
    market: <market pubkey>
    next_order_id: u64
    active_count: u32
    pool_free_head: u16 ──┐
                          │
    bid_tree: CritbitTree {
      root: u16 ──────────┼─── pointer into nodes[] (or NONE_INDEX)
      free_head: u16 ─────┼─── stack of free TreeNode slots
      nodes: [TreeNode; 256]
              ┌──────────────────────────────┐
              │ INNER: split_bit, l_idx, r_idx
              │ LEAF:  price, head_node, tail_node, order_count
              │ FREE:  next_free
              └──────────────────────────────┘
    }
    ask_tree: CritbitTree { … same shape … }

    nodes: [OrderNode; 1024]  ◄── shared OrderNode pool
            ┌─────────────────────────┐
            │ order_id, owner, size,  │
            │ next, prev              │  (FIFO links, or free-list link)
            └─────────────────────────┘
  }


Insert price 117, size 5, BID:
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


Match BID-taker, limit=110, size=25, max_fills=3:
  best ask leaf via critbit_find_best(ask_tree, want_max=false)
  pop FIFO head:
    if head.size <= remaining: pop_head_of_leaf() (may remove leaf)
    else:                      shrink head.size in place
  repeat until !crosses or remaining=0 or fills==3
```

### Three things to verify yourself

1. **The pool exhausts gracefully.** Insert 1024 orders at distinct prices. Try a 1025th — `pool_alloc_node` should fail with `AccountDataTooSmall` (no pool slot left), the place should fail, and `active_count` should stay at 1024.
2. **A drained level removes its leaf cleanly.** Insert two orders at price 100 (same level). Cross-match both via `SlabMatch`. The slab dump should show 0 active orders, the bid leaf at 100 should be gone (`describe_best` returns `(empty)`), and the tree's `free_head` should have advanced (a leaf and its parent inner went back on the free list — except when 100 was the only level, in which case only the leaf returns and the parent didn't exist).
3. **`active_count` invariant.** After any sequence of place + match + cancel, `active_count` should equal the actual count of `nodes[i].size > 0` (i.e., live orders). The slab dump prints `active_count` directly; counting from the pool walk takes one extra script pass. Catching a divergence here means a counter wasn't updated somewhere — a latent bug.

---

## What's not here (intentionally)

This is an implementation chapter, not a comparison chapter. We didn't write:

- A heads-up benchmark of slab vs flat at varying N. The cost crossover at N ≈ 8 is real; measuring it precisely is a follow-up.
- A side-by-side of Serum's slab vs Phoenix's slab vs ours. Both production slabs have features ours doesn't (Serum's order book stores cancel-on-disconnect tags; Phoenix has its own market-maker reservation slots), and a fair comparison would be its own chapter.
- A migration guide from flat books to slab books. The two account types are independent — there's no on-chain conversion. A market built on `OrderBook` (Chapter 7) and a market built on `Slab` (this chapter) can coexist on the same program.

If you got this far and want more, the §8.4 pseudocode's "3-4 day exercise" framing was accurate: spend the days, write the variants, measure the results.
