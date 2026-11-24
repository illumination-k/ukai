//! The dashboard of a gateway agent's pull requests (see "PR ごとの
//! メトリクス" in `docs/design.md`): per branch its sessions worked on, the
//! time, tokens, and cost of their turns and the commands they ran, joined
//! with the branch's pull request and CI runs. The gateway sums and joins;
//! this draws it, and `ukai_core::metrics` writes the numbers.

use std::time::Duration;

use dioxus::prelude::*;
use time::OffsetDateTime;
use ukai_acp::metrics::{Branch, MetricsEndpoint, ToolStat};
use ukai_config::AgentSpec;
use ukai_core::metrics::{cost_label, count_label, duration_label, lead_millis, totals};
use ukai_core::{PaneContent, PaneId, ProjectId};

use crate::follow::{self, Workspace, poll};
use crate::icons::{Icon, Svg};
use crate::layout::launch_target;
use crate::projects::AppSignals;
use crate::report_pane::open_external;

/// How often an open dashboard asks the gateway again.
const EVERY: Duration = Duration::from_secs(60);
/// The periods the user picks from, in days.
const PERIODS: [u32; 4] = [7, 30, 90, 365];

/// Show the dashboard of `agent` of `project` where [`launch_target`] puts
/// it, or focus the pane that already shows it.
pub fn open_metrics(app: AppSignals, project: &ProjectId, agent: &str) {
    let mut layout = app.layout;
    let shown = layout.peek().contents().find_map(|(pane, content)| {
        matches!(content, PaneContent::Metrics { project: p, agent: a } if p == project && a == agent)
            .then_some(pane)
    });
    if let Some(pane) = shown {
        layout.write().focus(pane);
        return;
    }
    if let Some(pane) = launch_target(app) {
        let mut layout = layout.write();
        layout.set_content(
            pane,
            PaneContent::Metrics {
                project: project.clone(),
                agent: agent.to_owned(),
            },
        );
        layout.focus(pane);
    }
}

fn resolve(spec: &AgentSpec, workspace: &Workspace) -> Result<MetricsEndpoint, String> {
    follow::found(
        MetricsEndpoint::resolve(spec, ukai_acp::system_lookup),
        || {
            format!(
                "agent `{}` is not a gateway agent, so nothing records its metrics",
                workspace.agent
            )
        },
    )
}

fn now_millis() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// `3h ago`, `2d ago`.
fn ago(millis: i64, now: i64) -> String {
    let elapsed = u64::try_from(now.saturating_sub(millis)).unwrap_or(0);
    if elapsed < 60_000 {
        "just now".to_owned()
    } else {
        let label = duration_label(elapsed);
        let head = label.split(' ').next().unwrap_or(&label);
        format!("{head} ago")
    }
}

/// The dashboard's rows in the sidebar: one per gateway agent of a project.
#[component]
pub fn MetricsNode(project: ProjectId, gateways: Vec<String>, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let focused = match app.layout.read().content(app.layout.read().focused()) {
        Some(PaneContent::Metrics { project: p, agent }) if *p == project => Some(agent.clone()),
        _ => None,
    };
    let single = gateways.len() == 1;
    rsx! {
        for agent in gateways {
            div {
                key: "metrics-{agent}",
                class: if focused.as_deref() == Some(agent.as_str()) { "row report-row open" } else { "row report-row" },
                style: "--depth: {depth}",
                title: "Time, tokens, cost, and CI of each pull request {agent} worked on",
                onclick: {
                    let project = project.clone();
                    let agent = agent.clone();
                    move |_| open_metrics(app, &project, &agent)
                },
                span { class: "row-icon",
                    Svg { icon: Icon::Chart, size: 12 }
                }
                span { class: "name", "PR metrics" }
                if !single {
                    span { class: "meta", "{agent}" }
                }
            }
        }
    }
}

