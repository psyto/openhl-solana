//! A zero-copy critbit order book that lives in one Solana account.
//!
//! The book is the `openhl_state::Slab` layout: two critbit trees over price
//! levels (bids and asks), each leaf a FIFO of `OrderNode`s drawn from a
//! shared fixed pool. Everything is indices into fixed arrays, so the same
//! bytes work as an on-chain account (`bytemuck::from_bytes_mut`) and as a
//! plain struct in a test or client.
//!
//! - [`init`]: set up both trees and the order-node pool.
//! - [`insert_order`]: rest an order, O(log N) by price level.
//! - [`best`]: the best level on one side and its oldest order.
//! - [`pop_head`]: remove a level's oldest order (and the level if empty).
//! - [`match_order`]: cross a taker against the book in price-time
//!   priority, at most `max_fills` fills.
//!
//! `no_std`, no allocation, no Solana types. The `openhl-core` program's
//! `CreateSlab`, `SlabPlaceOrder` and `SlabMatch` call these.

#![no_std]

use openhl_state::{
    side, tree_tag, CritbitTree, Slab, TreeNode, SLAB_NONE_INDEX, SLAB_POOL_CAPACITY,
    SLAB_TREE_CAPACITY,
};

/// Why a slab operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlabError {
    /// Every `OrderNode` in the pool is in use.
    PoolExhausted,
    /// Every `TreeNode` of that side's tree is in use.
    TreeExhausted,
    /// `price` or `size` was 0.
    InvalidOrder,
    /// A counter (`next_order_id`, `active_count`, a level's
    /// `order_count`) would overflow.
    Overflow,
    /// Walking the tree by price reached the wrong leaf.
    CorruptTree,
    /// The level has no orders.
    EmptyLevel,
}

// ---- Pool helpers ----------------------------------------------------------

/// Initialize the order-node free list: all `SLAB_POOL_CAPACITY` nodes start
/// free, linked via their `next` field.
fn init_pool(slab: &mut Slab) {
    for i in 0..SLAB_POOL_CAPACITY {
        slab.nodes[i].next = if i + 1 < SLAB_POOL_CAPACITY {
            (i as u16) + 1
        } else {
            SLAB_NONE_INDEX
        };
        slab.nodes[i].prev = SLAB_NONE_INDEX;
        slab.nodes[i].size = 0;
        slab.nodes[i].order_id = 0;
        slab.nodes[i].owner = [0u8; 32];
    }
    slab.pool_free_head = 0;
}

/// Initialize a critbit tree's tree-node free list.
fn tree_init(tree: &mut CritbitTree) {
    tree.root = SLAB_NONE_INDEX;
    for i in 0..SLAB_TREE_CAPACITY {
        tree.nodes[i].tag = tree_tag::FREE;
        tree.nodes[i].next_free = if i + 1 < SLAB_TREE_CAPACITY {
            (i as u16) + 1
        } else {
            SLAB_NONE_INDEX
        };
    }
    tree.free_head = 0;
}

/// Set up both trees and the order-node pool of a zeroed or reused slab.
///
/// The header (`discriminator`, `bump`, `market`, `next_order_id`,
/// `active_count`) is the caller's.
pub fn init(slab: &mut Slab) {
    tree_init(&mut slab.bid_tree);
    tree_init(&mut slab.ask_tree);
    init_pool(slab);
}

fn pool_alloc_node(slab: &mut Slab) -> Result<u16, SlabError> {
    let head = slab.pool_free_head;
    if head == SLAB_NONE_INDEX {
        return Err(SlabError::PoolExhausted);
    }
    let next = slab.nodes[head as usize].next;
    slab.pool_free_head = next;
    Ok(head)
}

fn pool_free_node(slab: &mut Slab, idx: u16) {
    let head = slab.pool_free_head;
    let node = &mut slab.nodes[idx as usize];
    node.next = head;
    node.prev = SLAB_NONE_INDEX;
    node.size = 0;
    node.order_id = 0;
    node.owner = [0u8; 32];
    slab.pool_free_head = idx;
}

