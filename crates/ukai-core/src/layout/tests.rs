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

#[rstest]
#[case::forward(1, 1)]
#[case::wraps_forward(3, 0)]
#[case::backward(-1, 2)]
fn step_tab_wraps(#[case] step: isize, #[case] expected: usize) {
    let mut layout = Layout::default();
    layout.new_tab();
    layout.new_tab();
    layout.select_tab(0);
    layout.step_tab(step);
    assert_eq!(layout.active(), expected);
    assert!(layout.active_tab().root.contains(layout.focused()));
}

#[rstest]
#[case::forward(1, 2)]
#[case::wraps_forward(2, 1)]
#[case::backward(-1, 1)]
fn step_focus_follows_display_order(#[case] step: isize, #[case] expected: PaneId) {
    // Tab 0 shows 0 | 2 | 1 after these splits.
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(0, H);
    layout.focus(0);
    layout.step_focus(step);
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

#[rstest]
#[case::first_child(&[], 0, 0.25, vec![0.5, 1.5, 2.0])]
#[case::clamped_low(&[], 1, 0.0, vec![1.0, 0.15, 2.85])]
#[case::clamped_high(&[], 1, 2.0, vec![1.0, 2.85, 0.15])]
fn resize_moves_one_boundary(
    #[case] path: &[usize],
    #[case] index: usize,
    #[case] share: f32,
    #[case] expected: Vec<f32>,
) {
    let mut layout = Layout::default();
    layout.tabs[0].root = sized(H, vec![p(0), p(1), p(2)], vec![1.0, 1.0, 2.0]);
    layout.next_id = 3;
    layout
        .contents
        .extend([(1, PaneContent::Empty), (2, PaneContent::Empty)]);
    assert!(layout.resize(path, index, share));
    let Node::Split { sizes, .. } = &layout.active_tab().root else {
        panic!("not a split");
    };
    for (size, want) in sizes.iter().zip(&expected) {
        assert!((size - want).abs() < 1e-5, "{sizes:?} != {expected:?}");
    }
}

#[rstest]
#[case::no_split(&[0], 0)]
#[case::last_child(&[], 1)]
#[case::past_the_tree(&[5], 0)]
fn resize_ignores_what_is_not_a_boundary(#[case] path: &[usize], #[case] index: usize) {
    let mut layout = Layout::default();
    layout.split(0, H);
    let before = layout.clone();
    assert!(!layout.resize(path, index, 0.3));
    assert_eq!(layout, before);
}

#[rstest]
#[case::first(0, Some((0.0, 201.0)))]
#[case::second(1, Some((101.0, 301.0)))]
#[case::past_the_end(2, None)]
fn boundary_span_measures_the_pair(#[case] index: usize, #[case] expected: Option<(f64, f64)>) {
    // 402 px with two 1 px dividers leaves 400 px for sizes 1:1:2.
    assert_eq!(boundary_span(&[1.0, 1.0, 2.0], index, 402.0, 1.0), expected);
}

#[test]
fn resize_reaches_nested_splits() {
    // H[0, V[1, 2]]
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, V);
    assert!(layout.resize(&[1], 0, 0.75));
    assert_eq!(
        layout.active_tab().root,
        split(H, vec![p(0), sized(V, vec![p(1), p(2)], vec![1.5, 0.5])])
    );
}

#[rstest]
#[case::left(Side::Left, split(H, vec![p(2), p(0), p(1)]))]
#[case::right(Side::Right, split(H, vec![p(0), p(2), p(1)]))]
#[case::top(Side::Top, split(H, vec![split(V, vec![p(2), p(0)]), p(1)]))]
#[case::bottom(Side::Bottom, split(H, vec![split(V, vec![p(0), p(2)]), p(1)]))]
fn move_pane_places_it_beside_the_target(#[case] side: Side, #[case] expected: Node) {
    // H[0, V[1, 2]] -- move 2 beside 0 --> 1 is left alone on the right.
    let mut layout = Layout::default();
    layout.split(0, H);
    layout.split(1, V);
    layout.resize(&[], 0, 0.5);
    assert!(layout.move_pane(2, 0, side));
    let mut root = layout.active_tab().root.clone();
    // Compare shapes; sizes are checked by the property test.
    equalize(&mut root);
    assert_eq!(root, expected);
    assert_eq!(layout.focused(), 2);
}

fn equalize(node: &mut Node) {
    if let Node::Split {
        children, sizes, ..
    } = node
    {
        sizes.iter_mut().for_each(|s| *s = 1.0);
        children.iter_mut().for_each(equalize);
    }
}

