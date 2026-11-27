//! What a chunk of an agent's message costs while it streams in: the
//! transcript is written and the pane renders what changed.
//!
//! `chunk` should cost about the same however many entries came before the
//! message, since only the page of entries the message is in is drawn
//! again; and about the same however long the message already is, since
//! only what follows its blocks that stand is parsed and drawn again.
//! `tool_update` is a tool call changing state in the middle of the
//! transcript.
//!
//! `mise run bench`, or `cargo bench -p ukai-app --bench stream`.

use dioxus::dioxus_core::NoOpMutations;
use dioxus::prelude::*;
use ukai_acp::{SessionEvent, ToolStatus};
use ukai_app::bench::{Entries, use_app};
use ukai_core::Transcript;
use ukai_core::tail::Tail;

use crate::common::MESSAGE;

mod common;

#[global_allocator]
static ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();

fn main() {
    divan::main();
}

/// Turns already in the transcript: a prompt, a tool call, and a message each.
const TURNS: [usize; 3] = [0, 20, 200];

/// Times [`MESSAGE`] is in the message that streams in.
const LENGTHS: [usize; 3] = [1, 10, 50];

#[component]
fn Root() -> Element {
    use_app();
    let transcript = use_context::<Signal<Transcript>>();
    let tail = use_signal(Tail::default);
    rsx! {
        Entries { pane: 0, transcript, tail }
    }
}

fn said(text: &str) -> SessionEvent {
    SessionEvent::AgentMessage {
        text: text.to_owned(),
    }
}

fn tool(id: usize, status: ToolStatus) -> SessionEvent {
    SessionEvent::ToolCall {
        id: id.to_string(),
        title: "crates/ukai-app/src/layout.rs".to_owned(),
        kind: "read".to_owned(),
        status,
        diffs: Vec::new(),
    }
}

/// A pane showing `turns` turns and then a message of `length` [`MESSAGE`]s
/// that is still streaming in.
fn pane(turns: usize, length: usize) -> (VirtualDom, Signal<Transcript>) {
    let mut transcript = Transcript::default();
    for turn in 0..turns {
        transcript.push_prompt("What changed in the layout?", Vec::new());
        transcript.apply(tool(turn, ToolStatus::Completed));
        transcript.apply(said(MESSAGE));
        transcript.apply(SessionEvent::TurnEnded {
            stop_reason: "end_turn".to_owned(),
            tokens: None,
        });
    }
    transcript.push_prompt("And then?", Vec::new());
    for _ in 0..length {
        transcript.apply(said(MESSAGE));
    }
    let mut dom = VirtualDom::new(Root);
    let transcript = dom.in_runtime(|| Signal::new_in_scope(transcript, ScopeId::ROOT));
    dom.provide_root_context(transcript);
    dom.rebuild_in_place();
    (dom, transcript)
}

fn apply(dom: &mut VirtualDom, mut transcript: Signal<Transcript>, event: SessionEvent) {
    dom.in_runtime(|| transcript.write().apply(event));
    dom.render_immediate(&mut NoOpMutations);
}

/// A few words more of the last message.
#[divan::bench(args = TURNS, consts = LENGTHS)]
fn chunk<const LENGTH: usize>(bencher: divan::Bencher, turns: usize) {
    bencher
        .with_inputs(|| pane(turns, LENGTH))
        .bench_local_refs(|(dom, transcript)| {
            apply(dom, *transcript, said(" and a few words more"));
        });
}

/// A tool call in the middle of the transcript finishing.
#[divan::bench(args = [20, 200])]
fn tool_update(bencher: divan::Bencher, turns: usize) {
    bencher
        .with_inputs(|| pane(turns, 1))
        .bench_local_refs(|(dom, transcript)| {
            apply(dom, *transcript, tool(turns / 2, ToolStatus::Failed));
        });
}
