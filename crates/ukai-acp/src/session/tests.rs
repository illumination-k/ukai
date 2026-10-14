//! End-to-end tests against an in-process mock agent over `Channel::duplex`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, InitializeResponse, LoadSessionResponse, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptResponse, RequestPermissionOutcome,
    ResumeSessionResponse, SessionCapabilities, SessionResumeCapabilities, StopReason,
    ToolCallUpdate, ToolCallUpdateFields, WriteTextFileRequest,
};
use agent_client_protocol::schema::v1::{PromptRequest, SessionId};
use agent_client_protocol::{Channel, ConnectionTo};
use tempfile::TempDir;

use super::*;

const SESSION: &str = "mock-session";

/// Which ways of continuing an earlier session the mock advertises.
#[derive(Debug, Clone, Copy, Default)]
struct Caps {
    load: bool,
    resume: bool,
}

/// Behaviour is chosen by the prompt text:
/// - `hello`: two message chunks, then `end_turn`
/// - `write <path>`: writes `<path>` through the client, then `end_turn`
/// - `ask`: requests permission, echoes the chosen option id, then `end_turn`
/// - anything else: the prompt fails
///
/// `session/load` replays one user and one agent message before answering.
fn mock_agent(caps: Caps) -> impl ConnectTo<Client> {
    Agent
        .builder()
        .name("mock")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _cx| {
                let session_caps = SessionCapabilities::new()
                    .resume(caps.resume.then(SessionResumeCapabilities::new));
                responder.respond(
                    InitializeResponse::new(request.protocol_version)
                        .agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(caps.load)
                                .session_capabilities(session_caps),
                        )
                        .agent_info(Implementation::new("mock", "0").title("Mock Agent")),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                for update in [
                    SessionUpdate::UserMessageChunk(ContentChunk::new("earlier question".into())),
                    SessionUpdate::AgentMessageChunk(ContentChunk::new("earlier answer".into())),
                ] {
                    cx.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        update,
                    ))?;
                }
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: ResumeSessionRequest, responder, _cx| {
                responder.respond(ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: NewSessionRequest, responder, _cx| {
                responder.respond(NewSessionResponse::new(SESSION))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                let text = match request.prompt.first() {
                    Some(ContentBlock::Text(text)) => text.text.clone(),
                    _ => String::new(),
                };
                // Waiting on the client from inside a handler would hold the
                // dispatch loop, so the turn runs in its own task.
                cx.spawn({
                    let cx = cx.clone();
                    async move {
                        let session = SessionId::new(SESSION);
                        let say = |text: &str| {
                            cx.send_notification(SessionNotification::new(
                                session.clone(),
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    ContentBlock::from(text.to_owned()),
                                )),
                            ))
                        };
                        if text == "hello" {
                            say("Hel")?;
                            say("lo")?;
                        } else if let Some(path) = text.strip_prefix("write ") {
                            cx.send_request(WriteTextFileRequest::new(
                                session.clone(),
                                path,
                                "<h1>report</h1>",
                            ))
                            .block_task()
                            .await?;
                        } else if text == "ask" {
                            let response = cx
                                .send_request(RequestPermissionRequest::new(
                                    session.clone(),
                                    ToolCallUpdate::new(
                                        "call-1",
                                        ToolCallUpdateFields::new().title("rm -rf build"),
                                    ),
                                    vec![
                                        PermissionOption::new(
                                            "allow",
                                            "Allow",
                                            PermissionOptionKind::AllowOnce,
                                        ),
                                        PermissionOption::new(
                                            "reject",
                                            "Reject",
                                            PermissionOptionKind::RejectOnce,
                                        ),
                                    ],
                                ))
                                .block_task()
                                .await?;
                            match response.outcome {
                                RequestPermissionOutcome::Selected(selected) => {
                                    say(&selected.option_id.0)?;
                                }
                                _ => say("cancelled")?,
                            }
                        } else {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params(),
                            );
                        }
                        responder.respond(PromptResponse::new(StopReason::EndTurn))
                    }
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
}

struct Harness {
    _dir: TempDir,
    root: PathBuf,
    handle: SessionHandle,
    events: mpsc::UnboundedReceiver<SessionEvent>,
}

impl Harness {
    async fn start() -> Self {
        let (harness, before_ready) = Self::connect(Caps::default(), None).await;
        assert_eq!(
            before_ready,
            vec![SessionEvent::Ready {
                agent: Some("Mock Agent".into()),
                session_id: SESSION.into(),
                restore: RestoreOutcome::Fresh,
            }]
        );
        harness
    }

    /// Connect and collect events up to and including `Ready`.
    async fn connect(caps: Caps, restore: Option<&str>) -> (Self, Vec<SessionEvent>) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let root = dir.path().canonicalize().unwrap_or_else(|e| panic!("{e}"));
        let (client_end, agent_end) = Channel::duplex();
        tokio::spawn(mock_agent(caps).connect_to(agent_end));
        let options = SessionOptions {
            cwd: root.clone(),
            fs_root: FsRoot::new(&root).unwrap_or_else(|e| panic!("{e}")),
            restore: restore.map(str::to_owned),
        };
        let (handle, events) = spawn_on(client_end, options);
        let mut harness = Self {
            _dir: dir,
            root,
            handle,
            events,
        };
        let mut seen = Vec::new();
        loop {
            let event = harness.next().await;
            let ready = matches!(event, SessionEvent::Ready { .. });
            seen.push(event);
            if ready {
                return (harness, seen);
            }
        }
    }

    async fn next(&mut self) -> SessionEvent {
        match tokio::time::timeout(Duration::from_secs(5), self.events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => panic!("event channel closed"),
            Err(_) => panic!("timed out waiting for a session event"),
        }
    }

    /// Prompt and collect events through the end of the turn.
    async fn turn(&mut self, prompt: &str) -> Vec<SessionEvent> {
        assert!(self.handle.prompt(prompt));
        self.until_turn_end().await
    }

    async fn until_turn_end(&mut self) -> Vec<SessionEvent> {
        let mut seen = Vec::new();
        loop {
            let event = self.next().await;
            let last = matches!(
                event,
                SessionEvent::TurnEnded { .. } | SessionEvent::PromptFailed { .. }
            );
            seen.push(event);
            if last {
                return seen;
            }
        }
    }
}

