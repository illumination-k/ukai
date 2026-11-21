//! The heap the app's view of panes keeps: what each tab's panes hold while
//! mounted, and what keeping the tabs not shown mounted costs, since
//! switching tabs hides panes rather than dropping them.
//!
//! Each bench mounts a workbench of `TABS` tabs of four panes, the last one
//! shown, and returns it: divan drops it after counting, so `alloc` less
//! `dealloc` in a row is what the mounted workbench keeps. `tabs_mounted`
//! is the app; `shown_tab_only` mounts the shown tab alone, as the app did
//! before, and the difference is the price of keeping the others. Only the
//! Rust side is counted, the virtual DOM; the web view's own DOM for the
//! same elements is not.
//!
//! `mise run bench`, or `cargo bench -p ukai-app --bench memory`.

use dioxus::prelude::*;
use ukai_app::bench::{Markdown, PlacedPanes};
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

/// Whether the tabs not shown are mounted, as the app does, or dropped.
#[derive(Clone, Copy)]
struct Hidden(bool);

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
    let Hidden(mounted) = use_context::<Hidden>();
    let mut panes = layout.read().placed();
    if !mounted {
        panes.retain(|placed| placed.shown);
    }
    rsx! {
        PlacedPanes { panes, pane: |pane| rsx! {
            Transcript { pane }
        } }
    }
}

/// `tabs` tabs of [`PANES`] panes each, the last one shown.
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

/// The workbench rendered, kept for divan to drop after counting.
fn mount(tabs: usize, messages: usize, hidden: Hidden) -> VirtualDom {
    let mut dom = VirtualDom::new(Root);
    let signal = dom.in_runtime(|| Signal::new_in_scope(layout(tabs), ScopeId::ROOT));
    dom.provide_root_context(messages);
    dom.provide_root_context(hidden);
    dom.provide_root_context(signal);
    dom.rebuild_in_place();
    dom
}

#[divan::bench(consts = [1, 4, 16], args = MESSAGES)]
fn tabs_mounted<const TABS: usize>(bencher: divan::Bencher, messages: usize) {
    bencher.bench_local(|| mount(TABS, messages, Hidden(true)));
}

#[divan::bench(consts = [1, 4, 16], args = MESSAGES)]
fn shown_tab_only<const TABS: usize>(bencher: divan::Bencher, messages: usize) {
    bencher.bench_local(|| mount(TABS, messages, Hidden(false)));
}
