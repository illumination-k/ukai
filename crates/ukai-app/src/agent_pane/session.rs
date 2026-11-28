//! Starting, restarting, and continuing a pane's session.

use std::rc::Rc;

use dioxus::prelude::*;
use ukai_acp::{
    Checkout, FsRoot, PaneWorkspace, PermissionId, RestartEndpoint, SessionEvent, SessionEvents,
    SessionHandle, SessionOptions, UpdateCheck,
};
use ukai_core::config::ApplyDefaults;
use ukai_core::permission::{self, Answer, Asked};
use ukai_core::{Alert, PaneContent, PaneId, ProjectId, Status, Transcript};

use super::{AgentSession, Composer, Lease, Sessions, branch};
use crate::alert;
use crate::projects::{AppSignals, ProjectContext, project_context};
use crate::voice::DictationOwner;

/// Start a session for the agent `pane` holds. With `resume`, continue the
/// ACP session recorded for the pane (if any) instead of starting a new one.
pub fn connect(app: AppSignals, pane: PaneId, resume: bool) {
    connect_expecting(app, pane, resume, None);
}

/// [`connect`] after restarting the agent to reach release `expecting`.
fn connect_expecting(app: AppSignals, pane: PaneId, resume: bool, expecting: Option<String>) {
    let AppSignals {
        projects,
        configs,
        mut layout,
        mut sessions,
        ..
    } = app;
    let Some(PaneContent::Agent {
        project,
        agent,
        session,
        workspace,
    }) = layout.peek().content(pane).cloned()
    else {
        return;
    };
    // A reconnect keeps the pane's composer and starts a transcript.
    let (lease, dictation) = sessions.peek().get(&pane).map_or_else(
        || {
            let lease = Lease::new();
            let owner = Rc::new(DictationOwner(lease.composer().dictation));
            (lease, owner)
        },
        |session| (session.lease.reconnected(), session.dictation.clone()),
    );
    let mut transcript = lease.transcript();
    let composer = lease.composer();
    let unread = lease.unread();
    let restore = if resume { session } else { None };
    let source = match projects.peek().get(&project) {
        Some(project) => format!("{agent} · {}", project.name()),
        None => agent.clone(),
    };
    let mut defaults = ApplyDefaults::new(projects.peek().defaults(&project, &agent));
    let context = project_context(&projects.peek(), &configs.peek(), &project);
    let checkout = context
        .as_ref()
        .ok()
        .and_then(|context| checkout_of(context, &agent, workspace.as_ref()));
    let started = context.and_then(|context| {
        start_session(&context, &agent, workspace.as_ref(), restore, expecting)
    });
    let handle = match started {
        Ok((handle, mut events)) => {
            let window = dioxus::desktop::window();
            // The events end once the pane's entry drops the handle, so
            // this holds none: only the lease, for the events still to come.
            lease.spawn(async move {
                while let Some(event) = events.recv().await {
                    // The app left this session itself, to restart its
                    // agent, and the transcript says so already.
                    if matches!(event, SessionEvent::Closed { error: None })
                        && left(sessions, pane, transcript)
                    {
                        break;
                    }
                    if let SessionEvent::Ready { session_id, .. } = &event {
                        layout.write().record_session(pane, session_id.clone());
                    }
                    if let Some((id, value)) = defaults.on_event(&event)
                        && let Some(handle) = handle_of(sessions, pane, transcript)
                    {
                        handle.set_config(id, value);
                    }
                    let ended = matches!(event, SessionEvent::TurnEnded { .. });
                    // A rule answers the request in the user's place, so
                    // it raises nothing.
                    let ruled = ruled(app, &project, &agent, &transcript.peek(), &event);
                    let raised = Alert::of(&event).filter(|_| ruled.is_none());
                    transcript.write().apply(event);
                    if let Some((id, answer)) = ruled
                        && let Some(handle) = handle_of(sessions, pane, transcript)
                        && handle.resolve_permission(id, Some(answer.option.id.clone()))
                    {
                        transcript.write().answer_permission_by(id, &answer);
                    }
                    if transcript.peek().queued.is_some() {
                        follow_queued(sessions, pane, transcript, composer);
                    }
                    if let Some(raised) = raised {
                        alert::raise(&window, layout, pane, unread, &source, &raised);
                    }
                    if ended {
                        branch::record(app, pane);
                    }
                }
            });
            Some(handle)
        }
        Err(error) => {
            transcript
                .write()
                .apply(SessionEvent::Closed { error: Some(error) });
            None
        }
    };
    // Replacing an entry drops the previous handle, which ends that session.
    sessions.write().insert(
        pane,
        AgentSession {
            handle,
            transcript,
            composer,
            checkout,
            unread,
            dictation,
            lease,
        },
    );
}

/// The answer the project's rules give to `event`, when it is a permission
/// request they cover: the configuration's `[[permissions]]`, then what the
/// user chose to always allow. Read anew for each request, so a rule saved
/// or forgotten a moment ago counts.
fn ruled(
    app: AppSignals,
    project: &ProjectId,
    agent: &str,
    transcript: &Transcript,
    event: &SessionEvent,
) -> Option<(PermissionId, Answer)> {
    let SessionEvent::PermissionRequested {
        id,
        tool_call,
        title,
        kind,
        options,
    } = event
    else {
        return None;
    };
    let configs = app.configs.peek();
    let rules = match configs.get(project) {
        Some(Ok(config)) => config.permissions.as_slice(),
        _ => &[],
    };
    let asked = Asked {
        agent,
        kind: kind.as_deref().or_else(|| transcript.tool_kind(tool_call)),
        title,
    };
    let projects = app.projects.peek();
    permission::decide(rules, projects.permissions(project), asked, options)
        .map(|answer| (*id, answer))
}

