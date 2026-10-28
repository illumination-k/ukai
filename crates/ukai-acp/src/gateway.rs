//! A gateway agent's declaration as the endpoints next to its `/acp` read
//! it: the URL with its references expanded, and clients that send the
//! agent's headers.

use std::collections::BTreeMap;
use std::time::Duration;

use ukai_config::{AgentSpec, Source, WorkspaceScope};
use ukai_proto::gateway::v1 as gw;

use crate::grpc::{self, Channel};
use crate::{PaneWorkspace, ResolveError, header_map, workspace_headers};

/// A declared gateway agent.
pub(crate) struct Gateway<'a, L> {
    /// The agent's `/acp` URL, its references expanded.
    pub url: String,
    headers: &'a BTreeMap<String, String>,
    default_scope: WorkspaceScope,
    lookup: L,
}

impl<'a, L: Fn(Source, &str) -> Option<String> + Copy> Gateway<'a, L> {
    /// `spec`, or `None` when it is not a gateway agent.
    pub fn of(spec: &'a AgentSpec, lookup: L) -> Result<Option<Self>, ResolveError> {
        let AgentSpec::Gateway {
            url,
            headers,
            workspace,
        } = spec
        else {
            return Ok(None);
        };
        Ok(Some(Self {
            url: ukai_config::interpolate(url, lookup)?,
            headers,
            default_scope: *workspace,
            lookup,
        }))
    }

    /// A client sending the agent's headers, built by `configure`.
    pub fn client(
        &self,
        configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> Result<reqwest::Client, ResolveError> {
        self.build(&[], configure)
    }

    /// [`Self::client`], also naming `pane`'s workspace as `/acp` does.
    pub fn workspace_client(
        &self,
        pane: Option<&PaneWorkspace>,
        configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> Result<reqwest::Client, ResolveError> {
        self.build(&workspace_headers(pane, self.default_scope), configure)
    }

    /// The workspace a request about `pane` names in its message.
    pub fn workspace(&self, pane: Option<&PaneWorkspace>) -> gw::Workspace {
        grpc::workspace(pane, self.default_scope)
    }

    fn build(
        &self,
        extra: &[(&'static str, String)],
        configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> Result<reqwest::Client, ResolveError> {
        let builder = reqwest::Client::builder().default_headers(header_map(
            self.headers,
            extra,
            self.lookup,
        )?);
        configure(builder)
            .build()
            .map_err(|error| ResolveError::Client(error.to_string()))
    }
}

/// A gateway agent as its gRPC services reach it.
pub(crate) struct GatewayAgent<'a, L> {
    pub gateway: Gateway<'a, L>,
    /// The gateway's root ([`grpc::agent_root`]).
    pub root: String,
    /// The agent's id.
    pub id: String,
}

impl<'a, L: Fn(Source, &str) -> Option<String> + Copy> GatewayAgent<'a, L> {
    /// `spec`, or `None` when it is not a gateway agent or its URL names no
    /// agent.
    pub fn resolve(spec: &'a AgentSpec, lookup: L) -> Result<Option<Self>, ResolveError> {
        let Some(gateway) = Gateway::of(spec, lookup)? else {
            return Ok(None);
        };
        let Some((root, id)) = grpc::agent_root(&gateway.url) else {
            return Ok(None);
        };
        Ok(Some(Self { gateway, root, id }))
    }

    /// The gateway, called with the agent's headers, each call within
    /// `timeout`.
    pub fn channel(&self, timeout: Duration) -> Result<Channel, ResolveError> {
        let client = self.gateway.client(|builder| builder.timeout(timeout))?;
        Ok(grpc::channel(client, self.root.clone()))
    }
}
