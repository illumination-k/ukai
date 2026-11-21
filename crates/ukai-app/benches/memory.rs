//! The heap the app's view of panes keeps: what each tab's panes hold while
//! mounted, and what keeping the tabs not shown mounted costs, since
//! switching tabs hides panes rather than dropping them.
//!
//! Each bench mounts a workbench of `TABS` tabs of four panes, every tab
//! shown in turn and the last one shown now, and returns it: divan drops it
//! after counting, so `alloc` less `dealloc` in a row is what the mounted
//! workbench keeps. `kept_tabs` is the app, which keeps the tabs shown last
//! (`KEPT_TABS`) mounted; `all_tabs` keeps every tab, and `shown_tab_only`
//! none but the one shown, for comparison. Only the
//! Rust side is counted, the virtual DOM; the web view's own DOM for the
//! same elements is not.
//!
//! `mise run bench`, or `cargo bench -p ukai-app --bench memory`.

use dioxus::dioxus_core::NoOpMutations;
use dioxus::prelude::*;
use ukai_app::bench::{Markdown, PlacedPanes, use_kept_panes};
use ukai_core::{Direction, Layout, PaneId};

use crate::common::MESSAGE;

mod common;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// Panes in each tab.
const PANES: usize = 4;
/// Messages in each pane's transcript.
const MESSAGES: [usize; 3] = [0, 20, 100];

/// Which tabs not shown stay mounted.
#[derive(Clone, Copy)]
enum Hidden {
    /// The tabs shown last, as the app keeps them.
    Kept,
    All,
    None,
}

#[component]
fn Transcript(pane: PaneId) -> Element {
    let messages = use_context::<usize>();
    rsx! {
        div { class: "transcript",
            for index in 0..messages {
                Markdown { key: "{index}", source: MESSAGE.to_owned() }
            }
        }
    }
}

#[component]
fn Root() -> Element {
    let layout = use_context::<Signal<Layout>>();
    let kept = use_kept_panes(layout);
    let panes = match use_context::<Hidden>() {
        Hidden::Kept => kept,
        Hidden::All => layout.read().placed(),
        Hidden::None => {
            let mut panes = layout.read().placed();
            panes.retain(|placed| placed.shown);
            panes
        }
    };
    rsx! {
        PlacedPanes { panes, pane: |pane| rsx! {
            Transcript { pane }
        } }
    }
}

/// `tabs` tabs of [`PANES`] panes each, the first one shown.
fn layout(tabs: usize) -> Layout {
    let mut layout = Layout::default();
    for tab in 0..tabs {
        if tab > 0 {
            layout.new_tab();
        }
        for _ in 1..PANES {
            let focused = layout.focused();
            layout.split(focused, Direction::Horizontal);
        }
    }
    layout
}

/// The workbench rendered with each tab shown in turn, the last one now,
/// kept for divan to drop after counting.
fn mount(tabs: usize, messages: usize, hidden: Hidden) -> VirtualDom {
    let mut dom = VirtualDom::new(Root);
    let mut signal = dom.in_runtime(|| {
        let mut layout = layout(tabs);
        layout.select_tab(0);
        Signal::new_in_scope(layout, ScopeId::ROOT)
    });
    dom.provide_root_context(messages);
    dom.provide_root_context(hidden);
    dom.provide_root_context(signal);
    dom.rebuild_in_place();
    for tab in 1..tabs {
        dom.in_runtime(|| signal.write().select_tab(tab));
        dom.render_immediate(&mut NoOpMutations);
    }
    dom
}

#[divan::bench(consts = [1, 4, 16], args = MESSAGES)]
fn kept_tabs<const TABS: usize>(bencher: divan::Bencher, messages: usize) {
    bencher.bench_local(|| mount(TABS, messages, Hidden::Kept));
}

#[divan::bench(consts = [1, 4, 16], args = MESSAGES)]
fn all_tabs<const TABS: usize>(bencher: divan::Bencher, messages: usize) {
    bencher.bench_local(|| mount(TABS, messages, Hidden::All));
}

#[divan::bench(consts = [1, 4, 16], args = MESSAGES)]
fn shown_tab_only<const TABS: usize>(bencher: divan::Bencher, messages: usize) {
    bencher.bench_local(|| mount(TABS, messages, Hidden::None));
}
