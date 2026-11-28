//! The metrics panes: what each turn took, as the relay in a gateway
//! agent's pod measured it and the gateway's store keeps it
//! ([`ukai_acp::metrics`]) — how long turns take and where that time goes,
//! the tokens they take, and which tool calls cost the most. The store sums
//! up every turn of the period chosen ([`TurnsSummary`]); the pane only
//! shows it. [`MetricsPane`] shows one workspace's turns, [`MetricsOverview`]
//! those of every workspace of each gateway the open projects declare, by
//! agent and by model. Reading never starts a pod.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dioxus::prelude::*;
use ukai_acp::metrics::{Group, MetricsReader, Summary, TurnRecord, TurnsSummary};
use ukai_core::metrics::{age_label, count_label, duration_label, percent};
use ukai_core::{PaneContent, ProjectId, ReportSource};

use crate::follow::poll;
use crate::icons::{Icon, Svg};
use crate::layout::show_content;
use crate::projects::{AppSignals, at_place};

/// How often an open metrics pane reads the turns again.
const RELOAD: Duration = Duration::from_secs(30);

/// Turns listed one by one, newest first.
const RECENT: u32 = 50;

/// How far back a pane sums up the turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Day,
    Week,
    Month,
    All,
}

impl Period {
    const ALL: [Self; 4] = [Self::Day, Self::Week, Self::Month, Self::All];

    fn label(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::All => "All",
        }
    }

    /// The first moment it takes in, in milliseconds since the Unix epoch.
    fn since(self, now_ms: u64) -> Option<u64> {
        let days = match self {
            Self::Day => 1,
            Self::Week => 7,
            Self::Month => 30,
            Self::All => return None,
        };
        Some(now_ms.saturating_sub(days * 24 * 3_600_000))
    }
}

/// Show the turns of `place` in `project`: focus the pane already showing
/// them, else open one where [`launch_target`] puts it.
pub fn open_metrics(app: AppSignals, project: &ProjectId, place: &ReportSource) {
    show_content(
        app,
        PaneContent::Metrics {
            project: project.clone(),
            place: place.clone(),
        },
    );
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
    at_place(
        app,
        project,
        place,
        |_| Err("only a gateway agent's turns are measured".to_owned()),
        |spec, workspace, _| MetricsReader::resolve(spec, ukai_acp::system_lookup, Some(workspace)),
    )
}

/// Show the turns of every gateway the open projects declare: focus the
/// pane already showing them, else open one where [`launch_target`] puts it.
pub fn open_overview(app: AppSignals) {
    show_content(app, PaneContent::MetricsOverview);
}

/// Whether a pane shows the turns of every gateway.
pub fn overview_open(layout: &ukai_core::Layout) -> bool {
    layout
        .contents()
        .any(|(_, content)| *content == PaneContent::MetricsOverview)
}

/// A reader of every workspace of each gateway the open projects declare,
/// one per gateway, named as the first project to declare it does, and why
/// any could not be made.
fn gateway_readers(app: AppSignals) -> (Vec<(String, MetricsReader)>, Vec<String>) {
    let mut readers: Vec<(String, MetricsReader)> = Vec::new();
    let mut problems = Vec::new();
    let projects = app.projects.peek();
    let configs = app.configs.peek();
    for project in projects.iter() {
        let Some(Ok(config)) = configs.get(&project.id) else {
            continue;
        };
        // `[gateways]` by their names, then any `[agents]` reaching a
        // gateway directly, by the agent's.
        let specs = config
            .gateways
            .iter()
            .map(|(name, gateway)| (name.clone(), gateway.agent("_")))
            .chain(config.agents.iter().map(|(id, spec)| {
                let name = id
                    .split_once('/')
                    .map_or(id.as_str(), |(gateway, _)| gateway);
                (name.to_owned(), spec.clone())
            }));
        for (name, spec) in specs {
            match MetricsReader::every_workspace(&spec, ukai_acp::system_lookup) {
                Ok(Some(reader)) => {
                    if !readers.iter().any(|(_, r)| r.gateway() == reader.gateway()) {
                        readers.push((name, reader));
                    }
                }
                Ok(None) => {}
                Err(error) => problems.push(format!("{}: {name}: {error}", project.name())),
            }
        }
    }
    problems.sort();
    problems.dedup();
    (readers, problems)
}

