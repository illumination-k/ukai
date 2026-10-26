//! Reports of the workspaces gateway agents work in.
//!
//! A gateway agent writes its reports in its pod, not here (see
//! [`ukai_acp::reports`]), so the app cannot scan a directory for them as it
//! does for a project. It asks the gateway instead: [`Follow`] keeps the
//! listing of every workspace an open agent pane uses, and of those the
//! sidebar shows, and the sidebar, the palette, and a report pane read what
//! it found rather than each polling on their own.
//!
//! The unit is the *workspace*, not the pane: two panes sharing one
//! (`workspace = "shared"`) see one listing, and a report stays readable
//! after the pane that opened the agent is closed, as long as the pod is
//! still running.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use dioxus::prelude::*;
use ukai_acp::reports::{ReportsEndpoint, ReportsError};
use ukai_config::AgentSpec;

use crate::follow::{Endpoints, Workspace, followed, relist};
use crate::projects::AppSignals;

/// How often the listings are read again while agent panes are open.
const RELIST: Duration = Duration::from_secs(3);

/// The last listing of a workspace.
#[derive(Debug, Clone, PartialEq)]
pub enum Listing {
    /// Project-relative paths, newest first.
    Reports(Vec<PathBuf>),
    /// The pod is not running, and these are what the gateway kept of them,
    /// as they were at `synced` (seconds since the epoch).
    Saved {
        paths: Vec<PathBuf>,
        synced: u64,
    },
    /// The pod is not running: the agent has to be opened again first.
    NotRunning,
    Failed(String),
}

/// Every workspace's last listing, kept by [`Follow`].
pub type Listings = Signal<HashMap<Workspace, Listing>>;

/// Where a workspace's reports are read from, for [`crate::follow::endpoint`].
pub fn resolve(spec: &AgentSpec, workspace: &Workspace) -> Result<ReportsEndpoint, String> {
    ReportsEndpoint::resolve(spec, ukai_acp::system_lookup, workspace.workspace.as_ref())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("agent `{}` serves no reports", workspace.agent))
}

/// Reads the listings of the [`followed`] workspaces, over and over.
/// Rendered once by the app; it draws nothing.
#[component]
pub fn Follow() -> Element {
    let app = use_context::<AppSignals>();
    use_future(move || async move {
        let mut endpoints = Endpoints::new(resolve);
        loop {
            let wanted = {
                let layout = app.layout.peek();
                let configs = app.configs.peek();
                followed(&layout, &configs, &app.folds.peek())
            };
            relist(
                app,
                app.reports,
                &mut endpoints,
                &wanted,
                async |_, endpoint| match endpoint {
                    Ok(endpoint) => read(&endpoint).await,
                    Err(problem) => Listing::Failed(problem),
                },
            )
            .await;
            tokio::time::sleep(RELIST).await;
        }
    });
    rsx! {}
}

async fn read(endpoint: &ReportsEndpoint) -> Listing {
    match endpoint.list().await {
        Ok(listing) => {
            let paths = listing
                .reports
                .into_iter()
                .map(|e| PathBuf::from(e.path))
                .collect();
            match listing.synced {
                Some(synced) => Listing::Saved { paths, synced },
                None => Listing::Reports(paths),
            }
        }
        Err(ReportsError::Gone) => Listing::NotRunning,
        Err(error) => Listing::Failed(error.to_string()),
    }
}
