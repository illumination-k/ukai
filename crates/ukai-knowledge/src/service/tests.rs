use rstest::rstest;
use tonic::Code;
use ukai_proto::knowledge::v1::knowledge_store_server::KnowledgeStore;

use super::*;
use crate::store::StoreConfig;

const SCOPE: &str = "plekti";

async fn service() -> Service {
    let store = Knowledge::connect(&StoreConfig {
        url: "mem://".into(),
        credentials: None,
    })
    .await
    .unwrap_or_else(|e| panic!("connect: {e}"));
    Service::new(Arc::new(OnceCell::new_with(Some(store))))
}

fn person() -> Option<pb::Caller> {
    Some(pb::Caller {
        scope: SCOPE.into(),
        editor: pb::Editor::User.into(),
        workspace: None,
    })
}

fn agent(workspace: &str) -> Option<pb::Caller> {
    Some(pb::Caller {
        scope: SCOPE.into(),
        editor: pb::Editor::Agent.into(),
        workspace: Some(workspace.into()),
    })
}

fn put(caller: Option<pb::Caller>, name: &str, body: &str, base: u64) -> Request<pb::PutRequest> {
    Request::new(pb::PutRequest {
        caller,
        name: name.into(),
        spec: Some(pb::NoteSpec {
            description: "Building".into(),
            body: body.into(),
            tags: vec!["build".into()],
            on_demand: false,
        }),
        base_revision: base,
        commit: None,
    })
}

fn review(caller: Option<pb::Caller>, revision: u64) -> Request<pb::ReviewRequest> {
    Request::new(pb::ReviewRequest {
        caller,
        name: "build".into(),
        revision,
    })
}

fn outcome(response: Result<Response<pb::ChangeResponse>, Status>) -> Outcome {
    response
        .unwrap_or_else(|status| panic!("{status:?}"))
        .into_inner()
        .outcome
        .unwrap_or_else(|| panic!("no outcome"))
}

#[tokio::test]
async fn a_change_answers_with_the_note() {
    let service = service().await;
    let Outcome::Changed(changed) = outcome(service.put(put(agent("w1"), "build", "a", 0)).await)
    else {
        panic!("not changed");
    };
    let note = changed
        .note
        .map(Note::try_from)
        .transpose()
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("no note"));
    assert_eq!((note.revision, note.updated_by), (1, Editor::Agent));
    assert_eq!(note.provenance.workspace.as_deref(), Some("w1"));
    assert_eq!(note.spec.tags, ["build"]);

    let listed = service
        .list(Request::new(pb::ListRequest { caller: person() }))
        .await
        .unwrap_or_else(|status| panic!("{status:?}"))
        .into_inner();
    assert_eq!(listed.notes.len(), 1);
}

#[tokio::test]
async fn conflicts_and_refusals_are_answers() {
    let service = service().await;
    outcome(service.put(put(person(), "build", "a", 0)).await);
    assert!(matches!(
        outcome(service.put(put(person(), "build", "b", 0)).await),
        Outcome::Conflict(pb::Conflict { current: Some(note) }) if note.revision == 1
    ));
    assert!(matches!(
        outcome(service.put(put(person(), "Bad Name", "b", 0)).await),
        Outcome::Refused(pb::Refused { problems }) if !problems.is_empty()
    ));
}

#[tokio::test]
async fn only_a_person_reviews() {
    let service = service().await;
    outcome(service.put(put(agent("w1"), "build", "a", 0)).await);
    for result in [
        service.approve(review(agent("w1"), 1)).await,
        service.reject(review(agent("w1"), 1)).await,
    ] {
        assert_eq!(
            result.map(drop).map_err(|s| s.code()),
            Err(Code::PermissionDenied)
        );
    }
    // Rejecting what nobody approved deletes it.
    assert!(matches!(
        outcome(service.reject(review(person(), 1)).await),
        Outcome::Changed(pb::Changed { note: None })
    ));
}

#[tokio::test]
async fn a_workspace_changes_only_so_many_notes() {
    let service = service().await;
    for i in 0..WORKSPACE_CHANGES {
        outcome(
            service
                .put(put(agent("w1"), &format!("n{i}"), "a", 0))
                .await,
        );
    }
    let over = service.put(put(agent("w1"), "over", "a", 0)).await;
    assert_eq!(
        over.map(drop).map_err(|s| s.code()),
        Err(Code::ResourceExhausted)
    );
    // Another workspace, and a person, still may.
    outcome(service.put(put(agent("w2"), "other", "a", 0)).await);
    outcome(service.put(put(person(), "mine", "a", 0)).await);
}