/// The dashboard.
#[component]
pub fn MetricsView(pane: PaneId, project: ProjectId, agent: String) -> Element {
    let app = use_context::<AppSignals>();
    let endpoint = use_memo({
        let agent = agent.clone();
        let project = project.clone();
        move || {
            let workspace = Workspace::of(&project, &agent, None);
            follow::endpoint(&app.configs.read(), &workspace, resolve)
        }
    });
    let mut days = use_signal(|| 30_u32);
    let mut revision = use_signal(|| 0_u64);
    let mut loaded = use_signal(|| None::<Result<Vec<Branch>, String>>);
    let mut busy = use_signal(|| false);
    let mut expanded = use_signal(|| None::<String>);
    let _ = pane;

    use_future(move || async move {
        poll(revision, EVERY, move || async move {
            let period = *days.peek();
            busy.set(true);
            let result = match endpoint.peek().clone() {
                Ok(endpoint) => endpoint
                    .branches(period)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error),
            };
            busy.set(false);
            loaded.set(Some(result));
        })
        .await;
    });

    let now = now_millis();
    let body = match &*loaded.read() {
        None => rsx! {
            div { class: "pane-body placeholder",
                p { class: "muted", "Reading the metrics from the gateway…" }
            }
        },
        Some(Err(error)) => rsx! {
            div { class: "pane-body placeholder",
                p { class: "muted", "{error}" }
            }
        },
        Some(Ok(branches)) => {
            let sum = totals(branches);
            let max_cost = branches
                .iter()
                .map(|b| b.cost.iter().map(|c| c.amount).sum::<f64>())
                .fold(0.0_f64, f64::max);
            let max_agent = branches.iter().map(|b| b.agent_millis).max().unwrap_or(0);
            rsx! {
                div { class: "metrics-tiles",
                    Tile {
                        label: "Pull requests",
                        value: sum.pull_requests.to_string(),
                        detail: format!("{} merged · {} branches", sum.merged, sum.branches),
                    }
                    Tile {
                        label: "Agent time",
                        value: duration_label(sum.agent_millis),
                        detail: format!("{} turns", sum.turns),
                    }
                    Tile {
                        label: "CI time",
                        value: duration_label(sum.ci_millis),
                        detail: "GitHub Actions".to_owned(),
                    }
                    Tile {
                        label: "Tokens",
                        value: count_label(sum.tokens),
                        detail: "as agents report them".to_owned(),
                    }
                    Tile {
                        label: "Cost",
                        value: cost_label(&sum.cost),
                        detail: format!("last {} days", days()),
                    }
                }
                if branches.is_empty() {
                    p { class: "muted metrics-empty",
                        "No turns recorded in this period. Turns are recorded as they end, by relays of this version or later."
                    }
                } else {
                    table { class: "metrics-table",
                        thead {
                            tr {
                                th { "Pull request / branch" }
                                th { class: "num", "Agent" }
                                th { class: "num", "CI" }
                                th { class: "num", "Lead" }
                                th { class: "num", "Turns" }
                                th { class: "num", "Tokens" }
                                th { class: "num", "Cost" }
                                th { class: "num", "Active" }
                            }
                        }
                        tbody {
                            for branch in branches.iter().cloned() {
                                BranchRows {
                                    key: "{branch.branch}",
                                    open: expanded.read().as_deref() == Some(branch.branch.as_str()),
                                    on_toggle: {
                                        let name = branch.branch.clone();
                                        move |()| {
                                            let open = expanded.peek().as_deref() == Some(name.as_str());
                                            expanded.set((!open).then(|| name.clone()));
                                        }
                                    },
                                    now,
                                    max_cost,
                                    max_agent,
                                    branch,
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        div { class: "pane-body metrics",
            div { class: "metrics-head",
                h2 { "PR metrics · {agent}" }
                div { class: "segmented",
                    for period in PERIODS {
                        button {
                            class: if days() == period { "active" } else { "" },
                            onclick: move |_| {
                                days.set(period);
                                *revision.write() += 1;
                            },
                            "{period}d"
                        }
                    }
                }
                button {
                    class: "icon",
                    title: "Read again",
                    disabled: busy(),
                    onclick: move |_| *revision.write() += 1,
                    Svg { icon: Icon::Reload }
                }
            }
            {body}
        }
    }
}

#[component]
fn Tile(label: String, value: String, detail: String) -> Element {
    rsx! {
        div { class: "metrics-tile",
            span { class: "metrics-tile-label", "{label}" }
            span { class: "metrics-tile-value", "{value}" }
            span { class: "metrics-tile-detail", "{detail}" }
        }
    }
}

/// `millis` as a share of `max`, as a bar under the number.
#[allow(clippy::cast_precision_loss)]
fn bar(millis: u64, max: u64) -> String {
    share(millis as f64, max as f64)
}

/// A share of the column's largest value, as a bar under the number.
fn share(value: f64, max: f64) -> String {
    let percent = if max > 0.0 { value / max * 100.0 } else { 0.0 };
    format!("width: {percent:.1}%")
}

#[component]
fn BranchRows(
    branch: Branch,
    open: bool,
    on_toggle: EventHandler<()>,
    now: i64,
    max_cost: f64,
    max_agent: u64,
) -> Element {
    let name = if branch.branch.is_empty() {
        "(no branch)".to_owned()
    } else {
        branch.branch.clone()
    };
    let cost: f64 = branch.cost.iter().map(|c| c.amount).sum();
    let agent_share = bar(branch.agent_millis, max_agent);
    let ci = branch
        .ci
        .as_ref()
        .map_or_else(|| "—".to_owned(), |ci| duration_label(ci.millis));
    let ci_meta = branch.ci.as_ref().map(|ci| {
        if ci.failed > 0 {
            format!("{} runs · {} failed", ci.runs, ci.failed)
        } else {
            format!("{} runs", ci.runs)
        }
    });
    let lead = branch
        .pull_request
        .as_ref()
        .map_or_else(|| "—".to_owned(), |pr| duration_label(lead_millis(pr, now)));
    let tokens = branch
        .tokens
        .map_or_else(|| "—".to_owned(), |t| count_label(t.total));
    rsx! {
        tr {
            class: if open { "metrics-row open" } else { "metrics-row" },
            onclick: move |_| on_toggle.call(()),
            td { class: "metrics-name",
                if let Some(pr) = branch.pull_request.clone() {
                    div { class: "metrics-pr",
                        span { class: "metrics-state {pr.state}", "{pr.state}" }
                        a {
                            href: "#",
                            title: "{pr.url}",
                            onclick: move |event: MouseEvent| {
                                event.prevent_default();
                                event.stop_propagation();
                                open_external(&pr.url);
                            },
                            "#{pr.number}"
                        }
                        span { class: "metrics-title", "{pr.title}" }
                    }
                    span { class: "metrics-branch mono", "{name}" }
                } else {
                    span { class: "metrics-branch mono", "{name}" }
                }
            }
            td { class: "num",
                "{duration_label(branch.agent_millis)}"
                div { class: "metrics-bar",
                    div { style: agent_share }
                }
            }
            td { class: "num", title: ci_meta.clone().unwrap_or_default(), "{ci}" }
            td { class: "num", "{lead}" }
            td { class: "num", title: "{branch.sessions} sessions", "{branch.turns}" }
            td { class: "num", "{tokens}" }
            td { class: "num",
                "{cost_label(&branch.cost)}"
                div { class: "metrics-bar",
                    div { style: share(cost, max_cost) }
                }
            }
            td { class: "num muted", "{ago(branch.last, now)}" }
        }
        if open {
            tr { class: "metrics-detail",
                td { colspan: 8,
                    div { class: "metrics-detail-grid",
                        Breakdown {
                            title: "Agent's tool calls and commands",
                            stats: branch.tools.clone(),
                            empty: "No tool calls recorded.",
                        }
                        Breakdown {
                            title: "CI workflows",
                            stats: branch.ci.as_ref().map(|ci| ci.workflows.clone()).unwrap_or_default(),
                            empty: "No GitHub Actions runs on this branch.",
                        }
                        div { class: "metrics-facts",
                            if let Some(t) = branch.tokens {
                                p { "Tokens: {count_label(t.input)} in · {count_label(t.output)} out" }
                                if let Some(read) = t.cached_read {
                                    p {
                                        "Cache: {count_label(read)} read · {count_label(t.cached_write.unwrap_or(0))} written"
                                    }
                                }
                                if let Some(thought) = t.thought {
                                    p { "Reasoning: {count_label(thought)}" }
                                }
                            }
                            p { "Peak context: {count_label(branch.context_peak)}" }
                            p { "Sessions: {branch.sessions}" }
                            if let Some(problem) = &branch.github_problem {
                                p { class: "muted", "{problem}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Breakdown(title: String, stats: Vec<ToolStat>, empty: String) -> Element {
    let max = stats.iter().map(|s| s.millis).max().unwrap_or(0);
    rsx! {
        div { class: "metrics-breakdown",
            h3 { "{title}" }
            if stats.is_empty() {
                p { class: "muted", "{empty}" }
            } else {
                table {
                    for stat in stats {
                        tr { key: "{stat.name}",
                            td { class: "mono", "{stat.name}" }
                            td { class: "num",
                                "{stat.count}×"
                                if stat.failed > 0 {
                                    span { class: "metrics-failed", " {stat.failed} failed" }
                                }
                            }
                            td { class: "num",
                                "{duration_label(stat.millis)}"
                                div { class: "metrics-bar",
                                    div { style: bar(stat.millis, max) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_long_ago() {
        assert_eq!(ago(0, 30_000), "just now");
        assert_eq!(ago(0, 3 * 3_600_000 + 5), "3h ago");
        assert_eq!(ago(0, 2 * 86_400_000), "2d ago");
    }

    #[test]
    fn shares_the_largest_value() {
        assert_eq!(share(1.0, 4.0), "width: 25.0%");
        assert_eq!(share(1.0, 0.0), "width: 0.0%");
    }

    #[test]
    fn bars_millis() {
        assert_eq!(bar(60_000, 0), "width: 0.0%");
    }
}
