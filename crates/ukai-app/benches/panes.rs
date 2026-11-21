//! What rearranging panes costs while they show long transcripts.
//!
//! A split, move, resize, or tab switch should only re-place or hide the
//! panes: each costs about
//! the same however long the transcripts are. If it grows with them, panes
//! are being remounted and their transcripts rebuilt again
//! (`layout::tests::rearranging_panes_never_remounts_one` fails then too).
//!
//! Each also reports what it allocates: a few kilobytes, likewise.
//!
//! `mise run bench`, or `cargo bench -p ukai-app --bench panes`.

use dioxus::dioxus_core::NoOpMutations;
use dioxus::prelude::*;
use ukai_app::bench::{Markdown, PlacedPanes, use_kept_panes};
use ukai_core::{Direction, Layout, PaneId, Side};

use crate::common::MESSAGE;

mod common;

/// Also counts what each operation allocates, next to its time.
#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// Messages in each pane's transcript.
const MESSAGES: [usize; 3] = [0, 20, 100];

/// Panes the workbench starts with; a pane split off later opens empty,
/// as a new pane does in the app.
const PANES: PaneId = 4;

#[component]
fn Transcript(pane: PaneId) -> Element {
    let messages = if pane < PANES {
        use_context::<usize>()
    } else {
        0
    };
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
    let panes = use_kept_panes(layout);
    rsx! {
        PlacedPanes { panes, pane: |pane| rsx! {
            Transcript { pane }
        } }
    }
}

/// A tab of [`PANES`] panes, each showing `messages` messages, rendered.
fn workbench(messages: usize) -> (VirtualDom, Signal<Layout>) {
    let mut dom = VirtualDom::new(Root);
    let mut layout = dom.in_runtime(|| Signal::new_in_scope(Layout::default(), ScopeId::ROOT));
    dom.in_runtime(|| {
        let mut layout = layout.write();
        layout.split(0, Direction::Horizontal);
        layout.split(0, Direction::Vertical);
        layout.split(1, Direction::Vertical);
    });
    dom.provide_root_context(messages);
    dom.provide_root_context(layout);
    dom.rebuild_in_place();
    (dom, layout)
}

/// Change the layout and render what changed.
fn rearrange(dom: &mut VirtualDom, mut layout: Signal<Layout>, change: impl FnOnce(&mut Layout)) {
    dom.in_runtime(|| change(&mut layout.write()));
    dom.render_immediate(&mut NoOpMutations);
}

#[divan::bench(args = MESSAGES)]
fn split(bencher: divan::Bencher, messages: usize) {
    bencher
        .with_inputs(|| workbench(messages))
        .bench_local_refs(|(dom, layout)| {
            rearrange(dom, *layout, |layout| {
                layout.split(0, Direction::Horizontal);
            });
        });
}

#[divan::bench(args = MESSAGES)]
fn move_pane(bencher: divan::Bencher, messages: usize) {
    bencher
        .with_inputs(|| workbench(messages))
        .bench_local_refs(|(dom, layout)| {
            rearrange(dom, *layout, |layout| {
                layout.move_pane(0, 3, Side::Right);
            });
        });
}

#[divan::bench(args = MESSAGES)]
fn resize(bencher: divan::Bencher, messages: usize) {
    bencher
        .with_inputs(|| workbench(messages))
        .bench_local_refs(|(dom, layout)| {
            rearrange(dom, *layout, |layout| {
                layout.resize(&[], 0, 0.3);
            });
        });
}

/// Back to the tab of long transcripts from another.
#[divan::bench(args = MESSAGES)]
fn switch_tab(bencher: divan::Bencher, messages: usize) {
    bencher
        .with_inputs(|| {
            let (mut dom, layout) = workbench(messages);
            rearrange(&mut dom, layout, |layout| {
                layout.new_tab();
            });
            (dom, layout)
        })
        .bench_local_refs(|(dom, layout)| {
            rearrange(dom, *layout, |layout| layout.select_tab(0));
        });
}

/// For scale: what one remount of a pane costs, building its transcript.
#[divan::bench(args = MESSAGES)]
fn mount_transcript(bencher: divan::Bencher, messages: usize) {
    bencher.bench_local(|| {
        let mut dom = VirtualDom::new(|| rsx! { Transcript { pane: 0 } });
        dom.provide_root_context(messages);
        dom.rebuild_in_place();
        dom
    });
}
