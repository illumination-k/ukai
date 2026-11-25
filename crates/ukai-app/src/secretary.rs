//! The secretary agent (see "秘書" in `docs/design.md`): a gateway agent
//! that `gateway.toml` marks `secretary = true`. Its pane is a pane of its
//! own, the conversation beside an overview of every running workspace's
//! sessions, and it is pinned above the sidebar's project dropdown with
//! what waits on the user. What the overview holds is [`ukai_core::secretary`]'s.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::secretary::{OverviewEndpoint, RunningWorkspace};
use ukai_acp::{PaneWorkspace, WorkspaceScope};
use ukai_config::{AgentSpec, gateway_agent_id};
use ukai_core::launch::GatewayListing;
use ukai_core::secretary::{self, Item, Standing};
use ukai_core::{Layout, PaneContent, PaneId, ProjectId};

use crate::agent_pane::{AgentView, agent_status, connect};
use crate::alert::is_unread;
use crate::follow::{self, Endpoints, Workspace, poll};
use crate::icons::{Icon, Svg};
use crate::layout::open_agent;
use crate::projects::AppSignals;

/// How often the overview is read again.
const LIST_EVERY: Duration = Duration::from_secs(15);

/// A secretary: the project it is opened in, and its agent id there
/// (`<gateway>/<agent>`).
pub type Secretary = (ProjectId, String);

/// What each secretary's gateway last listed of its running workspaces,
/// kept by [`FollowSecretaries`].
pub type Overviews = Signal<HashMap<Secretary, Result<Vec<RunningWorkspace>, String>>>;

/// Whether `agent` of `project` is a secretary, as its gateway listed it.
pub fn is_secretary(app: AppSignals, project: &ProjectId, agent: &str) -> bool {
    let Some((gateway, id)) = agent.split_once('/') else {
        return false;
    };
    matches!(
        app.gateways.read().get(project).and_then(|listings| listings.get(gateway)),
        Some(GatewayListing::Listed(agents))
            if agents.iter().any(|listed| listed.id == id && listed.secretary)
    )
}

/// Every secretary the open projects' gateways list, once each: a gateway
/// several projects declare (one of the user's `config.toml`) gives the
/// first of them.
pub fn secretaries(app: AppSignals) -> Vec<Secretary> {
    let projects = app.projects.read();
    let configs = app.configs.read();
    let listings = app.gateways.read();
    let mut seen = Vec::new();
    let mut found = Vec::new();
    for project in projects.iter() {
        let (Some(Ok(config)), Some(listed)) =
            (configs.get(&project.id), listings.get(&project.id))
        else {
            continue;
        };
        for (gateway, listing) in listed {
            let GatewayListing::Listed(agents) = listing else {
                continue;
            };
            let Some(declared) = config.gateways.get(gateway) else {
                continue;
            };
            for agent in agents.iter().filter(|agent| agent.secretary) {
                let known = (declared.url.clone(), agent.id.clone());
                if !seen.contains(&known) {
                    seen.push(known);
                    found.push((project.id.clone(), gateway_agent_id(gateway, &agent.id)));
                }
            }
        }
    }
    found
}

fn resolve(spec: &AgentSpec, workspace: &Workspace) -> Result<OverviewEndpoint, String> {
    follow::found(
        OverviewEndpoint::resolve(spec, ukai_acp::system_lookup),
        || format!("agent `{}` is not a gateway agent", workspace.agent),
    )
}

