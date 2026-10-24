//! The gateway and `ukai-knowledge` as deployed, checked from outside by
//! e2e/gateway/run.sh against a kind cluster; ignored otherwise. A person's
//! side goes through the gateway with the app's own client, and an agent's
//! is played on the knowledge service directly, with the token the gateway
//! holds, since no agent pod runs.
//!
//! Environment (set by the script):
//! - `UKAI_E2E_GATEWAY`, `UKAI_E2E_GATEWAY_TOKEN`: the gateway, port-forwarded
//! - `UKAI_E2E_KNOWLEDGE`, `UKAI_E2E_KNOWLEDGE_TOKEN`: `ukai-knowledge`, likewise

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use tonic::Code;
use tonic::transport::Channel;
use ukai_acp::knowledge::{KnowledgeEndpoint, KnowledgeError, NoteSpec, NoteWrite};
use ukai_config::{AgentSpec, WorkspaceScope};
use ukai_proto::knowledge::v1 as pb;
use ukai_proto::knowledge::v1::knowledge_store_client::KnowledgeStoreClient;

/// The agent in e2e/gateway/gateway.toml, and the scope it names.
const AGENT: &str = "e2e";
const SCOPE: &str = "e2e";

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set: run e2e/gateway/run.sh"))
}

/// A note name no earlier run used, so a cluster can be tested again.
fn fresh(what: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{what}-{nanos}")
}

/// The gateway agent's knowledge, as the app reaches it.
fn person() -> KnowledgeEndpoint {
    let token = env("UKAI_E2E_GATEWAY_TOKEN");
    let url = env("UKAI_E2E_GATEWAY").replacen("http", "ws", 1);
    let spec = AgentSpec::Gateway {
        url: format!("{url}/agents/{AGENT}/acp"),
        headers: BTreeMap::from([(
            "Authorization".to_owned(),
            "Bearer ${env:UKAI_E2E_GATEWAY_TOKEN}".to_owned(),
        )]),
        workspace: WorkspaceScope::Pod,
    };
    let token = &token;
    KnowledgeEndpoint::resolve(&spec, |_, name| {
        (name == "UKAI_E2E_GATEWAY_TOKEN").then(|| token.clone())
    })
    .unwrap_or_else(|e| panic!("{e}"))
    .unwrap_or_else(|| panic!("a gateway agent has knowledge"))
}

/// `ukai-knowledge` itself, presenting `token`.
struct Service {
    client: KnowledgeStoreClient<Channel>,
    token: String,
}

impl Service {
    async fn connect(token: &str) -> Self {
        let client = KnowledgeStoreClient::connect(env("UKAI_E2E_KNOWLEDGE"))
            .await
            .unwrap_or_else(|e| panic!("connect: {e}"));
        Self {
            client,
            token: token.to_owned(),
        }
    }

    fn request<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.token)
                .parse()
                .unwrap_or_else(|e| panic!("{e}")),
        );
        request
    }

    /// Write `name` as an agent in a workspace would.
    async fn write_as_agent(&mut self, name: &str, body: &str, base: u64) -> pb::ChangeResponse {
        let request = self.request(pb::PutRequest {
            caller: Some(agent()),
            name: name.to_owned(),
            spec: Some(pb::NoteSpec {
                description: "Written by an agent".into(),
                body: body.into(),
                tags: vec!["e2e".into()],
                on_demand: false,
            }),
            base_revision: base,
            commit: None,
        });
        self.client
            .put(request)
            .await
            .unwrap_or_else(|status| panic!("{status:?}"))
            .into_inner()
    }
}

fn agent() -> pb::Caller {
    pb::Caller {
        scope: SCOPE.into(),
        editor: pb::Editor::Agent.into(),
        workspace: Some("e2e-workspace".into()),
    }
}

fn write(body: &str, base: u64) -> NoteWrite {
    NoteWrite {
        spec: NoteSpec {
            description: "Written in e2e".into(),
            body: body.into(),
            tags: vec!["e2e".into(), "a-person".into()],
            on_demand: true,
        },
        base_revision: base,
        commit: None,
    }
}