fn end_turn() -> SessionEvent {
    SessionEvent::TurnEnded {
        stop_reason: "end_turn".into(),
    }
}

#[tokio::test]
async fn streams_message_chunks_before_turn_end() {
    let mut harness = Harness::start().await;
    assert_eq!(
        harness.turn("hello").await,
        vec![
            SessionEvent::AgentMessage { text: "Hel".into() },
            SessionEvent::AgentMessage { text: "lo".into() },
            end_turn(),
        ]
    );
}

#[tokio::test]
async fn failed_prompt_keeps_session_usable() {
    let mut harness = Harness::start().await;
    let events = harness.turn("unknown").await;
    assert!(matches!(
        events.as_slice(),
        [SessionEvent::PromptFailed { .. }]
    ));
    assert_eq!(harness.turn("hello").await.last(), Some(&end_turn()));
}

#[tokio::test]
async fn writes_files_inside_root() {
    let mut harness = Harness::start().await;
    let target = harness.root.join("reports/r.html");
    let events = harness.turn(&format!("write {}", target.display())).await;
    assert_eq!(
        events,
        vec![
            SessionEvent::FileWritten {
                path: target.clone()
            },
            end_turn()
        ]
    );
    assert_eq!(
        std::fs::read_to_string(&target).ok().as_deref(),
        Some("<h1>report</h1>")
    );
}

#[tokio::test]
async fn refuses_writes_outside_root() {
    let mut harness = Harness::start().await;
    let outside = Path::new("/tmp/ukai-should-not-exist/x.html");
    let events = harness.turn(&format!("write {}", outside.display())).await;
    // The agent's request fails, so its turn fails with it.
    assert!(matches!(
        events.as_slice(),
        [SessionEvent::PromptFailed { .. }] | [SessionEvent::Closed { .. }]
    ));
    assert!(!outside.exists());
}

#[tokio::test]
async fn permission_round_trip() {
    let mut harness = Harness::start().await;
    assert!(harness.handle.prompt("ask"));
    let SessionEvent::PermissionRequested { id, title, options } = harness.next().await else {
        panic!("expected a permission request");
    };
    assert_eq!(title, "rm -rf build");
    assert_eq!(
        options
            .iter()
            .map(|option| (option.id.as_str(), option.kind.as_str()))
            .collect::<Vec<_>>(),
        [("allow", "allow_once"), ("reject", "reject_once")]
    );
    assert!(harness.handle.resolve_permission(id, Some("reject".into())));
    assert_eq!(
        harness.until_turn_end().await,
        vec![
            SessionEvent::AgentMessage {
                text: "reject".into()
            },
            end_turn()
        ]
    );
}

#[tokio::test]
async fn cancel_answers_pending_permission() {
    let mut harness = Harness::start().await;
    assert!(harness.handle.prompt("ask"));
    assert!(matches!(
        harness.next().await,
        SessionEvent::PermissionRequested { .. }
    ));
    assert!(harness.handle.cancel());
    assert_eq!(
        harness.until_turn_end().await,
        vec![
            SessionEvent::AgentMessage {
                text: "cancelled".into()
            },
            end_turn()
        ]
    );
}

#[tokio::test]
async fn dropping_the_handle_closes_the_session() {
    let Harness {
        handle, mut events, ..
    } = Harness::start().await;
    drop(handle);
    let closed = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    assert!(matches!(closed, Ok(Some(SessionEvent::Closed { .. }))));
}

fn ready(session_id: &str, restore: RestoreOutcome) -> SessionEvent {
    SessionEvent::Ready {
        agent: Some("Mock Agent".into()),
        session_id: session_id.into(),
        restore,
    }
}

#[tokio::test]
async fn load_replays_history_before_ready() {
    let caps = Caps {
        load: true,
        resume: true,
    };
    let (_harness, events) = Harness::connect(caps, Some("earlier")).await;
    assert_eq!(
        events,
        vec![
            SessionEvent::UserMessage {
                text: "earlier question".into()
            },
            SessionEvent::AgentMessage {
                text: "earlier answer".into()
            },
            ready("earlier", RestoreOutcome::Replayed),
        ]
    );
}

#[rstest::rstest]
#[case::resume_without_load(Caps { load: false, resume: true }, "earlier", RestoreOutcome::Resumed)]
#[case::neither(Caps::default(), SESSION, RestoreOutcome::Unsupported)]
#[tokio::test]
async fn falls_back_when_load_is_unavailable(
    #[case] caps: Caps,
    #[case] session_id: &str,
    #[case] outcome: RestoreOutcome,
) {
    let (mut harness, events) = Harness::connect(caps, Some("earlier")).await;
    assert_eq!(events, vec![ready(session_id, outcome)]);
    assert_eq!(harness.turn("hello").await.last(), Some(&end_turn()));
}
