use std::collections::BTreeMap;
use std::time::Duration;

use rstest::rstest;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use ukai_config::WorkspaceScope;

use super::*;

/// How long a test waits on a shell.
const WAIT: Duration = Duration::from_secs(20);

/// Read events until one satisfies `done`, collecting the output on the way.
async fn read_until(
    events: &mut mpsc::UnboundedReceiver<TerminalEvent>,
    output: &mut Vec<u8>,
    mut done: impl FnMut(&TerminalEvent, &[u8]) -> bool,
) -> TerminalEvent {
    let reading = async {
        loop {
            let Some(event) = events.recv().await else {
                panic!("the terminal ended without saying so");
            };
            if let TerminalEvent::Output(bytes) = &event {
                output.extend_from_slice(bytes);
            }
            if done(&event, output) {
                return event;
            }
        }
    };
    tokio::time::timeout(WAIT, reading)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out; output so far: {}",
                String::from_utf8_lossy(output)
            )
        })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[rstest]
#[case::kept(Size { cols: 120, rows: 40 }, Size { cols: 120, rows: 40 })]
#[case::empty(Size { cols: 0, rows: 0 }, Size { cols: 1, rows: 1 })]
#[case::huge(Size { cols: u16::MAX, rows: u16::MAX }, Size { cols: 1000, rows: 500 })]
fn sizes_are_clamped(#[case] asked: Size, #[case] kept: Size) {
    assert_eq!(asked.clamped(), kept);
}