fn tree_alloc(tree: &mut CritbitTree) -> Result<u16, SlabError> {
    let head = tree.free_head;
    if head == SLAB_NONE_INDEX {
        return Err(SlabError::TreeExhausted);
    }
    let next = tree.nodes[head as usize].next_free;
    tree.free_head = next;
    Ok(head)
}

fn tree_free(tree: &mut CritbitTree, idx: u16) {
    let head = tree.free_head;
    let node = &mut tree.nodes[idx as usize];
    *node = TreeNode {
        tag: tree_tag::FREE,
        split_bit: 0,
        next_free: head,
        left_child: SLAB_NONE_INDEX,
        right_child: SLAB_NONE_INDEX,
        head_node: SLAB_NONE_INDEX,
        tail_node: SLAB_NONE_INDEX,
        order_count: 0,
        price: 0,
        _pad: [0u8; 8],
    };
    tree.free_head = idx;
}

// ---- Critbit operations ---------------------------------------------------

/// Walk the tree following `(price >> split_bit) & 1` at each inner node,
/// stopping at the first leaf encountered. Returns the leaf index (which
/// may or may not have `price` as its key — caller compares).
fn critbit_walk_to_leaf(tree: &CritbitTree, price: u64) -> u16 {
    let mut cur = tree.root;
    loop {
        let node = &tree.nodes[cur as usize];
        if node.tag == tree_tag::LEAF {
            return cur;
        }
        // INNER
        let bit = (price >> node.split_bit) & 1;
        cur = if bit == 1 {
            node.right_child
        } else {
            node.left_child
        };
    }
}

/// Find an existing leaf with `price`, or create a new one (splicing as
/// needed). Returns the leaf index. The leaf's FIFO bookkeeping
/// (`head_node` / `tail_node` / `order_count`) is left to the caller.
fn critbit_find_or_create_leaf(
    tree: &mut CritbitTree,
    price: u64,
) -> Result<(u16, bool), SlabError> {
    // Empty tree → create root leaf.
    if tree.root == SLAB_NONE_INDEX {
        let leaf_idx = tree_alloc(tree)?;
        tree.nodes[leaf_idx as usize] = TreeNode {
            tag: tree_tag::LEAF,
            split_bit: 0,
            next_free: SLAB_NONE_INDEX,
            left_child: SLAB_NONE_INDEX,
            right_child: SLAB_NONE_INDEX,
            head_node: SLAB_NONE_INDEX,
            tail_node: SLAB_NONE_INDEX,
            order_count: 0,
            price,
            _pad: [0u8; 8],
        };
        tree.root = leaf_idx;
        return Ok((leaf_idx, true));
    }

    // Walk to any leaf to compare prices.
    let found_leaf = critbit_walk_to_leaf(tree, price);
    let found_price = tree.nodes[found_leaf as usize].price;
    if found_price == price {
        return Ok((found_leaf, false));
    }

    // New price — find the bit position to splice at, then splice.
    let diff = price ^ found_price;
    // 0-based highest set bit. diff != 0 because prices differ.
    let shared_bit: u8 = 63u8 - (diff.leading_zeros() as u8);

    // Walk again from root, looking for the spot to splice. We splice
    // either above the first inner whose split_bit < shared_bit, or
    // above any leaf we hit (whose split_bit is conceptually -1 < any).
    let mut parent: u16 = SLAB_NONE_INDEX;
    let mut parent_side: u8 = 0; // side of parent that cur lives at
    let mut cur = tree.root;
    loop {
        let node = &tree.nodes[cur as usize];
        if node.tag == tree_tag::LEAF || node.split_bit < shared_bit {
            break;
        }
        let bit = ((price >> node.split_bit) & 1) as u8;
        parent = cur;
        parent_side = bit;
        cur = if bit == 1 {
            node.right_child
        } else {
            node.left_child
        };
    }

    // Allocate the new leaf and the new inner.
    let new_leaf_idx = tree_alloc(tree)?;
    tree.nodes[new_leaf_idx as usize] = TreeNode {
        tag: tree_tag::LEAF,
        split_bit: 0,
        next_free: SLAB_NONE_INDEX,
        left_child: SLAB_NONE_INDEX,
        right_child: SLAB_NONE_INDEX,
        head_node: SLAB_NONE_INDEX,
        tail_node: SLAB_NONE_INDEX,
        order_count: 0,
        price,
        _pad: [0u8; 8],
    };
    let new_inner_idx = tree_alloc(tree)?;

    // Which side does `price` go at shared_bit? That side gets new_leaf_idx;
    // the other side keeps the existing subtree (cur).
    let price_bit = ((price >> shared_bit) & 1) as u8;
    let (left, right) = if price_bit == 1 {
        (cur, new_leaf_idx)
    } else {
        (new_leaf_idx, cur)
    };
    tree.nodes[new_inner_idx as usize] = TreeNode {
        tag: tree_tag::INNER,
        split_bit: shared_bit,
        next_free: SLAB_NONE_INDEX,
        left_child: left,
        right_child: right,
        head_node: SLAB_NONE_INDEX,
        tail_node: SLAB_NONE_INDEX,
        order_count: 0,
        price: 0,
        _pad: [0u8; 8],
    };

    // Wire the new inner into parent (or as new root).
    if parent == SLAB_NONE_INDEX {
        tree.root = new_inner_idx;
    } else if parent_side == 1 {
        tree.nodes[parent as usize].right_child = new_inner_idx;
    } else {
        tree.nodes[parent as usize].left_child = new_inner_idx;
    }

    Ok((new_leaf_idx, true))
}

