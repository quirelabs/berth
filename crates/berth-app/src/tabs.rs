//! Tabs as views (D53, "cursor"): a tab is a layout of panes, each showing
//! one session, from any host. A session is in at most one pane at a time,
//! since a terminal has one size, so putting a session somewhere it is not
//! moves it there. Closing a pane or a tab detaches; ending a session is a
//! separate action, and nothing here ends one.
//!
//! Pure structure, like `panes`: pane ids and tab ids in, a new layout out,
//! no gpui and no sessions, so the rules are tested on their own.

use crate::panes::{Axis, Node};

/// A tab: a tree of panes, and which one has the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub id: u64,
    pub root: Node,
    pub focus: u64,
}

/// The tab holding a pane.
pub fn tab_of(tabs: &[Tab], pane: u64) -> Option<usize> {
    tabs.iter().position(|t| t.root.contains(pane))
}

/// Take a pane out of whichever tab holds it, collapsing the split it was
/// half of. A tab left with nothing goes, and `active` keeps pointing at the
/// same tab, or the one before it when the active tab went.
pub fn remove_pane(tabs: &mut Vec<Tab>, active: &mut usize, pane: u64) {
    let Some(at) = tab_of(tabs, pane) else {
        return;
    };
    let tab = tabs.remove(at);
    match tab.root.without(pane) {
        Some(root) => {
            let focus = if tab.focus == pane {
                root.panes().first().copied().unwrap_or(tab.focus)
            } else {
                tab.focus
            };
            tabs.insert(at, Tab { root, focus, ..tab });
        }
        None if *active > at => *active -= 1,
        None => {}
    }
    if *active >= tabs.len() {
        *active = tabs.len().saturating_sub(1);
    }
}

/// Put `pane` beside `beside`, splitting it, and give it the keyboard: in a
/// tab of its own already or not, and moved out of whatever tab held it.
/// Nothing changes, and false comes back, when `beside` is in no tab or is
/// the pane itself.
pub fn put_beside(
    tabs: &mut Vec<Tab>,
    active: &mut usize,
    pane: u64,
    beside: u64,
    axis: Axis,
) -> bool {
    if pane == beside || tab_of(tabs, beside).is_none() {
        return false;
    }
    remove_pane(tabs, active, pane);
    let Some(at) = tab_of(tabs, beside) else {
        return false;
    };
    let tab = &mut tabs[at];
    tab.root.split(beside, axis, pane);
    tab.focus = pane;
    *active = at;
    true
}

/// A new tab holding just `pane`, shown when `show` or when it is the only
/// one.
pub fn open_tab(tabs: &mut Vec<Tab>, active: &mut usize, id: u64, pane: u64, show: bool) {
    tabs.push(Tab {
        id,
        root: Node::leaf(pane),
        focus: pane,
    });
    if show || tabs.len() == 1 {
        *active = tabs.len() - 1;
    }
}

/// The tab ⌘N goes to: the Nth, and ⌘9 the last, as in every browser and
/// terminal with tabs.
pub fn numbered(tabs: &[Tab], n: usize) -> Option<usize> {
    match n {
        9 => tabs.len().checked_sub(1),
        1..=8 => (n <= tabs.len()).then(|| n - 1),
        _ => None,
    }
}

/// Move the tab at `from` to `to`, as a dropped drag does, keeping the
/// active tab the one it was.
pub fn reorder(tabs: &mut Vec<Tab>, active: &mut usize, from: usize, to: usize) {
    if from >= tabs.len() || to >= tabs.len() || from == to {
        return;
    }
    let shown = tabs.get(*active).map(|t| t.id);
    let tab = tabs.remove(from);
    tabs.insert(to, tab);
    if let Some(at) = shown.and_then(|id| tabs.iter().position(|t| t.id == id)) {
        *active = at;
    }
}

