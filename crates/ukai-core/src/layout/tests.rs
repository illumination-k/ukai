use proptest::prelude::*;
use rstest::rstest;

use super::*;
use crate::project::ProjectId;

use Direction::{Horizontal as H, Vertical as V};

/// A split with equal sizes; use `sized` for anything else.
fn split(direction: Direction, children: Vec<Node>) -> Node {
    let sizes = vec![1.0; children.len()];
    sized(direction, children, sizes)
}

fn sized(direction: Direction, children: Vec<Node>, sizes: Vec<f32>) -> Node {
    Node::Split {
        direction,
        children,
        sizes,
    }
}

fn p(id: PaneId) -> Node {
    Node::Pane(id)
}

#[test]
fn starts_with_one_empty_pane() {
    let layout = Layout::default();
    assert_eq!(layout.tabs().len(), 1);
    assert_eq!(layout.active_tab().root, p(0));
    assert_eq!(layout.focused(), 0);
    assert_eq!(layout.content(0), Some(&PaneContent::Empty));
}

#[rstest]
#[case::same_direction_adds_sibling(H, sized(H, vec![p(0), p(2), p(1)], vec![0.5, 0.5, 1.0]))]
#[case::cross_direction_nests(V, split(H, vec![split(V, vec![p(0), p(2)]), p(1)]))]
fn split_nests_or_extends(#[case] direction: Direction, #[case] expected: Node) {
    let mut layout = Layout::default();
    layout.split(0, H);
    assert_eq!(layout.split(0, direction), Some(2));
    assert_eq!(layout.active_tab().root, expected);
    assert_eq!(layout.focused(), 2);
}

#[test]
fn closing_collapses_and_flattens() {
    // H[0, V[1, H[2, 3]]] -- close 1 --> H[0, 2, 3]
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, V);
    layout.split(2, H);
    assert_eq!(
        layout.active_tab().root,
        split(
            H,
            vec![p(0), split(V, vec![p(1), split(H, vec![p(2), p(3)])])]
        )
    );
    assert!(layout.close_pane(1));
    // 2 and 3 share the slot 1's split had.
    assert_eq!(
        layout.active_tab().root,
        sized(H, vec![p(0), p(2), p(3)], vec![1.0, 0.5, 0.5])
    );
    assert_eq!(layout.content(1), None);
}

#[rstest]
#[case::middle_goes_to_previous(1, 0)]
#[case::first_goes_to_next(0, 1)]
fn focus_moves_to_a_neighbour(#[case] close: PaneId, #[case] expected: PaneId) {
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, H);
    layout.focus(close);
    layout.close_pane(close);
    assert_eq!(layout.focused(), expected);
}

#[test]
fn closing_the_last_pane_of_a_tab_closes_the_tab() {
    let mut layout = Layout::default();
    let second = layout.new_tab();
    assert_eq!(layout.active(), 1);
    layout.close_pane(second);
    assert_eq!(layout.tabs().len(), 1);
    assert_eq!(layout.focused(), 0);
}

#[test]
fn closing_the_last_tab_opens_a_fresh_one() {
    let mut layout = Layout::default();
    layout.set_content(
        0,
        PaneContent::Agent {
            project: ProjectId("p1".into()),
            agent: "claude".into(),
            session: None,
        },
    );
    assert_eq!(layout.close_tab(0), vec![0]);
    assert_eq!(layout.tabs().len(), 1);
    let fresh = layout.focused();
    assert_ne!(fresh, 0);
    assert_eq!(layout.content(fresh), Some(&PaneContent::Empty));
}

#[test]
fn closing_an_earlier_tab_keeps_the_active_one() {
    let mut layout = Layout::default();
    layout.new_tab();
    let third = layout.new_tab();
    layout.close_tab(0);
    assert_eq!(layout.active(), 1);
    assert_eq!(layout.focused(), third);
}

#[derive(Debug, Clone)]
enum Op {
    NewTab,
    SelectTab(usize),
    CloseTab(usize),
    Split(usize, bool),
    Close(usize),
    Focus(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::NewTab),
        (0..4usize).prop_map(Op::SelectTab),
        (0..4usize).prop_map(Op::CloseTab),
        (0..16usize, any::<bool>()).prop_map(|(i, h)| Op::Split(i, h)),
        (0..16usize).prop_map(Op::Close),
        (0..16usize).prop_map(Op::Focus),
    ]
}

/// Pick an existing pane by index so most generated ops hit something.
fn nth_pane(layout: &Layout, index: usize) -> PaneId {
    let all: Vec<PaneId> = layout.tabs().iter().flat_map(|t| t.root.panes()).collect();
    all[index % all.len()]
}

fn check_invariants(layout: &Layout) -> Result<(), TestCaseError> {
    let checked = layout.check();
    prop_assert!(checked.is_ok(), "{checked:?} in {layout:#?}");
    Ok(())
}

#[test]
fn sibling_splits_share_the_space_of_the_split_pane() {
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, H);
    assert_eq!(
        layout.active_tab().root,
        sized(H, vec![p(0), p(1), p(2)], vec![1.0, 0.5, 0.5])
    );
}

proptest! {
    #[test]
    fn operations_keep_invariants(ops in prop::collection::vec(op(), 0..60)) {
        let mut layout = Layout::default();
        for op in ops {
            match op {
                Op::NewTab => { layout.new_tab(); }
                Op::SelectTab(i) => layout.select_tab(i),
                Op::CloseTab(i) => { layout.close_tab(i); }
                Op::Split(i, horizontal) => {
                    let pane = nth_pane(&layout, i);
                    let direction = if horizontal { H } else { V };
                    let created = layout.split(pane, direction);
                    prop_assert!(created.is_some());
                }
                Op::Close(i) => {
                    let pane = nth_pane(&layout, i);
                    prop_assert!(layout.close_pane(pane));
                }
                Op::Focus(i) => {
                    let pane = nth_pane(&layout, i);
                    layout.focus(pane);
                    prop_assert_eq!(layout.focused(), pane);
                }
            }
            check_invariants(&layout)?;
        }
    }
}