/// Find the best leaf on the side dictated by `want_max`:
///   - `want_max = true`  (bids): walk right at every inner — highest price.
///   - `want_max = false` (asks): walk left at every inner — lowest price.
fn critbit_find_best(tree: &CritbitTree, want_max: bool) -> Option<u16> {
    if tree.root == SLAB_NONE_INDEX {
        return None;
    }
    let mut cur = tree.root;
    loop {
        let node = &tree.nodes[cur as usize];
        if node.tag == tree_tag::LEAF {
            return Some(cur);
        }
        cur = if want_max {
            node.right_child
        } else {
            node.left_child
        };
    }
}

/// Remove `leaf_idx` from the tree. Frees both the leaf and the inner above
/// it (the inner becomes redundant once a leaf is removed — its sibling
/// promotes up). Handles the root-leaf and single-leaf edge cases.
fn critbit_remove_leaf(tree: &mut CritbitTree, leaf_idx: u16) -> Result<(), SlabError> {
    // Edge case: leaf is the root.
    if tree.root == leaf_idx {
        tree_free(tree, leaf_idx);
        tree.root = SLAB_NONE_INDEX;
        return Ok(());
    }

    // Walk from root by price, tracking parent + grandparent.
    let target_price = tree.nodes[leaf_idx as usize].price;
    let mut grandparent: u16 = SLAB_NONE_INDEX;
    let mut parent: u16 = SLAB_NONE_INDEX;
    let mut gp_to_p_side: u8 = 0;
    let mut parent_side: u8 = 0; // which side of parent leads to cur
    let mut cur = tree.root;
    loop {
        if cur == leaf_idx {
            break;
        }
        let node = &tree.nodes[cur as usize];
        if node.tag != tree_tag::INNER {
            // Defensive: walking by price reached a non-target leaf — should
            // not happen with a well-formed tree.
            return Err(SlabError::CorruptTree);
        }
        let bit = ((target_price >> node.split_bit) & 1) as u8;
        grandparent = parent;
        gp_to_p_side = parent_side;
        parent = cur;
        parent_side = bit;
        cur = if bit == 1 {
            node.right_child
        } else {
            node.left_child
        };
    }

    // `parent` is the inner directly above the leaf. Its other child is the
    // sibling that promotes up.
    let parent_node = tree.nodes[parent as usize];
    let sibling = if parent_side == 1 {
        parent_node.left_child
    } else {
        parent_node.right_child
    };

    if grandparent == SLAB_NONE_INDEX {
        tree.root = sibling;
    } else if gp_to_p_side == 1 {
        tree.nodes[grandparent as usize].right_child = sibling;
    } else {
        tree.nodes[grandparent as usize].left_child = sibling;
    }
    tree_free(tree, parent);
    tree_free(tree, leaf_idx);
    Ok(())
}

