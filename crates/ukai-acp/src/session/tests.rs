//! End-to-end tests against an in-process mock agent over `Channel::duplex`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, Diff, InitializeResponse,
    ListSessionsResponse, LoadSessionResponse, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptCapabilities, PromptResponse, RequestPermissionOutcome,
    ResumeSessionResponse, SessionCapabilities, SessionConfigOptionCategory,
    SessionConfigSelectGroup, SessionConfigSelectOption, SessionInfo, SessionListCapabilities,
    SessionResumeCapabilities, SetSessionConfigOptionResponse, StopReason, ToolCall,
    ToolCallUpdate, ToolCallUpdateFields, UnstructuredCommandInput, UsageUpdate,
    WriteTextFileRequest,
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
    steering: bool,
    list: bool,
}

/// Behaviour is chosen by the prompt text:
/// - `hello`: two message chunks, then `end_turn`
/// - `write <path>`: writes `<path>` through the client, then `end_turn`
/// - `ask`: requests permission, echoes the chosen option id, then `end_turn`
/// - `fs`: says which `fs/*` methods the client offered, then `end_turn`
/// - `edit`: reports a tool call with a diff, then updates it with another
/// - `blocks`: describes every block of the prompt, then `end_turn`
/// - `usage`: reports context use and cost, then `end_turn`
/// - `commands`: lists two slash commands, then `end_turn`
/// - `wait`: waits for a steer, says `steered: <text>`, then `end_turn`
/// - anything else: the prompt fails
///
/// A steer outside a `wait` turn answers `promptRequired`.
///
/// `session/load` replays one user and one agent message before answering.
/// `session/list` answers two sessions in `cwd`, one page each.
/// Every session starts with [`mock_config`]'s settings; setting one answers
/// with it changed, and an unknown one fails.
fn mock_agent(caps: Caps) -> impl ConnectTo<Client> {
    let offered_fs = Arc::new(std::sync::Mutex::new(String::new()));
    // Where a running `wait` turn takes its steer.
    let waiting = Arc::new(std::sync::Mutex::new(
        None::<tokio::sync::oneshot::Sender<String>>,
    ));
    Agent
        .builder()
        .name("mock")
        .on_receive_request(
            {
                let offered_fs = offered_fs.clone();
                async move |request: InitializeRequest, responder, _cx| {
                    let fs = &request.client_capabilities.fs;
                    if let Ok(mut offered) = offered_fs.lock() {
                        *offered =
                            format!("read={} write={}", fs.read_text_file, fs.write_text_file);
                    }
                    let session_caps = SessionCapabilities::new()
                        .resume(caps.resume.then(SessionResumeCapabilities::new))
                        .list(caps.list.then(SessionListCapabilities::new));
                    let mut meta = agent_client_protocol::schema::v1::Meta::new();
                    if caps.steering {
                        meta.insert("steering".into(), serde_json::json!({ "supported": true }));
                    }
                    responder.respond(
                        InitializeResponse::new(request.protocol_version)
                            .meta(meta)
                            .agent_capabilities(
                                AgentCapabilities::new()
                                    .load_session(caps.load)
                                    .session_capabilities(session_caps)
                                    .prompt_capabilities(
                                        PromptCapabilities::new()
                                            .image(true)
                                            .embedded_context(true),
                                    ),
                            )
                            .agent_info(Implementation::new("mock", "0").title("Mock Agent")),
                    )
                }
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
                responder.respond(LoadSessionResponse::new().config_options(mock_config(None)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: ListSessionsRequest, responder, _cx| {
                let cwd = request.cwd.unwrap_or_default();
                let response = match request.cursor.as_deref() {
                    None => ListSessionsResponse::new(vec![
                        SessionInfo::new("earlier", cwd)
                            .title("Fix the build")
                            .updated_at("2026-10-14T12:00:00Z"),
                    ])
                    .next_cursor("2"),
                    Some(_) => ListSessionsResponse::new(vec![SessionInfo::new("oldest", cwd)]),
                };
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: ResumeSessionRequest, responder, _cx| {
                responder.respond(ResumeSessionResponse::new().config_options(mock_config(None)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |_: NewSessionRequest, responder, _cx| {
                responder
                    .respond(NewSessionResponse::new(SESSION).config_options(mock_config(None)))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async |request: SetSessionConfigOptionRequest, responder, _cx| {
                if !["model", "fast"].contains(&&*request.config_id.0) {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::invalid_params());
                }
                responder.respond(SetSessionConfigOptionResponse::new(mock_config(Some(
                    &request.value,
                ))))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let waiting = waiting.clone();
                async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let offered_fs = offered_fs.lock().map(|o| o.clone()).unwrap_or_default();
                    let steered = if let Some(ContentBlock::Text(text)) = request.prompt.first()
                        && text.text == "wait"
                    {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        if let Ok(mut waiting) = waiting.lock() {
                            *waiting = Some(tx);
                        }
                        Some(rx)
                    } else {
                        None
                    };
                    let text = match request.prompt.first() {
                        Some(ContentBlock::Text(text)) => text.text.clone(),
                        _ => String::new(),
                    };
                    let blocks = describe_blocks(&request.prompt);
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
                            if let Some(steered) = steered {
                                let text = steered.await.unwrap_or_default();
                                say(&format!("steered: {text}"))?;
                            } else if text == "hello" {
                                say("Hel")?;
                                say("lo")?;
                            } else if text == "fs" {
                                say(&offered_fs)?;
                            } else if text == "blocks" {
                                say(&blocks)?;
                            } else if text == "usage" {
                                cx.send_notification(SessionNotification::new(
                                    session.clone(),
                                    SessionUpdate::UsageUpdate(
                                        UsageUpdate::new(53_000, 200_000).cost(
                                            agent_client_protocol::schema::v1::Cost::new(
                                                0.045, "USD",
                                            ),
                                        ),
                                    ),
                                ))?;
                            } else if text == "commands" {
                                cx.send_notification(SessionNotification::new(
                                    session.clone(),
                                    SessionUpdate::AvailableCommandsUpdate(
                                        AvailableCommandsUpdate::new(vec![
                                            AvailableCommand::new("compact", "Compact the context"),
                                            AvailableCommand::new("review", "Review a change")
                                                .input(AvailableCommandInput::Unstructured(
                                                    UnstructuredCommandInput::new("pr number"),
                                                )),
                                        ]),
                                    ),
                                ))?;
                            } else if text == "edit" {
                                let update = |update| {
                                    cx.send_notification(SessionNotification::new(
                                        session.clone(),
                                        update,
                                    ))
                                };
                                update(SessionUpdate::ToolCall(
                                    ToolCall::new("edit-1", "Edit a.txt").content(vec![
                                        Diff::new("/p/a.txt", "new").old_text("old").into(),
                                    ]),
                                ))?;
                                update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                    "edit-1",
                                    ToolCallUpdateFields::new()
                                        .content(vec![Diff::new("/p/b.txt", "created").into()]),
                                )))?;
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
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: UntypedMessage, responder, _cx| {
                if !caps.steering || request.method() != STEER_METHOD {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::method_not_found());
                }
                let turn = waiting.lock().ok().and_then(|mut w| w.take());
                let Some(turn) = turn else {
                    return responder.respond(serde_json::json!({ "outcome": "promptRequired" }));
                };
                let text = request.params()["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let _ = turn.send(text);
                responder.respond(serde_json::json!({ "outcome": "injected" }))
            },
            agent_client_protocol::on_receive_request!(),
        )
}

/// A grouped model select and a boolean, with `set` applied to whichever it fits.
fn mock_config(set: Option<&SessionConfigOptionValue>) -> Vec<SessionConfigOption> {
    let model = set
        .and_then(SessionConfigOptionValue::as_value_id)
        .map_or_else(|| "sonnet".into(), |id| id.0.to_string());
    let fast = set.and_then(SessionConfigOptionValue::as_bool) == Some(true);
    vec![
        SessionConfigOption::select(
            "model",
            "Model",
            model,
            vec![SessionConfigSelectGroup::new(
                "claude",
                "Claude",
                vec![
                    SessionConfigSelectOption::new("sonnet", "Sonnet"),
                    SessionConfigSelectOption::new("opus", "Opus").description("Most capable"),
                ],
            )],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::boolean("fast", "Fast mode", fast),
    ]
}

/// [`mock_config`] as the client reports it.
fn config(model: &str, fast: bool) -> Vec<ConfigOption> {
    let choice = |value: &str, name: &str, description: Option<&str>| ConfigChoice {
        value: value.into(),
        name: name.into(),
        description: description.map(str::to_owned),
        group: Some("Claude".into()),
    };
    vec![
        ConfigOption {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: Some("model".into()),
            value: ConfigValue::Select {
                current: model.into(),
                choices: vec![
                    choice("sonnet", "Sonnet", None),
                    choice("opus", "Opus", Some("Most capable")),
                ],
            },
        },
        ConfigOption {
            id: "fast".into(),
            name: "Fast mode".into(),
            description: None,
            category: None,
            value: ConfigValue::Toggle(fast),
        },
    ]
}

/// `kind:detail` per prompt block, joined with `,`.
fn describe_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => format!("text:{}", text.text),
            ContentBlock::Image(image) => format!("image:{}:{}", image.mime_type, image.data),
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    format!("resource:{}:{}", text.uri, text.text)
                }
                _ => "resource:blob".to_owned(),
            },
            ContentBlock::ResourceLink(link) => format!("link:{}:{}", link.name, link.uri),
            _ => "other".to_owned(),
        })
        .collect::<Vec<_>>()
        .join(",")
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
        assert_eq!(before_ready, vec![ready(SESSION, RestoreOutcome::Fresh)]);
        harness
    }

    /// Connect and collect events up to and including `Ready`.
    async fn connect(caps: Caps, restore: Option<&str>) -> (Self, Vec<SessionEvent>) {
        Self::connect_with(caps, restore, true).await
    }

    /// [`Self::connect`], offering `fs/*` only when `fs` is set.
    async fn connect_with(
        caps: Caps,
        restore: Option<&str>,
        fs: bool,
    ) -> (Self, Vec<SessionEvent>) {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let root = dir.path().canonicalize().unwrap_or_else(|e| panic!("{e}"));
        let (client_end, agent_end) = Channel::duplex();
        tokio::spawn(mock_agent(caps).connect_to(agent_end));
        let options = SessionOptions {
            cwd: root.clone(),
            fs_root: fs.then(|| FsRoot::new(&root).unwrap_or_else(|e| panic!("{e}"))),
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
async fn translates_available_commands() {
    let mut harness = Harness::start().await;
    assert_eq!(
        harness.turn("commands").await,
        vec![
            SessionEvent::Commands {
                commands: vec![
                    SlashCommand {
                        name: "compact".into(),
                        description: "Compact the context".into(),
                        hint: None,
                    },
                    SlashCommand {
                        name: "review".into(),
                        description: "Review a change".into(),
                        hint: Some("pr number".into()),
                    },
                ],
            },
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
async fn offers_fs_only_with_a_root() {
    let mut harness = Harness::start().await;
    assert_eq!(
        harness.turn("fs").await,
        vec![
            SessionEvent::AgentMessage {
                text: "read=true write=true".into()
            },
            end_turn()
        ]
    );

    let (mut harness, _) = Harness::connect_with(Caps::default(), None, false).await;
    assert_eq!(
        harness.turn("fs").await,
        vec![
            SessionEvent::AgentMessage {
                text: "read=false write=false".into()
            },
            end_turn()
        ]
    );
    // An agent that writes anyway is refused, and nothing reaches the disk.
    let target = harness.root.join("r.html");
    let events = harness.turn(&format!("write {}", target.display())).await;
    assert!(matches!(
        events.as_slice(),
        [SessionEvent::PromptFailed { .. }] | [SessionEvent::Closed { .. }]
    ));
    assert!(!target.exists());
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
        prompt: PromptCaps {
            image: true,
            embedded_context: true,
            steering: false,
        },
        config: config("sonnet", false),
        list_sessions: false,
    }
}

#[tokio::test]
async fn lists_sessions_across_pages() {
    let caps = Caps {
        list: true,
        ..Caps::default()
    };
    let (mut harness, events) = Harness::connect(caps, None).await;
    assert!(matches!(
        events.last(),
        Some(SessionEvent::Ready {
            list_sessions: true,
            ..
        })
    ));
    assert!(harness.handle.list_sessions());
    assert_eq!(
        harness.next().await,
        SessionEvent::Sessions {
            sessions: vec![
                PastSession {
                    id: "earlier".into(),
                    title: Some("Fix the build".into()),
                    updated_at: Some("2026-10-14T12:00:00Z".into()),
                },
                PastSession {
                    id: "oldest".into(),
                    title: None,
                    updated_at: None,
                },
            ],
        }
    );
}

#[tokio::test]
async fn tool_calls_carry_their_diffs() {
    let mut harness = Harness::start().await;
    let events = harness.turn("edit").await;
    assert_eq!(
        events,
        vec![
            SessionEvent::ToolCall {
                id: "edit-1".into(),
                title: "Edit a.txt".into(),
                kind: "other".into(),
                status: ToolStatus::Pending,
                diffs: vec![FileDiff {
                    path: "/p/a.txt".into(),
                    old_text: Some("old".into()),
                    new_text: "new".into(),
                }],
            },
            SessionEvent::ToolCallUpdate {
                id: "edit-1".into(),
                title: None,
                status: None,
                diffs: Some(vec![FileDiff {
                    path: "/p/b.txt".into(),
                    old_text: None,
                    new_text: "created".into(),
                }]),
            },
            end_turn(),
        ]
    );
}

#[tokio::test]
async fn reports_usage() {
    let mut harness = Harness::start().await;
    let events = harness.turn("usage").await;
    assert_eq!(
        events,
        vec![
            SessionEvent::Usage(Usage {
                used: 53_000,
                size: 200_000,
                cost: Some(Cost {
                    amount: 0.045,
                    currency: "USD".into(),
                }),
            }),
            end_turn(),
        ]
    );
}

#[tokio::test]
async fn prompts_carry_their_attachments() {
    let mut harness = Harness::start().await;
    let prompt = Prompt {
        text: "blocks".into(),
        attachments: vec![
            Attachment::Image {
                mime_type: "image/png".into(),
                data: b"png".to_vec(),
            },
            Attachment::Text {
                uri: "file:///p/a.txt".into(),
                text: "hi".into(),
            },
            Attachment::Link {
                name: "b.bin".into(),
                uri: "file:///p/b.bin".into(),
            },
        ],
    };
    assert!(harness.handle.prompt(prompt));
    assert_eq!(
        harness.until_turn_end().await,
        vec![
            SessionEvent::AgentMessage {
                text: "text:blocks,image:image/png:cG5n,resource:file:///p/a.txt:hi,link:b.bin:file:///p/b.bin"
                    .into()
            },
            end_turn(),
        ]
    );
}

#[tokio::test]
async fn load_replays_history_before_ready() {
    let caps = Caps {
        load: true,
        resume: true,
        ..Caps::default()
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
#[case::resume_without_load(Caps { resume: true, ..Caps::default() }, "earlier", RestoreOutcome::Resumed)]
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

#[tokio::test]
async fn set_config_reports_the_agents_new_settings() {
    let mut harness = Harness::start().await;
    assert!(
        harness
            .handle
            .set_config("model", ConfigInput::Select("opus".into()))
    );
    assert_eq!(
        harness.next().await,
        SessionEvent::ConfigChanged {
            options: config("opus", false)
        }
    );
    assert!(harness.handle.set_config("fast", ConfigInput::Toggle(true)));
    assert_eq!(
        harness.next().await,
        SessionEvent::ConfigChanged {
            options: config("sonnet", true)
        }
    );
    assert!(harness.handle.set_config("nope", ConfigInput::Toggle(true)));
    assert!(matches!(
        harness.next().await,
        SessionEvent::ConfigFailed { .. }
    ));
    // The session goes on.
    assert_eq!(harness.turn("hello").await.last(), Some(&end_turn()));
}

async fn steering_harness() -> Harness {
    let caps = Caps {
        steering: true,
        ..Caps::default()
    };
    let (harness, before_ready) = Harness::connect(caps, None).await;
    assert!(matches!(
        before_ready.last(),
        Some(SessionEvent::Ready {
            prompt: PromptCaps { steering: true, .. },
            ..
        })
    ));
    harness
}

#[tokio::test]
async fn steering_joins_the_running_turn() {
    let mut harness = steering_harness().await;
    assert!(harness.handle.prompt("wait"));
    // Let the turn start before steering it.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(harness.handle.steer("left instead"));
    assert_eq!(
        harness.until_turn_end().await,
        vec![
            SessionEvent::AgentMessage {
                text: "steered: left instead".into()
            },
            end_turn()
        ]
    );
}

#[tokio::test]
async fn a_steer_after_the_turn_starts_one() {
    let mut harness = steering_harness().await;
    assert!(harness.handle.steer("hello"));
    assert_eq!(harness.next().await, SessionEvent::TurnRunning);
    assert_eq!(
        harness.until_turn_end().await,
        vec![
            SessionEvent::AgentMessage { text: "Hel".into() },
            SessionEvent::AgentMessage { text: "lo".into() },
            end_turn()
        ]
    );
}

#[tokio::test]
async fn a_rejected_steer_is_reported() {
    let mut harness = Harness::start().await;
    assert!(harness.handle.steer("hello"));
    assert!(matches!(
        harness.next().await,
        SessionEvent::SteerFailed { .. }
    ));
}
