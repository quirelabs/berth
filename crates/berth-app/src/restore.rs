//! Turning a tab's pane tree into the saved layout and back (D46).
//!
//! Pure, like `panes` and `sidebar`: the file format lives in `berth-core`,
//! which knows nothing of pane ids, and this is the translation between the
//! two. What can go wrong is a pane landing in the wrong half of a split, and
//! that is a property of the trees.

use berth_core::restore::Layout;

use crate::panes::{Axis, Node};

/// A pane tree as a saved layout, with each pane replaced by its position in
/// `order`. `None` if the tree holds a pane that is not in `order`.
pub fn layout_of(node: &Node, order: &[u64]) -> Option<Layout> {
    Some(match node {
        Node::Leaf(id) => Layout::Pane(order.iter().position(|o| o == id)?),
        Node::Split {
            axis,
            share,
            first,
            second,
        } => Layout::Split {
            down: *axis == Axis::Vertical,
            share: *share,
            first: Box::new(layout_of(first, order)?),
            second: Box::new(layout_of(second, order)?),
        },
    })
}

/// A saved layout as a pane tree, with each index replaced by the new pane id
/// at that position. `None` if an index has no id.
pub fn node_of(layout: &Layout, ids: &[u64]) -> Option<Node> {
    Some(match layout {
        Layout::Pane(index) => Node::leaf(*ids.get(*index)?),
        Layout::Split {
            down,
            share,
            first,
            second,
        } => Node::Split {
            share: *share,
            axis: if *down {
                Axis::Vertical
            } else {
                Axis::Horizontal
            },
            first: Box::new(node_of(first, ids)?),
            second: Box::new(node_of(second, ids)?),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested() -> Node {
        let mut node = Node::leaf(10);
        node.split(10, Axis::Horizontal, 20);
        node.split(20, Axis::Vertical, 30);
        node
    }

    /// Saved with one set of ids and restored with another, the shape and the
    /// order of the panes in it hold.
    #[test]
    fn a_nested_split_comes_back_in_the_same_shape() {
        let node = nested();
        let order = node.panes();
        let layout = layout_of(&node, &order).expect("every pane is in the order");
        let restored = node_of(&layout, &[1, 2, 3]).expect("every index has an id");

        let mut expected = Node::leaf(1);
        expected.split(1, Axis::Horizontal, 2);
        expected.split(2, Axis::Vertical, 3);
        assert_eq!(restored, expected);
    }

    #[test]
    fn side_by_side_is_not_stacked() {
        let mut node = Node::leaf(1);
        node.split(1, Axis::Horizontal, 2);
        assert!(matches!(
            layout_of(&node, &[1, 2]),
            Some(Layout::Split { down: false, .. })
        ));
    }

    #[test]
    fn a_pane_missing_from_the_order_is_refused() {
        assert_eq!(layout_of(&nested(), &[10, 20]), None);
    }

    #[test]
    fn an_index_with_no_id_is_refused() {
        let layout = layout_of(&nested(), &[10, 20, 30]).expect("complete");
        assert_eq!(node_of(&layout, &[1, 2]), None);
    }
}
