//! The workbench under a user's hands: whatever they click, drag, and
//! press, the layout is what `ukai_core::Layout` says those gestures make of
//! it, and the screen shows that layout and nothing else.

use std::collections::HashMap;

use proptest::prelude::*;
use ukai_core::keymap::{Keymap, Keystroke, Platform, Scope};
use ukai_core::sidebar::Folds;

use super::*;
use crate::shortcuts::on_keydown;
use crate::testing::{Dom, KeyPress, NodeRef, Screen};

/// What the app starts from.
#[derive(Clone)]
struct Seed {
    layout: Layout,
    platform: Platform,
}

/// The app's shell around the workbench alone: its state, the default
/// shortcuts, and the keys it listens to.
#[component]
fn Bench() -> Element {
    let seed = use_context::<Seed>();
    let app = use_context_provider(|| {
        AppSignals::new(
            Projects::default(),
            HashMap::new(),
            seed.layout.clone(),
            Folds::default(),
        )
    });
    let shortcuts = use_context_provider(|| Shortcuts {
        keymap: Keymap::default_for(seed.platform),
        problems: Vec::new(),
    });
    rsx! {
        div {
            class: "shell",
            tabindex: "0",
            onkeydown: move |event| on_keydown(app, &shortcuts, &event),
            Workbench {}
        }
    }
}

fn start(layout: &Layout, platform: Platform) -> Screen {
    let seed = Seed {
        layout: layout.clone(),
        platform,
    };
    Screen::new(VirtualDom::new(Bench).with_root_context(seed))
}

/// Where a dragged pane is let go.
#[derive(Debug, Clone)]
enum Release {
    /// On a zone of the `usize`th other pane: a side of it, or its center.
    Zone(usize, Option<Side>),
    Tab(usize),
    NewTab,
    /// Over the panes, on no target.
    Nowhere,
    /// Outside the window, which the app learns from the next move.
    Lost,
}

/// Something a user does. Indices pick among what is on screen, wrapping,
/// so most of what is generated hits something.
#[derive(Debug, Clone)]
enum Act {
    NewTab,
    SelectTab(usize),
    CloseTab(usize),
    Split(usize, Direction),
    ClosePane(usize),
    /// Press the mouse on a pane's body.
    Focus(usize),
    /// Double-click a boundary between panes.
    Equalize(usize),
    /// Hold a pane by its title, move the pointer by `by`, and let go.
    Drag {
        pane: usize,
        by: (f64, f64),
        release: Release,
    },
    Key(KeyPress),
}

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![
        Just(Side::Left),
        Just(Side::Right),
        Just(Side::Top),
        Just(Side::Bottom),
    ]
}

fn direction() -> impl Strategy<Value = Direction> {
    prop_oneof![Just(Direction::Horizontal), Just(Direction::Vertical)]
}

fn release() -> impl Strategy<Value = Release> {
    prop_oneof![
        4 => (0..8usize, prop::option::of(side())).prop_map(|(pane, side)| Release::Zone(pane, side)),
        2 => (0..4usize).prop_map(Release::Tab),
        1 => Just(Release::NewTab),
        1 => Just(Release::Nowhere),
        1 => Just(Release::Lost),
    ]
}

