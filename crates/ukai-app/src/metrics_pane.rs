//! The metrics pane: what each turn of a gateway agent's workspace took, as
//! the relay in its pod measured it and the gateway's store keeps it
//! ([`ukai_acp::metrics`]) — how long turns take and where that time goes,
//! the tokens they take, and which tool calls cost the most
//! ([`ukai_core::metrics::Summary`]). Reading never starts the workspace's pod.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dioxus::prelude::*;
use ukai_acp::metrics::{MetricsReader, TurnRecord};
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_core::metrics::{Summary, age_label, count_label, duration_label, percent};
use ukai_core::{PaneContent, ProjectId, ReportSource};

use crate::follow::poll;
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::projects::{AppSignals, project_context};

/// How often an open metrics pane reads the turns again.
const RELOAD: Duration = Duration::from_secs(30);

/// Turns listed one by one, newest first.
const RECENT: usize = 50;

/// Show the turns of `place` in `project`: focus the pane already showing
/// them, else open one where [`launch_target`] puts it.
pub fn open_metrics(app: AppSignals, project: &ProjectId, place: &ReportSource) {
    let mut layout = app.layout;
    let content = PaneContent::Metrics {
        project: project.clone(),
        place: place.clone(),
    };
    let shown = layout
        .peek()
        .contents()
        .find_map(|(pane, open)| (*open == content).then_some(pane));
    if let Some(pane) = shown {
        layout.write().focus(pane);
        return;
    }
    if let Some(pane) = launch_target(app) {
        let mut layout = layout.write();
        layout.set_content(pane, content);
        layout.focus(pane);
    }
    let mut projects = app.projects;
    projects.write().select(project);
}

/// Whether a metrics pane shows the turns of `place` in `project`.
pub fn metrics_open(layout: &ukai_core::Layout, project: &ProjectId, place: &ReportSource) -> bool {
    layout.contents().any(|(_, content)| {
        matches!(content, PaneContent::Metrics { project: p, place: q } if p == project && q == place)
    })
}

/// The reader of the turns `place` names in `project`.
fn reader_at(
    app: AppSignals,
    project: &ProjectId,
    place: &ReportSource,
) -> Result<MetricsReader, String> {
    let ReportSource::Agent { agent, workspace } = place else {
        return Err("only a gateway agent's turns are measured".to_owned());
    };
    let context = project_context(&app.projects.peek(), &app.configs.peek(), project)?;
    let spec = context
        .config
        .agent(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared anymore"))?;
    // A place names the shared workspace by leaving the key out.
    let workspace = workspace.clone().unwrap_or(PaneWorkspace {
        key: String::new(),
        scope: WorkspaceScope::Shared,
    });
    MetricsReader::resolve(&spec, ukai_acp::system_lookup, Some(&workspace))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("agent `{agent}` names no gateway agent"))
}