#[rstest]
#[case::resize(Control::Resize(Size { cols: 100, rows: 30 }), r#"{"resize":{"cols":100,"rows":30}}"#)]
#[case::exit(Control::Exit { code: Some(3) }, r#"{"exit":{"code":3}}"#)]
#[case::killed(Control::Exit { code: None }, r#"{"exit":{"code":null}}"#)]
fn control_messages_round_trip(#[case] control: Control, #[case] json: &str) {
    assert_eq!(serde_json::to_string(&control).ok().as_deref(), Some(json));
    assert_eq!(serde_json::from_str::<Control>(json).ok(), Some(control));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_local_shell_runs_in_the_root_and_exits() {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    std::fs::write(dir.path().join("marker-file"), b"").unwrap_or_else(|e| panic!("write: {e}"));
    let (terminal, mut events) = open(TerminalPlace::Local(dir.path().to_owned()), Size::default());
    // The shell's own startup may print anything; what matters is the
    // command's answer, which the echoed command line cannot contain.
    terminal.write(b"ls; echo \"sum-$((40 + 2))\"\n".to_vec());
    let mut output = Vec::new();
    read_until(&mut events, &mut output, |_, output| {
        contains(output, b"sum-42")
    })
    .await;
    assert!(
        contains(&output, b"marker-file"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    terminal.resize(Size {
        cols: 100,
        rows: 30,
    });
    terminal.write(b"exit 3\n".to_vec());
    let end = read_until(&mut events, &mut output, |event, _| {
        !matches!(event, TerminalEvent::Output(_))
    })
    .await;
    assert_eq!(end, TerminalEvent::Exited(Some(3)));
}

fn endpoint(url: &str) -> TerminalEndpoint {
    let spec = AgentSpec::Gateway {
        url: url.to_owned(),
        headers: BTreeMap::from([("Authorization".to_owned(), "Bearer t".to_owned())]),
        workspace: WorkspaceScope::Shared,
    };
    TerminalEndpoint::resolve(&spec, |_, _| None, None)
        .unwrap_or_else(|e| panic!("resolve: {e}"))
        .unwrap_or_else(|| panic!("a gateway agent has a terminal"))
}

#[test]
fn derives_the_terminal_url() {
    let endpoint = endpoint("wss://gw.example.com/agents/a/acp");
    assert_eq!(
        endpoint.url(Size { cols: 90, rows: 20 }),
        "https://gw.example.com/agents/a/terminal?cols=90&rows=20"
    );
}

#[test]
fn a_local_agent_opens_its_terminal_here() {
    let spec = AgentSpec::Stdio {
        command: "agent".into(),
        args: Vec::new(),
        env: BTreeMap::new(),
    };
    let place = TerminalPlace::resolve(&spec, |_, _| None, None, Path::new("/work"));
    assert!(matches!(place, Ok(Some(TerminalPlace::Local(root))) if root == Path::new("/work")));
}

/// A gateway that upgrades `/agents/a/terminal?cols=80&rows=24` when the
/// agent's header is there, or answers `status` instead. It echoes binary
/// frames, answers a resize with its size as text output, and on `exit`
/// says the shell exited with 7.
// The handshake callback's error type is tungstenite's, not ours to shrink.
#[allow(clippy::result_large_err)]
async fn fake_gateway(status: Option<u16>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let port = listener.local_addr().map(|a| a.port()).unwrap_or_default();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let check = |request: &Request, response: Response| {
                    let authorized = request
                        .headers()
                        .get("authorization")
                        .is_some_and(|value| value == "Bearer t");
                    let uri_ok = request.uri() == "/agents/a/terminal?cols=80&rows=24";
                    match status {
                        None if authorized && uri_ok => Ok(response),
                        _ => {
                            let mut refused =
                                tokio_tungstenite::tungstenite::http::Response::new(None);
                            *refused.status_mut() =
                                tokio_tungstenite::tungstenite::http::StatusCode::from_u16(
                                    status.unwrap_or(403),
                                )
                                .unwrap_or_default();
                            Err(refused)
                        }
                    }
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(socket, check).await else {
                    return;
                };
                while let Some(Ok(message)) = ws.next().await {
                    let reply = match message {
                        Message::Binary(bytes) if bytes.as_ref() == b"exit" => {
                            let exit = serde_json::to_string(&Control::Exit { code: Some(7) })
                                .unwrap_or_default();
                            let _ = ws.send(Message::text(exit)).await;
                            let _ = ws.close(None).await;
                            break;
                        }
                        Message::Binary(bytes) => Message::Binary(bytes),
                        Message::Text(text) => match serde_json::from_str(&text) {
                            Ok(Control::Resize(size)) => {
                                Message::binary(format!("{}x{}", size.cols, size.rows))
                            }
                            _ => continue,
                        },
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    if ws.send(reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn a_workspace_terminal_carries_bytes_and_says_how_it_ended() {
    let gateway = fake_gateway(None).await;
    let place = TerminalPlace::Gateway(endpoint(&format!("ws://127.0.0.1:{gateway}/agents/a/acp")));
    let (terminal, mut events) = open(place, Size::default());
    terminal.write(b"hello".to_vec());
    terminal.resize(Size { cols: 0, rows: 9 });
    let mut output = Vec::new();
    read_until(&mut events, &mut output, |_, output| output == b"hello1x9").await;
    terminal.write(b"exit".to_vec());
    let end = read_until(&mut events, &mut output, |event, _| {
        !matches!(event, TerminalEvent::Output(_))
    })
    .await;
    assert_eq!(end, TerminalEvent::Exited(Some(7)));
}

#[rstest]
#[case::gone(410, "not running")]
#[case::not_found(404, "not running")]
#[case::refused(403, "answered 403")]
#[tokio::test]
async fn a_workspace_terminal_says_why_it_cannot_open(#[case] status: u16, #[case] why: &str) {
    let gateway = fake_gateway(Some(status)).await;
    let place = TerminalPlace::Gateway(endpoint(&format!("ws://127.0.0.1:{gateway}/agents/a/acp")));
    let (_terminal, mut events) = open(place, Size::default());
    let mut output = Vec::new();
    let end = read_until(&mut events, &mut output, |_, _| true).await;
    assert!(
        matches!(&end, TerminalEvent::Failed(message) if message.contains(why)),
        "{end:?}"
    );
}
