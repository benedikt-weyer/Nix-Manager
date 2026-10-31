//! Frontend-agnostic client for talking to an AI coding agent about the NixOS
//! configuration, over the [Agent Client Protocol](https://agentclientprotocol.com).
//!
//! The agent is a program of the user's choosing that speaks ACP on its
//! standard input and output (for example `claude-code-acp`). [`start`] runs
//! it in the configuration directory and gives back a [`Handle`] to send
//! prompts with and a stream of [`Event`]s to show.
//!
//! This side offers the agent no file system or terminal of its own: the agent
//! reads and edits the configuration with its own tools, and asks through
//! [`Event::Permission`] before it does anything that needs allowing. Nothing
//! is ever allowed on the user's behalf.

mod model;

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::v1 as acp;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectTo, ConnectionTo, Responder};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::future::{select, Either};
use futures::{future, FutureExt, Stream, StreamExt};

pub use model::{
    unified_diff, Event, Mode, OptionKind, Permission, PermissionOption, PlanEntry, PlanStatus, StopReason, ToolCall,
    ToolContent, ToolKind, ToolStatus,
};

/// Told to the agent with the first message of a conversation, so that it
/// knows what it is working on and what is left to the user.
fn briefing(config_dir: &str, host: &str) -> String {
    let host = if host.is_empty() { String::new() } else { format!(" for the host '{host}'") };
    format!(
        "You are helping with a NixOS configuration: a flake in {config_dir}{host}. \
         Explain it when asked. When asked to change it, edit the files in that directory, keep the \
         changes small and in the style of what is there, and say what you changed. \
         Do not run nixos-rebuild or `nix flake update`, and do not commit: the user rebuilds and \
         commits from Nix Manager, where they review your changes first. A flake only sees files \
         that git tracks, so `git add` any file you create."
    )
}

pub struct Config {
    /// The command that starts the agent, e.g. `claude-code-acp` or
    /// `gemini --experimental-acp`.
    pub command: String,
    /// The configuration directory; the agent works in it.
    pub cwd: PathBuf,
    /// The host the configuration is for, told to the agent. May be empty.
    pub host: String,
}

enum Command {
    Prompt(String),
    Cancel,
    Answer { id: u64, option: Option<String> },
    SetMode(String),
}

/// Sends things to the agent. Cheap to clone. Dropping every handle ends the
/// session and stops the agent.
#[derive(Clone, Debug)]
pub struct Handle {
    commands: UnboundedSender<Command>,
}

impl Handle {
    fn send(&self, command: Command) {
        // The session is over if this fails, which the event stream reports.
        let _ = self.commands.unbounded_send(command);
    }

    /// Asks something, or asks for a change. One at a time: wait for
    /// [`Event::TurnEnded`] or [`Event::TurnFailed`] before the next.
    pub fn prompt(&self, text: impl Into<String>) {
        self.send(Command::Prompt(text.into()));
    }

    /// Stops what the agent is doing. Whatever it asked permission for is refused.
    pub fn cancel(&self) {
        self.send(Command::Cancel);
    }

    /// Answers an [`Event::Permission`] with one of its options; `None` refuses
    /// without choosing.
    pub fn answer(&self, permission: u64, option: Option<String>) {
        self.send(Command::Answer { id: permission, option });
    }

    pub fn set_mode(&self, mode: impl Into<String>) {
        self.send(Command::SetMode(mode.into()));
    }
}

pub type Events = Pin<Box<dyn Stream<Item = Event> + Send>>;