// ---- Higher-level slab operations ----------------------------------------

/// Rest an order on `side` (`side::BID`; anything else is treated as
/// `side::ASK`). Allocates an `OrderNode`, finds or creates the level for
/// `price`, and appends the order to that level's FIFO. Returns the
/// assigned order id.
pub fn insert_order(
    slab: &mut Slab,
    side: u8,
    price: u64,
    owner: [u8; 32],
    size: u64,
) -> Result<u64, SlabError> {
    if size == 0 || price == 0 {
        return Err(SlabError::InvalidOrder);
    }

    // Alloc + init the order node.
    let order_idx = pool_alloc_node(slab)?;
    let order_id = slab.next_order_id;
    slab.next_order_id = slab
        .next_order_id
        .checked_add(1)
        .ok_or(SlabError::Overflow)?;
    {
        let node = &mut slab.nodes[order_idx as usize];
        node.order_id = order_id;
        node.owner = owner;
        node.size = size;
        node.next = SLAB_NONE_INDEX;
        node.prev = SLAB_NONE_INDEX;
    }

    // Find or create the leaf, then attach.
    let tree = if side == side::BID {
        &mut slab.bid_tree
    } else {
        &mut slab.ask_tree
    };
    let (leaf_idx, _was_new) = critbit_find_or_create_leaf(tree, price)?;
    let old_tail_idx: u16;
    {
        let leaf = &mut tree.nodes[leaf_idx as usize];
        if leaf.head_node == SLAB_NONE_INDEX {
            leaf.head_node = order_idx;
            leaf.tail_node = order_idx;
            leaf.order_count = 1;
            slab.active_count = slab
                .active_count
                .checked_add(1)
                .ok_or(SlabError::Overflow)?;
            return Ok(order_id);
        }
        old_tail_idx = leaf.tail_node;
        leaf.tail_node = order_idx;
        leaf.order_count = leaf.order_count.checked_add(1).ok_or(SlabError::Overflow)?;
    }
    // Borrow on tree ends here; now touch the OrderNode pool.
    slab.nodes[old_tail_idx as usize].next = order_idx;
    slab.nodes[order_idx as usize].prev = old_tail_idx;
    slab.active_count = slab
        .active_count
        .checked_add(1)
        .ok_or(SlabError::Overflow)?;
    Ok(order_id)
}

/// The best price level on one side and the oldest order resting there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    /// Tree-node index of the level (pass to [`pop_head`]).
    pub leaf: u16,
    /// Index into `slab.nodes` of the level's oldest order.
    pub head: u16,
    pub price: u64,
}

/// The best level of the bid tree (`want_max = true`, highest price) or the
/// ask tree (`want_max = false`, lowest price), or `None` if that side is
/// empty.
pub fn best(slab: &Slab, want_max: bool) -> Option<Level> {
    let tree = if want_max {
        &slab.bid_tree
    } else {
        &slab.ask_tree
    };
    let leaf_idx = critbit_find_best(tree, want_max)?;
    let leaf = &tree.nodes[leaf_idx as usize];
    if leaf.head_node == SLAB_NONE_INDEX {
        return None;
    }
    Some(Level {
        leaf: leaf_idx,
        head: leaf.head_node,
        price: leaf.price,
    })
}