/// Whether `pane` still shows `transcript` but has dropped its handle.
fn left(sessions: Sessions, pane: PaneId, transcript: Signal<Transcript>) -> bool {
    sessions
        .peek()
        .get(&pane)
        .is_some_and(|session| session.transcript == transcript && session.handle.is_none())
}

/// The handle of the session showing in `transcript`, while it is still
/// `pane`'s: a reconnect replaces the pane's session, and closing ends it.
fn handle_of(
    sessions: Sessions,
    pane: PaneId,
    transcript: Signal<Transcript>,
) -> Option<SessionHandle> {
    sessions
        .peek()
        .get(&pane)
        .filter(|session| session.transcript == transcript)
        .and_then(|session| session.handle.clone())
}

/// Where `pane`'s agent works: what its session found, else resolved anew.
pub(super) fn checkout_for(app: AppSignals, pane: PaneId) -> Option<Checkout> {
    if let Some(session) = app.sessions.peek().get(&pane) {
        return session.checkout.clone();
    }
    let Some(PaneContent::Agent {
        project,
        agent,
        workspace,
        ..
    }) = app.layout.peek().content(pane).cloned()
    else {
        return None;
    };
    let context = project_context(&app.projects.peek(), &app.configs.peek(), &project).ok()?;
    checkout_of(&context, &agent, workspace.as_ref())
}

/// Where the pane's agent works: the project root, or its gateway workspace.
fn checkout_of(
    context: &ProjectContext,
    agent: &str,
    workspace: Option<&PaneWorkspace>,
) -> Option<Checkout> {
    let spec = context.config.agent(agent)?;
    Checkout::resolve(&spec, ukai_acp::system_lookup, workspace, &context.root)
        .inspect_err(|error| tracing::warn!(%error, "cannot reach the agent's checkout"))
        .ok()
        .flatten()
}

fn start_session(
    context: &ProjectContext,
    agent: &str,
    workspace: Option<&PaneWorkspace>,
    restore: Option<String>,
    expecting: Option<String>,
) -> Result<(SessionHandle, SessionEvents), String> {
    let spec = context
        .config
        .agent(agent)
        .ok_or_else(|| format!("agent `{agent}` is not declared for this project"))?;
    let transport =
        ukai_acp::resolve(&spec, ukai_acp::system_lookup, workspace).map_err(|e| e.to_string())?;
    let fs_root = FsRoot::new(&context.root).map_err(|e| e.to_string())?;
    Ok(ukai_acp::spawn_session(
        transport,
        SessionOptions {
            cwd: context.root.clone(),
            fs_root: Some(fs_root),
            restore,
            update_check: Some(match expecting {
                Some(latest) => UpdateCheck::npm().expecting(latest),
                None => UpdateCheck::npm(),
            }),
        },
    ))
}

/// Restart `pane`'s agent so that it comes back at release `latest`, then
/// start a new session on it: a gateway agent's workspace pod is replaced,
/// and a local agent's process is started again.
pub(super) fn restart_to_update(app: AppSignals, pane: PaneId, latest: String) {
    let Some(PaneContent::Agent {
        project,
        agent,
        workspace,
        ..
    }) = app.layout.peek().content(pane).cloned()
    else {
        return;
    };
    let endpoint =
        project_context(&app.projects.peek(), &app.configs.peek(), &project).and_then(|context| {
            let spec = context
                .config
                .agent(&agent)
                .ok_or_else(|| format!("agent `{agent}` is not declared for this project"))?;
            RestartEndpoint::resolve(&spec, ukai_acp::system_lookup, workspace.as_ref())
                .map_err(|e| e.to_string())
        });
    let lease = app.sessions.peek().get(&pane).map(|s| s.lease.clone());
    let transcript = lease.as_ref().map(Lease::transcript);
    let report = move |event: SessionEvent| {
        if let Some(mut transcript) = transcript {
            transcript.write().apply(event);
        }
    };
    match endpoint {
        Ok(None) => connect_expecting(app, pane, false, Some(latest)),
        Ok(Some(endpoint)) => {
            if let Some(mut transcript) = transcript {
                transcript.write().skip_update();
            }
            // Leave the pod before it goes, rather than be cut off by it.
            let mut sessions = app.sessions;
            if let Some(session) = sessions.write().get_mut(&pane) {
                session.handle = None;
            }
            report(SessionEvent::Preparing {
                message: "Restarting the agent's workspace".to_owned(),
            });
            let restart = async move {
                match endpoint.restart().await {
                    Ok(()) => connect_expecting(app, pane, false, Some(latest)),
                    Err(error) => report(SessionEvent::Closed {
                        error: Some(format!("could not restart the agent: {error}")),
                    }),
                }
            };
            match lease {
                Some(lease) => lease.spawn(restart),
                None => {
                    dioxus::core::spawn_forever(restart);
                }
            }
        }
        Err(error) => report(SessionEvent::Closed {
            error: Some(format!("could not restart the agent: {error}")),
        }),
    }
}

/// Send the message queued behind a turn that has stopped, or hand it back to
/// the composer when the session cannot take it.
fn follow_queued(
    sessions: Sessions,
    pane: PaneId,
    mut transcript: Signal<Transcript>,
    composer: Composer,
) {
    let Some(steer) = transcript.write().take_queued() else {
        return;
    };
    // A reconnect replaces the pane's session; this transcript's handle is gone then.
    let handle = handle_of(sessions, pane, transcript);
    let idle = transcript.peek().status == Status::Idle;
    if idle && handle.is_some_and(|handle| handle.prompt(steer.prompt())) {
        let sent = steer.shown();
        transcript.write().push_prompt(steer.text, sent);
    } else {
        composer.restore(steer);
    }
}
