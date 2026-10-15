//! The relay's state machine: one agent connection shared by any number of
//! client connections that come and go, over plain JSON-RPC values.
//!
//! ACP's remote transport expects a session to outlive the connection that
//! made it: a client reconnects with a new `initialize` and `session/load`
//! (RFD "Streamable HTTP & WebSocket Transport"). A stdio agent cannot do that
//! by itself, so the hub does it in front of one:
//!
//! - It initializes the agent once and answers every client's `initialize`
//!   from that response, advertising `loadSession`.
//! - It records each session's `session/update` notifications, plus the user
//!   turns it forwards, and answers `session/load` for a known session by
//!   replaying them, so the history includes what happened while no client
//!   was connected.
//! - A prompt turn keeps running without a client. Whoever is attached when
//!   it ends hears `_ukai/turn_state` (shaped after ACP v2's `state_update`);
//!   a client attaching mid-turn hears `running` first.
//! - Agent requests, such as permission prompts, wait for a client and are
//!   sent again to each client that attaches until one answers.
//! - A client's `cwd` names a directory on its own machine, not the agent's,
//!   so session setup is pointed at the relay's working directory instead.
//!
//! Every input returns the messages to send; nothing here does I/O.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

/// A client connection, numbered by the caller.
pub type ConnId = u64;

/// Notification the relay sends about a session's prompt turn.
pub const TURN_STATE: &str = "_ukai/turn_state";

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Agent(Value),
    Client(ConnId, Value),
    /// A session's prompt turn finished, whoever was connected: a point to
    /// snapshot the work.
    TurnEnded(String),
}

/// What a request the relay sent to the agent was for.
#[derive(Debug)]
enum Origin {
    Initialize,
    Client {
        conn: ConnId,
        id: Value,
        method: String,
        session: Option<String>,
    },
}

#[derive(Debug, Default)]
struct Session {
    /// The `session/new` (or first `session/load`) result, returned again to
    /// later `session/load`s.
    setup: Value,
    history: Vec<Value>,
    attached: Option<ConnId>,
    /// The relay's id for the running `session/prompt`.
    turn: Option<u64>,
    /// Agent requests not answered yet, by the agent's id (as JSON text).
    pending: BTreeMap<String, Pending>,
    /// Set until the agent answers a forwarded `session/load`: the session is
    /// not known to the relay yet, and the replay is being recorded.
    loading: bool,
}

impl Session {
    /// Keep the settings a later `session/load` answers with current.
    fn set_config(&mut self, options: Value) {
        if let Some(setup) = self.setup.as_object_mut() {
            setup.insert("configOptions".to_owned(), options);
        }
    }
}

#[derive(Debug)]
struct Pending {
    request: Value,
    /// The id the request carries towards the client it was last sent to.
    sent: Option<(ConnId, u64)>,
}

#[derive(Debug, Default)]
pub struct Hub {
    /// The directory sessions are set up in, whatever `cwd` clients send.
    cwd: Option<String>,
    agent_init: Option<Value>,
    waiting_init: Vec<(ConnId, Value)>,
    next_id: u64,
    to_agent: BTreeMap<u64, Origin>,
    /// Client-facing id of a forwarded agent request -> (session, agent id).
    to_client: BTreeMap<u64, (String, String)>,
    sessions: BTreeMap<String, Session>,
    conns: BTreeSet<ConnId>,
}