/// Keys the default keymap binds, and some it does not, with the modifiers
/// that make them shortcuts on either platform more often than not.
fn key_press() -> impl Strategy<Value = KeyPress> {
    const KEYS: &[(&str, &str)] = &[
        ("t", "KeyT"),
        ("w", "KeyW"),
        ("d", "KeyD"),
        ("k", "KeyK"),
        ("m", "KeyM"),
        ("x", "KeyX"),
        ("]", "BracketRight"),
        ("[", "BracketLeft"),
        ("1", "Digit1"),
        ("2", "Digit2"),
        ("3", "Digit3"),
        ("9", "Digit9"),
        ("Tab", "Tab"),
        ("Escape", "Escape"),
        ("Enter", "Enter"),
    ];
    (
        prop::sample::select(KEYS),
        prop::bool::weighted(0.6),
        prop::bool::weighted(0.1),
        prop::bool::weighted(0.3),
        prop::bool::weighted(0.3),
    )
        .prop_map(|((key, code), ctrl, alt, shift, meta)| KeyPress {
            ctrl,
            alt,
            shift,
            meta,
            key,
            code,
        })
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        2 => Just(Act::NewTab),
        2 => (0..4usize).prop_map(Act::SelectTab),
        1 => (0..4usize).prop_map(Act::CloseTab),
        6 => (0..8usize, direction()).prop_map(|(pane, direction)| Act::Split(pane, direction)),
        2 => (0..8usize).prop_map(Act::ClosePane),
        2 => (0..8usize).prop_map(Act::Focus),
        1 => (0..8usize).prop_map(Act::Equalize),
        6 => (0..8usize, (-6.0..6.0f64, -6.0..6.0f64), release())
            .prop_map(|(pane, by, release)| Act::Drag { pane, by, release }),
        4 => key_press().prop_map(Act::Key),
    ]
}

/// The app's state as the test follows it: what [`ukai_core::Layout`] makes
/// of each gesture, which its own property tests hold to its invariants.
#[derive(Debug)]
struct Model {
    layout: Layout,
    palette: bool,
    keymap: Keymap,
}

impl Model {
    /// The panes of the active tab, in the order they are shown.
    fn panes(&self) -> Vec<PaneId> {
        self.layout.active_tab().root.panes()
    }

    /// What the shortcut `press` is, if one, does.
    fn press(&mut self, press: &KeyPress) {
        let stroke = Keystroke {
            ctrl: press.ctrl,
            alt: press.alt,
            shift: press.shift,
            meta: press.meta,
            key: press.key,
            code: press.code,
        };
        let Some(action) = self.keymap.action(Scope::App, &stroke) else {
            return;
        };
        let focused = self.layout.focused();
        match action {
            Action::Palette => self.palette = !self.palette,
            Action::NewTab => {
                self.layout.new_tab();
            }
            Action::ClosePane => {
                self.layout.close_pane(focused);
            }
            Action::SplitRight => {
                self.layout.split(focused, Direction::Horizontal);
            }
            Action::SplitDown => {
                self.layout.split(focused, Direction::Vertical);
            }
            Action::NextTab => self.layout.step_tab(1),
            Action::PrevTab => self.layout.step_tab(-1),
            Action::NextPane => self.layout.step_focus(1),
            Action::PrevPane => self.layout.step_focus(-1),
            Action::Tab(n) => self.layout.select_tab(usize::from(n).saturating_sub(1)),
            // Nothing to stop or dictate into: no pane has a session.
            Action::Stop | Action::Dictate | Action::Send => {}
        }
    }
}

/// The boundaries of the tree under `node`, in the order they are shown:
/// the path of each one's split and the child it follows.
fn boundaries(node: &Node, path: &mut Vec<usize>, out: &mut Vec<(Vec<usize>, usize)>) {
    if let Node::Split { children, .. } = node {
        for (index, child) in children.iter().enumerate() {
            path.push(index);
            boundaries(child, path, out);
            path.pop();
            if index + 1 < children.len() {
                out.push((path.clone(), index));
            }
        }
    }
}

/// What the pane area shows, or should.
#[derive(Debug, PartialEq)]
enum Shape {
    Pane {
        id: String,
        class: String,
    },
    Split {
        class: String,
        /// Each child with its `style`, and `None` for a divider.
        parts: Vec<Option<(String, Shape)>>,
    },
}

/// The shape `layout` should show when nothing is being dragged.
fn expected_shape(layout: &Layout, node: &Node) -> Shape {
    match node {
        Node::Pane(pane) => Shape::Pane {
            id: pane.to_string(),
            class: if layout.focused() == *pane {
                "pane focused".to_owned()
            } else {
                "pane".to_owned()
            },
        },
        Node::Split {
            direction,
            children,
            sizes,
        } => {
            let mut parts = Vec::new();
            for (index, (child, size)) in children.iter().zip(sizes).enumerate() {
                if index > 0 {
                    parts.push(None);
                }
                parts.push(Some((
                    format!("flex: {size} 1 0"),
                    expected_shape(layout, child),
                )));
            }
            Shape::Split {
                class: match direction {
                    Direction::Horizontal => "split horizontal".to_owned(),
                    Direction::Vertical => "split vertical".to_owned(),
                },
                parts,
            }
        }
    }
}