#[test]
fn move_pane_across_tabs_closes_the_tab_it_empties() {
    let mut layout = Layout::default();
    let other = layout.new_tab();
    layout.select_tab(0);
    assert!(layout.move_pane(0, other, Side::Right));
    assert_eq!(layout.tabs().len(), 1);
    assert_eq!(layout.active_tab().root, split(H, vec![p(other), p(0)]));
    assert_eq!(layout.focused(), 0);
}

#[rstest]
#[case::onto_itself(0, 0)]
#[case::missing_target(0, 9)]
#[case::missing_pane(9, 0)]
fn move_pane_refuses_what_cannot_move(#[case] pane: PaneId, #[case] target: PaneId) {
    let mut layout = Layout::default();
    layout.split(0, H);
    let before = layout.clone();
    assert!(!layout.move_pane(pane, target, Side::Left));
    assert!(!layout.swap_panes(pane, target));
    assert_eq!(layout, before);
}

#[test]
fn swap_exchanges_places_across_tabs() {
    let mut layout = Layout::default();
    layout.split(0, H);
    let other = layout.new_tab();
    assert!(layout.swap_panes(0, other));
    assert_eq!(layout.tabs()[0].root, split(H, vec![p(other), p(1)]));
    assert_eq!(layout.tabs()[1].root, p(0));
    assert_eq!(layout.active(), 1);
    assert_eq!(layout.focused(), 0);
}

#[test]
fn move_to_tab_appends_on_the_right() {
    let mut layout = Layout::default();
    layout.split(0, V);
    let other = layout.new_tab();
    assert!(layout.move_to_tab(1, Some(1)));
    assert_eq!(layout.tabs()[0].root, p(0));
    assert_eq!(layout.tabs()[1].root, split(H, vec![p(other), p(1)]));
    assert_eq!(layout.active(), 1);
    assert_eq!(layout.focused(), 1);
}

#[test]
fn move_to_a_later_tab_counts_the_tab_it_empties() {
    let mut layout = Layout::default();
    let second = layout.new_tab();
    let third = layout.new_tab();
    assert!(layout.move_to_tab(0, Some(2)));
    assert_eq!(layout.tabs().len(), 2);
    assert_eq!(layout.tabs()[0].root, p(second));
    assert_eq!(layout.tabs()[1].root, split(H, vec![p(third), p(0)]));
}

#[test]
fn move_to_a_new_tab() {
    let mut layout = Layout::default();
    layout.split(0, H);
    assert!(layout.move_to_tab(0, None));
    assert_eq!(layout.tabs().len(), 2);
    assert_eq!(layout.tabs()[0].root, p(1));
    assert_eq!(layout.tabs()[1].root, p(0));
    assert_eq!(layout.active(), 1);
}

#[derive(Debug, Clone)]
enum Op {
    NewTab,
    SelectTab(usize),
    CloseTab(usize),
    Split(usize, bool),
    Close(usize),
    Focus(usize),
    Resize(Vec<usize>, usize, f32),
    Move(usize, usize, Side),
    Swap(usize, usize),
    MoveToTab(usize, Option<usize>),
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![
        Just(Side::Left),
        Just(Side::Right),
        Just(Side::Top),
        Just(Side::Bottom),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::NewTab),
        (0..4usize).prop_map(Op::SelectTab),
        (0..4usize).prop_map(Op::CloseTab),
        (0..16usize, any::<bool>()).prop_map(|(i, h)| Op::Split(i, h)),
        (0..16usize).prop_map(Op::Close),
        (0..16usize).prop_map(Op::Focus),
        (
            prop::collection::vec(0..4usize, 0..3),
            0..4usize,
            -0.5..1.5f32
        )
            .prop_map(|(path, i, share)| Op::Resize(path, i, share)),
        (0..16usize, 0..16usize, side()).prop_map(|(a, b, s)| Op::Move(a, b, s)),
        (0..16usize, 0..16usize).prop_map(|(a, b)| Op::Swap(a, b)),
        (0..16usize, prop::option::of(0..4usize)).prop_map(|(a, t)| Op::MoveToTab(a, t)),
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
                Op::Resize(path, i, share) => { layout.resize(&path, i, share); }
                Op::Move(a, b, side) => {
                    let (pane, target) = (nth_pane(&layout, a), nth_pane(&layout, b));
                    prop_assert_eq!(layout.move_pane(pane, target, side), pane != target);
                }
                Op::Swap(a, b) => {
                    let (pane, other) = (nth_pane(&layout, a), nth_pane(&layout, b));
                    prop_assert_eq!(layout.swap_panes(pane, other), pane != other);
                }
                Op::MoveToTab(a, tab) => {
                    let pane = nth_pane(&layout, a);
                    let tab = tab.filter(|t| *t < layout.tabs().len());
                    prop_assert!(layout.move_to_tab(pane, tab));
                    prop_assert_eq!(layout.focused(), pane);
                }
            }
            check_invariants(&layout)?;
        }
    }
}
