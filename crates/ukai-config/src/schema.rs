//! Serde types mirroring the `ukai.toml` file layout.
//!
//! The file declares what can be opened, not where: tabs and panes are
//! arranged by the user at runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The whole `ukai.toml` document.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSpec>,
    #[serde(default)]
    pub permissions: Vec<PermissionRule>,
    #[serde(default)]
    pub reports: Reports,
    /// Keyboard shortcuts by action name, over the defaults. Only the
    /// user-level `config.toml` may set them: they apply to the whole app.
    #[serde(default)]
    pub keybindings: BTreeMap<String, Keys>,
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
    /// profile, plus where each pane's work runs.
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