fn attribute(dom: &Dom, node: NodeRef, name: &str) -> String {
    dom.attribute(node, name).unwrap_or_default().to_owned()
}

/// The one element child of `node`.
fn only_child(dom: &Dom, node: NodeRef) -> Result<NodeRef, TestCaseError> {
    let children = dom.children(node);
    prop_assert_eq!(children.len(), 1, "children of a <{}>", dom.tag(node));
    Ok(children[0])
}

fn shown_shape(dom: &Dom, node: NodeRef) -> Result<Shape, TestCaseError> {
    if dom.tag(node) == "section" {
        return Ok(Shape::Pane {
            id: attribute(dom, node, "data-pane"),
            class: attribute(dom, node, "class"),
        });
    }
    prop_assert!(dom.has_class(node, "split"), "neither a pane nor a split");
    let mut parts = Vec::new();
    for child in dom.children(node) {
        parts.push(if dom.has_class(child, "divider") {
            None
        } else {
            prop_assert!(dom.has_class(child, "split-child"));
            Some((
                attribute(dom, child, "style"),
                shown_shape(dom, only_child(dom, child)?)?,
            ))
        });
    }
    Ok(Shape::Split {
        class: attribute(dom, node, "class"),
        parts,
    })
}

/// A screen with the model of what it should show.
struct Run {
    screen: Screen,
    app: AppSignals,
    model: Model,
    platform: Platform,
}

/// Where the pointer goes down when a gesture needs a place.
const FROM: (f64, f64) = (100.0, 100.0);

impl Run {
    fn new(platform: Platform) -> Self {
        let layout = Layout::default();
        let screen = start(&layout, platform);
        let app = screen.with(consume_context::<AppSignals>);
        Self {
            screen,
            app,
            model: Model {
                layout,
                palette: false,
                keymap: Keymap::default_for(platform),
            },
            platform,
        }
    }

    fn one(&self, class: &str) -> NodeRef {
        let found = self.screen.dom.by_class(class);
        assert_eq!(found.len(), 1, "elements of class `{class}`");
        found[0]
    }

    /// The `<section>` of pane `pane` of the active tab.
    fn section(&self, pane: PaneId) -> NodeRef {
        let id = pane.to_string();
        let found = self
            .screen
            .dom
            .find_in(self.screen.dom.root(), |dom, node| {
                dom.attribute(node, "data-pane") == Some(&id)
            });
        assert_eq!(found.len(), 1, "sections of pane {pane}");
        found[0]
    }

    /// The button of `pane`'s header whose title starts with `title`.
    fn tool(&self, pane: PaneId, title: &str) -> NodeRef {
        let found = self.screen.dom.find_in(self.section(pane), |dom, node| {
            dom.tag(node) == "button"
                && dom
                    .attribute(node, "title")
                    .is_some_and(|t| t.starts_with(title))
        });
        assert_eq!(found.len(), 1, "`{title}` buttons of pane {pane}");
        found[0]
    }

    /// Press and release the mouse on `node`, as a click is made.
    fn click(&mut self, node: NodeRef) {
        self.screen.mouse_down(node, FROM);
        self.screen.mouse_up(node, FROM);
        self.screen.click(node);
    }

    /// Click the button of `pane`'s header titled `title`. Finding it again
    /// for each event keeps the test from holding on to a node a render
    /// replaced.
    fn click_tool(&mut self, pane: PaneId, title: &str) {
        let button = self.tool(pane, title);
        self.screen.mouse_down(button, FROM);
        let button = self.tool(pane, title);
        self.screen.mouse_up(button, FROM);
        let button = self.tool(pane, title);
        self.screen.click(button);
    }