/// Remove the oldest order of level `leaf_idx` on `side`. If the level
/// becomes empty, also remove it from the tree. Returns the freed
/// order-node index.
pub fn pop_head(slab: &mut Slab, side: u8, leaf_idx: u16) -> Result<u16, SlabError> {
    let tree = if side == side::BID {
        &mut slab.bid_tree
    } else {
        &mut slab.ask_tree
    };

    let head_idx: u16;
    let became_empty: bool;
    {
        let leaf = &mut tree.nodes[leaf_idx as usize];
        head_idx = leaf.head_node;
        if head_idx == SLAB_NONE_INDEX {
            return Err(SlabError::EmptyLevel);
        }
        leaf.order_count = leaf.order_count.saturating_sub(1);
    }
    let new_head = slab.nodes[head_idx as usize].next;
    {
        let tree2 = if side == side::BID {
            &mut slab.bid_tree
        } else {
            &mut slab.ask_tree
        };
        let leaf = &mut tree2.nodes[leaf_idx as usize];
        leaf.head_node = new_head;
        if new_head == SLAB_NONE_INDEX {
            leaf.tail_node = SLAB_NONE_INDEX;
            became_empty = true;
        } else {
            became_empty = false;
        }
    }
    if new_head != SLAB_NONE_INDEX {
        slab.nodes[new_head as usize].prev = SLAB_NONE_INDEX;
    }
    if became_empty {
        let tree3 = if side == side::BID {
            &mut slab.bid_tree
        } else {
            &mut slab.ask_tree
        };
        critbit_remove_leaf(tree3, leaf_idx)?;
    }
    pool_free_node(slab, head_idx);
    slab.active_count = slab.active_count.saturating_sub(1);
    Ok(head_idx)
}

/// One fill reported by [`match_order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// 1-based fill number within this match.
    pub n: usize,
    pub price: u64,
    pub size: u64,
    /// Maker order id.
    pub maker_order_id: u64,
    /// Taker size still unfilled after this fill.
    pub remaining: u64,
}

/// What [`match_order`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchOutcome {
    pub fills: usize,
    /// Taker size left unfilled.
    pub remaining: u64,
}