#[tokio::test]
async fn a_missing_note_is_not_found() {
    let service = service().await;
    let result = service
        .delete(Request::new(pb::DeleteRequest {
            caller: person(),
            name: "nothing".into(),
            revision: 1,
        }))
        .await;
    assert_eq!(result.map(drop).map_err(|s| s.code()), Err(Code::NotFound));
    let got = service
        .get(Request::new(pb::GetRequest {
            caller: person(),
            name: "nothing".into(),
        }))
        .await
        .unwrap_or_else(|status| panic!("{status:?}"));
    assert_eq!(got.into_inner().note, None);
}

#[rstest]
#[case::no_caller(None)]
#[case::no_scope(Some(pb::Caller { scope: String::new(), editor: pb::Editor::User.into(), workspace: None }))]
#[case::no_editor(Some(pb::Caller { scope: SCOPE.into(), editor: pb::Editor::Unspecified.into(), workspace: None }))]
#[tokio::test]
async fn a_request_must_say_who_asks(#[case] caller: Option<pb::Caller>) {
    let service = service().await;
    let result = service.list(Request::new(pb::ListRequest { caller })).await;
    assert_eq!(
        result.map(drop).map_err(|s| s.code()),
        Err(Code::InvalidArgument)
    );
}

#[tokio::test]
async fn nothing_is_served_before_the_store_is_reached() {
    let service = Service::new(Arc::new(OnceCell::new()));
    let result = service
        .list(Request::new(pb::ListRequest { caller: person() }))
        .await;
    assert_eq!(
        result.map(drop).map_err(|s| s.code()),
        Err(Code::Unavailable)
    );
}

#[rstest]
#[case::right(Some("Bearer s3cret"), true)]
#[case::wrong(Some("Bearer s3crex"), false)]
#[case::longer(Some("Bearer s3cret!"), false)]
#[case::not_bearer(Some("s3cret"), false)]
#[case::none(None, false)]
fn only_the_gateways_token_is_let_through(#[case] header: Option<&str>, #[case] allowed: bool) {
    let mut request = Request::new(());
    if let Some(header) = header {
        request.metadata_mut().insert(
            "authorization",
            header.parse().unwrap_or_else(|e| panic!("{e}")),
        );
    }
    let mut authorize = authorize("s3cret".into());
    assert_eq!(authorize(request).is_ok(), allowed);
}

/// The service as the gateway reaches it: over TCP, behind the token.
#[tokio::test]
async fn the_gateway_reaches_it_over_grpc() {
    use tonic::transport::server::TcpIncoming;
    use ukai_proto::knowledge::v1::knowledge_store_client::KnowledgeStoreClient;
    use ukai_proto::knowledge::v1::knowledge_store_server::KnowledgeStoreServer;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let address = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("address: {e}"));
    let server = tonic::transport::Server::builder()
        .add_service(KnowledgeStoreServer::with_interceptor(
            service().await,
            authorize("s3cret".into()),
        ))
        .serve_with_incoming(TcpIncoming::from(listener));
    tokio::spawn(server);

    let mut client = KnowledgeStoreClient::connect(format!("http://{address}"))
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"));
    let list = |token: &str| {
        let mut request = Request::new(pb::ListRequest { caller: person() });
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .unwrap_or_else(|e| panic!("{e}")),
        );
        request
    };
    let refused = client.list(list("wrong")).await;
    assert_eq!(
        refused.map(drop).map_err(|s| s.code()),
        Err(Code::Unauthenticated)
    );

    let mut write = put(person(), "build", "a", 0);
    write.metadata_mut().insert(
        "authorization",
        "Bearer s3cret".parse().unwrap_or_else(|e| panic!("{e}")),
    );
    let written = client
        .put(write)
        .await
        .unwrap_or_else(|status| panic!("{status:?}"))
        .into_inner();
    assert!(matches!(written.outcome, Some(Outcome::Changed(_))));
    let listed = client
        .list(list("s3cret"))
        .await
        .unwrap_or_else(|status| panic!("{status:?}"))
        .into_inner();
    assert_eq!(
        listed
            .notes
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>(),
        ["build"]
    );
}
