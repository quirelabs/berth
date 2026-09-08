//! The pane tree behind a tab.
//!
//! Pure structure: pane ids in, pane ids out, no gpui and no sessions. A split
//! layout is the kind of thing that goes subtly wrong when a pane is removed
//! from the middle of a nest, and that is a property of the tree rather than of
//! the terminals hanging off it, so it is tested on its own.

/// Which way a split divides its two children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Side by side, divided by a vertical line.
    Horizontal,
    /// Stacked, divided by a horizontal line.
    Vertical,
}

/// A tab's panes. Binary, because every split is one pane becoming two.
///
/// There is no ratio here: splits are even. Dragging a divider is a real thing
/// to want and it is deliberately not in this version, because a ratio that
/// cannot be dragged is a field nobody can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Leaf(u64),
    Split {
        axis: Axis,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    pub fn leaf(id: u64) -> Self {
        Node::Leaf(id)
    }

    /// Every pane in the tree, left to right, top to bottom.
    ///
    /// The order is the reading order of the layout, which is what makes it
    /// usable for "focus the next pane".
    pub fn panes(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        match self {
            Node::Leaf(leaf) => *leaf == id,
            Node::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// Turn one pane into a split of itself and `other`, in place.
    ///
    /// Returns false when `id` is not in this tree, so a caller splitting the
    /// wrong tab is a no-op rather than a panic or a pane grafted somewhere
    /// nobody asked for.
    pub fn split(&mut self, id: u64, axis: Axis, other: u64) -> bool {
        match self {
            Node::Leaf(leaf) if *leaf == id => {
                *self = Node::Split {
                    axis,
                    first: Box::new(Node::Leaf(id)),
                    second: Box::new(Node::Leaf(other)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                first.split(id, axis, other) || second.split(id, axis, other)
            }
        }
    }

    /// Drop a pane, collapsing the split it was half of.
    ///
    /// `None` means nothing is left, which is how a tab learns it should close.
    /// The surviving sibling takes the split's whole place rather than being
    /// left as a half-width pane beside an empty one.
    pub fn without(self, id: u64) -> Option<Node> {
        match self {
            Node::Leaf(leaf) if leaf == id => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Split {
                axis,
                first,
                second,
            } => match (first.without(id), second.without(id)) {
                (None, None) => None,
                (Some(only), None) | (None, Some(only)) => Some(only),
                (Some(a), Some(b)) => Some(Node::Split {
                    axis,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
            },
        }
    }

    /// The pane after `id` in reading order, wrapping at the end.
    ///
    /// Wrapping rather than stopping, because a cycle key that stops at the
    /// last pane makes the user work out where they are in the tree first.
    pub fn next_after(&self, id: u64) -> Option<u64> {
        let panes = self.panes();
        let at = panes.iter().position(|p| *p == id)?;
        panes.get((at + 1) % panes.len()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_of(a: u64, b: u64) -> Node {
        let mut node = Node::leaf(a);
        assert!(node.split(a, Axis::Horizontal, b));
        node
    }

    #[test]
    fn a_lone_pane_is_the_whole_tree() {
        let node = Node::leaf(1);
        assert_eq!(node.panes(), vec![1]);
        assert!(node.contains(1));
        assert!(!node.contains(2));
    }

    #[test]
    fn splitting_puts_the_new_pane_second() {
        let node = split_of(1, 2);
        assert_eq!(node.panes(), vec![1, 2]);
    }

    #[test]
    fn splitting_a_pane_that_is_not_here_changes_nothing() {
        let mut node = Node::leaf(1);
        assert!(!node.split(99, Axis::Vertical, 2));
        assert_eq!(node, Node::leaf(1));
    }

    #[test]
    fn splitting_nests() {
        let mut node = split_of(1, 2);
        assert!(node.split(2, Axis::Vertical, 3));
        assert_eq!(node.panes(), vec![1, 2, 3]);
        assert!(node.contains(3));
    }

    /// The case that goes wrong quietly: removing from inside a nest must
    /// promote the sibling, not leave a split with one live half.
    #[test]
    fn removing_from_a_nest_promotes_the_sibling() {
        let mut node = split_of(1, 2);
        node.split(2, Axis::Vertical, 3);
        let node = node.without(2).expect("two panes are left");
        assert_eq!(node.panes(), vec![1, 3]);
        assert_eq!(
            node,
            Node::Split {
                axis: Axis::Horizontal,
                first: Box::new(Node::Leaf(1)),
                second: Box::new(Node::Leaf(3)),
            },
            "the nested split should have collapsed, not kept an empty half"
        );
    }

    #[test]
    fn removing_the_last_pane_leaves_nothing() {
        assert_eq!(Node::leaf(1).without(1), None);
    }

    #[test]
    fn removing_a_pane_that_is_not_here_changes_nothing() {
        let node = split_of(1, 2);
        assert_eq!(node.clone().without(99), Some(node));
    }

    #[test]
    fn removing_down_to_one_leaves_a_bare_leaf() {
        let node = split_of(1, 2);
        assert_eq!(node.without(1), Some(Node::Leaf(2)));
    }

    #[test]
    fn a_deep_nest_survives_removal_from_the_middle() {
        let mut node = Node::leaf(1);
        node.split(1, Axis::Horizontal, 2);
        node.split(2, Axis::Vertical, 3);
        node.split(3, Axis::Horizontal, 4);
        assert_eq!(node.panes(), vec![1, 2, 3, 4]);

        let node = node.without(3).unwrap();
        assert_eq!(node.panes(), vec![1, 2, 4]);

        let node = node.without(1).unwrap();
        assert_eq!(node.panes(), vec![2, 4]);

        let node = node.without(2).unwrap();
        assert_eq!(node, Node::Leaf(4));
        assert_eq!(node.without(4), None);
    }

    #[test]
    fn focus_cycles_in_reading_order_and_wraps() {
        let mut node = split_of(1, 2);
        node.split(2, Axis::Vertical, 3);
        assert_eq!(node.next_after(1), Some(2));
        assert_eq!(node.next_after(2), Some(3));
        assert_eq!(node.next_after(3), Some(1));
    }

    #[test]
    fn cycling_from_a_lone_pane_stays_put() {
        assert_eq!(Node::leaf(1).next_after(1), Some(1));
    }

    #[test]
    fn cycling_from_a_pane_that_is_not_here_is_none() {
        assert_eq!(Node::leaf(1).next_after(99), None);
    }
}