/// Cross a taker of `taker_side` and `size` against the opposite side, best
/// price first and oldest first within a level, while the level price is
/// within `limit_price`. Stops after `max_fills` fills, when the taker is
/// filled, or when the next level does not cross. Calls `on_fill` after
/// each fill. Nothing rests: an unfilled remainder is only reported.
pub fn match_order(
    slab: &mut Slab,
    taker_side: u8,
    limit_price: u64,
    size: u64,
    max_fills: usize,
    mut on_fill: impl FnMut(&Fill),
) -> Result<MatchOutcome, SlabError> {
    // Maker side = the side we cross against. A BID taker (wants to buy)
    // takes from asks (min price); an ASK taker (wants to sell) takes from
    // bids (max price).
    let maker_side = if taker_side == side::BID {
        side::ASK
    } else {
        side::BID
    };
    let want_max = maker_side == side::BID; // crossing the bid tree → want highest

    let mut remaining = size;
    let mut fills: usize = 0;
    while fills < max_fills && remaining > 0 {
        let level = match best(slab, want_max) {
            Some(v) => v,
            None => break, // book empty on the maker side
        };

        // Limit-price gate. Taker is willing to:
        //   BID taker: buy up to limit_price → cross asks where level_price <= limit_price.
        //   ASK taker: sell down to limit_price → cross bids where level_price >= limit_price.
        let crosses = if taker_side == side::BID {
            level.price <= limit_price
        } else {
            level.price >= limit_price
        };
        if !crosses {
            break;
        }

        // How much can we take from this maker?
        let maker = slab.nodes[level.head as usize];
        let fill = remaining.min(maker.size);

        if fill == maker.size {
            // Maker fully consumed — pop it from the FIFO. May also drop
            // the leaf if this was the last order at this level.
            pop_head(slab, maker_side, level.leaf)?;
        } else {
            // Partial fill — maker stays, size shrinks.
            slab.nodes[level.head as usize].size = maker.size - fill;
        }

        remaining -= fill;
        fills = fills.saturating_add(1);
        on_fill(&Fill {
            n: fills,
            price: level.price,
            size: fill,
            maker_order_id: maker.order_id,
            remaining,
        });
    }

    Ok(MatchOutcome { fills, remaining })
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;
    use super::*;
    use alloc::{boxed::Box, vec::Vec};

    fn new_slab() -> Box<Slab> {
        let mut s: Box<Slab> = bytemuck::zeroed_box();
        init(&mut s);
        s
    }

    #[test]
    fn price_then_time_priority() {
        let mut s = new_slab();
        let a = insert_order(&mut s, side::ASK, 101, [1; 32], 5).unwrap();
        let b = insert_order(&mut s, side::ASK, 100, [2; 32], 5).unwrap();
        let c = insert_order(&mut s, side::ASK, 100, [3; 32], 5).unwrap();
        let mut seen = Vec::new();
        let out = match_order(&mut s, side::BID, 101, 12, 10, |f| {
            seen.push((f.maker_order_id, f.price, f.size))
        })
        .unwrap();
        assert_eq!(seen, [(b, 100, 5), (c, 100, 5), (a, 101, 2)]);
        assert_eq!(
            out,
            MatchOutcome {
                fills: 3,
                remaining: 0
            }
        );
        assert_eq!(s.active_count, 1);
        assert_eq!(best(&s, false).unwrap().price, 101);
    }

    #[test]
    fn limit_and_max_fills_stop_the_match() {
        let mut s = new_slab();
        for p in [100, 99, 98] {
            insert_order(&mut s, side::BID, p, [0; 32], 1).unwrap();
        }
        let out = match_order(&mut s, side::ASK, 99, 10, 10, |_| {}).unwrap();
        assert_eq!(
            out,
            MatchOutcome {
                fills: 2,
                remaining: 8
            }
        );
        insert_order(&mut s, side::BID, 97, [0; 32], 1).unwrap();
        let out = match_order(&mut s, side::ASK, 1, 10, 1, |_| {}).unwrap();
        assert_eq!(out.fills, 1);
        assert_eq!(best(&s, true).unwrap().price, 97);
    }

    #[test]
    fn drained_book_frees_everything() {
        let mut s = new_slab();
        // many levels (at most 100 distinct; a tree holds 128) and many
        // orders per level, to exercise splicing, FIFOs and removal
        let mut x: u64 = 12345;
        for _ in 0..200 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let price = 1 + (x >> 40) % 100;
            insert_order(&mut s, side::ASK, price, [0; 32], 1 + (x & 7)).unwrap();
        }
        let mut last = 0;
        let out = match_order(&mut s, side::BID, u64::MAX, u64::MAX, usize::MAX, |f| {
            assert!(f.price >= last, "asks come out lowest first");
            last = f.price;
        })
        .unwrap();
        assert_eq!(out.fills, 200);
        assert_eq!(s.active_count, 0);
        assert_eq!(s.ask_tree.root, SLAB_NONE_INDEX);
        // every node back on the free lists
        let mut n = 0;
        let mut i = s.pool_free_head;
        while i != SLAB_NONE_INDEX {
            n += 1;
            i = s.nodes[i as usize].next;
        }
        assert_eq!(n, SLAB_POOL_CAPACITY);
        let mut t = 0;
        let mut i = s.ask_tree.free_head;
        while i != SLAB_NONE_INDEX {
            t += 1;
            i = s.ask_tree.nodes[i as usize].next_free;
        }
        assert_eq!(t, SLAB_TREE_CAPACITY);
    }

    #[test]
    fn errors() {
        let mut s = new_slab();
        assert_eq!(
            insert_order(&mut s, side::BID, 0, [0; 32], 1),
            Err(SlabError::InvalidOrder)
        );
        // 256 tree nodes → at most 128 distinct levels per side
        let mut r = Ok(0);
        for p in 1..=200 {
            r = insert_order(&mut s, side::BID, p, [0; 32], 1);
            if r.is_err() {
                break;
            }
        }
        assert_eq!(r, Err(SlabError::TreeExhausted));
    }
}
