//! [`KnowledgeEndpoint`] against a gateway of its own: the generated servers
//! behind gRPC-Web, as the gateway mounts them, on a local port.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use tonic::{Request, Response, Status};
use tonic_web::GrpcWebLayer;
use tower::Layer;
use ukai_config::WorkspaceScope;
use ukai_proto::gateway::v1::knowledge_server::{Knowledge, KnowledgeServer};
use ukai_proto::gateway::v1::workspace_knowledge_server::{
    WorkspaceKnowledge, WorkspaceKnowledgeServer,
};

use super::*;

/// What the fake gateway was asked, by whom.
#[derive(Default)]
struct Seen {
    /// `authorization` of each request.
    tokens: Vec<String>,
    /// The agent of each person's request; `-` for a pod's.
    agents: Vec<String>,
    commits: Vec<Option<String>>,
}

#[derive(Clone, Default)]
struct Fake {
    seen: Arc<Mutex<Seen>>,
}

fn note(name: &str, revision: u64) -> pb::Note {
    Note {
        name: name.into(),
        spec: NoteSpec {
            description: "d".into(),
            body: "b".into(),
            tags: vec!["t".into()],
            on_demand: true,
        },
        revision,
        updated: 1,
        updated_by: Editor::User,
        provenance: Provenance::default(),
        approved: None,
    }
    .into()
}

fn changed(note: Option<pb::Note>) -> Response<pb::ChangeResponse> {
    Response::new(pb::ChangeResponse {
        outcome: Some(Outcome::Changed(pb::Changed { note })),
    })
}

impl Fake {
    /// Note who asks, refusing anything without the token `t`.
    fn see<T>(&self, request: &Request<T>, agent: &str) -> Result<(), Status> {
        let token = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.tokens.push(token.clone());
        seen.agents.push(agent.to_owned());
        if token == "Bearer t" {
            Ok(())
        } else {
            Err(Status::unauthenticated("no"))
        }
    }

    /// The answer to a write of `name` at `base`.
    fn write(name: &str, base: u64) -> Result<Response<pb::ChangeResponse>, Status> {
        let outcome = match (name, base) {
            ("bad", _) => Outcome::Refused(pb::Refused {
                problems: vec!["a name is lowercase".into()],
            }),
            (_, 9) => Outcome::Conflict(pb::Conflict {
                current: Some(note(name, 2)),
            }),
            ("busy", _) => return Err(Status::resource_exhausted("too many")),
            _ => Outcome::Changed(pb::Changed {
                note: Some(note(name, base + 1)),
            }),
        };
        Ok(Response::new(pb::ChangeResponse {
            outcome: Some(outcome),
        }))
    }

    fn get(name: &str) -> Response<pb::GetResponse> {
        Response::new(pb::GetResponse {
            note: (name != "missing").then(|| note(name, 1)),
        })
    }
}

#[tonic::async_trait]
impl Knowledge for Fake {
    async fn list(
        &self,
        request: Request<gw::AgentRequest>,
    ) -> Result<Response<gw::Listing>, Status> {
        self.see(&request, &request.get_ref().agent)?;
        if request.get_ref().agent != "a" {
            return Err(Status::not_found("no such agent"));
        }
        Ok(Response::new(gw::Listing {
            scope: "plekti".into(),
            notes: vec![
                Note::try_from(note("build", 1))
                    .map(|n| n.summary().into())
                    .map_err(|e| Status::internal(e.to_string()))?,
            ],
        }))
    }

    async fn get(
        &self,
        request: Request<gw::NoteRequest>,
    ) -> Result<Response<pb::GetResponse>, Status> {
        self.see(&request, &request.get_ref().agent)?;
        Ok(Self::get(&request.get_ref().name))
    }

    async fn revisions(
        &self,
        request: Request<gw::NoteRequest>,
    ) -> Result<Response<pb::RevisionsResponse>, Status> {
        self.see(&request, &request.get_ref().agent)?;
        Ok(Response::new(pb::RevisionsResponse::default()))
    }

    async fn put(
        &self,
        request: Request<gw::PutRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        self.see(&request, &request.get_ref().agent)?;
        let request = request.into_inner();
        Self::write(&request.name, request.base_revision)
    }

    async fn delete(
        &self,
        request: Request<gw::RevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        self.see(&request, &request.get_ref().agent)?;
        Ok(changed(None))
    }

    async fn approve(
        &self,
        request: Request<gw::RevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        self.see(&request, &request.get_ref().agent)?;
        Ok(changed(Some(note(&request.get_ref().name, 1))))
    }

    async fn reject(
        &self,
        request: Request<gw::RevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        self.see(&request, &request.get_ref().agent)?;
        Ok(changed(None))
    }
}

#[tonic::async_trait]
impl WorkspaceKnowledge for Fake {
    async fn list(
        &self,
        request: Request<gw::WorkspaceListRequest>,
    ) -> Result<Response<gw::Listing>, Status> {
        self.see(&request, "-")?;
        Ok(Response::new(gw::Listing {
            scope: "github.com/o/r".into(),
            notes: Vec::new(),
        }))
    }

    async fn get(
        &self,
        request: Request<gw::WorkspaceNoteRequest>,
    ) -> Result<Response<pb::GetResponse>, Status> {
        self.see(&request, "-")?;
        Ok(Self::get(&request.get_ref().name))
    }

    async fn revisions(
        &self,
        request: Request<gw::WorkspaceNoteRequest>,
    ) -> Result<Response<pb::RevisionsResponse>, Status> {
        self.see(&request, "-")?;
        Ok(Response::new(pb::RevisionsResponse::default()))
    }

