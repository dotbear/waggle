use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use secrecy::{ExposeSecret, SecretString};
use tokio::{
    sync::{broadcast, mpsc},
    time::{self, Instant},
};
use xmpp::{
    ClientBuilder, ClientType, Event as XmppEvent,
    delay::StanzaTimeInfo,
    jid::{BareJid, ResourcePart},
    muc::room::JoinRoomSettings,
    parsers::{
        chatstates::ChatState,
        iq::Iq,
        message::{Id as MessageId, Message, MessageType},
        minidom::Element,
        presence::{Presence, Type as PresenceType},
        stanza_error::{DefinedCondition, StanzaError},
    },
    tokio_xmpp::{connect::ServerConnector, xmlstream::Timeouts},
};

use tracing::{debug, info, warn};

use crate::{
    app::AppIdentity,
    config::{JabberConfig, PapsConfig},
    directorbot::*,
};

/// Bounds on how fast a repeatedly failing Jabber worker is restarted.
const WORKER_RESTART_MIN: Duration = Duration::from_secs(5);
const WORKER_RESTART_MAX: Duration = Duration::from_secs(300);
/// Bound a connection attempt, including DNS, TLS and authentication.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long Directorbot has to answer `!me` before the request is abandoned.
const PAP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a stream may be silent before the client pings to keep it alive.
///
/// Goonfleet's server advertises `urn:xmpp:stream-limits:0` with
/// `idle-seconds` of 30, and it means it: a stream quiet for longer is closed.
/// The library's own default is 300 seconds, so every quiet stretch between
/// broadcasts killed the connection and the client reconnected into the same
/// silence, forever. This has to stay comfortably under whatever the server
/// advertises, and the two below have to add up to less than it as well.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(12);
/// How long the server then has to answer that ping before the stream is dead.
const STREAM_RESPONSE_TIMEOUT: Duration = Duration::from_secs(12);
/// A stanza handed to a stream that is down must not block the worker.
///
/// `send_stanza` waits for the stanza to reach the connection. With the stream
/// gone it waits forever, and because that await sits in the worker's `select!`
/// the whole loop stops: no messages, no timers, no commands, no restart, and
/// a status that still cheerfully reads "Connected".
const STANZA_SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// Commands are rare and tiny; this only needs to absorb a burst of clicks.
const COMMAND_QUEUE_DEPTH: usize = 16;

#[derive(Debug, Clone)]
pub enum ConnectionStatus {
    Connecting,
    Connected,
    Disconnected(String),
}

/// Requests the daemon pushes into the running XMPP worker.
///
/// A broadcast channel is used rather than an mpsc so the supervisor can hand a
/// fresh receiver to each attempt: an mpsc receiver moved into a task that
/// panics would be lost, taking every future command with it.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Command {
    /// Ask Directorbot for PAP counts now.
    RefreshPaps,
    /// Drop the current connection and build a new one immediately.
    Reconnect,
}

/// The daemon's handle on a running worker.
#[derive(Debug, Clone)]
pub struct Handle {
    commands: broadcast::Sender<Command>,
}

impl Handle {
    pub fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .context("the Jabber worker is not accepting commands")?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Connection(ConnectionStatus),
    Ping(Box<NewJabberPing>),
    Paps(PapSnapshot),
    Room(RoomEvent),
}

/// What the watched room did. Only its topic is ever read: messages in the
/// room are neither parsed, kept nor logged.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RoomEvent {
    Joining,
    Joined,
    /// The room turned the join down.
    Refused(RoomRefusal),
    Left,
    /// The room's topic.
    Subject(String),
}

/// Why a room turned a join down, for the front end to put in words a pilot
/// can act on.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RoomRefusal {
    /// The room said nothing about why.
    Unexplained,
    /// Members only, or this account is not allowed in.
    MembersOnly,
    /// Someone in the room already uses this nickname.
    NicknameTaken,
    /// No such room.
    NoSuchRoom,
    /// Any other stanza error condition, as the server named it.
    Other(String),
}

/// A multi-user chat room whose topic is worth following, and the nickname to
/// join it with. One per connection: a front end that needs one has a reason.
#[derive(Debug, Clone)]
pub struct WatchedRoom {
    room: BareJid,
    nick: String,
}

impl WatchedRoom {
    /// `room` is the room's bare JID, `nick` the name to appear as.
    pub fn new(room: &str, nick: &str) -> Result<Self> {
        let room = room
            .parse::<BareJid>()
            .context("the room is not a valid bare JID")?;
        ResourcePart::new(nick).context("the nickname is not a valid room nickname")?;
        Ok(Self {
            room,
            nick: nick.to_owned(),
        })
    }
}

