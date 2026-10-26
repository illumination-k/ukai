//! Starting, restarting, and continuing a pane's session.

use std::rc::Rc;

use dioxus::core::spawn_forever;
use dioxus::prelude::*;
use ukai_acp::{
    Checkout, FsRoot, PaneWorkspace, RestartEndpoint, SessionEvent, SessionEvents, SessionHandle,
    SessionOptions, UpdateCheck,
};
use ukai_core::config::ApplyDefaults;
use ukai_core::{Alert, PaneContent, PaneId, Status, Transcript};

use super::{AgentSession, Composer, Sessions};
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
    let mut transcript = Signal::new_in_scope(Transcript::default(), ScopeId::APP);
    let (composer, unread, dictation) = sessions.peek().get(&pane).map_or_else(
        || {
            let composer = Composer::new();
            let owner = Rc::new(DictationOwner(composer.dictation));
            (composer, Signal::new_in_scope(false, ScopeId::APP), owner)
        },
        |session| (session.composer, session.unread, session.dictation.clone()),
    );
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
            let control = handle.clone();
            spawn_forever(async move {
                while let Some(event) = events.recv().await {
                    if let SessionEvent::Ready { session_id, .. } = &event {
                        layout.write().record_session(pane, session_id.clone());
                    }
                    if let Some((id, value)) = defaults.on_event(&event) {
                        control.set_config(id, value);
                    }
                    let raised = Alert::of(&event);
                    transcript.write().apply(event);
                    if transcript.peek().queued.is_some() {
                        follow_queued(sessions, pane, transcript, composer);
                    }
                    if let Some(raised) = raised {
                        alert::raise(&window, layout, pane, unread, &source, &raised);
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
        },
    );
}

/// Continue the agent's session `id` in `pane`, replacing the one there.
pub(super) fn open_session(app: AppSignals, pane: PaneId, id: String) {
    let mut layout = app.layout;
    layout.write().record_session(pane, id);
    connect(app, pane, true);
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
    let transcript = app.sessions.peek().get(&pane).map(|s| s.transcript);
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
            spawn_forever(async move {
                match endpoint.restart().await {
                    Ok(()) => connect_expecting(app, pane, false, Some(latest)),
                    Err(error) => report(SessionEvent::Closed {
                        error: Some(format!("could not restart the agent: {error}")),
                    }),
                }
            });
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
    let handle = sessions
        .peek()
        .get(&pane)
        .filter(|session| session.transcript == transcript)
        .and_then(|session| session.handle.clone());
    let idle = transcript.peek().status == Status::Idle;
    if idle && handle.is_some_and(|handle| handle.prompt(steer.prompt())) {
        let names = steer.names();
        transcript.write().push_prompt(steer.text, names);
    } else {
        composer.restore(steer);
    }
}
