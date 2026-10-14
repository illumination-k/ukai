//! Serde types mirroring the `ukai.toml` file layout.
//!
//! The file declares what can be opened, not where: tabs and panes are
//! arranged by the user at runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

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
    /// A local subprocess speaking ACP over stdio.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reports {
    /// Directories watched for reports, relative to the project root.
    #[serde(default)]
    pub dirs: Vec<PathBuf>,
    #[serde(default = "Reports::default_globs")]
    pub globs: Vec<String>,
}

impl Reports {
    fn default_globs() -> Vec<String> {
        vec!["**/*.html".to_owned()]
    }
}

impl Default for Reports {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            globs: Self::default_globs(),
        }
    }
}