/// Everything one Jabber worker needs to run.
///
/// Grouped because these travel together through `start`, `supervise` and
/// `run`, and eight positional arguments is a list you can transpose without
/// the compiler noticing.
#[derive(Clone)]
struct Worker {
    jid: BareJid,
    directorbot: BareJid,
    password: SecretString,
    pap_refresh: Duration,
    pap_auto_refresh: bool,
    identity: AppIdentity,
    room: Option<WatchedRoom>,
    /// Whether `!me` has been asked since the daemon started. Outside the
    /// worker because the worker is respawned on every reconnect.
    asked_once: Arc<AtomicBool>,
}

/// Connects as `config.jid` and keeps the connection up until the handle's
/// last clone is dropped, announcing the application as `identity`.
pub fn start(
    identity: AppIdentity,
    config: &JabberConfig,
    paps: &PapsConfig,
    room: Option<WatchedRoom>,
    password: SecretString,
) -> Result<(mpsc::UnboundedReceiver<Event>, Handle)> {
    let jid = config
        .jid
        .as_deref()
        .context("jabber.jid is not configured")?
        .parse::<BareJid>()
        .context("jabber.jid is not a valid bare JID")?;
    let directorbot = config
        .directorbot
        .as_deref()
        .context("jabber.directorbot is not configured")?
        .parse::<BareJid>()
        .context("jabber.directorbot is not a valid bare JID")?;
    let pap_refresh = Duration::from_secs(paps.refresh_minutes.saturating_mul(60));
    // Outside the worker, so a restart does not reset it — see `asked_once`.
    let asked_once = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::unbounded_channel();
    let (commands, _) = broadcast::channel(COMMAND_QUEUE_DEPTH);
    let handle = Handle {
        commands: commands.clone(),
    };

    let worker = Worker {
        jid,
        directorbot,
        password,
        pap_refresh,
        pap_auto_refresh: paps.auto_refresh,
        identity,
        room,
        asked_once,
    };

    tokio::spawn(async move {
        supervise(worker, sender, commands).await;
    });

    Ok((receiver, handle))
}

/// `xmpp::Agent::wait_for_events` panics if its stream ever closes without
/// reconnecting, and a panicked task would otherwise take Jabber down until the
/// daemon is restarted by hand. Each attempt therefore runs in its own task and
/// is restarted with bounded backoff.
async fn supervise(
    worker: Worker,
    sender: mpsc::UnboundedSender<Event>,
    commands: broadcast::Sender<Command>,
) {
    let events = sender.clone();
    supervise_attempts(sender, commands, move |commands| {
        run(worker.clone(), events.clone(), commands)
    })
    .await;
}