/// What a pane last read: each gateway's summary, by the gateway's name, or
/// why it could not be read.
#[derive(Debug, Clone, PartialEq)]
enum Read {
    Loading,
    Loaded(Vec<(String, Result<TurnsSummary, String>)>),
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
    let revision = use_signal(|| 0_u64);
    let period = use_signal(|| Period::Week);
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
                    let since = period.peek().since(now_ms());
                    let now = match reader_at(app, &project, &place) {
                        Ok(reader) => Read::Loaded(vec![(
                            String::new(),
                            reader
                                .summarize(since, RECENT)
                                .await
                                .map_err(|error| error.to_string()),
                        )]),
                        Err(problem) => Read::Failed(problem),
                    };
                    if *read.peek() != now {
                        read.set(now);
                    }
                }
            })
        }
    });
    rsx! {
        MetricsBody {
            read: read(),
            revision,
            period,
            every: false,
            empty: "No turns measured in this period. The relay in the workspace's pod records each turn once it ends.",
        }
    }
}

/// The turns of every workspace of each gateway the open projects declare.
#[component]
pub fn MetricsOverview() -> Element {
    let app = use_context::<AppSignals>();
    let mut read = use_signal(|| Read::Loading);
    let revision = use_signal(|| 0_u64);
    let period = use_signal(|| Period::Week);
    use_future(move || {
        poll(revision, RELOAD, move || async move {
            let since = period.peek().since(now_ms());
            let (readers, problems) = gateway_readers(app);
            let now = if readers.is_empty() && problems.is_empty() {
                Read::Failed(
                    "No open project declares a gateway agent; only a gateway's relays measure turns."
                        .to_owned(),
                )
            } else {
                let read = futures::future::join_all(
                    readers
                        .iter()
                        .map(|(_, reader)| reader.summarize(since, RECENT)),
                )
                .await;
                Read::Loaded(
                    readers
                        .into_iter()
                        .zip(read)
                        .map(|((name, _), read)| (name, read.map_err(|error| error.to_string())))
                        .chain(
                            problems
                                .into_iter()
                                .map(|problem| (String::new(), Err(problem))),
                        )
                        .collect(),
                )
            };
            if *read.peek() != now {
                read.set(now);
            }
        })
    });
    rsx! {
        MetricsBody {
            read: read(),
            revision,
            period,
            every: true,
            empty: "No turns measured in this period. The relays in the gateways' pods record each turn once it ends.",
        }
    }
}

