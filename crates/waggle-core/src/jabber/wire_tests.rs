//! Exercise the real client and worker without contacting Directorbot.
//!
//! The only listener and destination are IPv4 loopback. All JIDs, credentials
//! and messages are synthetic; production configuration is never read.

use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio::{io::BufStream, net::TcpListener, net::TcpStream, task::JoinHandle};
use xmpp::{
    parsers::{bind::BindFeature, minidom::Element, ns, stream_features::StreamFeatures},
    tokio_xmpp::{
        connect::{DnsConfig, TcpServerConnector},
        xmlstream::{StreamHeader, XmlStream, accept_stream},
    },
};

const ACCOUNT: &str = "pilot@example.invalid";
const BOT: &str = "directorbot@example.invalid/bot";
const WAIT: Duration = Duration::from_secs(5);
// Short enough to exercise multiple timer ticks without waiting an hour.
const REFRESH: Duration = Duration::from_millis(20);

struct LocalSession {
    stream: XmlStream<BufStream<TcpStream>, Element>,
    received: Vec<Element>,
    events: mpsc::UnboundedReceiver<Event>,
    commands: broadcast::Sender<Command>,
    worker: JoinHandle<Result<Stopped>>,
}

impl LocalSession {
    async fn connect(worker: Worker) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let client = build_client(
            ClientBuilder::new_with_connector(
                worker.jid.clone(),
                worker.password.expose_secret(),
                TcpServerConnector(DnsConfig::addr(&listener.local_addr().unwrap().to_string())),
            ),
            worker.identity,
        );
        let resource = worker.identity.name;
        let (sender, events) = mpsc::unbounded_channel();
        let (commands, receiver) = broadcast::channel(COMMAND_QUEUE_DEPTH);
        let worker = tokio::spawn(run_with_client(worker, client, sender, receiver));

