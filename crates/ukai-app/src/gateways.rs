//! The agents each project's gateways serve: asked for whenever its
//! configuration loads, and put in it as `<gateway>/<agent>` so the rest of
//! the app finds them among the project's agents.

use std::collections::{BTreeMap, HashMap};

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use ukai_acp::GatewayEndpoint;
use ukai_core::ProjectId;
use ukai_core::launch::GatewayListing;

use crate::projects::AppSignals;

/// What each project's gateways last listed, by gateway id.
pub type GatewayListings = Signal<HashMap<ProjectId, BTreeMap<String, GatewayListing>>>;

/// Ask every gateway of `project`'s configuration for its agents.
pub fn list_gateways(app: AppSignals, project: &ProjectId) {
    let gateways: Vec<_> = match app.configs.peek().get(project) {
        Some(Ok(config)) => config.gateways.clone().into_iter().collect(),
        _ => Vec::new(),
    };
    let mut listings = app.gateways;
    listings.write().insert(
        project.clone(),
        gateways
            .iter()
            .map(|(id, _)| (id.clone(), GatewayListing::Loading))
            .collect(),
    );
    for (id, gateway) in gateways {
        let project = project.clone();
        spawn_forever(async move {
            let listed = match GatewayEndpoint::resolve(&gateway, ukai_acp::system_lookup) {
                Ok(endpoint) => endpoint.agents().await.map_err(|e| e.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let mut configs = app.configs;
            {
                let mut configs = configs.write();
                // A reload since may have changed or dropped the gateway.
                let Some(Ok(config)) = configs.get_mut(&project) else {
                    return;
                };
                if config.gateways.get(&id) != Some(&gateway) {
                    return;
                }
                if let Ok(agents) = &listed {
                    config.list_gateway(&id, agents.iter().map(|agent| &agent.id));
                }
            }
            if let Err(error) = &listed {
                tracing::warn!(gateway = %id, %error, "cannot list the gateway's agents");
            }
            if let Some(listings) = listings.write().get_mut(&project) {
                listings.insert(
                    id,
                    match listed {
                        Ok(agents) => GatewayListing::Listed(agents),
                        Err(error) => GatewayListing::Failed(error),
                    },
                );
            }
        });
    }
}