    /// Do `act` on the screen and tell the model what it should come to.
    fn perform(&mut self, act: &Act) -> Result<(), TestCaseError> {
        let panes = self.model.panes();
        let pick = |index: usize| panes[index % panes.len()];
        let tabs = self.model.layout.tabs().len();
        match act {
            Act::NewTab => {
                self.click(self.one("new-tab"));
                self.model.layout.new_tab();
            }
            Act::SelectTab(index) => {
                let index = index % tabs;
                self.click(self.screen.dom.by_class("tab")[index]);
                self.model.layout.select_tab(index);
            }
            Act::CloseTab(index) => {
                let index = index % tabs;
                let tab = self.screen.dom.by_class("tab")[index];
                let close = self.screen.dom.by_class_in(tab, "close");
                prop_assert_eq!(close.len(), 1);
                self.click(close[0]);
                self.model.layout.close_tab(index);
            }
            Act::Split(pane, direction) => {
                let pane = pick(*pane);
                let title = match direction {
                    Direction::Horizontal => "Split right",
                    Direction::Vertical => "Split down",
                };
                self.click_tool(pane, title);
                // The press focuses the pane before the click splits it.
                self.model.layout.focus(pane);
                self.model.layout.split(pane, *direction);
            }
            Act::ClosePane(pane) => {
                let pane = pick(*pane);
                self.click_tool(pane, "Close pane");
                self.model.layout.focus(pane);
                self.model.layout.close_pane(pane);
            }
            Act::Focus(pane) => {
                let pane = pick(*pane);
                let body = self.screen.dom.by_class_in(self.section(pane), "pane-body");
                prop_assert_eq!(body.len(), 1);
                self.click(body[0]);
                self.model.layout.focus(pane);
            }
            Act::Equalize(index) => {
                let mut all = Vec::new();
                boundaries(
                    &self.model.layout.active_tab().root,
                    &mut Vec::new(),
                    &mut all,
                );
                let dividers = self.screen.dom.by_class("divider");
                prop_assert_eq!(dividers.len(), all.len());
                if !all.is_empty() {
                    let index = index % all.len();
                    prop_assert!(self.screen.double_click(dividers[index]));
                    let (path, boundary) = &all[index];
                    self.model.layout.resize(path, *boundary, 0.5);
                }
            }
            Act::Drag { pane, by, release } => {
                self.drag(pick(*pane), *by, release)?;
            }
            Act::Key(press) => {
                self.screen.key_down(self.one("shell"), press);
                self.model.press(press);
            }
        }
        Ok(())
    }

    fn drag(
        &mut self,
        pane: PaneId,
        by: (f64, f64),
        release: &Release,
    ) -> Result<(), TestCaseError> {
        let title = self.screen.dom.by_class_in(self.section(pane), "title");
        prop_assert_eq!(title.len(), 1);
        prop_assert!(self.screen.mouse_down(title[0], FROM));
        self.model.layout.focus(pane);

        let to = (FROM.0 + by.0, FROM.1 + by.1);
        self.screen.mouse_move(self.one("panes"), true, to);
        let moved = by.0.hypot(by.1) >= DRAG_THRESHOLD;
        let others: Vec<PaneId> = self
            .model
            .panes()
            .into_iter()
            .filter(|other| *other != pane)
            .collect();
        // Every other pane of the tab offers itself as a target, and only
        // once the pointer went far enough to mean a drag.
        let dom = &self.screen.dom;
        let targets = if moved { others.len() } else { 0 };
        prop_assert_eq!(dom.by_class("drop-zones").len(), targets);
        prop_assert_eq!(dom.by_class("drop-zone").len(), targets * 5);
        prop_assert_eq!(dom.has_class(self.section(pane), "moving"), moved);
        prop_assert_eq!(dom.has_class(self.one("workbench"), "moving"), moved);

        let tabs = self.model.layout.tabs().len();
        match release {
            Release::Zone(target, side) if moved && !others.is_empty() => {
                let target = others[target % others.len()];
                let name = match side {
                    Some(Side::Left) => "left",
                    Some(Side::Right) => "right",
                    Some(Side::Top) => "top",
                    Some(Side::Bottom) => "bottom",
                    None => "center",
                };
                let zone = self.screen.dom.find_in(self.section(target), |dom, node| {
                    dom.has_class(node, "drop-zone") && dom.has_class(node, name)
                });
                prop_assert_eq!(zone.len(), 1);
                self.screen.mouse_up(zone[0], to);
                match side {
                    Some(side) => self.model.layout.move_pane(pane, target, *side),
                    None => self.model.layout.swap_panes(pane, target),
                };
            }
            // No zone to let go on: the pane it was held over, then.
            Release::Zone(..) | Release::Nowhere => {
                self.screen.mouse_up(self.section(pane), to);
            }
            Release::Tab(index) => {
                let index = index % tabs;
                self.screen
                    .mouse_up(self.screen.dom.by_class("tab")[index], to);
                if moved {
                    self.model.layout.move_to_tab(pane, Some(index));
                }
            }
            Release::NewTab => {
                self.screen.mouse_up(self.one("new-tab"), to);
                if moved {
                    self.model.layout.move_to_tab(pane, None);
                }
            }
            Release::Lost => {
                self.screen.mouse_move(self.one("panes"), false, to);
            }
        }
        Ok(())
    }