        let (socket, peer) = time::timeout(WAIT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert!(peer.ip().is_loopback());
        socket.set_nodelay(true).unwrap();
        let mut stream = time::timeout(WAIT, async {
            accept_stream(
                BufStream::new(socket),
                ns::JABBER_CLIENT,
                Timeouts::default(),
            )
            .await
            .unwrap()
            .send_header(StreamHeader::default())
            .await
            .unwrap()
            .send_features::<Element>(&StreamFeatures {
                sasl_mechanisms: ["PLAIN".to_owned()].into(),
                ..Default::default()
            })
            .await
            .unwrap()
        })
        .await
        .unwrap();
        let auth = time::timeout(WAIT, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(auth.is("auth", ns::SASL));
        // This fake server accepts only our fixed synthetic login. It does
        // not implement a real authentication service or load any secrets.
        assert_eq!(auth.text(), "AHBpbG90AG9mZmxpbmUtdGVzdA==");
        let mut stream = time::timeout(WAIT, async {
            stream
                .accept_reset(&Element::builder("success", ns::SASL).build())
                .await
                .unwrap()
                .send_header(StreamHeader::default())
                .await
                .unwrap()
                .send_features::<Element>(&StreamFeatures {
                    bind: Some(BindFeature { required: false }),
                    ..Default::default()
                })
                .await
                .unwrap()
        })
        .await
        .unwrap();
        let bind = time::timeout(WAIT, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let request = bind.get_child("bind", ns::BIND).unwrap();
        assert_eq!(
            request.get_child("resource", ns::BIND).unwrap().text(),
            resource
        );
        let bound: Element = format!(
            "<iq xmlns='jabber:client' type='result' id='{}'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>{ACCOUNT}/{resource}</jid></bind></iq>",
            bind.attr("id").unwrap()
        ).parse().unwrap();
        stream.send(&bound).await.unwrap();

        let mut session = Self {
            stream,
            received: vec![],
            events,
            commands,
            worker,
        };
        time::timeout(WAIT, async {
            loop {
                match session.events.recv().await.unwrap() {
                    Event::Connection(ConnectionStatus::Connected) => break,
                    Event::Connection(ConnectionStatus::Connecting) => {}
                    event => panic!("unexpected event before connected: {event:?}"),
                }
            }
        })
        .await
        .unwrap();
        session
    }

    async fn send(&mut self, xml: &str) {
        let element: Element = xml.parse().unwrap();
        self.stream.send(&element).await.unwrap();
    }

    async fn receive(&mut self) -> Element {
        let element = self.stream.next().await.unwrap().unwrap();
        if element.is("iq", ns::JABBER_CLIENT) && element.attr("type") == Some("get") {
            let id = element.attr("id").unwrap();
            if element.get_child("query", ns::ROSTER).is_some() {
                self.send(&format!("<iq xmlns='jabber:client' type='result' id='{id}'><query xmlns='jabber:iq:roster'/></iq>")).await;
            } else {
                self.send(&format!("<iq xmlns='jabber:client' type='error' id='{id}'><error type='cancel'><service-unavailable xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></iq>")).await;
            }
        }
        self.received.push(element.clone());
        element
    }

    async fn until(&mut self, matches: impl Fn(&Element) -> bool) -> Element {
        time::timeout(WAIT, async {
            loop {
                let element = self.receive().await;
                if matches(&element) {
                    return element;
                }
            }
        })
        .await
        .unwrap()
    }

    async fn drain(&mut self) {
        let _ = time::timeout(REFRESH * 5, async {
            loop {
                self.receive().await;
            }
        })
        .await;
    }

    fn requests(&self) -> Vec<&Element> {
        self.received
            .iter()
            .filter(|e| e.is("message", ns::JABBER_CLIENT))
            .collect()
    }

    async fn stop(self) {
        self.commands.send(Command::Reconnect).unwrap();
        assert!(matches!(
            time::timeout(WAIT, self.worker)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Stopped::Requested
        ));
    }
}

fn paused_worker() -> Worker {
    Worker {
        jid: ACCOUNT.parse().unwrap(),
        directorbot: "directorbot@example.invalid".parse().unwrap(),
        password: SecretString::from("offline-test"),
        pap_refresh: REFRESH,
        pap_auto_refresh: false,
        identity: crate::app::TEST_APP,
        room: None,
        asked_once: Arc::new(AtomicBool::new(false)),
    }
}

const HIVE: &str = "hive@conference.example.invalid";

fn hive_worker() -> Worker {
    Worker {
        room: Some(WatchedRoom::new(HIVE, "pilot").unwrap()),
        ..paused_worker()
    }
}

/// The next room event, failing on anything else the room might have
/// turned into: a fleet ping or a PAP count from room traffic would mean room
/// messages were being read.
async fn next_room_event(session: &mut LocalSession) -> RoomEvent {
    time::timeout(WAIT, async {
        loop {
            match session.events.recv().await.unwrap() {
                Event::Room(event) => return event,
                Event::Connection(_) => {}
                event => panic!("room traffic produced {event:?}"),
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn the_watched_room_is_joined_and_only_its_topic_is_reported() {
    let mut session = LocalSession::connect(hive_worker()).await;
    assert_eq!(next_room_event(&mut session).await, RoomEvent::Joining);
    let join = session
        .until(|e| e.is("presence", ns::JABBER_CLIENT) && e.attr("to").is_some())
        .await;
    assert_eq!(
        join.attr("to"),
        Some("hive@conference.example.invalid/pilot")
    );
    assert!(join.get_child("x", ns::MUC).is_some());

    session.send(&format!("<presence xmlns='jabber:client' from='{HIVE}/pilot'><x xmlns='http://jabber.org/protocol/muc#user'><item affiliation='member' role='participant'/><status code='110'/></x></presence>")).await;
    assert_eq!(next_room_event(&mut session).await, RoomEvent::Joined);

    // Somebody talking in the room is not the status, and is not read.
    session.send(&format!("<message xmlns='jabber:client' type='groupchat' from='{HIVE}/Guardbee'><body>anyone got a spare beacon</body></message>")).await;
    session.send(&format!("<message xmlns='jabber:client' type='groupchat' from='{HIVE}/Guardbee'><subject>Status: GREEN</subject></message>")).await;
    assert_eq!(
        next_room_event(&mut session).await,
        RoomEvent::Subject("Status: GREEN".to_owned())
    );

    // A topic from some other room is not the watched room's.
    session.send("<message xmlns='jabber:client' type='groupchat' from='other@conference.example.invalid/x'><subject>RED</subject></message>").await;
    session.send(&format!("<message xmlns='jabber:client' type='groupchat' from='{HIVE}/Guardbee'><subject>Status: RED</subject></message>")).await;
    assert_eq!(
        next_room_event(&mut session).await,
        RoomEvent::Subject("Status: RED".to_owned())
    );
    session.stop().await;
}

#[tokio::test]
async fn a_refused_join_says_why() {
    for (condition, expected) in [
        ("registration-required", RoomRefusal::MembersOnly),
        ("forbidden", RoomRefusal::MembersOnly),
        ("conflict", RoomRefusal::NicknameTaken),
        ("item-not-found", RoomRefusal::NoSuchRoom),
    ] {
        let mut session = LocalSession::connect(hive_worker()).await;
        assert_eq!(next_room_event(&mut session).await, RoomEvent::Joining);
        session
            .until(|e| e.is("presence", ns::JABBER_CLIENT) && e.attr("to").is_some())
            .await;
        session.send(&format!("<presence xmlns='jabber:client' type='error' from='{HIVE}/pilot'><x xmlns='http://jabber.org/protocol/muc'/><error type='auth'><{condition} xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/></error></presence>")).await;
        match next_room_event(&mut session).await {
            RoomEvent::Refused(reason) => assert_eq!(reason, expected, "{condition}"),
            event => panic!("{condition}: {event:?}"),
        }
        session.stop().await;
    }
}

#[tokio::test]
async fn no_room_is_joined_unless_one_is_configured() {
    let mut session = LocalSession::connect(paused_worker()).await;
    session.drain().await;
    assert!(
        !session
            .received
            .iter()
            .any(|e| e.is("presence", ns::JABBER_CLIENT) && e.attr("to").is_some()),
        "a directed presence means a room was joined"
    );
    session.stop().await;
}

#[tokio::test]
async fn paused_workers_answer_real_version_queries_on_each_connection() {
    {
        let worker = paused_worker();
        // The supervisor rebuilds a client with the same Worker on reconnect.
        for _ in 0..2 {
            let mut session = LocalSession::connect(worker.clone()).await;
            for (id, from) in [("server-version", "example.invalid"), ("bot-version", BOT)] {
                session.send(&format!("<iq xmlns='jabber:client' from='{from}' type='get' id='{id}'><query xmlns='jabber:iq:version'/></iq>")).await;
                let answer = session.until(|e| e.attr("id") == Some(id)).await;
                assert_eq!(answer.attr("type"), Some("result"));
                assert_eq!(answer.attr("to"), Some(from));
                let query = answer.get_child("query", ns::VERSION).unwrap();
                let expected: Element = "<query xmlns='jabber:iq:version'><name>TestApp</name><version>0.0.0</version></query>".parse().unwrap();
                assert_eq!(query, &expected);
            }
            session.drain().await;
            for id in ["server-version", "bot-version"] {
                assert_eq!(
                    session
                        .received
                        .iter()
                        .filter(|e| e.attr("id") == Some(id))
                        .count(),
                    1
                );
            }
            assert!(
                session.requests().is_empty(),
                "startup and timer must stay paused"
            );
            assert!(!worker.asked_once.load(Ordering::Relaxed));
            session.stop().await;
        }
    }
}

#[tokio::test]
async fn paused_worker_sends_one_manual_request_and_accepts_chat_and_normal_replies() {
    for message_type in ["chat", "normal"] {
        let mut session = LocalSession::connect(paused_worker()).await;
        session.drain().await;
        assert!(session.requests().is_empty());

        session.commands.send(Command::RefreshPaps).unwrap();
        let request = session.until(|e| e.is("message", ns::JABBER_CLIENT)).await;
        let mut request = Message::try_from(request).unwrap();
        // Ignore the generated ID when comparing against the captured Smack
        // stanza. The ID itself must be present for error correlation.
        assert!(request.id.is_some());
        request.id = Some(MessageId("offline-pap-test".to_owned()));
        let expected: Element =
            include_str!("../../tests/fixtures/directorbot/smack_pap_request.xml")
                .parse()
                .unwrap();
        assert_eq!(request, Message::try_from(expected).unwrap());

        for _ in 0..10 {
            session.commands.send(Command::RefreshPaps).unwrap();
        }
        session.drain().await;
        assert_eq!(session.requests().len(), 1, "duplicates must be dropped");

        // Matches the arrow-prefixed shape observed from the live bot. XML
        // serialization escapes the arrow, then the client decodes it before
        // handing the body to the production PAP parser.
        let body = include_str!("../../tests/fixtures/directorbot/account_info_arrows.txt");
        let mut reply = Message::new(None);
        reply.from = Some(BOT.parse().unwrap());
        reply.type_ = match message_type {
            "chat" => MessageType::Chat,
            "normal" => MessageType::Normal,
            _ => unreachable!(),
        };
        reply.bodies.insert("da".into(), body.to_owned());
        session.stream.send(&Element::from(reply)).await.unwrap();
        let paps = time::timeout(WAIT, async {
            loop {
                if let Event::Paps(paps) = session.events.recv().await.unwrap() {
                    return paps;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            paps.month,
            PapStats {
                strategic: 12,
                peacetime: 13
            }
        );
        assert_eq!(
            paps.last_30_days,
            PapStats {
                strategic: 20,
                peacetime: 24
            }
        );
        assert_eq!(
            paps.last_90_days,
            PapStats {
                strategic: 55,
                peacetime: 108
            }
        );
        session.drain().await;
        assert_eq!(
            session.requests().len(),
            1,
            "answering must not release a backlog or enable the timer"
        );
        session.stop().await;
    }
}
