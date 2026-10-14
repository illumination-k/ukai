//! The visible history of one agent session, folded from [`SessionEvent`]s.

use std::path::PathBuf;

use ukai_acp::{
    PermissionChoice, PermissionId, PlanItem, RestoreOutcome, SessionEvent, ToolStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Connecting,
    /// Ready for a prompt.
    Idle,
    /// A prompt turn is running.
    Busy,
    Closed {
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User {
        text: String,
    },
    Agent {
        text: String,
    },
    Thought {
        text: String,
    },
    Tool {
        id: String,
        title: String,
        kind: String,
        status: ToolStatus,
    },
    Plan {
        items: Vec<PlanItem>,
    },
    Permission {
        id: PermissionId,
        title: String,
        options: Vec<PermissionChoice>,
        /// Name of the chosen option, or `cancelled`; `None` while pending.
        answer: Option<String>,
    },
    FileWritten {
        path: PathBuf,
    },
    Notice {
        text: String,
        error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub agent: Option<String>,
    pub status: Status,
    pub entries: Vec<Entry>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            agent: None,
            status: Status::Connecting,
            entries: Vec::new(),
        }
    }
}

impl Transcript {
    /// Record a prompt the user sent.
    pub fn push_prompt(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::User { text: text.into() });
        self.status = Status::Busy;
    }

    /// Record the user's answer to a permission request.
    pub fn answer_permission(&mut self, id: PermissionId, answer: impl Into<String>) {
        if let Some(Entry::Permission { answer: slot, .. }) = self
            .entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry, Entry::Permission { id: pid, .. } if *pid == id))
        {
            *slot = Some(answer.into());
        }
    }

    /// Whether the agent is blocked on a permission request nobody answered.
    pub fn awaiting_permission(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, Entry::Permission { answer: None, .. }))
    }

    /// Record a cancel: every pending permission request is answered with it.
    pub fn cancel(&mut self) {
        for entry in &mut self.entries {
            if let Entry::Permission {
                answer: answer @ None,
                ..
            } = entry
            {
                *answer = Some("cancelled".to_owned());
            }
        }
    }

    pub fn apply(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Preparing { message } => self.notice(message, false),
            SessionEvent::TurnRunning => self.status = Status::Busy,
            SessionEvent::Ready { agent, restore, .. } => {
                self.agent = agent;
                self.status = Status::Idle;
                if let Some(text) = restore_notice(restore) {
                    self.notice(text, false);
                }
            }
            SessionEvent::UserMessage { text } => self.stream(text, |text| Entry::User { text }),
            SessionEvent::AgentMessage { text } => self.stream(text, |text| Entry::Agent { text }),
            SessionEvent::AgentThought { text } => {
                self.stream(text, |text| Entry::Thought { text });
            }
            SessionEvent::ToolCall {
                id,
                title,
                kind,
                status,
            } => self.tool_call(id, title, kind, status),
            SessionEvent::ToolCallUpdate { id, title, status } => {
                self.tool_call_update(id, title, status);
            }
            SessionEvent::Plan { entries: items } => self.plan(items),
            SessionEvent::PermissionRequested { id, title, options } => {
                self.entries.push(Entry::Permission {
                    id,
                    title,
                    options,
                    answer: None,
                });
            }
            SessionEvent::FileWritten { path } => self.entries.push(Entry::FileWritten { path }),
            SessionEvent::TurnEnded { stop_reason } => {
                if stop_reason != "end_turn" {
                    self.notice(format!("turn ended: {stop_reason}"), false);
                }
                self.status = Status::Idle;
            }
            SessionEvent::PromptFailed { message } => {
                self.notice(message, true);
                self.status = Status::Idle;
            }
            SessionEvent::Closed { error } => {
                if let Some(error) = &error {
                    self.notice(format!("disconnected: {error}"), true);
                }
                self.status = Status::Closed { error };
            }
        }
    }

    fn notice(&mut self, text: impl Into<String>, error: bool) {
        self.entries.push(Entry::Notice {
            text: text.into(),
            error,
        });
    }

    /// Append a streamed chunk to the last entry if it is of the same kind,
    /// or start a new entry with it.
    fn stream(&mut self, chunk: String, wrap: fn(String) -> Entry) {
        match (self.entries.last_mut(), wrap(chunk)) {
            (Some(Entry::User { text }), Entry::User { text: chunk })
            | (Some(Entry::Agent { text }), Entry::Agent { text: chunk })
            | (Some(Entry::Thought { text }), Entry::Thought { text: chunk }) => {
                text.push_str(&chunk);
            }
            (_, entry) => self.entries.push(entry),
        }
    }

    fn tool_call(&mut self, id: String, title: String, kind: String, status: ToolStatus) {
        match self.tool_mut(&id) {
            Some(Entry::Tool {
                title: t,
                kind: k,
                status: s,
                ..
            }) => {
                *t = title;
                *k = kind;
                *s = status;
            }
            _ => self.entries.push(Entry::Tool {
                id,
                title,
                kind,
                status,
            }),
        }
    }

    fn tool_call_update(&mut self, id: String, title: Option<String>, status: Option<ToolStatus>) {
        match self.tool_mut(&id) {
            Some(Entry::Tool {
                title: t,
                status: s,
                ..
            }) => {
                if let Some(title) = title {
                    *t = title;
                }
                if let Some(status) = status {
                    *s = status;
                }
            }
            // An update for a call we never saw: show it rather than drop it.
            _ => self.entries.push(Entry::Tool {
                title: title.unwrap_or_else(|| id.clone()),
                id,
                kind: "other".to_owned(),
                status: status.unwrap_or(ToolStatus::Pending),
            }),
        }
    }

    /// One plan per turn: later plans replace the earlier one in place.
    fn plan(&mut self, items: Vec<PlanItem>) {
        let turn_start = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry, Entry::User { .. }))
            .map_or(0, |index| index + 1);
        match self.entries[turn_start..]
            .iter_mut()
            .find(|entry| matches!(entry, Entry::Plan { .. }))
        {
            Some(Entry::Plan { items: current }) => *current = items,
            _ => self.entries.push(Entry::Plan { items }),
        }
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry, Entry::Tool { id: tid, .. } if tid == id))
    }
}

