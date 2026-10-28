//! Serde types mirroring the `ukai.toml` file layout.
//!
//! The file declares what can be opened, not where: tabs and panes are
//! arranged by the user at runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The whole `ukai.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSpec>,
    /// `ukai-gateway`s, each serving the agents its `gateway.toml` declares.
    /// The pane names one of them as `<gateway>/<agent>`, which the app adds
    /// to `agents` once the gateway lists it (see [`Config::list_gateway`]).
    #[serde(default)]
    pub gateways: BTreeMap<String, Gateway>,
    #[serde(default)]
    pub permissions: Vec<PermissionRule>,
    #[serde(default)]
    pub reports: Reports,
    #[serde(default)]
    pub review: Review,
    /// Keyboard shortcuts by action name, over the defaults. Only the
    /// user-level `config.toml` may set them: they apply to the whole app.
    #[serde(default)]
    pub keybindings: BTreeMap<String, Keys>,
    /// Voice input. Only the user-level `config.toml` may set it: the model
    /// is a file on this machine, not part of a project.
    pub voice: Option<Voice>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: Option<String>,
}

/// What a pane runs when the user opens this agent in it: a local command
/// speaking ACP over stdio, or a remote ACP endpoint.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum AgentSpec {
    /// Streamable HTTP (HTTP/2 + SSE) at an `/acp` endpoint.
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// The WebSocket profile of the same `/acp` endpoint.
    Websocket {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// An agent served by `ukai-gateway` at `/agents/<id>/acp`: the WebSocket
    /// profile, plus where each pane's work runs. Never written in
    /// `[agents]`: it is what a `[gateways]` entry gives for one of its
    /// agents ([`Gateway::agent`]).
    #[serde(skip_deserializing)]
    Gateway {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// The default for new panes; a pane can choose the other.
        #[serde(default)]
        workspace: WorkspaceScope,
    },
    /// A local subprocess speaking ACP over stdio.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

/// A `ukai-gateway`, reached at its root URL. Its agents are not declared
/// here: the gateway lists them (`Agents.List`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gateway {
    /// The root, e.g. `https://gw.example.com`; `wss://` names the same.
    pub url: String,
    /// Sent with every request to it, e.g. its token.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Where a new pane of one of its agents works by default; a pane can
    /// choose the other.
    #[serde(default)]
    pub workspace: WorkspaceScope,
}

impl Gateway {
    /// The agent `id` of this gateway, at `<root>/agents/<id>/acp`.
    pub fn agent(&self, id: &str) -> AgentSpec {
        let root = self.url.trim_end_matches('/');
        let root = match root.split_once("://") {
            Some(("https", rest)) => format!("wss://{rest}"),
            Some(("http", rest)) => format!("ws://{rest}"),
            _ => root.to_owned(),
        };
        AgentSpec::Gateway {
            url: format!("{root}/agents/{id}/acp"),
            headers: self.headers.clone(),
            workspace: self.workspace,
        }
    }
}

/// The id a pane gives agent `agent` of gateway `gateway`.
pub fn gateway_agent_id(gateway: &str, agent: &str) -> String {
    format!("{gateway}/{agent}")
}

impl Config {
    /// The agent a pane names `id`: one of `[agents]`, or `<gateway>/<agent>`
    /// of a declared gateway, whether or not it has been listed yet, so a
    /// pane restored before the listing arrives still connects.
    pub fn agent(&self, id: &str) -> Option<AgentSpec> {
        if let Some(spec) = self.agents.get(id) {
            return Some(spec.clone());
        }
        let (gateway, agent) = id.split_once('/')?;
        let valid = !agent.is_empty() && !agent.contains('/');
        valid
            .then(|| self.gateways.get(gateway))
            .flatten()
            .map(|gateway| gateway.agent(agent))
    }

    /// Put the agents `gateway` lists in `agents`, as `<gateway>/<agent>`,
    /// in place of those it listed before.
    pub fn list_gateway<I, S>(&mut self, gateway: &str, agents: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let Some(declared) = self.gateways.get(gateway) else {
            return;
        };
        let prefix = gateway_agent_id(gateway, "");
        let mut listed: BTreeMap<String, AgentSpec> = agents
            .into_iter()
            .filter(|agent| {
                let agent = agent.as_ref();
                !agent.is_empty() && !agent.contains('/')
            })
            .map(|agent| {
                let agent = agent.as_ref();
                (gateway_agent_id(gateway, agent), declared.agent(agent))
            })
            .collect();
        self.agents.retain(|id, _| !id.starts_with(&prefix));
        self.agents.append(&mut listed);
    }
}

/// Where a gateway agent's pane works.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceScope {
    /// The pod and checkout every pane of this agent shares.
    #[default]
    Shared,
    /// A pod and checkout of the pane's own.
    Pod,
}

/// The chords bound to one action: one, several, or none (`[]` unbinds it).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Keys {
    One(String),
    Many(Vec<String>),
}

impl Keys {
    pub fn chords(&self) -> &[String] {
        match self {
            Keys::One(chord) => std::slice::from_ref(chord),
            Keys::Many(chords) => chords,
        }
    }
}

/// Dictation into the composer, transcribed on this machine by whisper.cpp.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Voice {
    /// A whisper.cpp ggml model file (`ggml-base.bin`, ...). A relative
    /// path is relative to the directory of `config.toml`.
    pub model: PathBuf,
    /// Spoken language as an ISO 639-1 code (`ja`, `en`); detected when
    /// absent.
    pub language: Option<String>,
}

impl Voice {
    /// The model's path, with a relative one taken from `config_dir`.
    pub fn model_path(&self, config_dir: Option<&Path>) -> PathBuf {
        match config_dir {
            Some(dir) if self.model.is_relative() => dir.join(&self.model),
            _ => self.model.clone(),
        }
    }
}

/// An automatic answer to `session/request_permission`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionRule {
    /// Agent id the rule applies to; every agent when absent.
    pub agent: Option<String>,
    /// ACP `ToolKind` (`read`, `edit`, `execute`, ...); every kind when absent.
    pub tool_kind: Option<String>,
    pub decision: Decision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

/// How agents annotate the checkout's diff for the user (the relay's
/// `review_diff` tool): which tags they may use, and what each priority
/// means here. Both go into what the agent is told, word for word.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// Tag → what it marks. With none, an agent picks its own tags.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// `p0` to `p3` → what it means in this project, over the defaults.
    #[serde(default)]
    pub priorities: BTreeMap<String, String>,
}

/// The priorities an annotation may have, most pressing first.
pub const PRIORITIES: [&str; 4] = ["p0", "p1", "p2", "p3"];

/// Longest tag of `[review.tags]`.
pub const REVIEW_TAG_MAX: usize = 32;

/// Longest description in `[review]`, in characters: one line.
pub const REVIEW_DESCRIPTION_MAX: usize = 200;

/// Agent-written files a pane can show: HTML reports and images.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reports {
    /// Directories listed in the sidebar, relative to the project root.
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    /// Which files under `dirs` are listed, relative to each directory.
    #[serde(default = "Reports::default_globs")]
    pub globs: Vec<String>,
    /// Let reports load from the network (CDN scripts, remote images). Off
    /// by default: a report sees only the files next to it.
    #[serde(default)]
    pub allow_network: bool,
}

impl Reports {
    fn default_globs() -> Vec<String> {
        ["html", "htm", "svg", "png", "jpg", "jpeg", "gif", "webp"]
            .iter()
            .map(|ext| format!("**/*.{ext}"))
            .collect()
    }
}

impl Default for Reports {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            globs: Self::default_globs(),
            allow_network: false,
        }
    }
}