#[tokio::test]
#[ignore = "needs a cluster: e2e/gateway/run.sh"]
async fn a_person_writes_reads_and_deletes_a_note_through_the_gateway() {
    let notes = person();
    let name = fresh("person");
    let note = notes
        .put(&name, &write("first", 0))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(note.revision, 1);
    assert!(!note.awaits_review(), "a person's write is approved");
    assert_eq!(note.spec.tags, ["a-person", "e2e"]);

    let listing = notes.list().await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(listing.scope, SCOPE);
    assert!(listing.notes.iter().any(|n| n.name == name && n.on_demand));

    let stale = notes.put(&name, &write("second", 0)).await;
    assert!(
        matches!(stale, Err(KnowledgeError::Conflict(_))),
        "{stale:?}"
    );
    let refused = notes.put(&name, &write("hidden\u{202E}", 1)).await;
    assert!(
        matches!(refused, Err(KnowledgeError::Refused(_))),
        "{refused:?}"
    );

    notes
        .put(&name, &write("second", 1))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let revisions = notes
        .revisions(&name)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        revisions.iter().map(|r| r.revision).collect::<Vec<_>>(),
        [2, 1]
    );
    notes
        .delete(&name, 2)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(
        notes.get(&name).await,
        Err(KnowledgeError::NotFound)
    ));
}

#[tokio::test]
#[ignore = "needs a cluster: e2e/gateway/run.sh"]
async fn a_person_approves_and_rejects_what_an_agent_wrote() {
    let notes = person();
    let mut service = Service::connect(&env("UKAI_E2E_KNOWLEDGE_TOKEN")).await;

    // Approved, then a newer revision rejected: back to the approved one.
    let name = fresh("agent");
    service.write_as_agent(&name, "first", 0).await;
    let note = notes.get(&name).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(note.awaits_review());
    assert_eq!(note.provenance.workspace.as_deref(), Some("e2e-workspace"));
    let approved = notes
        .approve(&name, 1)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(!approved.awaits_review());
    service.write_as_agent(&name, "second", 1).await;
    let back = notes
        .reject(&name, 2)
        .await
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("the approved revision stays"));
    assert_eq!((back.revision, back.spec.body.as_str()), (3, "first"));

    // Nothing approved: rejecting deletes it.
    let unwanted = fresh("unwanted");
    service.write_as_agent(&unwanted, "no", 0).await;
    assert_eq!(
        notes
            .reject(&unwanted, 1)
            .await
            .unwrap_or_else(|e| panic!("{e}")),
        None
    );
    assert!(matches!(
        notes.get(&unwanted).await,
        Err(KnowledgeError::NotFound)
    ));

    // An agent cannot review, even when it reaches the service.
    let request = service.request(pb::ReviewRequest {
        caller: Some(agent()),
        name: name.clone(),
        revision: 3,
    });
    let status = service.client.approve(request).await.map(drop);
    assert_eq!(status.map_err(|s| s.code()), Err(Code::PermissionDenied));
}

#[tokio::test]
#[ignore = "needs a cluster: e2e/gateway/run.sh"]
async fn the_knowledge_service_answers_its_token_only() {
    for token in ["", "not-the-token"] {
        let mut service = Service::connect(token).await;
        let request = service.request(pb::ListRequest {
            caller: Some(agent()),
        });
        let status = service.client.list(request).await.map(drop);
        assert_eq!(status.map_err(|s| s.code()), Err(Code::Unauthenticated));
    }
    let mut service = Service::connect(&env("UKAI_E2E_KNOWLEDGE_TOKEN")).await;
    let request = service.request(pb::ListRequest {
        caller: Some(agent()),
    });
    assert!(service.client.list(request).await.is_ok());
}