    async fn put(
        &self,
        request: Request<gw::WorkspacePutRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        self.see(&request, "-")?;
        let request = request.into_inner();
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .commits
            .push(request.commit);
        Self::write(&request.name, request.base_revision)
    }

    async fn delete(
        &self,
        request: Request<gw::WorkspaceRevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        self.see(&request, "-")?;
        Ok(changed(None))
    }
}

/// Serve `fake` as the gateway does, returning its root URL.
async fn serve(fake: Fake) -> String {
    let router =
        tonic::service::Routes::new(GrpcWebLayer::new().layer(KnowledgeServer::new(fake.clone())))
            .add_service(GrpcWebLayer::new().layer(WorkspaceKnowledgeServer::new(fake)))
            .prepare()
            .into_axum_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("address: {e}"));
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

fn person(root: &str, agent: &str, token: &str) -> KnowledgeEndpoint {
    let spec = AgentSpec::Gateway {
        url: format!("{}/agents/{agent}/acp", root.replacen("http", "ws", 1)),
        headers: BTreeMap::from([("Authorization".into(), format!("Bearer {token}"))]),
        workspace: WorkspaceScope::Pod,
    };
    KnowledgeEndpoint::resolve(&spec, |_, _| None)
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("a gateway agent"))
}

#[tokio::test]
async fn a_person_asks_through_the_agent_with_its_headers() {
    let fake = Fake::default();
    let root = serve(fake.clone()).await;
    let notes = person(&root, "a", "t");

    let listing = notes.list().await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(listing.scope, "plekti");
    assert_eq!(
        listing
            .notes
            .first()
            .map(|n| (n.name.as_str(), n.on_demand)),
        Some(("build", true))
    );
    let note = notes.get("build").await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(note.spec.tags, ["t"]);
    assert!(matches!(
        notes.get("missing").await,
        Err(KnowledgeError::NotFound)
    ));
    let write = |base| NoteWrite {
        spec: note.spec.clone(),
        base_revision: base,
        commit: None,
    };
    let saved = notes
        .put("build", &write(1))
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(saved.revision, 2);
    assert!(matches!(
        notes.put("build", &write(9)).await,
        Err(KnowledgeError::Conflict(current)) if current.as_ref().as_ref().map(|n| n.revision) == Some(2)
    ));
    assert!(matches!(
        notes.put("bad", &write(0)).await,
        Err(KnowledgeError::Refused(problems)) if problems == ["a name is lowercase"]
    ));
    assert!(matches!(
        notes.put("busy", &write(0)).await,
        Err(KnowledgeError::TooMany)
    ));
    assert!(
        notes
            .approve("build", 2)
            .await
            .is_ok_and(|n| n.name == "build")
    );
    assert_eq!(notes.reject("build", 2).await.ok(), Some(None));
    assert!(notes.delete("build", 2).await.is_ok());
    assert_eq!(notes.revisions("build").await.ok(), Some(Vec::new()));

    let seen = fake.seen.lock().unwrap_or_else(PoisonError::into_inner);
    assert!(
        seen.tokens.iter().all(|t| t == "Bearer t"),
        "{:?}",
        seen.tokens
    );
    assert!(seen.agents.iter().all(|a| a == "a"), "{:?}", seen.agents);
}

#[tokio::test]
async fn a_pod_asks_with_its_token_and_cannot_review() {
    let fake = Fake::default();
    let root = serve(fake.clone()).await;
    let notes = KnowledgeEndpoint::for_workspace(&format!("{root}/"), "t")
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        notes.list().await.map(|l| l.scope).ok(),
        Some("github.com/o/r".to_owned())
    );
    let saved = notes
        .put(
            "build",
            &NoteWrite {
                spec: NoteSpec {
                    description: "d".into(),
                    body: "b".into(),
                    tags: Vec::new(),
                    on_demand: false,
                },
                base_revision: 0,
                commit: Some("0123abcd".into()),
            },
        )
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(saved.revision, 1);
    assert!(matches!(
        notes.approve("build", 1).await,
        Err(KnowledgeError::Denied(_))
    ));
    assert!(matches!(
        notes.reject("build", 1).await,
        Err(KnowledgeError::Denied(_))
    ));
    let seen = fake.seen.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(seen.agents, ["-", "-"]);
    assert_eq!(seen.commits, [Some("0123abcd".to_owned())]);
}

#[tokio::test]
async fn what_goes_wrong_is_told_apart() {
    let root = serve(Fake::default()).await;
    assert!(matches!(
        person(&root, "a", "wrong").list().await,
        Err(KnowledgeError::Denied(_))
    ));
    assert!(matches!(
        person(&root, "nobody", "t").list().await,
        Err(KnowledgeError::NotFound)
    ));

    // A server that is not the gateway: nothing answers the service.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("address: {e}"));
    tokio::spawn(async move { axum::serve(listener, axum::Router::new()).await });
    let old = person(&format!("http://{address}"), "a", "t").list().await;
    assert!(matches!(old, Err(KnowledgeError::Failed(_))), "{old:?}");

    // Nothing listening at all.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let gone = closed
        .local_addr()
        .unwrap_or_else(|e| panic!("address: {e}"));
    drop(closed);
    let unreachable = person(&format!("http://{gone}"), "a", "t").list().await;
    assert!(
        matches!(unreachable, Err(KnowledgeError::Unreachable(_))),
        "{unreachable:?}"
    );
}
