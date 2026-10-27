//! What a project offers to open in a pane: its own agents, then each of
//! its gateways with the agents that gateway lists, so a pane picks a
//! gateway's agent as `plekti ▸ opencode` rather than from one flat list.

use std::collections::BTreeMap;

use ukai_acp::GatewayAgent;
use ukai_config::{AgentSpec, Config, WorkspaceScope, gateway_agent_id};

/// What a gateway last answered when asked for its agents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayListing {
    Loading,
    Listed(Vec<GatewayAgent>),
    Failed(String),
}

/// One way to open an agent. A gateway agent is offered once per workspace
/// scope, its gateway's default first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The id the pane opens: `claude`, or `plekti/claude` for a gateway's.
    pub agent: String,
    /// What the user reads: the id within its group.
    pub name: String,
    /// The command or endpoint it starts.
    pub line: String,
    pub badge: &'static str,
    pub scope: Option<WorkspaceScope>,
}

/// The agents of this machine (`gateway` is `None`), or of one gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub gateway: Option<String>,
    /// Why a gateway's group has no choices yet, or none at all.
    pub status: Status,
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Ready,
    Loading,
    Failed(String),
}

/// The groups `config` offers, given what its gateways listed (by gateway
/// id; a gateway missing there is still being asked). A machine with no
/// agents of its own has no group for them.
pub fn groups(config: &Config, listings: Option<&BTreeMap<String, GatewayListing>>) -> Vec<Group> {
    let local: Vec<Choice> = config
        .agents
        .iter()
        .filter(|(id, _)| !id.contains('/'))
        .flat_map(|(id, spec)| choices(id, id, launch_line(spec), spec))
        .collect();
    let mut groups = Vec::new();
    if !local.is_empty() {
        groups.push(Group {
            gateway: None,
            status: Status::Ready,
            choices: local,
        });
    }
    for (name, gateway) in &config.gateways {
        let listing = listings.and_then(|listings| listings.get(name));
        let (status, choices) = match listing {
            None | Some(GatewayListing::Loading) => (Status::Loading, Vec::new()),
            Some(GatewayListing::Failed(why)) => (Status::Failed(why.clone()), Vec::new()),
            Some(GatewayListing::Listed(agents)) => (
                Status::Ready,
                agents
                    .iter()
                    .flat_map(|agent| {
                        let line = std::iter::once(&agent.command)
                            .chain(&agent.args)
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(" ");
                        choices(
                            &gateway_agent_id(name, &agent.id),
                            &agent.id,
                            line,
                            &gateway.agent(&agent.id),
                        )
                    })
                    .collect(),
            ),
        };
        groups.push(Group {
            gateway: Some(name.clone()),
            status,
            choices,
        });
    }
    groups
}

fn choices(agent: &str, name: &str, line: String, spec: &AgentSpec) -> Vec<Choice> {
    let choice = |badge, scope| Choice {
        agent: agent.to_owned(),
        name: name.to_owned(),
        line: line.clone(),
        badge,
        scope,
    };
    match spec {
        AgentSpec::Gateway { workspace, .. } => {
            let other = match workspace {
                WorkspaceScope::Shared => WorkspaceScope::Pod,
                WorkspaceScope::Pod => WorkspaceScope::Shared,
            };
            [*workspace, other]
                .into_iter()
                .map(|scope| choice(scope_label(scope), Some(scope)))
                .collect()
        }
        _ => vec![choice(transport_label(spec), None)],
    }
}

fn scope_label(scope: WorkspaceScope) -> &'static str {
    match scope {
        WorkspaceScope::Shared => "shared",
        WorkspaceScope::Pod => "own pod",
    }
}

pub fn transport_label(spec: &AgentSpec) -> &'static str {
    match spec {
        AgentSpec::Http { .. } => "http",
        AgentSpec::Websocket { .. } => "ws",
        AgentSpec::Gateway { .. } => "gateway",
        AgentSpec::Stdio { .. } => "stdio",
    }
}

fn launch_line(spec: &AgentSpec) -> String {
    match spec {
        AgentSpec::Http { url, .. }
        | AgentSpec::Websocket { url, .. }
        | AgentSpec::Gateway { url, .. } => url.clone(),
        AgentSpec::Stdio { command, args, .. } => std::iter::once(command.as_str())
            .chain(args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        ukai_config::parse(
            r#"
            [agents.claude]
            transport = "stdio"
            command = "npx"
            args = ["-y", "claude-agent-acp"]

            [gateways.plekti]
            url = "https://plekti.example"
            workspace = "pod"

            [gateways.down]
            url = "https://down.example"

            [gateways.slow]
            url = "https://slow.example"
            "#,
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    fn listed(ids: &[&str]) -> GatewayListing {
        GatewayListing::Listed(
            ids.iter()
                .map(|id| GatewayAgent {
                    id: (*id).into(),
                    command: (*id).into(),
                    args: vec!["acp".into()],
                })
                .collect(),
        )
    }

    #[test]
    fn local_agents_come_first_then_each_gateway_with_its_agents() {
        let mut config = config();
        // Listed agents are also in `agents`; they are not this machine's.
        config.list_gateway("plekti", ["codex", "opencode"]);
        let listings = BTreeMap::from([
            ("plekti".to_owned(), listed(&["codex", "opencode"])),
            ("down".to_owned(), GatewayListing::Failed("refused".into())),
        ]);
        let groups = groups(&config, Some(&listings));

        let heads: Vec<(Option<&str>, &Status)> = groups
            .iter()
            .map(|g| (g.gateway.as_deref(), &g.status))
            .collect();
        assert_eq!(
            heads,
            [
                (None, &Status::Ready),
                (Some("down"), &Status::Failed("refused".into())),
                (Some("plekti"), &Status::Ready),
                (Some("slow"), &Status::Loading),
            ]
        );
        let local: Vec<(&str, &str, &str)> = groups[0]
            .choices
            .iter()
            .map(|c| (c.agent.as_str(), c.line.as_str(), c.badge))
            .collect();
        assert_eq!(local, [("claude", "npx -y claude-agent-acp", "stdio")]);

        let plekti: Vec<(&str, &str, &str, Option<WorkspaceScope>)> = groups[2]
            .choices
            .iter()
            .map(|c| (c.agent.as_str(), c.name.as_str(), c.badge, c.scope))
            .collect();
        assert_eq!(
            plekti,
            [
                (
                    "plekti/codex",
                    "codex",
                    "own pod",
                    Some(WorkspaceScope::Pod)
                ),
                (
                    "plekti/codex",
                    "codex",
                    "shared",
                    Some(WorkspaceScope::Shared)
                ),
                (
                    "plekti/opencode",
                    "opencode",
                    "own pod",
                    Some(WorkspaceScope::Pod)
                ),
                (
                    "plekti/opencode",
                    "opencode",
                    "shared",
                    Some(WorkspaceScope::Shared)
                ),
            ]
        );
        assert_eq!(groups[2].choices[2].line, "opencode acp");
        assert!(groups[1].choices.is_empty() && groups[3].choices.is_empty());
    }

    #[test]
    fn a_machine_without_agents_of_its_own_offers_only_its_gateways() {
        let mut config = config();
        config.agents.clear();
        let groups = groups(&config, None);
        assert!(groups.iter().all(|g| g.gateway.is_some()));
        assert!(groups.iter().all(|g| g.status == Status::Loading));
    }
}