/// What the pane last read.
#[derive(Debug, Clone, PartialEq)]
enum Read {
    Loading,
    Loaded(Vec<TurnRecord>),
    Failed(String),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// The first characters of a session id, enough to tell sessions apart.
fn short(session: Option<&str>) -> String {
    session.map_or_else(|| "?".to_owned(), |id| id.chars().take(8).collect())
}

#[component]
pub fn MetricsPane(project: ProjectId, place: ReportSource) -> Element {
    let app = use_context::<AppSignals>();
    let mut read = use_signal(|| Read::Loading);
    let mut revision = use_signal(|| 0_u64);
    let mut opened = use_signal(|| None::<usize>);
    use_future({
        let project = project.clone();
        let place = place.clone();
        move || {
            let project = project.clone();
            let place = place.clone();
            poll(revision, RELOAD, move || {
                let project = project.clone();
                let place = place.clone();
                async move {
                    let now = match reader_at(app, &project, &place) {
                        Ok(reader) => match reader.list(None, 0).await {
                            Ok(turns) => Read::Loaded(turns),
                            Err(error) => Read::Failed(error.to_string()),
                        },
                        Err(problem) => Read::Failed(problem),
                    };
                    if *read.peek() != now {
                        read.set(now);
                    }
                }
            })
        }
    });

    let current = read();
    let turns = match &current {
        Read::Loaded(turns) => turns.clone(),
        _ => Vec::new(),
    };
    let summary = Summary::of(&turns);
    let now = now_ms();
    let split_total = summary.split.total();
    let tokens = summary.tokens;
    rsx! {
        div { class: "pane-body metrics-pane",
            div { class: "diff-toolbar",
                if summary.turns > 0 {
                    span { class: "muted",
                        "{summary.turns} turns · {summary.sessions} sessions"
                    }
                }
                span { class: "spacer" }
                button {
                    class: "icon",
                    title: "Read the turns again",
                    onclick: move |_| *revision.write() += 1,
                    Svg { icon: Icon::Reload }
                }
            }
            match current {
                Read::Loading => rsx! {
                    p { class: "muted hint diff-hint", "Reading the turns…" }
                },
                Read::Failed(problem) => rsx! {
                    pre { class: "config-error diff-hint", "{problem}" }
                },
                Read::Loaded(_) if turns.is_empty() => rsx! {
                    p { class: "muted hint diff-hint",
                        "No turns measured yet. The relay in the workspace's pod records each turn once it ends."
                    }
                },
                Read::Loaded(_) => rsx! {
                    div { class: "metrics-body",
                        div { class: "metrics-tiles",
                            Tile {
                                label: "Turn time",
                                value: duration_label(summary.duration.median),
                                detail: format!(
                                    "median · p90 {} · max {}",
                                    duration_label(summary.duration.p90),
                                    duration_label(summary.duration.max),
                                ),
                            }
                            Tile {
                                label: "First output",
                                value: duration_label(summary.first_output.median),
                                detail: format!("median · p90 {}", duration_label(summary.first_output.p90)),
                            }
                            Tile {
                                label: "Tokens",
                                value: format!("{} in · {} out", count_label(tokens.input), count_label(tokens.output)),
                                detail: match tokens.cache_share() {
                                    Some(share) => format!(
                                        "{} thinking · {:.0}% of input cached · {} of {} turns",
                                        count_label(tokens.thought),
                                        share * 100.0,
                                        tokens.turns,
                                        summary.turns,
                                    ),
                                    None => "the agent reports none".to_owned(),
                                },
                            }
                            Tile {
                                label: "Cost",
                                value: match &summary.cost {
                                    Some((amount, currency)) => format!("{amount:.2} {currency}"),
                                    None => "—".to_owned(),
                                },
                                detail: "the sessions' latest totals".to_owned(),
                            }
                            Tile {
                                label: "Outcomes",
                                value: summary
                                    .outcomes
                                    .first()
                                    .map(|(outcome, count)| format!("{count} {outcome}"))
                                    .unwrap_or_default(),
                                detail: summary
                                    .outcomes
                                    .iter()
                                    .skip(1)
                                    .map(|(outcome, count)| format!("{count} {outcome}"))
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            }
                        }
                        section { class: "metrics-section",
                            h3 { "Where the time went" }
                            div { class: "metrics-bar",
                                for (index, (name, ms)) in summary.split.parts().into_iter().enumerate() {
                                    if ms > 0 {
                                        span {
                                            key: "{name}",
                                            class: "part part-{index}",
                                            style: "flex-grow: {ms}",
                                            title: "{name}: {duration_label(ms)} ({percent(ms, split_total)}%)",
                                        }
                                    }
                                }
                            }
                            div { class: "metrics-legend",
                                for (index, (name, ms)) in summary.split.parts().into_iter().enumerate() {
                                    span { key: "{name}", class: "item",
                                        span { class: "swatch part-{index}" }
                                        "{name} {duration_label(ms)} · {percent(ms, split_total)}%"
                                    }
                                }
                            }
                        }
                        if !summary.tools.is_empty() {
                            section { class: "metrics-section",
                                h3 { "Tool calls by kind" }
                                table { class: "metrics-table",
                                    thead {
                                        tr {
                                            th { "Kind" }
                                            th { class: "num", "Calls" }
                                            th { class: "num", "Failed" }
                                            th { class: "num", "Total" }
                                            th { class: "num", "Median" }
                                            th { class: "num", "p90" }
                                            th { class: "num", "Max" }
                                        }
                                    }
                                    tbody {
                                        for kind in summary.tools.iter().cloned() {
                                            tr { key: "{kind.kind}",
                                                td { "{kind.kind}" }
                                                td { class: "num", "{kind.calls}" }
                                                td { class: if kind.failed > 0 { "num failed" } else { "num" },
                                                    "{kind.failed}"
                                                }
                                                td { class: "num", "{duration_label(kind.spread.total)}" }
                                                td { class: "num", "{duration_label(kind.spread.median)}" }
                                                td { class: "num", "{duration_label(kind.spread.p90)}" }
                                                td { class: "num", "{duration_label(kind.spread.max)}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if !summary.slowest.is_empty() {
                            section { class: "metrics-section",
                                h3 { "Slowest tool calls" }
                                table { class: "metrics-table",
                                    tbody {
                                        for (index, call) in summary.slowest.iter().cloned().enumerate() {
                                            tr { key: "{index}",
                                                td { class: "num", "{duration_label(call.duration_ms)}" }
                                                td { class: "muted", "{call.kind}" }
                                                td { class: "title", title: "{call.title}", "{call.title}" }
                                                td { class: "muted mono", "{short(call.session.as_deref())}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        section { class: "metrics-section",
                            h3 { "Recent turns" }
                            table { class: "metrics-table",
                                thead {
                                    tr {
                                        th { "Started" }
                                        th { "Session" }
                                        th { class: "num", "Time" }
                                        th { class: "num", "First output" }
                                        th { class: "num", "Tokens in / out" }
                                        th { class: "num", "Tools" }
                                        th { "Outcome" }
                                    }
                                }
                                tbody {
                                    for (index, turn) in turns.iter().take(RECENT).cloned().enumerate() {
                                        tr {
                                            key: "{index}",
                                            class: if opened() == Some(index) { "turn open" } else { "turn" },
                                            title: "Show its tool calls",
                                            onclick: move |_| {
                                                let now = if opened() == Some(index) { None } else { Some(index) };
                                                opened.set(now);
                                            },
                                            td { "{age_label(now, turn.started_at_ms)}" }
                                            td { class: "mono", "{short(turn.session_id.as_deref())}" }
                                            td { class: "num", "{duration_label(turn.duration_ms)}" }
                                            td { class: "num",
                                                {turn.first_output_ms.map(duration_label).unwrap_or_else(|| "—".to_owned())}
                                            }
                                            td { class: "num",
                                                {turn.tokens.map_or_else(
                                                    || "—".to_owned(),
                                                    |t| format!("{} / {}", count_label(t.input), count_label(t.output)),
                                                )}
                                            }
                                            td { class: "num", "{turn.tools.len()}" }
                                            td { class: if turn.error.is_some() { "failed" } else { "" },
                                                title: turn.error.clone().unwrap_or_default(),
                                                "{turn.outcome}"
                                            }
                                        }
                                        if opened() == Some(index) {
                                            tr { key: "{index}-tools", class: "turn-tools",
                                                td { colspan: "7",
                                                    TurnDetail { turn: turn.clone() }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
}

#[component]
fn Tile(label: String, value: String, detail: String) -> Element {
    rsx! {
        div { class: "metrics-tile",
            div { class: "label", "{label}" }
            div { class: "value", "{value}" }
            if !detail.is_empty() {
                div { class: "detail", "{detail}" }
            }
        }
    }
}

/// One turn's phases and tool calls.
#[component]
fn TurnDetail(turn: TurnRecord) -> Element {
    let settings = [turn.model.clone(), turn.effort.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    rsx! {
        div { class: "turn-detail",
            div { class: "muted",
                "thinking {duration_label(turn.thinking_ms)} · reply {duration_label(turn.message_ms)} · permission {duration_label(turn.permission_ms)}"
                if !settings.is_empty() {
                    " · {settings}"
                }
                if turn.joined {
                    " · joined while running"
                }
                if turn.reconnected {
                    " · reconnected"
                }
            }
            if turn.tools.is_empty() {
                div { class: "muted", "No tool calls." }
            }
            for (index, tool) in turn.tools.iter().cloned().enumerate() {
                div { key: "{index}", class: if tool.parent.is_some() { "turn-tool sub" } else { "turn-tool" },
                    span { class: "num",
                        {tool.duration_ms.map(duration_label).unwrap_or_else(|| "—".to_owned())}
                    }
                    span { class: "muted", "{tool.kind}" }
                    span { class: "title", title: "{tool.title}", "{tool.title}" }
                    span { class: if tool.status == "failed" { "failed" } else { "muted" }, "{tool.status}" }
                }
            }
        }
    }
}