async fn supervise_attempts<F, Fut>(
    sender: mpsc::UnboundedSender<Event>,
    commands: broadcast::Sender<Command>,
    mut attempt: F,
) where
    F: FnMut(broadcast::Receiver<Command>) -> Fut,
    Fut: std::future::Future<Output = Result<Stopped>> + Send + 'static,
{
    let mut backoff = WORKER_RESTART_MIN;

    loop {
        let mut retry_commands = commands.subscribe();
        let started = Instant::now();
        let attempt = tokio::spawn(attempt(commands.subscribe()));
        let outcome = attempt.await;

        if sender.is_closed() {
            return;
        }
        let (detail, requested) = match outcome {
            Ok(Ok(Stopped::Requested)) => ("a reconnect was requested".to_owned(), true),
            Ok(Ok(Stopped::Ended)) => ("the Jabber client stopped".to_owned(), false),
            Ok(Err(error)) => (format!("{error:#}"), false),
            Err(error) if error.is_panic() => ("the Jabber client panicked".to_owned(), false),
            Err(_) => return,
        };

        // A reconnect the user asked for should happen now, not after whatever
        // backoff a previous failure had accumulated.
        // A long-lived connection starts a new failure streak.
        if started.elapsed() >= WORKER_RESTART_MAX {
            backoff = WORKER_RESTART_MIN;
        }
        let delay = if requested {
            backoff = WORKER_RESTART_MIN;
            Duration::from_millis(0)
        } else {
            backoff
        };
        if sender
            .send(Event::Connection(ConnectionStatus::Disconnected(format!(
                "{detail}; reconnecting in {} seconds",
                delay.as_secs()
            ))))
            .is_err()
        {
            return;
        }
        warn!(detail, backoff = delay.as_secs(), "Restarting Jabber");
        if !delay.is_zero() {
            let sleep = time::sleep(delay);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => {
                        backoff = (backoff * 2).min(WORKER_RESTART_MAX);
                        break;
                    }
                    _ = sender.closed() => return,
                    command = retry_commands.recv() => match command {
                        Ok(Command::Reconnect) => {
                            backoff = WORKER_RESTART_MIN;
                            break;
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Why one worker attempt ended.
enum Stopped {
    /// The daemon asked for a fresh connection.
    Requested,
    /// The client returned on its own, or its channel closed.
    Ended,
}

async fn run(
    worker: Worker,
    sender: mpsc::UnboundedSender<Event>,
    commands: broadcast::Receiver<Command>,
) -> Result<Stopped> {
    let client = build_client(
        ClientBuilder::new(worker.jid.clone(), worker.password.expose_secret()),
        worker.identity,
    );
    run_with_client(worker, client, sender, commands).await
}

fn build_client<C: ServerConnector>(
    builder: ClientBuilder<'_, C>,
    identity: AppIdentity,
) -> xmpp::Agent {
    // What this connection says it is: the application's own name, as the
    // resource, the discovery identity and the software-version answer.
    builder
        .set_client(ClientType::Pc, identity.name)
        .set_resource(identity.name)
        .set_website(identity.website)
        // The server asks `jabber:iq:version` seconds after login. Without
        // this the library refuses the question, which reads as refusing to
        // say what this client is.
        .set_software_version(identity.name, identity.version)
        .set_timeouts(Timeouts {
            read_timeout: STREAM_IDLE_TIMEOUT,
            response_timeout: STREAM_RESPONSE_TIMEOUT,
        })
        .build()
}

// The worker is also exercised against a loopback server. Only its transport
// changes in those tests; identity, requests, timers and reply handling all run
// through the same code as the live connection.
async fn run_with_client(
    worker: Worker,
    mut client: xmpp::Agent,
    sender: mpsc::UnboundedSender<Event>,
    mut commands: broadcast::Receiver<Command>,
) -> Result<Stopped> {
    let Worker {
        jid,
        directorbot,
        pap_refresh,
        pap_auto_refresh,
        room,
        asked_once,
        ..
    } = worker;
    let _ = sender.send(Event::Connection(ConnectionStatus::Connecting));
    let first_refresh = Instant::now() + pap_refresh;
    let mut pap_timer = time::interval_at(first_refresh, pap_refresh);
    pap_timer.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut pap_deadline = time::interval(PAP_RESPONSE_TIMEOUT);
    pap_deadline.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    let mut online = false;
    let connect_deadline = time::sleep(CONNECT_TIMEOUT);
    tokio::pin!(connect_deadline);
    // Only one `!me` is ever outstanding, so a slow or missing answer cannot
    // stack up requests against Directorbot.
    let mut paps_asked = PapRequests::new(pap_auto_refresh);

    if !paps_asked.automatic() {
        info!("Automatic PAP requests are disabled; fleet pings remain connected");
    }

    loop {
        tokio::select! {
            _ = &mut connect_deadline, if !online => {
                anyhow::bail!("Jabber connection timed out");
            }
            events = client.wait_for_events() => {
                for event in events {
                    match event {
                        XmppEvent::Online => {
                            online = true;
                            if sender.send(Event::Connection(ConnectionStatus::Connected)).is_err() {
                                return Ok(Stopped::Ended);
                            }
                            // Asked once when the connection first comes up,
                            // and on the interval after that — not on every
                            // reconnect. A daemon restart, a dropped stream
                            // and a failed keep-alive each produced another
                            // `!me`, so a day of development meant dozens of
                            // unanswered requests at somebody else's bot.
                            // Whatever the silence turns out to be, that was
                            // not a polite way to ask.
                            //
                            // The flag lives outside the worker: it is
                            // respawned on every reconnect, so anything held
                            // here would reset with it.
                            // `asked_once` is only spent when an automatic
                            // request is actually permitted, so a paused
                            // daemon does not silently consume the one
                            // startup request it is allowed.
                            if paps_asked.may_ask_automatically()
                                && !asked_once.swap(true, Ordering::Relaxed)
                            {
                                paps_asked.started(request_paps(&mut client, directorbot.clone()).await?);
                                pap_deadline.reset();
                            }
                            // Each connection is a fresh client with no rooms,
                            // so the room is joined on every one.
                            if let Some(room) = &room {
                                join_room(&mut client, room).await?;
                                let _ = sender.send(Event::Room(RoomEvent::Joining));
                            }
                        }
                        XmppEvent::RoomJoined(joined) if is_watched(&room, &joined) => {
                            info!(room = %joined, "Joined the watched room");
                            let _ = sender.send(Event::Room(RoomEvent::Joined));
                        }
                        XmppEvent::RoomLeft(left) if is_watched(&room, &left) => {
                            warn!(room = %left, "Left the watched room");
                            let _ = sender.send(Event::Room(RoomEvent::Left));
                        }
                        // The topic is the status, and the status is not to be
                        // shared: it goes to the panel and nowhere near a log.
                        XmppEvent::RoomSubject(subject_room, _, subject, _) if is_watched(&room, &subject_room) => {
                            debug!("The watched room's topic changed");
                            let _ = sender.send(Event::Room(RoomEvent::Subject(subject)));
                        }
                        XmppEvent::Disconnected(error) => {
                            return Err(anyhow::anyhow!("Jabber disconnected: {error}"));
                        }
                        XmppEvent::ChatMessage(_, from, body, time_info)
                        | XmppEvent::ServiceMessage(_, from, body, time_info) => {
                            if from == directorbot
                                && emit_directorbot_message(&sender, from, body, time_info)
                            {
                                paps_asked.answered();
                            }
                        }
                        // Roster and presence metadata can be observed without
                        // spending a PAP request or logging alliance text.
                        XmppEvent::ContactAdded(item)
                        | XmppEvent::ContactChanged(item) => {
                            if item.jid == directorbot {
                                info!(
                                    subscription = ?item.subscription,
                                    ask = ?item.ask,
                                    "Directorbot is a roster contact"
                                );
                            }
                        }
                        // A subscription request that is never answered leaves
                        // the asker permanently pending. Smack accepts these
                        // by default, which is why RIFT would never have
                        // noticed; discarding them is a choice this made by
                        // omission.
                        XmppEvent::Presence(presence)
                            if presence.type_ == PresenceType::Error
                                && presence
                                    .from
                                    .as_ref()
                                    .is_some_and(|from| is_watched(&room, &from.to_bare())) =>
                        {
                            let reason = room_refusal(&presence.payloads);
                            warn!(?reason, "The watched room refused the join");
                            let _ = sender.send(Event::Room(RoomEvent::Refused(reason)));
                        }
                        XmppEvent::Presence(presence) => {
                            if presence.from.as_ref().is_some_and(|from| from.to_bare() == directorbot) {
                                info!(
                                    from = ?presence.from,
                                    presence_type = ?presence.type_,
                                    priority = ?presence.priority,
                                    show = ?presence.show,
                                    "Received Directorbot presence"
                                );
                            }
                            if presence.type_ == PresenceType::Subscribe
                                && presence
                                    .from
                                    .as_ref()
                                    .is_some_and(|from| from.to_bare() == directorbot)
                            {
                                debug!("Directorbot asked to subscribe; accepting");
                                let mut answer =
                                    Presence::new(PresenceType::Subscribed);
                                answer.to = Some(directorbot.clone().into());
                                send_stanza(client.send_stanza(answer)).await?;
                            }
                        }
                        // The library has already answered the query. Routine
                        // keepalives are not identity probes.
                        XmppEvent::Iq(Iq::Get { ref payload, .. })
                            if payload.is("ping", xmpp::parsers::ns::PING) => {}
                        XmppEvent::Iq(Iq::Get { from, payload, .. }) => {
                            info!(
                                from = ?from.as_ref().map(|from| from.to_bare()),
                                namespace = payload.ns(),
                                element = payload.name(),
                                "Asked about this client"
                            );
                        }
                        XmppEvent::Message(message) => {
                            if let Some(request_id) = paps_asked.pending() {
                                // Inspect the raw event before the high-level
                                // library can ignore an unexpected reply shape.
                                // Protocol metadata only; never log the body.
                                info!(
                                    request_id = %request_id.0,
                                    from = ?message.from,
                                    to = ?message.to,
                                    message_type = ?message.type_,
                                    bodies = message.bodies.len(),
                                    payloads = ?message.payloads.iter().map(|p| (p.name(), p.ns())).collect::<Vec<_>>(),
                                    account_info = ?message.bodies.values().find_map(|body| pap_reply_shape(body)),
                                    "Received XMPP message while awaiting PAP reply"
                                );
                            }
                            if is_pap_request_error(&message, paps_asked.pending(), &directorbot, &jid) {
                                warn!(
                                    request_id = ?paps_asked.pending(),
                                    condition = %describe_stanza_error(&message),
                                    "PAP request was refused"
                                );
                                paps_asked.answered();
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ = pap_timer.tick(), if online && paps_asked.may_ask_automatically() => {
                paps_asked.started(request_paps(&mut client, directorbot.clone()).await?);
                pap_deadline.reset();
            }
            _ = pap_deadline.tick(), if paps_asked.is_outstanding() => {
                warn!(request_id = ?paps_asked.answered(), "No recognised PAP reply arrived before the deadline");
            }
            command = commands.recv() => {
                match command {
                    Ok(Command::RefreshPaps) => {
                        if !online {
                            let _ = sender.send(Event::Connection(
                                ConnectionStatus::Disconnected(
                                    "cannot refresh PAPs while disconnected".to_owned(),
                                ),
                            ));
                        } else if !paps_asked.may_ask_manually() {
                            warn!("A PAP request is already outstanding; ignoring duplicate refresh");
                        } else {
                            paps_asked.started(request_paps(&mut client, directorbot.clone()).await?);
                            pap_deadline.reset();
                            pap_timer.reset();
                        }
                    }
                    Ok(Command::Reconnect) => return Ok(Stopped::Requested),
                    // Lagging only happens if commands arrive faster than the
                    // worker drains them; the newest state is what matters.
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return Ok(Stopped::Ended),
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "jabber/wire_tests.rs"]
mod wire_tests;

/// Decides when this daemon may say `!me`, and holds the one request it is
/// waiting on.
///
/// Both halves exist to avoid asking somebody else's bot more often than it
/// was asked for. `auto_refresh` is read from configuration and never changes
/// for the life of a worker, so a quiet period survives a reconnect: the
/// worker is respawned from a `Worker` that still carries the flag. Only one
/// request is ever outstanding, so a slow or unanswered `!me` cannot stack
/// further requests behind it.
struct PapRequests {
    /// Whether the connect-time and interval requests are allowed at all.
    auto_refresh: bool,
    /// The request this worker is waiting on, by stanza ID.
    pending: Option<MessageId>,
}

impl PapRequests {
    fn new(auto_refresh: bool) -> Self {
        Self {
            auto_refresh,
            pending: None,
        }
    }

    fn automatic(&self) -> bool {
        self.auto_refresh
    }

    fn pending(&self) -> Option<&MessageId> {
        self.pending.as_ref()
    }

    fn is_outstanding(&self) -> bool {
        self.pending.is_some()
    }

    /// The connect-time request and the interval timer. Paused by
    /// configuration, and never while another request is unanswered.
    fn may_ask_automatically(&self) -> bool {
        self.auto_refresh && self.pending.is_none()
    }

    /// An operator asking on purpose, through `paps refresh` or the panel.
    ///
    /// Deliberately not gated on `auto_refresh`: pausing the automatic
    /// requests is what makes a single controlled attempt possible, so the
    /// explicit path has to stay open during a quiet period.
    fn may_ask_manually(&self) -> bool {
        self.pending.is_none()
    }

    fn started(&mut self, id: MessageId) {
        self.pending = Some(id);
    }

    /// The request is no longer outstanding, whether it was answered, refused
    /// or timed out. Returns it for logging.
    fn answered(&mut self) -> Option<MessageId> {
        self.pending.take()
    }
}

/// Servers can generate the error themselves. Correlate by request ID rather
/// than assuming an error must have Directorbot's JID as its sender.
fn is_pap_request_error(
    message: &Message,
    pending: Option<&MessageId>,
    directorbot: &BareJid,
    account: &BareJid,
) -> bool {
    message.type_ == MessageType::Error
        && pending.is_some_and(|id| message.id.as_ref() == Some(id))
        && message.from.as_ref().is_some_and(|from| {
            from.to_bare() == *directorbot
                || (from.node().is_none() && from.domain() == account.domain())
        })
}

/// Names an error stanza's condition, and nothing else it carries.
///
/// The condition is the machine-readable half — `service-unavailable`,
/// `not-authorized`, `policy-violation` — and is what says why a message was
/// refused. The optional text alongside it is free-form and may quote the
/// message, so it is deliberately not read.
fn is_watched(watched: &Option<WatchedRoom>, room: &BareJid) -> bool {
    watched
        .as_ref()
        .is_some_and(|watched| watched.room == *room)
}

async fn join_room(client: &mut xmpp::Agent, room: &WatchedRoom) -> Result<()> {
    let nick = ResourcePart::new(&room.nick).context("the room nickname is not valid")?;
    let join = client.join_room(JoinRoomSettings::new(room.room.clone()).with_nick(nick));
    time::timeout(STANZA_SEND_TIMEOUT, join)
        .await
        .context("the Jabber stream did not accept the room join within 30 seconds")
}

/// Why a room turned a join down.
fn room_refusal(payloads: &[Element]) -> RoomRefusal {
    let Some(error) = payloads
        .iter()
        .find_map(|payload| StanzaError::try_from(payload.clone()).ok())
    else {
        return RoomRefusal::Unexplained;
    };
    match error.defined_condition {
        DefinedCondition::Forbidden
        | DefinedCondition::RegistrationRequired
        | DefinedCondition::NotAuthorized => RoomRefusal::MembersOnly,
        DefinedCondition::Conflict => RoomRefusal::NicknameTaken,
        DefinedCondition::ItemNotFound | DefinedCondition::RemoteServerNotFound => {
            RoomRefusal::NoSuchRoom
        }
        other => RoomRefusal::Other(format!("{other:?}")),
    }
}

fn describe_stanza_error(message: &Message) -> String {
    message
        .payloads
        .iter()
        .find_map(|payload| StanzaError::try_from(payload.clone()).ok())
        .map_or_else(
            || "an error with no condition".to_owned(),
            |error| format!("{:?}/{:?}", error.type_, error.defined_condition),
        )
}

/// A stanza ID: random, and unique enough that a reply can be matched to it.
fn stanza_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("pap-{:x}-{sequence:x}", Utc::now().timestamp_millis())
}

/// A transport failure must leave the worker so the supervisor replaces the
/// connection. Merely updating health leaves the same stalled stream alive.
async fn send_stanza<T, E: std::fmt::Display>(
    send: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    time::timeout(STANZA_SEND_TIMEOUT, send)
        .await
        .context("the Jabber stream did not accept a stanza within 30 seconds")?
        .map_err(|error| anyhow::anyhow!("could not send a Jabber stanza: {error}"))
}

/// Sends `!me` to Directorbot. A successful send starts the response deadline;
/// an unanswered request alone is not evidence of a broken connection.
async fn request_paps(client: &mut xmpp::Agent, directorbot: BareJid) -> Result<MessageId> {
    let id = MessageId(stanza_id());
    let message = pap_request_message(directorbot, id.clone());
    send_stanza(client.send_stanza(message)).await?;
    info!(request_id = %id.0, bound_jid = ?client.bound_jid(), "Sent PAP request");
    Ok(id)
}

fn pap_request_message(directorbot: BareJid, id: MessageId) -> Message {
    let mut message = Message::new(Some(directorbot.into()));
    message.type_ = MessageType::Chat;
    message.id = Some(id);
    message.bodies.insert("".into(), "!me".to_owned());
    // RIFT initializes Smack's ChatStateManager, which adds this to the first
    // message. Match that initial request without claiming another identity.
    message.payloads.push(ChatState::Active.into());
    message
}

/// Classifies one Directorbot message. Returns whether it answered a PAP
/// request, so an unrelated broadcast never clears a pending `!me`.
fn emit_directorbot_message(
    sender: &mpsc::UnboundedSender<Event>,
    from: BareJid,
    body: String,
    time_info: StanzaTimeInfo,
) -> bool {
    let occurred_at = time_info
        .delays
        .iter()
        .map(|delay| delay.stamp.0.with_timezone(&Utc))
        .min()
        .unwrap_or(time_info.received);
    if let Some(paps) = parse_paps(&body, time_info.received) {
        let _ = sender.send(Event::Paps(paps));
        return true;
    }
    if let Some(shape) = pap_reply_shape(&body) {
        warn!(
            ?shape,
            "Received account info but could not parse all PAP periods"
        );
        // This is an answer, even though it cannot produce a valid snapshot.
        // Do not follow it with a misleading 'no reply' timeout or a ping.
        return true;
    }
    let pings = parse_pings(&from.to_string(), &body, occurred_at, time_info.received);
    if !pings.is_empty() {
        // One event per fleet, so each is stored, announced and joined as the
        // separate ping it is.
        for ping in pings {
            let _ = sender.send(Event::Ping(Box::new(ping)));
        }
        return false;
    }
    // Directorbot's format is not a contract, so a message that matches
    // neither shape is surfaced rather than silently dropped. The warning
    // describes the message's *shape* — line count and the label names it
    // uses — because that is what a parser fix needs. Message text is never
    // logged: it is alliance communication.
    let (known, unknown) = label_summary(&body);
    let looks_like_a_broadcast = !known.is_empty() || body.contains("~~~");
    if !looks_like_a_broadcast {
        // Directorbot greets on connect and answers unknown commands. Neither
        // is a fleet ping, and warning about them would train the reader to
        // ignore the warning that matters.
        debug!(
            lines = body.lines().filter(|line| !line.trim().is_empty()).count(),
            unknown_labels = unknown,
            "Ignored a Directorbot chat message"
        );
        return false;
    }
    warn!(
        lines = body.lines().filter(|line| !line.trim().is_empty()).count(),
        characters = body.chars().count(),
        known_labels = ?known,
        unknown_labels = unknown,
        has_signature = body.contains("~~~"),
        "Ignored an unrecognised Directorbot message"
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quiet period is only worth anything if it holds by itself. The
    /// pause is configuration carried on `Worker`, and `Worker` is cloned for
    /// every reconnect attempt, so nothing about dropping a stream or
    /// restarting the client can turn automatic requests back on.
    #[test]
    fn a_pause_survives_reconnects_and_worker_restarts() {
        let worker = Worker {
            jid: "pilot@example.invalid".parse().unwrap(),
            directorbot: "directorbot@example.invalid".parse().unwrap(),
            password: SecretString::from("not-a-real-password"),
            pap_refresh: Duration::from_secs(3600),
            pap_auto_refresh: false,
            identity: crate::app::TEST_APP,
            room: None,
            asked_once: Arc::new(AtomicBool::new(false)),
        };

        // `supervise` respawns the worker by cloning it, once per attempt.
        for _ in 0..5 {
            let attempt = worker.clone();
            assert!(!attempt.pap_auto_refresh);
            let mut paps = PapRequests::new(attempt.pap_auto_refresh);
            // Neither the connect-time request nor the interval timer.
            assert!(!paps.may_ask_automatically());
            assert!(!paps.is_outstanding());
            paps.answered();
            assert!(!paps.may_ask_automatically());
        }

        // And the one startup request is not silently spent while paused, so
        // it is still available whenever the pause is lifted.
        assert!(!worker.asked_once.load(Ordering::Relaxed));
    }

    /// The pause stops this daemon asking on its own. It deliberately does not
    /// stop an operator asking, because a single controlled request is the
    /// whole point of the quiet period — see `PapRequests::may_ask_manually`.
    #[test]
    fn a_pause_still_allows_one_deliberate_request() {
        let mut paps = PapRequests::new(false);

        assert!(!paps.may_ask_automatically());
        assert!(paps.may_ask_manually());

        paps.started(MessageId("deliberate".to_owned()));
        // Even asked by hand, it is still one at a time.
        assert!(!paps.may_ask_manually());
        assert!(!paps.may_ask_automatically());
    }

    /// Directorbot gets one question at a time however hard the panel's `P`
    /// shortcut is pressed. Repeated refreshes are dropped, not queued, so a
    /// bot that is slow or silent cannot be handed a backlog when it answers.
    #[test]
    fn repeated_refreshes_cannot_stack_while_one_is_outstanding() {
        let mut paps = PapRequests::new(true);
        assert!(paps.may_ask_manually());

        let first = MessageId("request-one".to_owned());
        paps.started(first.clone());

        // Every further refresh, and every interval tick, is refused while
        // the first is unanswered.
        for _ in 0..10 {
            assert!(!paps.may_ask_manually());
            assert!(!paps.may_ask_automatically());
            assert!(paps.is_outstanding());
            // The request that is waiting stays the first one; a dropped
            // refresh must not overwrite the ID a reply is correlated against.
            assert_eq!(paps.pending(), Some(&first));
        }

        // Only once it is resolved does the next request become possible.
        assert_eq!(paps.answered(), Some(first));
        assert!(!paps.is_outstanding());
        assert!(paps.may_ask_manually());
        assert!(paps.may_ask_automatically());
    }

    /// A timeout, a refusal and a real answer all have to clear the way for
    /// the next request. If any of them failed to, PAPs would stop refreshing
    /// until the daemon was restarted.
    #[test]
    fn every_ending_releases_the_outstanding_request() {
        for ending in ["answered", "refused", "timed out"] {
            let mut paps = PapRequests::new(true);
            paps.started(MessageId(ending.to_owned()));
            assert!(paps.is_outstanding(), "{ending}");

            assert_eq!(
                paps.answered(),
                Some(MessageId(ending.to_owned())),
                "{ending}"
            );

            assert!(paps.may_ask_automatically(), "{ending}");
            // And releasing it twice is not a way to get an extra request.
            assert_eq!(paps.answered(), None, "{ending}");
        }
    }

    #[test]
    fn pap_request_matches_smacks_first_chat_message() {
        let expected: xmpp::parsers::minidom::Element =
            include_str!("../tests/fixtures/directorbot/smack_pap_request.xml")
                .parse()
                .unwrap();
        let expected = Message::try_from(expected).unwrap();
        let request = pap_request_message(
            "directorbot@example.invalid".parse().unwrap(),
            MessageId("offline-pap-test".to_owned()),
        );

        assert_eq!(request, expected);
    }

    #[test]
    fn pap_errors_must_match_the_request_and_come_from_the_bot_or_server() {
        let bot: BareJid = "directorbot@bot.example.invalid".parse().unwrap();
        let account = "pilot@example.invalid".parse().unwrap();
        let id = MessageId("request-one".to_owned());
        let mut message = Message::new(None);
        message.type_ = MessageType::Error;
        message.id = Some(id.clone());

        for from in ["example.invalid", "directorbot@bot.example.invalid/bot"] {
            message.from = Some(from.parse().unwrap());
            assert!(is_pap_request_error(&message, Some(&id), &bot, &account));
        }
        for from in ["stranger@example.invalid", "other.invalid"] {
            message.from = Some(from.parse().unwrap());
            assert!(!is_pap_request_error(&message, Some(&id), &bot, &account));
        }
        message.from = Some(bot.clone().into());
        assert!(!is_pap_request_error(&message, None, &bot, &account));
        message.id = Some(MessageId("unrelated-request".to_owned()));
        assert!(!is_pap_request_error(&message, Some(&id), &bot, &account));
        message.id = Some(id.clone());
        message.type_ = MessageType::Chat;
        assert!(!is_pap_request_error(&message, Some(&id), &bot, &account));
    }

    #[test]
    fn incomplete_account_info_is_an_answer_without_becoming_a_snapshot_or_ping() {
        let (sender, mut events) = mpsc::unbounded_channel();
        let body =
            "Account Info: Private Pilot\nThis Month: 12 STR / 3 PCT\nLast 30 Days: unavailable";
        assert_eq!(
            pap_reply_shape(body),
            Some(PapReplyShape {
                month_parsed: true,
                days_30_parsed: false,
                days_90_parsed: false,
            })
        );
        assert!(!format!("{:?}", pap_reply_shape(body)).contains("Private Pilot"));
        assert!(emit_directorbot_message(
            &sender,
            "directorbot@example.invalid".parse().unwrap(),
            body.to_owned(),
            StanzaTimeInfo {
                received: Utc::now(),
                delays: vec![]
            },
        ));
        assert!(events.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_send_restarts_worker_and_recovers_health() {
        let (sender, mut events) = mpsc::unbounded_channel();
        let (commands, _) = broadcast::channel(COMMAND_QUEUE_DEPTH);
        let recovered = sender.clone();
        let mut first = true;
        let supervisor = tokio::spawn(supervise_attempts(sender, commands, move |_| {
            let stalled = std::mem::replace(&mut first, false);
            let recovered = recovered.clone();
            async move {
                if stalled {
                    send_stanza(std::future::pending::<Result<()>>()).await?;
                }
                recovered
                    .send(Event::Connection(ConnectionStatus::Connected))
                    .unwrap();
                std::future::pending::<Result<Stopped>>().await
            }
        }));
        let started = Instant::now();
        let Event::Connection(ConnectionStatus::Disconnected(detail)) =
            events.recv().await.unwrap()
        else {
            panic!("expected a failed send to schedule reconnection");
        };
        assert!(detail.contains("did not accept a stanza"));
        assert!(detail.contains("reconnecting in 5 seconds"));
        assert!(matches!(
            events.recv().await,
            Some(Event::Connection(ConnectionStatus::Connected))
        ));
        assert_eq!(started.elapsed(), STANZA_SEND_TIMEOUT + WORKER_RESTART_MIN);
        supervisor.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn failures_back_off_and_manual_retry_interrupts_delay() {
        let (sender, mut events) = mpsc::unbounded_channel();
        let (commands, _) = broadcast::channel(COMMAND_QUEUE_DEPTH);
        let supervisor = tokio::spawn(supervise_attempts(sender, commands.clone(), |_| async {
            send_stanza(async { Err::<(), _>("transport closed") }).await?;
            Ok(Stopped::Ended)
        }));
        for seconds in [5, 10, 20, 40, 80, 160, 300, 300] {
            let Event::Connection(ConnectionStatus::Disconnected(detail)) =
                events.recv().await.unwrap()
            else {
                panic!("expected a retry after transport failure");
            };
            assert!(detail.contains("transport closed"));
            assert!(detail.ends_with(&format!("reconnecting in {seconds} seconds")));
        }
        let requested = Instant::now();
        commands.send(Command::Reconnect).unwrap();
        let Event::Connection(ConnectionStatus::Disconnected(detail)) =
            events.recv().await.unwrap()
        else {
            panic!("expected manual retry to start a fresh attempt");
        };
        assert!(detail.ends_with("reconnecting in 5 seconds"));
        assert_eq!(requested.elapsed(), Duration::ZERO);
        supervisor.abort();
    }

    #[tokio::test]
    async fn successful_send_does_not_require_a_bot_reply() {
        assert_eq!(
            send_stanza(async { Ok::<_, anyhow::Error>(42) })
                .await
                .unwrap(),
            42
        );
    }

    #[test]
    fn every_request_carries_a_distinct_id() {
        // A bot or server that keys its reply, or its refusal, to the request
        // has nothing to key to without one — and two requests sharing an ID
        // would be worse than none.
        let first = stanza_id();
        let second = stanza_id();

        assert_ne!(first, second);
        assert!(first.starts_with("pap-"), "{first}");
        assert!(!first.contains(' '), "{first}");
    }

    #[test]
    fn an_error_with_no_condition_still_says_something() {
        // Reached when a peer sends `type="error"` with no `<error/>` payload,
        // which is malformed but happens. Returning an empty string would put
        // a blank field in the log where the reason should be.
        let message = Message::new(None);

        assert_eq!(
            describe_stanza_error(&message),
            "an error with no condition"
        );
    }

    #[test]
    fn a_refusal_is_described_by_its_condition_and_not_its_text() {
        use xmpp::parsers::stanza_error::{DefinedCondition, ErrorType};

        let mut message = Message::new(None);
        message.type_ = MessageType::Error;
        message.payloads.push(
            StanzaError::new(
                ErrorType::Cancel,
                DefinedCondition::ServiceUnavailable,
                "en",
                "quoted alliance text that must not be logged",
            )
            .into(),
        );

        let described = describe_stanza_error(&message);

        assert!(described.contains("ServiceUnavailable"), "{described}");
        assert!(described.contains("Cancel"), "{described}");
        // The human-readable half is free-form and may quote the message, so
        // it is deliberately not read.
        assert!(!described.contains("alliance text"), "{described}");
    }
}