impl Hub {
    /// A hub and the `initialize` it sends the agent. The relay offers the
    /// agent no client capabilities: `fs/*` and terminals would have to reach
    /// a client that may not be there, and an agent's edits belong in the
    /// checkout it works in. What it writes there is read back over HTTP
    /// instead (see [`crate::reports`]).
    pub fn new(cwd: Option<String>) -> (Self, Vec<Output>) {
        let mut hub = Self {
            cwd,
            ..Self::default()
        };
        let id = hub.id(Origin::Initialize);
        let initialize = request(
            id,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                },
                "clientInfo": { "name": "ukai-relay", "version": env!("CARGO_PKG_VERSION") }
            }),
        );
        (hub, vec![Output::Agent(initialize)])
    }

    /// Whether the agent still has something to do or someone to serve: a
    /// connected client or a running turn.
    pub fn busy(&self) -> bool {
        !self.conns.is_empty() || self.sessions.values().any(|s| s.turn.is_some())
    }

    pub fn connect(&mut self, conn: ConnId) {
        self.conns.insert(conn);
    }

    pub fn disconnect(&mut self, conn: ConnId) {
        self.conns.remove(&conn);
        self.waiting_init.retain(|(c, _)| *c != conn);
        for session in self.sessions.values_mut() {
            if session.attached == Some(conn) {
                session.attached = None;
            }
            for pending in session.pending.values_mut() {
                if pending.sent.is_some_and(|(c, _)| c == conn) {
                    pending.sent = None;
                }
            }
        }
        self.to_client.retain(|_, (session, agent_id)| {
            self.sessions
                .get(session)
                .is_some_and(|s| s.pending.get(agent_id).is_some_and(|p| p.sent.is_some()))
        });
    }

    pub fn from_client(&mut self, conn: ConnId, message: Value) -> Vec<Output> {
        match Kind::of(&message) {
            Kind::Request { id, method } => self.client_request(conn, id, &method, message),
            Kind::Notification { .. } => vec![Output::Agent(message)],
            Kind::Response { id } => self.client_response(&id, message),
            Kind::Invalid => Vec::new(),
        }
    }

    pub fn from_agent(&mut self, message: Value) -> Vec<Output> {
        match Kind::of(&message) {
            Kind::Request { id, method } => self.agent_request(&id, &method, message),
            Kind::Notification { method } => self.agent_notification(&method, message),
            Kind::Response { id } => self.agent_response(&id, message),
            Kind::Invalid => Vec::new(),
        }
    }

    fn id(&mut self, origin: Origin) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.to_agent.insert(id, origin);
        id
    }

    fn client_request(
        &mut self,
        conn: ConnId,
        id: Value,
        method: &str,
        mut message: Value,
    ) -> Vec<Output> {
        let session = session_id(&message);
        if let (Some(cwd), "session/new" | "session/load" | "session/resume") = (&self.cwd, method)
            && let Some(params) = message.get_mut("params").and_then(Value::as_object_mut)
        {
            params.insert("cwd".to_owned(), json!(cwd));
        }
        match method {
            "initialize" => match &self.agent_init {
                Some(init) => vec![Output::Client(conn, result(id, init.clone()))],
                None => {
                    self.waiting_init.push((conn, id));
                    Vec::new()
                }
            },
            "session/load" | "session/resume" => {
                let Some(session) = session else {
                    return vec![Output::Client(conn, error(id, -32602, "missing sessionId"))];
                };
                if self.sessions.get(&session).is_some_and(|s| !s.loading) {
                    return self.attach(conn, id, &session, method == "session/load");
                }
                // Unknown to the relay (it restarted): let the agent restore it
                // and record its replay as the history.
                let entry = self.sessions.entry(session.clone()).or_default();
                entry.loading = true;
                entry.attached = Some(conn);
                self.forward(conn, id, method, Some(session), message)
            }
            "session/prompt" => {
                if let Some(state) = session.as_ref().and_then(|s| self.sessions.get_mut(s)) {
                    // The agent does not echo the user's turn, but a replay should show it.
                    let blocks = message
                        .pointer("/params/prompt")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let sid = session.clone().unwrap_or_default();
                    state.history.extend(blocks.into_iter().map(|content| {
                        notification(
                            "session/update",
                            json!({
                                "sessionId": sid,
                                "update": { "sessionUpdate": "user_message_chunk", "content": content }
                            }),
                        )
                    }));
                }
                let outputs = self.forward(conn, id, method, session.clone(), message.take());
                if let (Some(session), Some(Output::Agent(sent))) = (session, outputs.first()) {
                    let turn = sent.get("id").and_then(Value::as_u64);
                    if let Some(state) = self.sessions.get_mut(&session) {
                        state.turn = turn;
                    }
                }
                outputs
            }
            _ => self.forward(conn, id, method, session, message),
        }
    }

    fn forward(
        &mut self,
        conn: ConnId,
        id: Value,
        method: &str,
        session: Option<String>,
        mut message: Value,
    ) -> Vec<Output> {
        let ours = self.id(Origin::Client {
            conn,
            id,
            method: method.to_owned(),
            session,
        });
        message["id"] = json!(ours);
        vec![Output::Agent(message)]
    }

    /// Attach `conn` to a session the relay knows and bring it up to date.
    fn attach(&mut self, conn: ConnId, id: Value, session: &str, replay: bool) -> Vec<Output> {
        let mut outputs = Vec::new();
        let Some(state) = self.sessions.get_mut(session) else {
            return outputs;
        };
        state.attached = Some(conn);
        if replay {
            outputs.extend(
                state
                    .history
                    .iter()
                    .map(|update| Output::Client(conn, update.clone())),
            );
        }
        let mut setup = state.setup.clone();
        if let Some(fields) = setup.as_object_mut() {
            fields.remove("sessionId");
        }
        outputs.push(Output::Client(conn, result(id, setup)));
        if state.turn.is_some() {
            outputs.push(Output::Client(conn, turn_state(session, "running", None)));
        }
        let resend: Vec<String> = state.pending.keys().cloned().collect();
        for agent_id in resend {
            outputs.extend(self.send_pending(session, &agent_id, conn));
        }
        outputs
    }

    fn send_pending(&mut self, session: &str, agent_id: &str, conn: ConnId) -> Vec<Output> {
        let ours = self.next_id;
        self.next_id += 1;
        let Some(pending) = self
            .sessions
            .get_mut(session)
            .and_then(|s| s.pending.get_mut(agent_id))
        else {
            return Vec::new();
        };
        if let Some((_, old)) = pending.sent.replace((conn, ours)) {
            self.to_client.remove(&old);
        }
        let mut request = pending.request.clone();
        request["id"] = json!(ours);
        self.to_client
            .insert(ours, (session.to_owned(), agent_id.to_owned()));
        vec![Output::Client(conn, request)]
    }

    fn client_response(&mut self, id: &Value, mut message: Value) -> Vec<Output> {
        let Some(ours) = id.as_u64() else {
            return Vec::new();
        };
        let Some((session, agent_id)) = self.to_client.remove(&ours) else {
            return Vec::new();
        };
        let Some(pending) = self
            .sessions
            .get_mut(&session)
            .and_then(|s| s.pending.remove(&agent_id))
        else {
            return Vec::new();
        };
        message["id"] = pending.request["id"].clone();
        vec![Output::Agent(message)]
    }

    fn agent_request(&mut self, id: &Value, _method: &str, message: Value) -> Vec<Output> {
        let agent_id = id.to_string();
        let Some(session) = session_id(&message).filter(|s| self.sessions.contains_key(s)) else {
            // Nothing the relay offered asks for a request outside a session.
            return vec![Output::Agent(error(id.clone(), -32601, "method not found"))];
        };
        let attached = self.sessions.get_mut(&session).and_then(|state| {
            state.pending.insert(
                agent_id.clone(),
                Pending {
                    request: message,
                    sent: None,
                },
            );
            state.attached
        });
        match attached {
            Some(conn) => self.send_pending(&session, &agent_id, conn),
            None => Vec::new(),
        }
    }

    fn agent_notification(&mut self, method: &str, message: Value) -> Vec<Output> {
        let session = session_id(&message);
        let state = session.as_ref().and_then(|s| self.sessions.get_mut(s));
        match state {
            Some(state) => {
                if method == "session/update" {
                    if let Some(options) = message
                        .pointer("/params/update")
                        .filter(|update| {
                            update.get("sessionUpdate") == Some(&json!("config_option_update"))
                        })
                        .and_then(|update| update.get("configOptions"))
                    {
                        state.set_config(options.clone());
                    }
                    state.history.push(message.clone());
                }
                state
                    .attached
                    .map(|conn| Output::Client(conn, message))
                    .into_iter()
                    .collect()
            }
            // Not about a known session: everyone connected may care.
            None => self
                .conns
                .iter()
                .map(|conn| Output::Client(*conn, message.clone()))
                .collect(),
        }
    }

    fn agent_response(&mut self, id: &Value, mut message: Value) -> Vec<Output> {
        let Some(origin) = id.as_u64().and_then(|ours| self.to_agent.remove(&ours)) else {
            return Vec::new();
        };
        let Origin::Client {
            conn,
            id: client_id,
            method,
            session,
        } = origin
        else {
            return self.agent_initialized(&message);
        };
        message["id"] = client_id;
        let mut outputs = Vec::new();
        match (method.as_str(), session) {
            ("session/new", _) => self.session_created(conn, &message),
            ("session/load" | "session/resume", Some(sid)) => self.session_restored(&sid, &message),
            ("session/set_config_option", Some(sid)) => {
                if let (Some(state), Some(options)) = (
                    self.sessions.get_mut(&sid),
                    message.pointer("/result/configOptions"),
                ) {
                    state.set_config(options.clone());
                }
            }
            ("session/prompt", Some(sid)) => {
                outputs.push(Output::TurnEnded(sid.clone()));
                if let Some(attached) = self.end_turn(&sid, conn) {
                    // The client that asked is gone; tell whoever is here.
                    let stop = message.pointer("/result/stopReason").cloned();
                    outputs.extend(
                        attached.map(|other| Output::Client(other, turn_state(&sid, "idle", stop))),
                    );
                    return outputs;
                }
            }
            _ => {}
        }
        if self.conns.contains(&conn) {
            outputs.push(Output::Client(conn, message));
        }
        outputs
    }

    /// Remember the agent's `initialize` result and answer the clients that
    /// were waiting for it.
    fn agent_initialized(&mut self, message: &Value) -> Vec<Output> {
        let mut init = message.get("result").cloned().unwrap_or(Value::Null);
        // The relay can replay any session it holds.
        if let Some(caps) = init
            .get_mut("agentCapabilities")
            .and_then(Value::as_object_mut)
        {
            caps.insert("loadSession".to_owned(), json!(true));
        }
        self.agent_init = Some(init.clone());
        std::mem::take(&mut self.waiting_init)
            .into_iter()
            .map(|(conn, id)| Output::Client(conn, result(id, init.clone())))
            .collect()
    }

    fn session_created(&mut self, conn: ConnId, response: &Value) {
        let Some(setup) = response.get("result") else {
            return;
        };
        if let Some(sid) = setup.get("sessionId").and_then(Value::as_str) {
            let session = Session {
                setup: setup.clone(),
                attached: self.conns.contains(&conn).then_some(conn),
                ..Session::default()
            };
            self.sessions.insert(sid.to_owned(), session);
        }
    }

    /// The agent answered a `session/load` or `session/resume` the relay
    /// forwarded: keep the session if it succeeded, forget it otherwise.
    fn session_restored(&mut self, sid: &str, response: &Value) {
        match response.get("result") {
            Some(setup) => {
                if let Some(state) = self.sessions.get_mut(sid) {
                    state.loading = false;
                    state.setup = setup.clone();
                }
            }
            None => {
                self.sessions.remove(sid);
            }
        }
    }

    /// Mark `sid`'s turn finished. `None` when `requester` is still the one
    /// attached (or the session is unknown), so the response goes to it;
    /// otherwise the client attached now, if any.
    fn end_turn(&mut self, sid: &str, requester: ConnId) -> Option<Option<ConnId>> {
        let state = self.sessions.get_mut(sid)?;
        state.turn = None;
        (state.attached != Some(requester)).then_some(state.attached)
    }
}

enum Kind {
    Request { id: Value, method: String },
    Notification { method: String },
    Response { id: Value },
    Invalid,
}

impl Kind {
    fn of(message: &Value) -> Self {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        match (method, id) {
            (Some(method), Some(id)) => Self::Request {
                id,
                method: method.to_owned(),
            },
            (Some(method), None) => Self::Notification {
                method: method.to_owned(),
            },
            (None, Some(id)) => Self::Response { id },
            (None, None) => Self::Invalid,
        }
    }
}

fn session_id(message: &Value) -> Option<String> {
    message
        .pointer("/params/sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn turn_state(session: &str, state: &str, stop_reason: Option<Value>) -> Value {
    let mut params = Map::new();
    params.insert("sessionId".to_owned(), json!(session));
    params.insert("state".to_owned(), json!(state));
    if let Some(stop) = stop_reason {
        params.insert("stopReason".to_owned(), stop);
    }
    notification(TURN_STATE, Value::Object(params))
}

#[cfg(test)]
mod tests;