/// Starts the agent. The agent runs for as long as the returned stream is polled
/// and the [`Handle`] (or a clone) is alive; the stream ends with
/// [`Event::Closed`].
pub fn start(config: Config) -> (Handle, Events) {
    let (commands_tx, commands) = unbounded();
    let (events_tx, events) = unbounded();

    let command = config.command.clone();
    let driver = async move {
        match AcpAgent::from_str(&config.command) {
            Ok(agent) => run(agent, config.cwd, config.host, commands, events_tx).await,
            Err(error) => {
                let _ = events_tx.unbounded_send(Event::Closed(Some(format!(
                    "'{}' is not a command that can be run: {error}",
                    config.command
                ))));
            }
        }
    };
    // The driver does its work while the stream is polled and produces nothing itself.
    let stream = futures::stream::select(events, driver.into_stream().filter_map(|()| future::ready(None)));
    let stream = stream.map(move |event| match event {
        Event::Closed(Some(reason)) => Event::Closed(Some(explain(&command, &reason))),
        other => other,
    });
    (Handle { commands: commands_tx }, Box::pin(stream))
}

/// Says what went wrong in the user's terms, where the error alone doesn't.
fn explain(command: &str, reason: &str) -> String {
    let program = command.split_whitespace().next().unwrap_or(command);
    if reason.contains("No such file or directory") || reason.contains("not found") {
        format!("'{program}' was not found. Is it installed, and on your PATH?")
    } else if reason.contains("Permission denied") {
        format!("'{program}' could not be run: permission denied.")
    } else {
        reason.to_owned()
    }
}

/// What the connection's callbacks and its main task share.
struct Shared {
    events: UnboundedSender<Event>,
    tools: Mutex<HashMap<String, ToolCall>>,
    pending: Mutex<HashMap<u64, Responder<acp::RequestPermissionResponse>>>,
    next_permission: AtomicU64,
}

impl Shared {
    fn send(&self, event: Event) {
        let _ = self.events.unbounded_send(event);
    }

    fn update(&self, update: acp::SessionUpdate) {
        use acp::SessionUpdate as U;
        let text = |chunk: acp::ContentChunk| match chunk.content {
            acp::ContentBlock::Text(text) => Some(text.text),
            _ => None,
        };
        match update {
            U::AgentMessageChunk(chunk) => {
                if let Some(text) = text(chunk) {
                    self.send(Event::Message(text));
                }
            }
            U::AgentThoughtChunk(chunk) => {
                if let Some(text) = text(chunk) {
                    self.send(Event::Thought(text));
                }
            }
            U::ToolCall(call) => self.tool(call.into()),
            U::ToolCallUpdate(update) => {
                let before = self.tools.lock().unwrap_or_else(|e| e.into_inner()).get(&*update.tool_call_id.0).cloned();
                self.tool(ToolCall::updated(before.as_ref(), update));
            }
            U::Plan(plan) => self.send(Event::Plan(
                plan.entries.into_iter().map(|e| PlanEntry { text: e.content, status: e.status.into() }).collect(),
            )),
            U::CurrentModeUpdate(update) => self.send(Event::Mode(update.current_mode_id.0.to_string())),
            // The user's own words are already on screen; the rest is not something we show.
            _ => {}
        }
    }

    fn tool(&self, call: ToolCall) {
        self.tools.lock().unwrap_or_else(|e| e.into_inner()).insert(call.id.clone(), call.clone());
        self.send(Event::Tool(call));
    }

    fn permission(&self, request: acp::RequestPermissionRequest, responder: Responder<acp::RequestPermissionResponse>) {
        let before =
            self.tools.lock().unwrap_or_else(|e| e.into_inner()).get(&*request.tool_call.tool_call_id.0).cloned();
        let tool = ToolCall::updated(before.as_ref(), request.tool_call);
        let options = request
            .options
            .into_iter()
            .map(|o| PermissionOption { id: o.option_id.0.to_string(), name: o.name, kind: o.kind.into() })
            .collect();

        let id = self.next_permission.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, responder);
        self.send(Event::Permission(Permission { id, tool, options }));
    }

    /// Answers a permission request: with the option, or (`None`) by refusing.
    fn answer(&self, id: u64, option: Option<String>) {
        let Some(responder) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) else {
            return;
        };
        let outcome = match option {
            Some(option) => acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option)),
            None => acp::RequestPermissionOutcome::Cancelled,
        };
        let _ = responder.respond(acp::RequestPermissionResponse::new(outcome));
    }

    /// Refuses everything that is waiting for an answer.
    fn refuse_all(&self) {
        let ids: Vec<u64> = self.pending.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
        for id in ids {
            self.answer(id, None);
        }
    }
}