/// The toolbar and what a metrics pane read: one gateway's summary, or each
/// gateway's under its name. With `every`, of many workspaces and agents, so
/// it also groups them by agent and by model.
#[component]
fn MetricsBody(
    read: Read,
    revision: Signal<u64>,
    period: Signal<Period>,
    every: bool,
    empty: String,
) -> Element {
    let mut revision = revision;
    let mut period = period;
    let (turns, sessions) = match &read {
        Read::Loaded(read) => read
            .iter()
            .filter_map(|(_, read)| read.as_ref().ok())
            .fold((0, 0), |(t, s), summary| {
                (t + summary.summary.turns, s + summary.summary.sessions)
            }),
        _ => (0, 0),
    };
    rsx! {
        div { class: "pane-body metrics-pane",
            div { class: "diff-toolbar",
                div { class: "segmented",
                    for choice in Period::ALL {
                        button {
                            key: "{choice.label()}",
                            class: if period() == choice { "active" } else { "" },
                            title: "Sum up the turns of this period",
                            onclick: move |_| {
                                period.set(choice);
                                *revision.write() += 1;
                            },
                            "{choice.label()}"
                        }
                    }
                }
                if turns > 0 {
                    span { class: "muted", "{turns} turns · {sessions} sessions" }
                }
                span { class: "spacer" }
                button {
                    class: "icon",
                    title: "Read the turns again",
                    onclick: move |_| *revision.write() += 1,
                    Svg { icon: Icon::Reload }
                }
            }
            match read {
                Read::Loading => rsx! {
                    p { class: "muted hint diff-hint", "Reading the turns…" }
                },
                Read::Failed(problem) => rsx! {
                    pre { class: "config-error diff-hint", "{problem}" }
                },
                Read::Loaded(read) => {
                    let titled = read.len() > 1;
                    rsx! {
                        div { class: "metrics-body",
                            for (index, (name, summary)) in read.into_iter().enumerate() {
                                div { key: "{index}", class: "metrics-gateway",
                                    if titled && !name.is_empty() {
                                        h2 { class: "metrics-gateway-name", "{name}" }
                                    }
                                    match summary {
                                        Err(problem) => rsx! {
                                            pre { class: "config-error metrics-problem",
                                                if name.is_empty() {
                                                    "{problem}"
                                                } else {
                                                    "{name}: {problem}"
                                                }
                                            }
                                        },
                                        Ok(summary) if summary.summary.turns == 0 => rsx! {
                                            p { class: "muted hint", "{empty}" }
                                        },
                                        Ok(summary) => rsx! {
                                            SummaryView { summary, every }
                                        },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One gateway's summary of the turns.
#[component]
fn SummaryView(summary: TurnsSummary, every: bool) -> Element {
    let mut opened = use_signal(|| None::<usize>);
    let TurnsSummary {
        summary: all,
        agents,
        models,
        tools,
        slowest,
        recent,
    } = summary;
    let now = now_ms();
    let split_total = all.split.total();
    let tokens = all.tokens;
    let columns = if every { "8" } else { "7" };
    let models_named = models.len() > 1 || models.first().is_some_and(|g| g.name != "unknown");
    let mut outcomes = all
        .outcomes
        .iter()
        .map(|(outcome, count)| format!("{count} {outcome}"));
    let outcome_first = outcomes.next().unwrap_or_default();
    let outcome_rest = outcomes.collect::<Vec<_>>().join(" · ");
    rsx! {
        div { class: "metrics-tiles",
            Tile {
                label: "Turn time",
                value: duration_label(all.duration.median),
                detail: format!(
                    "median · p90 {} · max {}",
                    duration_label(all.duration.p90),
                    duration_label(all.duration.max),
                ),
            }
            Tile {
                label: "First output",
                value: duration_label(all.first_output.median),
                detail: format!("median · p90 {}", duration_label(all.first_output.p90)),
            }
            Tile {
                label: "Tokens",
                value: format!("{} in · {} out", count_label(tokens.input), count_label(tokens.output)),
                detail: match tokens.cache_share() {
                    Some(share) => {
                        format!(
                            "{} thinking · {:.0}% of input cached · {} of {} turns",
                            count_label(tokens.thought),
                            share * 100.0,
                            tokens.turns,
                            all.turns,
                        )
                    }
                    None => "the agent reports none".to_owned(),
                },
            }
            Tile {
                label: "Cost",
                value: cost_label(&all),
                detail: "the sessions' latest totals".to_owned(),
            }
            Tile {
                label: "Outcomes",
                value: outcome_first,
                detail: outcome_rest,
            }
        }
        if every && !agents.is_empty() {
            GroupTable { title: "By agent", label: "Agent", groups: agents }
        }
        if every && models_named {
            GroupTable { title: "By model", label: "Model", groups: models }
        }
        section { class: "metrics-section",
            h3 { "Where the time went" }
            div { class: "metrics-bar",
                for (index, (name, ms)) in all.split.parts().into_iter().enumerate() {
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
                for (index, (name, ms)) in all.split.parts().into_iter().enumerate() {
                    span { key: "{name}", class: "item",
                        span { class: "swatch part-{index}" }
                        "{name} {duration_label(ms)} · {percent(ms, split_total)}%"
                    }
                }
            }
        }
        if !tools.is_empty() {
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
                        for kind in tools {
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
        if !slowest.is_empty() {
            section { class: "metrics-section",
                h3 { "Slowest tool calls" }
                table { class: "metrics-table",
                    tbody {
                        for (index, call) in slowest.into_iter().enumerate() {
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
                        if every {
                            th { "Agent" }
                        }
                        th { "Session" }
                        th { class: "num", "Time" }
                        th { class: "num", "First output" }
                        th { class: "num", "Tokens in / out" }
                        th { class: "num", "Tools" }
                        th { "Outcome" }
                    }
                }
                tbody {
                    for (index, turn) in recent.into_iter().enumerate() {
                        tr {
                            key: "{index}",
                            class: if opened() == Some(index) { "turn open" } else { "turn" },
                            title: "Show its tool calls",
                            onclick: move |_| {
                                let now = if opened() == Some(index) { None } else { Some(index) };
                                opened.set(now);
                            },
                            td { "{age_label(now, turn.started_at_ms)}" }
                            if every {
                                td { class: "muted", {turn.agent.clone().unwrap_or_default()} }
                            }
                            td { class: "mono", "{short(turn.session_id.as_deref())}" }
                            td { class: "num", "{duration_label(turn.duration_ms)}" }
                            td { class: "num",
                                {turn.first_output_ms.map(duration_label).unwrap_or_else(|| "—".to_owned())}
                            }
                            td { class: "num",
                                {
                                    turn.tokens
                                        .map_or_else(
                                            || "—".to_owned(),
                                            |t| format!("{} / {}", count_label(t.input), count_label(t.output)),
                                        )
                                }
                            }
                            td { class: "num", "{turn.tools.len()}" }
                            td {
                                class: if turn.error.is_some() { "failed" } else { "" },
                                title: turn.error.clone().unwrap_or_default(),
                                "{turn.outcome}"
                            }
                        }
                        if opened() == Some(index) {
                            tr { key: "{index}-tools", class: "turn-tools",
                                td { colspan: columns,
                                    TurnDetail { turn: turn.clone() }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The sessions' latest costs, summed, or a dash when there is none.
fn cost_label(summary: &Summary) -> String {
    match &summary.cost {
        Some((amount, currency)) => format!("{amount:.2} {currency}"),
        None => "—".to_owned(),
    }
}

/// The turns grouped by agent or model, one row each.
#[component]
fn GroupTable(title: String, label: String, groups: Vec<Group>) -> Element {
    rsx! {
        section { class: "metrics-section",
            h3 { "{title}" }
            table { class: "metrics-table",
                thead {
                    tr {
                        th { "{label}" }
                        th { class: "num", "Turns" }
                        th { class: "num", "Sessions" }
                        th { class: "num", "Median" }
                        th { class: "num", "p90" }
                        th { class: "num", "First output" }
                        th { class: "num", "Tokens in / out" }
                        th { class: "num", "Cost" }
                        th { class: "num", "Failed" }
                    }
                }
                tbody {
                    for group in groups {
                        tr { key: "{group.name}",
                            td { class: "mono", "{group.name}" }
                            td { class: "num", "{group.summary.turns}" }
                            td { class: "num", "{group.summary.sessions}" }
                            td { class: "num", "{duration_label(group.summary.duration.median)}" }
                            td { class: "num", "{duration_label(group.summary.duration.p90)}" }
                            td { class: "num", "{duration_label(group.summary.first_output.median)}" }
                            td { class: "num",
                                "{count_label(group.summary.tokens.input)} / {count_label(group.summary.tokens.output)}"
                            }
                            td { class: "num", "{cost_label(&group.summary)}" }
                            {
                                let failed = group.summary.ended("failed");
                                rsx! {
                                    td { class: if failed > 0 { "num failed" } else { "num" }, "{failed}" }
                                }
                            }
                        }
                    }
                }
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
                div {
                    key: "{index}",
                    class: if tool.parent.is_some() { "turn-tool sub" } else { "turn-tool" },
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