#[tokio::test]
#[ignore = "needs a cluster: e2e/gateway/run.sh"]
async fn the_gateway_answers_its_tokens_only() {
    let gateway = env("UKAI_E2E_GATEWAY");
    let with_token = |token: &str| {
        let spec = AgentSpec::Gateway {
            url: format!("{}/agents/{AGENT}/acp", gateway.replacen("http", "ws", 1)),
            headers: BTreeMap::from([("Authorization".to_owned(), format!("Bearer {token}"))]),
            workspace: WorkspaceScope::Pod,
        };
        KnowledgeEndpoint::resolve(&spec, |_, _| None)
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("a gateway agent has knowledge"))
    };
    let wrong = with_token("nope").list().await;
    assert!(matches!(wrong, Err(KnowledgeError::Denied(_))), "{wrong:?}");
    // A pod is known by its relay's token; there is no pod with this one.
    let pod = KnowledgeEndpoint::for_workspace(&gateway, "nope")
        .unwrap_or_else(|e| panic!("{e}"))
        .list()
        .await;
    assert!(matches!(pod, Err(KnowledgeError::Denied(_))), "{pod:?}");
    // The JSON API is gone.
    let old = reqwest::Client::new()
        .get(format!("{gateway}/agents/{AGENT}/knowledge"))
        .bearer_auth(env("UKAI_E2E_GATEWAY_TOKEN"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(old.status().as_u16(), 404);
}

/// The e2e agent as the app declares it, with `token`.
fn agent_spec(token: &str) -> AgentSpec {
    let url = env("UKAI_E2E_GATEWAY").replacen("http", "ws", 1);
    AgentSpec::Gateway {
        url: format!("{url}/agents/{AGENT}/acp"),
        headers: BTreeMap::from([("Authorization".to_owned(), format!("Bearer {token}"))]),
        workspace: WorkspaceScope::Pod,
    }
}

#[tokio::test]
#[ignore = "needs a cluster: e2e/gateway/run.sh"]
async fn schedules_are_kept_through_the_gateway() {
    use ukai_acp::schedules::{ScheduleSpec, SchedulesEndpoint, SchedulesError, Task};

    let token = env("UKAI_E2E_GATEWAY_TOKEN");
    let schedules = SchedulesEndpoint::resolve(&agent_spec(&token), |_, _| None)
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("a gateway agent has schedules"));
    // Disabled, so the gateway never fires it during the test.
    let spec = ScheduleSpec {
        name: fresh("nightly"),
        cron: "0 3 * * *".into(),
        utc_offset_minutes: 540,
        task: Task::Command {
            command: "true".into(),
        },
        enabled: false,
    };
    let created = schedules
        .create(&spec)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let id = created.schedule.id.clone();
    assert_eq!(created.schedule.agent, AGENT);
    assert_eq!(created.schedule.spec, spec);
    let listed = schedules.list().await.unwrap_or_else(|e| panic!("{e}"));
    assert!(listed.iter().any(|info| info.schedule.id == id));

    let renamed = ScheduleSpec {
        task: Task::Prompt {
            prompt: "triage".into(),
        },
        ..spec.clone()
    };
    let updated = schedules
        .update(&id, &renamed)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(updated.schedule.spec.task, renamed.task);
    let bad = ScheduleSpec {
        cron: "not a pattern".into(),
        ..spec.clone()
    };
    let refused = schedules.update(&id, &bad).await;
    assert!(
        matches!(refused, Err(SchedulesError::Refused(_))),
        "{refused:?}"
    );
    assert_eq!(schedules.runs(&id).await.ok(), Some(Vec::new()));
    assert!(schedules.stop(&id).await.is_ok());

    schedules
        .delete(&id)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(
        schedules.get(&id).await,
        Err(SchedulesError::NotFound)
    ));

    let denied = SchedulesEndpoint::resolve(&agent_spec("nope"), |_, _| None)
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("a gateway agent has schedules"))
        .list()
        .await;
    assert!(
        matches!(denied, Err(SchedulesError::Denied(_))),
        "{denied:?}"
    );
}

#[tokio::test]
#[ignore = "needs a cluster: e2e/gateway/run.sh"]
async fn a_workspace_that_is_not_running_is_left_alone() {
    use ukai_acp::PaneWorkspace;
    use ukai_acp::ports::{PortsEndpoint, PortsError};
    use ukai_acp::updates::RestartEndpoint;

    let token = env("UKAI_E2E_GATEWAY_TOKEN");
    let pane = PaneWorkspace {
        key: fresh("pane"),
        scope: WorkspaceScope::Pod,
    };
    let ports = PortsEndpoint::resolve(&agent_spec(&token), |_, _| None, Some(&pane))
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("a gateway agent has ports"))
        .list()
        .await;
    assert!(matches!(ports, Err(PortsError::Gone)), "{ports:?}");
    // Restarting a workspace with no pod leaves it without one.
    RestartEndpoint::resolve(&agent_spec(&token), |_, _| None, Some(&pane))
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("a gateway agent restarts"))
        .restart()
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let still = PortsEndpoint::resolve(&agent_spec(&token), |_, _| None, Some(&pane))
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("a gateway agent has ports"))
        .list()
        .await;
    assert!(matches!(still, Err(PortsError::Gone)), "{still:?}");
}

/// Plain gRPC over h2c, which the gateway answers next to gRPC-Web.
#[tokio::test]
#[ignore = "needs a cluster: e2e/gateway/run.sh"]
async fn the_gateway_answers_plain_grpc_too() {
    use ukai_proto::gateway::v1::agents_client::AgentsClient;

    let mut agents = AgentsClient::connect(env("UKAI_E2E_GATEWAY"))
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    let mut request = tonic::Request::new(ukai_proto::gateway::v1::ListAgentsRequest {});
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", env("UKAI_E2E_GATEWAY_TOKEN"))
            .parse()
            .unwrap_or_else(|e| panic!("{e}")),
    );
    let listed = agents
        .list(request)
        .await
        .unwrap_or_else(|status| panic!("{status:?}"))
        .into_inner();
    assert_eq!(listed.agents, [AGENT]);
    let anonymous = agents
        .list(ukai_proto::gateway::v1::ListAgentsRequest {})
        .await
        .map(drop);
    assert_eq!(anonymous.map_err(|s| s.code()), Err(Code::Unauthenticated));
}