/// Reads every secretary's overview, over and over, and as soon as a
/// gateway lists a new one. Rendered once by the app, so the sidebar's
/// counts go on without a secretary pane; it draws nothing.
#[component]
pub fn FollowSecretaries() -> Element {
    let app = use_context::<AppSignals>();
    let mut overviews = app.secretaries;
    let wanted = use_memo(move || secretaries(app));
    let mut revision = use_signal(|| 0u64);
    use_effect(move || {
        wanted.read();
        *revision.write() += 1;
    });
    use_future(move || {
        let endpoints = Rc::new(RefCell::new(Endpoints::new(resolve)));
        poll(revision, LIST_EVERY, move || {
            let endpoints = endpoints.clone();
            async move {
                let wanted = wanted.peek().clone();
                let mut fetched = HashMap::new();
                let mut asked = Vec::new();
                for (project, agent) in wanted {
                    let workspace = Workspace::of(&project, &agent, None);
                    let endpoint = endpoints.borrow_mut().get(&app.configs.peek(), &workspace);
                    let listed = match endpoint {
                        Ok(endpoint) => endpoint.list().await.map_err(|error| error.to_string()),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = &listed {
                        tracing::warn!(%agent, %error, "cannot read the secretary's overview");
                    }
                    fetched.insert((project, agent), listed);
                    asked.push(workspace);
                }
                endpoints.borrow_mut().keep(&asked);
                if *overviews.peek() != fetched {
                    overviews.set(fetched);
                }
            }
        })
    });
    rsx! {}
}

/// The overview of `secretary`, or why there is none yet.
fn items(app: AppSignals, secretary: &Secretary) -> Option<Result<Vec<Item>, String>> {
    let own = secretary
        .1
        .split_once('/')
        .map_or(secretary.1.as_str(), |(_, id)| id);
    app.secretaries.read().get(secretary).map(|listed| {
        listed
            .clone()
            .map(|workspaces| secretary::overview(&workspaces, own))
    })
}

/// The pane showing `agent` of `project`, nearest the user.
fn pane_of(layout: &Layout, project: &ProjectId, agent: &str) -> Option<PaneId> {
    layout.nearest(|_, content| {
        matches!(content, PaneContent::Agent { project: p, agent: a, .. } if p == project && a == agent)
    })
}

/// Focus the secretary's pane, or open it in a tab of its own.
fn open_secretary(app: AppSignals, secretary: &Secretary) {
    let (project, agent) = secretary;
    let mut layout = app.layout;
    let open = pane_of(&layout.peek(), project, agent);
    if let Some(pane) = open {
        layout.write().focus(pane);
        return;
    }
    let pane = layout.write().new_tab();
    open_agent(app, pane, project, agent, None);
}

/// Focus the pane of `item`'s session, or open it in a tab of its own, so
/// the secretary's pane keeps its room. It continues the session the
/// workspace's relay keeps.
fn open_session(app: AppSignals, secretary: &Secretary, item: &Item) {
    let Some(session) = &item.session else {
        return;
    };
    let (project, own) = secretary;
    let gateway = own.split_once('/').map_or("", |(gateway, _)| gateway);
    let agent = gateway_agent_id(gateway, &item.agent);
    let mut layout = app.layout;
    let open = layout.peek().nearest(|_, content| {
        matches!(
            content,
            PaneContent::Agent { project: p, agent: a, session: Some(s), .. }
                if p == project && *a == agent && *s == session.id
        )
    });
    if let Some(pane) = open {
        layout.write().focus(pane);
        return;
    }
    let workspace = match &item.pane {
        Some(key) => PaneWorkspace {
            key: key.clone(),
            scope: WorkspaceScope::Pod,
        },
        None => PaneWorkspace {
            key: uuid::Uuid::new_v4().simple().to_string(),
            scope: WorkspaceScope::Shared,
        },
    };
    let pane = {
        let mut layout = layout.write();
        let pane = layout.new_tab();
        layout.set_content(
            pane,
            PaneContent::Agent {
                project: project.clone(),
                agent,
                session: Some(session.id.clone()),
                workspace: Some(workspace),
            },
        );
        pane
    };
    connect(app, pane, true);
}

/// The secretary rows above the sidebar's project dropdown: each opens its
/// pane, and says how many sessions wait on the user and how many work.
#[component]
pub fn SecretaryPins() -> Element {
    let app = use_context::<AppSignals>();
    let found = secretaries(app);
    if found.is_empty() {
        return rsx! {};
    }
    let several = found.len() > 1;
    rsx! {
        div { class: "secretary-pins",
            for secretary in found {
                SecretaryPin {
                    key: "{secretary.0}/{secretary.1}",
                    secretary,
                    several,
                }
            }
        }
    }
}

#[component]
fn SecretaryPin(secretary: Secretary, several: bool) -> Element {
    let app = use_context::<AppSignals>();
    // Read, not peeked: the row follows the pane as it opens and closes.
    let pane = pane_of(&app.layout.read(), &secretary.0, &secretary.1);
    let state = pane.map_or("idle", |pane| agent_status(app.sessions, pane).0);
    let focused = pane.is_some_and(|pane| app.layout.read().focused() == pane);
    let unread = pane.is_some_and(|pane| is_unread(app.sessions, pane));
    let listed = items(app, &secretary);
    let (waiting, running, failed) = match &listed {
        Some(Ok(items)) => (
            secretary::count(items, Standing::Waiting),
            secretary::count(items, Standing::Running),
            None,
        ),
        Some(Err(error)) => (0, 0, Some(error.clone())),
        None => (0, 0, None),
    };
    let name = if several {
        secretary.1.clone()
    } else {
        "Secretary".to_owned()
    };
    let class = match (focused, unread) {
        (true, _) => "row secretary-pin focused",
        (false, true) => "row secretary-pin unread",
        (false, false) => "row secretary-pin",
    };
    rsx! {
        div {
            class,
            title: "{secretary.1}",
            onclick: {
                let secretary = secretary.clone();
                move |_| open_secretary(app, &secretary)
            },
            span { class: "secretary-mark",
                Svg { icon: Icon::Secretary, size: 12 }
            }
            span { class: "name", "{name}" }
            if waiting > 0 {
                span {
                    class: "secretary-count waiting",
                    title: "Sessions waiting on you",
                    span { class: "dot attention" }
                    "{waiting}"
                }
            }
            if running > 0 {
                span { class: "secretary-count", title: "Sessions working",
                    span { class: "dot busy" }
                    "{running}"
                }
            }
            if let Some(error) = failed {
                span { class: "meta", title: "{error}", "unreachable" }
            }
            span { class: "dot {state}" }
        }
    }
}

/// The secretary's pane: its conversation, and the overview beside it.
#[component]
pub fn SecretaryPane(
    pane: PaneId,
    project: ProjectId,
    agent: String,
    has_saved_session: bool,
) -> Element {
    rsx! {
        div { class: "secretary-pane",
            div { class: "secretary-chat",
                AgentView { pane, has_saved_session }
            }
            Overview { secretary: (project, agent) }
        }
    }
}

/// A section of the overview: its heading, class, and rows with their keys.
type Section = (String, &'static str, Vec<(String, Item)>);

/// The overview's sections, those with something in them, in order.
fn sections(items: Vec<Item>) -> Vec<Section> {
    [
        (Standing::Waiting, "Waiting on you", "waiting"),
        (Standing::Running, "Working", ""),
        (Standing::Finished, "Finished", ""),
        (Standing::Unreachable, "Not answering", ""),
    ]
    .into_iter()
    .filter_map(|(standing, label, class)| {
        let rows: Vec<(String, Item)> = items
            .iter()
            .filter(|item| item.standing == standing)
            .map(|item| {
                let session = item
                    .session
                    .as_ref()
                    .map_or("", |session| session.id.as_str());
                (format!("{}/{session}", item.place()), item.clone())
            })
            .collect();
        (!rows.is_empty()).then(|| (format!("{label} · {}", rows.len()), class, rows))
    })
    .collect()
}

/// Every session of the running workspaces, those waiting on the user first.
#[component]
fn Overview(secretary: Secretary) -> Element {
    let app = use_context::<AppSignals>();
    let listed = items(app, &secretary);
    let body = match listed {
        None => rsx! {
            p { class: "muted empty", "Reading the workspaces…" }
        },
        Some(Err(error)) => rsx! {
            p { class: "muted empty", title: "{error}", "The gateway did not answer." }
        },
        Some(Ok(items)) if items.is_empty() => rsx! {
            p { class: "muted empty", "No other workspace is running." }
        },
        Some(Ok(items)) => rsx! {
            for (heading, class, rows) in sections(items) {
                section { key: "{heading}", class: "overview-section {class}",
                    h3 { "{heading}" }
                    for (key, item) in rows {
                        OverviewItem {
                            key: "{key}",
                            secretary: secretary.clone(),
                            item,
                        }
                    }
                }
            }
        },
    };
    rsx! {
        aside { class: "secretary-overview", aria_label: "Workspaces",
            header {
                span { class: "title", "Workspaces" }
                span { class: "meta", "every 15 s" }
            }
            {body}
        }
    }
}

#[component]
fn OverviewItem(secretary: Secretary, item: Item) -> Element {
    let app = use_context::<AppSignals>();
    let dot = match item.standing {
        Standing::Waiting => "attention",
        Standing::Running => "busy",
        Standing::Finished => "ready",
        Standing::Unreachable => "idle",
    };
    let detail = match (&item.standing, &item.session) {
        (Standing::Waiting, _) => Some("asks for a permission".to_owned()),
        (_, Some(session)) => session
            .last_reply
            .as_deref()
            .and_then(|reply| reply.lines().map(str::trim).find(|line| !line.is_empty()))
            .map(str::to_owned)
            .or_else(|| session.stop_reason.clone()),
        (_, None) => Some("its relay did not answer".to_owned()),
    };
    let title = item.title();
    let openable = item.session.is_some();
    // A workspace that did not answer is already titled by its place.
    let place = openable.then(|| item.place());
    rsx! {
        button {
            class: "overview-item",
            r#type: "button",
            disabled: !openable,
            title: if openable { "Open this session" },
            onclick: move |_| open_session(app, &secretary, &item),
            span { class: "line",
                span { class: "dot {dot}" }
                span { class: "name", "{title}" }
            }
            if let Some(place) = place {
                span { class: "place", "{place}" }
            }
            if let Some(detail) = detail {
                span { class: "detail", "{detail}" }
            }
        }
    }
}
