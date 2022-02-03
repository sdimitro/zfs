use more_asserts::*;

/// Binary Index Tree (aka Fenwick tree) that implements a suffix sum rather than
/// traditional prefix sum. Updates and sum operations occur in _O_(log n).
pub struct BinaryIndexTree {
    tree: Vec<u64>, // indexed by 1, node 0 is unused
}

impl BinaryIndexTree {
    /// Constructs a `BinaryIndexTree` using the `input` vector in _O_(n log n).
    /// Extra nodes can be added to anticipate future growth.
    pub fn new(input: &[u64], extra_nodes: Option<usize>) -> Self {
        let mut node_count = input.len() + 1;
        if let Some(i) = extra_nodes {
            // Initialize with extra nodes to minimize new_from() calls
            node_count += i;
        }
        let mut tree = BinaryIndexTree {
            tree: vec![0; node_count],
        };

        for (i, x) in input.iter().enumerate() {
            tree.insert_at(i, *x);
        }
        tree
    }

    pub fn len(&self) -> usize {
        self.tree.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.tree.len() == 1
    }

    /// Returns sum of all elements from the end to the given index. Sum is computed in _O_(log n).
    pub fn suffix_sum(&self, index: usize) -> u64 {
        let mut i = index + 1;
        let mut sum = 0;

        while i < self.tree.len() {
            sum += self.tree[i];
            i += i & (1 << i.trailing_zeros());
        }
        sum
    }

    /// Inserts a new `value` at an existing `index` in the `BinaryIndexTree` in _O_(log n).
    /// Note that `insert_at()` cannot be used to grow the tree.
    pub fn insert_at(&mut self, index: usize, value: u64) {
        let mut i = index + 1; // map 0-relative to 1-relative

        // We can only grow by rebuilding the tree! This is currently the responsibility
        // of the caller (who has the context of the histogram).
        assert_lt!(i, self.tree.len());

        while i > 0 {
            self.tree[i] += value;
            i -= i & (1 << i.trailing_zeros());
        }
    }

    /// Removes `value` at an existing `index` in the `BinaryIndexTree` in _O_(log n).
    pub fn remove_at(&mut self, index: usize, value: u64) {
        let mut i = index + 1; // map 0-relative to 1-relative

        while i > 0 {
            assert_ge!(self.tree[i], value);
            self.tree[i] -= value;
            i -= i & (1 << i.trailing_zeros());
        }
    }
}