/// Where a dragged tab would land, from the pointer and each tab's
/// horizontal middle in strip order: before the first middle it is left
/// of, not counting its own.
pub fn landing(middles: &[f32], from: usize, pointer: f32) -> usize {
    let others: Vec<f32> = middles
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != from)
        .map(|(_, m)| *m)
        .collect();
    others.iter().filter(|m| **m < pointer).count()
}

/// How long the other tabs take to slide aside, and where a slide that
/// began `elapsed` ago is: from `start` pixels off its place to none, easing
/// out. Reduced motion skips it.
pub const SLIDE: std::time::Duration = std::time::Duration::from_millis(120);

pub fn slide(start: f32, elapsed: std::time::Duration, still: bool) -> f32 {
    if still || elapsed >= SLIDE {
        return 0.0;
    }
    let k = elapsed.as_secs_f32() / SLIDE.as_secs_f32();
    let eased = 1.0 - (1.0 - k) * (1.0 - k) * (1.0 - k);
    start * (1.0 - eased)
}

/// The row of a tab's hover panel the arrow keys move to.
pub fn step(selected: usize, rows: usize, down: bool) -> usize {
    match (rows, down) {
        (0, _) => 0,
        (_, true) => (selected + 1).min(rows - 1),
        (_, false) => selected.saturating_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: u64, panes: &[u64]) -> Tab {
        let mut root = Node::leaf(panes[0]);
        for pair in panes.windows(2) {
            root.split(pair[0], Axis::Horizontal, pair[1]);
        }
        Tab {
            id,
            root,
            focus: panes[0],
        }
    }

    fn layout(tabs: &[Tab]) -> Vec<Vec<u64>> {
        tabs.iter().map(|t| t.root.panes()).collect()
    }

    /// Splitting a session into a tab moves it there: it is never shown in
    /// two panes, and the tab it left closes if it was alone there.
    #[test]
    fn a_session_moves_into_another_tab_and_is_never_in_two() {
        let mut tabs = vec![tab(1, &[10]), tab(2, &[20]), tab(3, &[30, 31])];
        let mut active = 2;
        assert!(put_beside(&mut tabs, &mut active, 10, 20, Axis::Horizontal));
        assert_eq!(layout(&tabs), [vec![20, 10], vec![30, 31]]);
        assert_eq!(active, 0, "the tab it went to is shown");
        assert_eq!(tabs[0].focus, 10, "and the moved pane has the keyboard");

        // Out of a split: the split it left collapses.
        assert!(put_beside(&mut tabs, &mut active, 31, 10, Axis::Vertical));
        assert_eq!(layout(&tabs), [vec![20, 10, 31], vec![30]]);
        let all: Vec<u64> = layout(&tabs).concat();
        let mut unique = all.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(all.len(), unique.len(), "every pane in one place");
    }

    /// A session not shown anywhere is placed beside the pane, from any
    /// host: the tab is a view, not a host.
    #[test]
    fn a_new_pane_joins_beside_the_focused_one() {
        let mut tabs = vec![tab(1, &[10])];
        let mut active = 0;
        assert!(put_beside(&mut tabs, &mut active, 99, 10, Axis::Horizontal));
        assert_eq!(layout(&tabs), [vec![10, 99]]);
    }

    #[test]
    fn nothing_moves_beside_itself_or_a_pane_in_no_tab() {
        let mut tabs = vec![tab(1, &[10]), tab(2, &[20])];
        let mut active = 1;
        assert!(!put_beside(
            &mut tabs,
            &mut active,
            10,
            10,
            Axis::Horizontal
        ));
        assert!(!put_beside(
            &mut tabs,
            &mut active,
            10,
            77,
            Axis::Horizontal
        ));
        assert_eq!(layout(&tabs), [vec![10], vec![20]]);
        assert_eq!(active, 1);
    }

    /// Closing takes the pane out and nothing else; the active tab stays
    /// the one it was.
    #[test]
    fn closing_keeps_the_active_tab() {
        let mut tabs = vec![tab(1, &[10]), tab(2, &[20]), tab(3, &[30])];
        let mut active = 2;
        remove_pane(&mut tabs, &mut active, 10);
        assert_eq!(layout(&tabs), [vec![20], vec![30]]);
        assert_eq!(tabs[active].id, 3);

        remove_pane(&mut tabs, &mut active, 30);
        assert_eq!(layout(&tabs), [vec![20]]);
        assert_eq!(active, 0);

        let mut split = vec![tab(1, &[10, 11])];
        split[0].focus = 11;
        let mut active = 0;
        remove_pane(&mut split, &mut active, 11);
        assert_eq!(split[0].focus, 10, "focus moves to what is left");
        remove_pane(&mut split, &mut active, 10);
        assert!(split.is_empty());
        assert_eq!(active, 0);
    }

    #[test]
    fn a_new_tab_is_shown_when_asked_or_alone() {
        let mut tabs = Vec::new();
        let mut active = 0;
        open_tab(&mut tabs, &mut active, 1, 10, false);
        assert_eq!(active, 0);
        open_tab(&mut tabs, &mut active, 2, 20, false);
        assert_eq!(active, 0);
        open_tab(&mut tabs, &mut active, 3, 30, true);
        assert_eq!(active, 2);
    }

    /// A dropped tab moves and nothing else does; the tab on screen stays
    /// on screen.
    #[test]
    fn a_dropped_tab_moves_and_the_shown_one_stays() {
        let mut tabs = vec![tab(1, &[10]), tab(2, &[20]), tab(3, &[30])];
        let mut active = 1;
        reorder(&mut tabs, &mut active, 0, 2);
        assert_eq!(tabs.iter().map(|t| t.id).collect::<Vec<_>>(), [2, 3, 1]);
        assert_eq!(tabs[active].id, 2);
        reorder(&mut tabs, &mut active, 2, 9);
        assert_eq!(tabs.iter().map(|t| t.id).collect::<Vec<_>>(), [2, 3, 1]);
    }

    #[test]
    fn a_dragged_tab_lands_by_the_middles_it_has_passed() {
        let middles = [50.0, 150.0, 250.0, 350.0];
        assert_eq!(landing(&middles, 0, 10.0), 0);
        assert_eq!(landing(&middles, 0, 200.0), 1);
        assert_eq!(landing(&middles, 0, 999.0), 3);
        assert_eq!(landing(&middles, 3, 120.0), 1);
        assert_eq!(
            landing(&middles, 1, 120.0),
            1,
            "its own middle is not counted"
        );
    }

    #[test]
    fn the_others_slide_aside_in_120_ms_or_not_at_all() {
        let ms = std::time::Duration::from_millis;
        assert_eq!(slide(-80.0, ms(0), false), -80.0);
        let half = slide(-80.0, ms(60), false);
        assert!(half > -20.0 && half < 0.0, "eases out: {half}");
        assert_eq!(slide(-80.0, ms(120), false), 0.0);
        assert_eq!(slide(-80.0, ms(0), true), 0.0, "reduced motion");
    }

    #[test]
    fn the_hover_panels_rows_are_walked_with_the_arrows() {
        assert_eq!(step(0, 3, true), 1);
        assert_eq!(step(2, 3, true), 2);
        assert_eq!(step(0, 3, false), 0);
        assert_eq!(step(2, 3, false), 1);
        assert_eq!(step(0, 0, true), 0);
    }

    #[test]
    fn command_numbers_pick_tabs_and_nine_is_the_last() {
        let tabs = vec![tab(1, &[10]), tab(2, &[20]), tab(3, &[30])];
        assert_eq!(numbered(&tabs, 1), Some(0));
        assert_eq!(numbered(&tabs, 3), Some(2));
        assert_eq!(numbered(&tabs, 4), None);
        assert_eq!(numbered(&tabs, 9), Some(2));
        assert_eq!(numbered(&[], 9), None);
        assert_eq!(numbered(&tabs, 0), None);
    }
}