    /// The app holds the layout the model came to, and shows it.
    fn check(&self) -> Result<(), TestCaseError> {
        let (layout, palette, drag) = self.screen.with(|| {
            (
                self.app.layout.peek().clone(),
                *self.app.palette.peek(),
                self.app.drag.peek().clone(),
            )
        });
        prop_assert_eq!(&layout, &self.model.layout);
        let checked = layout.check();
        prop_assert!(checked.is_ok(), "{:?}", checked);
        prop_assert_eq!(palette, self.model.palette);
        // No gesture leaves a drag behind.
        prop_assert_eq!(drag, None);

        let dom = &self.screen.dom;
        prop_assert_eq!(
            attribute(dom, self.one("workbench"), "class"),
            "workbench".to_owned()
        );
        prop_assert!(dom.by_class("drop-zones").is_empty());

        let tabs: Vec<(String, String)> = dom
            .by_class("tab")
            .into_iter()
            .map(|tab| {
                let count = dom.by_class_in(tab, "count");
                (
                    attribute(dom, tab, "class"),
                    count.first().map(|c| dom.text(*c)).unwrap_or_default(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = layout
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let class = if index == layout.active() {
                    "tab active"
                } else {
                    "tab"
                };
                let panes = tab.root.panes().len();
                let count = if panes > 1 {
                    panes.to_string()
                } else {
                    String::new()
                };
                (class.to_owned(), count)
            })
            .collect();
        prop_assert_eq!(tabs, expected);

        let shown = shown_shape(dom, only_child(dom, self.one("panes"))?)?;
        prop_assert_eq!(shown, expected_shape(&layout, &layout.active_tab().root));

        // What the screen came to through every change is what it shows
        // when it starts from the same layout.
        let fresh = start(&layout, self.platform);
        prop_assert_eq!(dom.html(), fresh.dom.html());
        Ok(())
    }
}

fn platform() -> impl Strategy<Value = Platform> {
    prop_oneof![Just(Platform::Mac), Just(Platform::Other)]
}

#[test]
fn starts_with_one_empty_focused_pane() -> Result<(), TestCaseError> {
    let run = Run::new(Platform::Other);
    run.check()?;
    let dom = &run.screen.dom;
    assert_eq!(dom.by_class("tab").len(), 1);
    let panes = dom.by_class("pane");
    assert_eq!(panes.len(), 1);
    assert!(dom.has_class(panes[0], "focused"));
    assert!(dom.text(panes[0]).contains("Open an agent"));
    Ok(())
}

proptest! {
    #[test]
    fn gestures_arrange_the_layout_the_screen_shows(
        platform in platform(),
        acts in prop::collection::vec(act(), 0..30),
    ) {
        let mut run = Run::new(platform);
        run.check()?;
        for act in &acts {
            run.perform(act)?;
            run.check()?;
        }
    }
}