fn restore_notice(restore: RestoreOutcome) -> Option<&'static str> {
    match restore {
        RestoreOutcome::Fresh | RestoreOutcome::Replayed => None,
        RestoreOutcome::Resumed => Some("session resumed; earlier messages were not replayed"),
        RestoreOutcome::Unsupported => {
            Some("this agent cannot restore sessions; started a new one")
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn folded(events: impl IntoIterator<Item = SessionEvent>) -> Transcript {
        let mut transcript = Transcript::default();
        for event in events {
            transcript.apply(event);
        }
        transcript
    }

    fn ready(agent: Option<&str>, restore: RestoreOutcome) -> SessionEvent {
        SessionEvent::Ready {
            agent: agent.map(str::to_owned),
            session_id: "s".into(),
            restore,
        }
    }

    #[test]
    fn replayed_history_builds_turns() {
        let transcript = folded([
            SessionEvent::UserMessage { text: "q".into() },
            SessionEvent::UserMessage { text: "?".into() },
            msg("a"),
            ready(None, RestoreOutcome::Replayed),
        ]);
        assert_eq!(
            transcript.entries,
            vec![
                Entry::User { text: "q?".into() },
                Entry::Agent { text: "a".into() }
            ]
        );
    }

    #[rstest]
    #[case::resumed(RestoreOutcome::Resumed)]
    #[case::unsupported(RestoreOutcome::Unsupported)]
    fn lossy_restores_leave_a_notice(#[case] restore: RestoreOutcome) {
        let transcript = folded([ready(None, restore)]);
        assert!(matches!(
            transcript.entries.as_slice(),
            [Entry::Notice { error: false, .. }]
        ));
    }

    fn msg(text: &str) -> SessionEvent {
        SessionEvent::AgentMessage { text: text.into() }
    }

    fn tool(id: &str, status: ToolStatus) -> SessionEvent {
        SessionEvent::ToolCall {
            id: id.into(),
            title: format!("tool {id}"),
            kind: "read".into(),
            status,
        }
    }

    fn plan(contents: &[&str]) -> SessionEvent {
        SessionEvent::Plan {
            entries: contents
                .iter()
                .map(|content| PlanItem {
                    content: (*content).into(),
                    done: false,
                    active: false,
                })
                .collect(),
        }
    }

    #[test]
    fn ready_sets_agent_and_idle() {
        let transcript = folded([ready(Some("Claude"), RestoreOutcome::Fresh)]);
        assert_eq!(transcript.agent.as_deref(), Some("Claude"));
        assert_eq!(transcript.status, Status::Idle);
    }

    #[test]
    fn consecutive_chunks_merge_but_not_across_other_entries() {
        let transcript = folded([msg("a"), msg("b"), tool("1", ToolStatus::Pending), msg("c")]);
        assert_eq!(
            transcript.entries,
            vec![
                Entry::Agent { text: "ab".into() },
                Entry::Tool {
                    id: "1".into(),
                    title: "tool 1".into(),
                    kind: "read".into(),
                    status: ToolStatus::Pending,
                },
                Entry::Agent { text: "c".into() },
            ]
        );
    }

    #[rstest]
    #[case::known_call(
        vec![tool("1", ToolStatus::Pending)],
        Entry::Tool { id: "1".into(), title: "renamed".into(), kind: "read".into(), status: ToolStatus::Completed },
    )]
    #[case::unknown_call(
        vec![],
        Entry::Tool { id: "1".into(), title: "renamed".into(), kind: "other".into(), status: ToolStatus::Completed },
    )]
    fn tool_updates_patch_in_place(#[case] before: Vec<SessionEvent>, #[case] expected: Entry) {
        let mut transcript = folded(before);
        transcript.apply(SessionEvent::ToolCallUpdate {
            id: "1".into(),
            title: Some("renamed".into()),
            status: Some(ToolStatus::Completed),
        });
        assert_eq!(transcript.entries, vec![expected]);
    }

    #[test]
    fn plan_is_replaced_within_a_turn_and_new_per_turn() {
        let mut transcript = folded([plan(&["a"]), msg("x"), plan(&["a", "b"])]);
        assert_eq!(transcript.entries.len(), 2);
        assert!(matches!(&transcript.entries[0], Entry::Plan { items } if items.len() == 2));

        transcript.push_prompt("next");
        transcript.apply(plan(&["c"]));
        assert_eq!(transcript.entries.len(), 4);
    }

    #[test]
    fn prompt_and_turn_end_toggle_busy() {
        let mut transcript = folded([ready(None, RestoreOutcome::Fresh)]);
        transcript.push_prompt("hi");
        assert_eq!(transcript.status, Status::Busy);
        transcript.apply(SessionEvent::TurnEnded {
            stop_reason: "end_turn".into(),
        });
        assert_eq!(transcript.status, Status::Idle);
        assert_eq!(transcript.entries, vec![Entry::User { text: "hi".into() }]);
    }

    #[rstest]
    #[case::cancelled(
        SessionEvent::TurnEnded { stop_reason: "cancelled".into() },
        Entry::Notice { text: "turn ended: cancelled".into(), error: false },
        Status::Idle,
    )]
    #[case::failed(
        SessionEvent::PromptFailed { message: "auth required".into() },
        Entry::Notice { text: "auth required".into(), error: true },
        Status::Idle,
    )]
    #[case::closed(
        SessionEvent::Closed { error: Some("eof".into()) },
        Entry::Notice { text: "disconnected: eof".into(), error: true },
        Status::Closed { error: Some("eof".into()) },
    )]
    fn abnormal_endings_leave_a_notice(
        #[case] event: SessionEvent,
        #[case] notice: Entry,
        #[case] status: Status,
    ) {
        let mut transcript = Transcript::default();
        transcript.push_prompt("hi");
        transcript.apply(event);
        assert_eq!(transcript.entries.last(), Some(&notice));
        assert_eq!(transcript.status, status);
    }

    #[test]
    fn permissions_are_answered_individually_or_by_cancel() {
        let request = |id| SessionEvent::PermissionRequested {
            id,
            title: "t".into(),
            options: vec![],
        };
        let mut transcript = folded([request(0), request(1), request(2)]);
        assert!(transcript.awaiting_permission());
        transcript.answer_permission(1, "Allow");
        assert!(transcript.awaiting_permission());
        transcript.cancel();
        assert!(!transcript.awaiting_permission());
        let answers: Vec<_> = transcript
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Permission { answer, .. } => answer.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(
            answers,
            [Some("cancelled"), Some("Allow"), Some("cancelled")]
        );
    }
}