/// Runs a session over `transport` until the handle is dropped or the agent goes away.
async fn run(
    transport: impl ConnectTo<Client> + 'static,
    cwd: PathBuf,
    host: String,
    commands: UnboundedReceiver<Command>,
    events: UnboundedSender<Event>,
) {
    let shared = Arc::new(Shared {
        events: events.clone(),
        tools: Mutex::default(),
        pending: Mutex::default(),
        next_permission: AtomicU64::new(1),
    });

    let result = Client
        .builder()
        .name("nix-manager")
        .on_receive_notification(
            {
                let shared = shared.clone();
                async move |notification: acp::SessionNotification, _cx| {
                    shared.update(notification.update);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let shared = shared.clone();
                // The answer comes from the user, so the responder is kept and used later:
                // the connection goes on meanwhile.
                async move |request: acp::RequestPermissionRequest, responder, _cx| {
                    shared.permission(request, responder);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, {
            let shared = shared.clone();
            async move |cx: ConnectionTo<Agent>| converse(cx, shared, cwd, host, commands).await
        })
        .await;

    shared.refuse_all();
    let _ = events.unbounded_send(Event::Closed(result.err().map(|error| error.to_string())));
}

/// The conversation itself: opens the session, then does what the handle says.
async fn converse(
    cx: ConnectionTo<Agent>,
    shared: Arc<Shared>,
    cwd: PathBuf,
    host: String,
    mut commands: UnboundedReceiver<Command>,
) -> agent_client_protocol::Result<()> {
    let init = cx
        .send_request(
            acp::InitializeRequest::new(ProtocolVersion::V1)
                .client_info(acp::Implementation::new("nix-manager", env!("CARGO_PKG_VERSION"))),
        )
        .block_task()
        .await?;
    let session = cx.send_request(acp::NewSessionRequest::new(cwd.clone())).block_task().await?;
    let session_id = session.session_id.clone();

    let agent = init
        .agent_info
        .map(|info| {
            let name = info.title.unwrap_or(info.name);
            if info.version.is_empty() {
                name
            } else {
                format!("{name} {}", info.version)
            }
        })
        .unwrap_or_else(|| "Agent".to_owned());
    let (modes, mode) = match session.modes {
        Some(state) => (
            state
                .available_modes
                .into_iter()
                .map(|m| Mode { id: m.id.0.to_string(), name: m.name, description: m.description.unwrap_or_default() })
                .collect(),
            Some(state.current_mode_id.0.to_string()),
        ),
        None => (Vec::new(), None),
    };
    shared.send(Event::Ready { agent, modes, mode });

    let mut briefing = Some(briefing(&cwd.display().to_string(), &host));
    loop {
        let command = match select(commands.next(), Box::pin(cx.terminated())).await {
            Either::Left((Some(command), _)) => command,
            // The handle is gone, or the agent is.
            _ => return Ok(()),
        };
        match command {
            Command::Prompt(text) => {
                let mut blocks: Vec<acp::ContentBlock> = briefing.take().into_iter().map(Into::into).collect();
                blocks.push(text.into());
                let sent = cx.send_request(acp::PromptRequest::new(session_id.clone(), blocks));
                let shared = shared.clone();
                // Waited for elsewhere, so that cancelling and answering stay possible meanwhile.
                cx.spawn(async move {
                    shared.send(match sent.block_task().await {
                        Ok(response) => Event::TurnEnded(response.stop_reason.into()),
                        Err(error) => Event::TurnFailed(error.to_string()),
                    });
                    Ok(())
                })?;
            }
            Command::Cancel => {
                cx.send_notification(acp::CancelNotification::new(session_id.clone()))?;
                shared.refuse_all();
            }
            Command::Answer { id, option } => shared.answer(id, option),
            Command::SetMode(mode) => {
                let sent = cx.send_request(acp::SetSessionModeRequest::new(session_id.clone(), mode.clone()));
                let shared = shared.clone();
                cx.spawn(async move {
                    if sent.block_task().await.is_ok() {
                        shared.send(Event::Mode(mode));
                    }
                    Ok(())
                })?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::Channel;

    /// A stand-in agent. Prompts containing "edit" make it ask before it edits.
    fn fake_agent() -> impl ConnectTo<Client> {
        use acp::SessionUpdate as U;
        Agent
            .builder()
            .name("fake-agent")
            .on_receive_request(
                async move |init: acp::InitializeRequest, responder, _cx| {
                    responder.respond(
                        acp::InitializeResponse::new(init.protocol_version)
                            .agent_info(acp::Implementation::new("fake", "1.0")),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_: acp::NewSessionRequest, responder, _cx| {
                    responder.respond(acp::NewSessionResponse::new("s1".to_owned()).modes(acp::SessionModeState::new(
                        "ask".to_owned(),
                        vec![
                            acp::SessionMode::new("ask".to_owned(), "Ask first"),
                            acp::SessionMode::new("plan".to_owned(), "Plan only"),
                        ],
                    )))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: acp::SetSessionModeRequest, responder, _cx| {
                    assert_eq!(&*request.mode_id.0, "plan");
                    responder.respond(acp::SetSessionModeResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: acp::PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let id = request.session_id.clone();
                    let note = |update: U| cx.send_notification(acp::SessionNotification::new(id.clone(), update));
                    let text = |s: &str| acp::ContentChunk::new(acp::ContentBlock::from(s.to_owned()));
                    let blocks = request.prompt.len();
                    let asked = match &request.prompt.last() {
                        Some(acp::ContentBlock::Text(t)) => t.text.clone(),
                        _ => String::new(),
                    };

                    note(U::AgentThoughtChunk(text("thinking")))?;
                    note(U::AgentMessageChunk(text(&format!("blocks={blocks} "))))?;
                    note(U::AgentMessageChunk(text("hello")))?;
                    if !asked.contains("edit") {
                        return responder.respond(acp::PromptResponse::new(acp::StopReason::EndTurn));
                    }

                    note(U::ToolCall(
                        acp::ToolCall::new("t1".to_owned(), "Edit flake.nix").kind(acp::ToolKind::Edit).content(vec![
                            acp::ToolCallContent::Diff(
                                acp::Diff::new("flake.nix", "new\n").old_text("old\n".to_owned()),
                            ),
                        ]),
                    ))?;
                    let ask = cx.send_request(acp::RequestPermissionRequest::new(
                        id.clone(),
                        acp::ToolCallUpdate::new("t1".to_owned(), acp::ToolCallUpdateFields::new()),
                        vec![
                            acp::PermissionOption::new(
                                "allow".to_owned(),
                                "Allow",
                                acp::PermissionOptionKind::AllowOnce,
                            ),
                            acp::PermissionOption::new(
                                "deny".to_owned(),
                                "Deny",
                                acp::PermissionOptionKind::RejectOnce,
                            ),
                        ],
                    ));
                    // Waiting for the user must not hold up the connection.
                    cx.clone().spawn(async move {
                        let allowed = matches!(
                            ask.block_task().await?.outcome,
                            acp::RequestPermissionOutcome::Selected(selected) if &*selected.option_id.0 == "allow"
                        );
                        let status = if allowed { acp::ToolCallStatus::Completed } else { acp::ToolCallStatus::Failed };
                        cx.send_notification(acp::SessionNotification::new(
                            id.clone(),
                            U::ToolCallUpdate(acp::ToolCallUpdate::new(
                                "t1".to_owned(),
                                acp::ToolCallUpdateFields::new().status(status),
                            )),
                        ))?;
                        let stop = if allowed { acp::StopReason::EndTurn } else { acp::StopReason::Cancelled };
                        responder.respond(acp::PromptResponse::new(stop))
                    })
                },
                agent_client_protocol::on_receive_request!(),
            )
    }

    /// Runs a session against the stand-in agent while `script` talks to it.
    fn session<T>(script: impl AsyncFnOnce(Handle, &mut Events) -> T) -> T {
        let (client_end, agent_end) = Channel::duplex();
        let (commands_tx, commands) = unbounded();
        let (events_tx, events) = unbounded();
        let mut events: Events = Box::pin(events);
        let handle = Handle { commands: commands_tx };

        futures::executor::block_on(async {
            let agent = fake_agent().connect_to(agent_end);
            let client = run(client_end, PathBuf::from("/etc/nixos"), "laptop".to_owned(), commands, events_tx);
            let script = async { script(handle, &mut events).await };
            let (_, _, out) = future::join3(agent, client, script).await;
            out
        })
    }

    /// The events up to and including the first one `done` accepts.
    async fn until(events: &mut Events, done: impl Fn(&Event) -> bool) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(event) = events.next().await {
            let last = done(&event);
            seen.push(event);
            if last {
                break;
            }
        }
        seen
    }

    fn text_of(events: &[Event]) -> String {
        events.iter().filter_map(|e| if let Event::Message(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    #[test]
    fn a_question_is_answered_in_pieces() {
        let seen = session(async |handle, events| {
            let ready = until(events, |e| matches!(e, Event::Ready { .. })).await;
            assert!(matches!(
                ready.last(),
                Some(Event::Ready { agent, modes, mode })
                    if agent == "fake 1.0" && modes.len() == 2 && mode.as_deref() == Some("ask")
            ));

            handle.prompt("what does this do?");
            let first = until(events, |e| matches!(e, Event::TurnEnded(_))).await;
            handle.prompt("and then?");
            let second = until(events, |e| matches!(e, Event::TurnEnded(_))).await;
            (first, second)
        });

        let (first, second) = seen;
        assert!(first.contains(&Event::Thought("thinking".to_owned())));
        assert_eq!(text_of(&first), "blocks=2 hello", "the first question carries the briefing");
        assert_eq!(first.last(), Some(&Event::TurnEnded(StopReason::EndTurn)), "the end comes after all of it");
        assert_eq!(text_of(&second), "blocks=1 hello", "later ones do not");
    }

    #[test]
    fn an_edit_waits_for_permission() {
        let seen = session(async |handle, events| {
            until(events, |e| matches!(e, Event::Ready { .. })).await;
            handle.prompt("please edit it");
            let asked = until(events, |e| matches!(e, Event::Permission(_))).await;
            let Some(Event::Permission(permission)) = asked.last().cloned() else { panic!("no permission asked") };
            handle.answer(permission.id, Some("allow".to_owned()));
            let rest = until(events, |e| matches!(e, Event::TurnEnded(_))).await;
            (asked, permission, rest)
        });

        let (asked, permission, rest) = seen;
        assert!(asked
            .iter()
            .any(|e| matches!(e, Event::Tool(t) if t.title == "Edit flake.nix" && t.kind == ToolKind::Edit)));
        assert_eq!(permission.tool.title, "Edit flake.nix", "the request is about the tool call announced before");
        assert!(matches!(permission.tool.content.as_slice(), [ToolContent::Diff { new, .. }] if new == "new\n"));
        assert_eq!(
            permission.options.iter().map(|o| o.kind).collect::<Vec<_>>(),
            [OptionKind::AllowOnce, OptionKind::RejectOnce]
        );
        assert!(rest
            .iter()
            .any(|e| matches!(e, Event::Tool(t) if t.status == ToolStatus::Completed && t.title == "Edit flake.nix")));
        assert_eq!(rest.last(), Some(&Event::TurnEnded(StopReason::EndTurn)));
    }

    #[test]
    fn cancelling_refuses_what_is_waiting() {
        let rest = session(async |handle, events| {
            until(events, |e| matches!(e, Event::Ready { .. })).await;
            handle.prompt("edit it");
            until(events, |e| matches!(e, Event::Permission(_))).await;
            handle.cancel();
            until(events, |e| matches!(e, Event::TurnEnded(_))).await
        });
        assert!(rest.iter().any(|e| matches!(e, Event::Tool(t) if t.status == ToolStatus::Failed)), "{rest:?}");
        assert_eq!(rest.last(), Some(&Event::TurnEnded(StopReason::Cancelled)));
    }

    #[test]
    fn the_mode_can_be_changed() {
        let seen = session(async |handle, events| {
            until(events, |e| matches!(e, Event::Ready { .. })).await;
            handle.set_mode("plan");
            until(events, |e| matches!(e, Event::Mode(_))).await
        });
        assert_eq!(seen.last(), Some(&Event::Mode("plan".to_owned())));
    }

    #[test]
    fn dropping_the_handle_ends_the_session() {
        let seen = session(async |handle, events| {
            until(events, |e| matches!(e, Event::Ready { .. })).await;
            drop(handle);
            until(events, |e| matches!(e, Event::Closed(_))).await
        });
        assert_eq!(seen.last(), Some(&Event::Closed(None)));
    }

    #[test]
    fn an_agent_that_cannot_be_started_says_so() {
        let (_handle, mut events) = start(Config {
            command: "/definitely/not/an/agent".to_owned(),
            cwd: PathBuf::from("/tmp"),
            host: String::new(),
        });
        let seen = futures::executor::block_on(until(&mut events, |e| matches!(e, Event::Closed(_))));
        assert!(
            matches!(seen.last(), Some(Event::Closed(Some(reason))) if reason.starts_with("'/definitely/not/an/agent' was not found")),
            "{seen:?}"
        );
    }

    /// The same conversation with a real child process, as it happens in the app.
    #[test]
    fn talks_to_a_real_process() {
        const AGENT: &str = r#"
import sys, json
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    method, id = m.get("method"), m.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1, "agentCapabilities": {}, "agentInfo": {"name": "py", "version": "0"}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "s"}})
    elif method == "session/prompt":
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi from python"}}}})
        send({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}})
"#;
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 is not available; skipping");
            return;
        }
        let script = std::env::temp_dir().join(format!("nix-agent-test-{}.py", std::process::id()));
        std::fs::write(&script, AGENT).unwrap();

        let (handle, mut events) = start(Config {
            command: format!("python3 {}", script.display()),
            cwd: std::env::temp_dir(),
            host: String::new(),
        });
        let seen = futures::executor::block_on(async {
            let ready = until(&mut events, |e| matches!(e, Event::Ready { .. })).await;
            handle.prompt("hello");
            let turn = until(&mut events, |e| matches!(e, Event::TurnEnded(_))).await;
            drop(handle);
            let closed = until(&mut events, |e| matches!(e, Event::Closed(_))).await;
            (ready, turn, closed)
        });
        let _ = std::fs::remove_file(&script);

        let (ready, turn, closed) = seen;
        assert!(matches!(ready.last(), Some(Event::Ready { agent, .. }) if agent == "py 0"), "{ready:?}");
        assert_eq!(text_of(&turn), "hi from python");
        assert_eq!(turn.last(), Some(&Event::TurnEnded(StopReason::EndTurn)));
        assert_eq!(closed.last(), Some(&Event::Closed(None)), "{closed:?}");
    }
}
