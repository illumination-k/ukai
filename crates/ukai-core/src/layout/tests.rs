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
            workspace: None,
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
    Resize(usize, usize, f32),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::NewTab),
        (0..4usize).prop_map(Op::SelectTab),
        (0..4usize).prop_map(Op::CloseTab),
        (0..16usize, any::<bool>()).prop_map(|(i, h)| Op::Split(i, h)),
        (0..16usize).prop_map(Op::Close),
        (0..16usize).prop_map(Op::Focus),
        (0..4usize, 0..4usize, -2.0f32..3.0).prop_map(|(d, i, s)| Op::Resize(d, i, s)),
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
                // Dividers are dragged with whatever path and share the UI
                // computed, so resizing must never break the tree.
                Op::Resize(depth, index, share) => {
                    let path = vec![0; depth];
                    layout.resize(&path, index, share);
                }
            }
            check_invariants(&layout)?;
        }
    }
}

fn agent(name: &str) -> PaneContent {
    PaneContent::Agent {
        project: ProjectId("p1".into()),
        agent: name.to_owned(),
        session: None,
        workspace: None,
    }
}

#[test]
fn agent_panes_of_the_active_tab_are_listed_in_display_order() {
    // H[0, V[1, 2]], with 1 holding a report and 2 an agent.
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, V);
    layout.set_content(0, agent("claude"));
    layout.set_content(
        1,
        PaneContent::Report {
            project: ProjectId("p1".into()),
            path: "r.html".into(),
        },
    );
    layout.set_content(2, agent("codex"));
    assert_eq!(layout.active_agent_panes(), vec![0, 2]);
    layout.new_tab();
    assert_eq!(layout.active_agent_panes(), Vec::<PaneId>::new());
}

/// H[0, V[1, 2]]: 0 and the column are side by side, 1 is above 2.
fn nested() -> Layout {
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, V);
    layout
}

#[rstest]
#[case::across_the_outer_split(0, H, true, Some(1))]
#[case::back_across_it_lands_on_the_last_pane(1, H, false, Some(0))]
#[case::down_the_column(1, V, true, Some(2))]
#[case::up_the_column(2, V, false, Some(1))]
#[case::nothing_to_the_left_of_the_first(0, H, false, None)]
#[case::no_column_around_the_first_pane(0, V, true, None)]
#[case::out_through_the_column_to_the_left(2, H, false, Some(0))]
fn neighbour_crosses_the_innermost_matching_divider(
    #[case] from: PaneId,
    #[case] direction: Direction,
    #[case] forward: bool,
    #[case] expected: Option<PaneId>,
) {
    assert_eq!(nested().neighbour(from, direction, forward), expected);
}

#[test]
fn neighbour_of_a_lone_pane_is_nothing() {
    let layout = Layout::default();
    assert_eq!(layout.neighbour(0, H, true), None);
    assert_eq!(layout.neighbour(99, H, true), None);
}

#[test]
fn resize_moves_one_divider_and_leaves_the_rest() {
    // H[0, 1, 2] with equal sizes; drag the divider between 1 and 2.
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, H);
    let mut layout = layout;
    layout.resize(&[], 0, 0.25);
    let Node::Split { sizes, .. } = layout.active_tab().root.clone() else {
        panic!("expected a split");
    };
    // The pair keeps its combined size, the third child is untouched.
    assert!((sizes[0] + sizes[1] - 1.5).abs() < 1e-6, "{sizes:?}");
    assert!((sizes[0] / (sizes[0] + sizes[1]) - 0.25).abs() < 1e-6, "{sizes:?}");
    assert!((sizes[2] - 0.5).abs() < 1e-6, "{sizes:?}");
    assert!(layout.check().is_ok());
}

#[rstest]
#[case::collapsing_the_first_is_clamped(-5.0)]
#[case::collapsing_the_second_is_clamped(5.0)]
#[case::zero(0.0)]
fn resize_never_collapses_a_pane(#[case] share: f32) {
    let mut layout = nested();
    assert!(layout.resize(&[], 0, share));
    assert!(layout.check().is_ok(), "{layout:#?}");
}

#[rstest]
#[case::no_such_split(&[7], 0)]
#[case::a_pane_is_not_a_split(&[0], 0)]
#[case::no_such_divider(&[], 9)]
#[case::not_a_number(&[], 0)]
fn resize_reports_what_it_could_not_find(#[case] path: &[usize], #[case] index: usize) {
    let mut layout = nested();
    let share = if index == 0 && path.is_empty() {
        f32::NAN
    } else {
        0.5
    };
    assert!(!layout.resize(path, index, share));
}
