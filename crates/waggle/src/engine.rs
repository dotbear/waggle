//! The background work: chat logs, intel, ESI, Directorbot, mining.
//!
//! Runs on a Tokio runtime on its own thread and owns every piece of state,
//! the way EVEmarchy's daemon loop does, so nothing is shared behind locks.
//! After anything visible changes it publishes a fresh [`ViewModel`] to the
//! window, and it takes [`Command`]s from the window and the tray.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use chrono::{TimeDelta, Utc};
use tokio::{sync::mpsc, time};
use tracing::{debug, info, warn};
use waggle_core::{
    alert::{AlertPolicy, persisted_alert},
    app::AppIdentity,
    bridges::JumpBridges,
    esi::{
        CharacterPresence, EsiService, IndustryPolling, LOCATION_SCOPE, ONLINE_SCOPE, TokenStore,
    },
    intel::Parser as IntelParser,
    jabber::{self, ConnectionStatus},
    logs::{self, ChatLogMonitor},
    mining::MiningService,
    notify::Notifier,
    secrets::JabberPasswordStore,
    skills::{SkillQueue, SkillQueueEntry},
    store::Store,
    universe::Universe,
};

use crate::{config::Config, paths::AppPaths, toast::Desktop, view::ViewModel};

/// How Waggle names itself to EVE's servers and to Jabber, and which scopes a
/// character must grant. Ship, skills and the skill queue are asked for at
/// login too, but a character without them still works.
pub const APP: AppIdentity = AppIdentity {
    name: "Waggle",
    version: env!("CARGO_PKG_VERSION"),
    website: "https://github.com/dotbear/waggle",
    required_scopes: &[ONLINE_SCOPE, LOCATION_SCOPE],
    relink: |character| format!("Add {character} again"),
};

/// What the window or the tray asks the engine to do.
#[derive(Debug)]
pub enum Command {
    RefreshPaps,
    /// The linked characters changed; pick them up now rather than at the
    /// next periodic check.
    SyncCharacters,
}

/// The keyring this platform has.
pub fn secrets() -> Arc<dyn PlatformSecrets> {
    #[cfg(windows)]
    {
        Arc::new(waggle_core::secrets::CredentialManager::new("waggle"))
    }
    #[cfg(not(windows))]
    {
        Arc::new(waggle_core::secrets::SecretTool::new("waggle", "Waggle"))
    }
}

/// Both kinds of secret, from one store.
pub trait PlatformSecrets:
    waggle_core::secrets::RefreshTokenStore + JabberPasswordStore + Send + Sync
{
}

impl<T> PlatformSecrets for T where
    T: waggle_core::secrets::RefreshTokenStore + JabberPasswordStore + Send + Sync
{
}

/// Starts the engine on its own thread. `publish` is called with every new
/// view, from that thread.
pub fn spawn(
    paths: AppPaths,
    config: Config,
    publish: impl Fn(ViewModel) + Send + 'static,
) -> mpsc::UnboundedSender<Command> {
    let (commands, receiver) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("waggle-engine".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "Could not start the engine runtime");
                    return;
                }
            };
            if let Err(error) = runtime.block_on(run(paths, config, receiver, &publish)) {
                let message = format!("{error:#}");
                tracing::error!(error = message, "The engine stopped");
                // Say so in the window rather than leaving it quietly stale.
                publish(ViewModel {
                    now: Utc::now(),
                    setup_hint: format!(
                        "Waggle stopped working: {message}. Restart it from the tray; the log folder is listed in setup."
                    ),
                    jabber_text: "Stopped".to_owned(),
                    ..ViewModel::default()
                });
            }
        })
        .expect("the engine thread should start");
    commands
}

async fn run(
    paths: AppPaths,
    config: Config,
    mut commands: mpsc::UnboundedReceiver<Command>,
    publish: &impl Fn(ViewModel),
) -> Result<()> {
    let store = Store::open(&paths.database_file)?;
    let roots = logs::discover_roots(&config.logs, &paths.home_dir);
    let mut universe = Universe::bundled()?;
    load_jump_bridges(&mut universe, &paths, &config)?;
    let intel_parser = IntelParser::new(&universe);
    let secrets = secrets();
    let token_store: TokenStore = Arc::new(TokenStoreAdapter(Arc::clone(&secrets)));

    let mut presence = presence_service(&config, store.linked_characters()?, &token_store)?;
    let mut presence_state = presence.characters();
    name_presence_systems(&universe, &mut presence_state);
    attach_skill_queues(&store, &mut presence_state);

    let jabber_configured = config.jabber.jid.is_some() && config.jabber.directorbot.is_some();
    let jabber_password = match config.jabber.jid.as_deref() {
        Some(jid) => secrets.lookup_jabber_password(jid).unwrap_or_else(|error| {
            warn!(%error, "Could not read the Jabber password");
            None
        }),
        None => None,
    };
    let jabber_credential = jabber_password.is_some();
    let mut jabber_connection: Option<ConnectionStatus> = None;
    let (mut jabber_events, jabber_handle) = match (jabber_configured, jabber_password) {
        (true, Some(password)) => {
            let (events, handle) =
                jabber::start(APP, &config.jabber, &config.paps, None, password)?;
            jabber_connection = Some(ConnectionStatus::Connecting);
            (Some(events), Some(handle))
        }
        _ => (None, None),
    };

    let notifier = Notifier::with_sink(Desktop::detect(), config.notifications.privacy)
        .delivering(config.notifications.enabled);
    let mut alert_policy = AlertPolicy::new();
    let mut mining = MiningService::restore(&store, roots.clone(), Utc::now().timestamp())?;
    let mut log_monitor = ChatLogMonitor::new(roots.clone(), Duration::from_secs(5));
    match store.log_checkpoints() {
        Ok(checkpoints) => log_monitor.restore_checkpoints(checkpoints),
        Err(error) => warn!(%error, "Could not load chat-log checkpoints"),
    }
    let initial = log_monitor.initialize();
    for warning in initial.warnings {
        warn!(warning, "Chat-log monitor warning");
    }
    let mut seen_channels: BTreeSet<String> = log_monitor.active_channels().into_iter().collect();

    info!(
        log_roots = roots.len(),
        characters = presence.linked_character_count(),
        jabber = jabber_configured,
        "Waggle engine started"
    );

    let view = |presence_state: &[CharacterPresence],
                jabber_connection: &Option<ConnectionStatus>,
                mining: &MiningService,
                seen_channels: &BTreeSet<String>|
     -> Result<ViewModel> {
        let now = Utc::now();
        let mut pings = store.recent_jabber_pings(30)?;
        for ping in &mut pings {
            ping.muted = config.jabber.target_is_muted(ping.target.as_deref());
        }
        let (jabber_text, jabber_ok) =
            match (jabber_configured, jabber_credential, jabber_connection) {
                (false, _, _) => ("Jabber not set up".to_owned(), false),
                (true, false, _) => ("Jabber password missing".to_owned(), false),
                (_, _, Some(ConnectionStatus::Connected)) => {
                    ("Directorbot connected".to_owned(), true)
                }
                (_, _, Some(ConnectionStatus::Disconnected(reason))) => {
                    (format!("Jabber: {reason}"), false)
                }
                _ => ("Connecting to Jabber…".to_owned(), false),
            };
        let matched = config
            .intel
            .channels
            .keys()
            .filter(|channel| seen_channels.contains(*channel))
            .count();
        Ok(ViewModel {
            now,
            paps: store.latest_paps()?,
            jabber_text,
            jabber_ok,
            setup_hint: setup_hint(&config, &roots, presence_state, jabber_configured),
            status: format!(
                "{matched} of {} intel channels · {} pilot{}",
                config.intel.channels.len(),
                presence_state.len(),
                if presence_state.len() == 1 { "" } else { "s" }
            ),
            intel: store.recent_intel_alerts(30)?,
            pings,
            pilots: presence_state.to_vec(),
            mining: mining.view(&store, now.timestamp()).ok(),
        })
    };
    publish(view(
        &presence_state,
        &jabber_connection,
        &mining,
        &seen_channels,
    )?);

    let mut log_poll = interval(Duration::from_millis(500));
    let mut presence_poll = interval(Duration::from_secs(1));
    let mut mining_tick = interval(Duration::from_secs(1));
    let mut heartbeat = interval(Duration::from_secs(15));
    let mut character_sync = interval(Duration::from_secs(15));

    loop {
        let mut changed = false;
        tokio::select! {
            _ = log_poll.tick() => {
                let polled = log_monitor.poll();
                for warning in polled.warnings {
                    warn!(warning, "Chat-log monitor warning");
                }
                for event in &polled.events {
                    let Some(region_hints) = config.intel.channels.get(&event.channel) else {
                        continue;
                    };
                    if event.message.author == "EVE System" {
                        continue;
                    }
                    let understanding = intel_parser.parse(&event.message.text, region_hints);
                    match alert_policy.evaluate(
                        Utc::now(),
                        event.message.timestamp,
                        &understanding,
                        &presence_state,
                        &universe,
                        &config.intel,
                    ) {
                        Ok(matched) => {
                            let pending = persisted_alert(event, &matched);
                            if let Some(alert) = store.record_intel_alert(&pending)? {
                                alert_policy.acknowledge(Utc::now(), &matched);
                                let exposed = presence_state.iter().any(|character| {
                                    character.character_id == alert.character_id
                                        && character.docked == Some(false)
                                });
                                match notifier.intel(&alert, exposed) {
                                    Ok(()) => store.mark_intel_notified(alert.id)?,
                                    Err(error) => warn!(%error, "Could not show the intel notification"),
                                }
                                info!(system = alert.system_name, jumps = alert.distance, "Intel alert");
                                changed = true;
                            }
                        }
                        Err(reason) => debug!(?reason, "Intel report did not alert"),
                    }
                }
                for channel in log_monitor.active_channels() {
                    changed |= seen_channels.insert(channel);
                }
            }
            _ = presence_poll.tick() => {
                let polled = presence.poll().await;
                for warning in &polled.warnings {
                    warn!(warning, "ESI warning");
                }
                for (character_id, entries) in &polled.skill_queues_refreshed {
                    let mut entries = entries.clone();
                    name_skills(&universe, &mut entries);
                    store.reconcile_skill_queue(*character_id, &entries)?;
                }
                if !polled.skill_queues_refreshed.is_empty() {
                    deliver_due_skill_notifications(&store, &notifier)?;
                }
                if polled.changed || !polled.skill_queues_refreshed.is_empty() {
                    let mut fresh = polled.characters;
                    name_presence_systems(&universe, &mut fresh);
                    attach_skill_queues(&store, &mut fresh);
                    // The alert policy needs every fresh observation; the
                    // window only needs one when something it shows moved.
                    changed |= !waggle_core::esi::presence_unchanged(&presence_state, &fresh)
                        || !polled.skill_queues_refreshed.is_empty();
                    presence_state = fresh;
                }
            }
            _ = mining_tick.tick() => {
                let now = Utc::now().timestamp();
                for character in &presence_state {
                    let ship = character.ship.as_ref();
                    mining.set_ship(
                        &character.character_id.to_string(),
                        ship.map(|ship| ship.item_id),
                        ship.and_then(|ship| ship.mining_hold.as_ref()).map(|hold| hold.capacity_m3),
                    );
                }
                match mining.poll(&store, now) {
                    Ok(updated) => {
                        let active = mining
                            .view(&store, now)
                            .ok()
                            .and_then(|view| view.session)
                            .is_some_and(|session| session.end.is_none());
                        changed |= updated || active;
                    }
                    Err(error) => warn!(%error, "Mining poll failed"),
                }
            }
            _ = heartbeat.tick() => {
                store.prune_jabber_pings_before(Utc::now() - TimeDelta::hours(48))?;
                persist_log_checkpoints(&store, &log_monitor);
                deliver_due_skill_notifications(&store, &notifier)?;
                // Ages and skill timers move with the clock.
                attach_skill_queues(&store, &mut presence_state);
                changed = true;
            }
            _ = character_sync.tick() => {
                let linked = store.linked_characters()?;
                if !presence.matches_linked_characters(&linked) {
                    presence = presence_service(&config, linked, &token_store)?;
                    presence_state = presence.characters();
                    name_presence_systems(&universe, &mut presence_state);
                    attach_skill_queues(&store, &mut presence_state);
                    info!(characters = presence.linked_character_count(), "Reloaded linked characters");
                    changed = true;
                }
            }
            event = next_jabber_event(&mut jabber_events) => {
                match event {
                    Some(jabber::Event::Connection(status)) => {
                        if matches!(status, ConnectionStatus::Connected) {
                            info!("Connected to Directorbot");
                        }
                        jabber_connection = Some(status);
                        changed = true;
                    }
                    Some(jabber::Event::Ping(ping)) => {
                        // Notifies unless already seen before a restart, or
                        // replayed from well outside the freshness window.
                        let age = ping.received_at.signed_duration_since(ping.occurred_at);
                        let already_seen = store
                            .latest_jabber_ping_at()?
                            .is_some_and(|high_water| ping.occurred_at <= high_water);
                        let baseline = already_seen
                            || age > TimeDelta::seconds(
                                i64::try_from(config.jabber.freshness_seconds).unwrap_or(i64::MAX),
                            );
                        let muted = config.jabber.target_is_muted(ping.target.as_deref());
                        if let Some(stored) = store.record_jabber_ping(&ping, baseline || muted)? {
                            if !baseline && !muted {
                                match notifier.ping(&stored) {
                                    Ok(()) => store.mark_jabber_ping_notified(stored.id)?,
                                    Err(error) => warn!(%error, "Could not show the ping notification"),
                                }
                            }
                            changed = true;
                        }
                    }
                    Some(jabber::Event::Paps(paps)) => {
                        store.record_paps(&paps)?;
                        info!("Updated PAP counts");
                        changed = true;
                    }
                    // Waggle watches no rooms.
                    Some(jabber::Event::Room(_)) => {}
                    None => {
                        jabber_events = None;
                        jabber_connection = Some(ConnectionStatus::Disconnected(
                            "the Jabber worker stopped".to_owned(),
                        ));
                        changed = true;
                    }
                }
            }
            command = commands.recv() => {
                match command {
                    Some(Command::RefreshPaps) => {
                        if let Some(handle) = &jabber_handle
                            && let Err(error) = handle.send(jabber::Command::RefreshPaps)
                        {
                            warn!(%error, "Could not ask Directorbot for PAPs");
                        }
                    }
                    Some(Command::SyncCharacters) => character_sync.reset_immediately(),
                    None => {
                        persist_log_checkpoints(&store, &log_monitor);
                        info!("Engine stopping");
                        return Ok(());
                    }
                }
            }
        }
        if changed {
            publish(view(
                &presence_state,
                &jabber_connection,
                &mining,
                &seen_channels,
            )?);
        }
    }
}

fn interval(period: Duration) -> time::Interval {
    let mut interval = time::interval(period);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    interval
}

/// What to set up next, in one sentence, or nothing once it all works.
fn setup_hint(
    config: &Config,
    roots: &[std::path::PathBuf],
    pilots: &[CharacterPresence],
    jabber_configured: bool,
) -> String {
    if roots.is_empty() {
        "EVE's log folder was not found. Has EVE run on this PC yet?".to_owned()
    } else if pilots.is_empty() {
        "Add a character so intel can be measured from where it is.".to_owned()
    } else if config.intel.channels.is_empty() {
        "Choose your intel channels.".to_owned()
    } else if !jabber_configured {
        "Set up Jabber to get Directorbot pings and PAP counts.".to_owned()
    } else {
        String::new()
    }
}

fn presence_service(
    config: &Config,
    characters: Vec<waggle_core::store::LinkedCharacter>,
    tokens: &TokenStore,
) -> Result<EsiService> {
    EsiService::new(
        APP,
        Some(config.esi.client_id.clone()),
        IndustryPolling::default(),
        characters,
        Arc::clone(tokens),
    )
    .context("could not start the ESI poller")
}

/// Lends the platform keyring to the ESI poller, which only needs tokens.
struct TokenStoreAdapter(Arc<dyn PlatformSecrets>);

impl waggle_core::secrets::RefreshTokenStore for TokenStoreAdapter {
    fn store(
        &self,
        character_id: u64,
        character_name: &str,
        token: &secrecy::SecretString,
    ) -> Result<()> {
        self.0.store(character_id, character_name, token)
    }

    fn lookup(&self, character_id: u64) -> Result<Option<secrecy::SecretString>> {
        self.0.lookup(character_id)
    }

    fn delete(&self, character_id: u64) -> Result<()> {
        self.0.delete(character_id)
    }
}

async fn next_jabber_event(
    events: &mut Option<mpsc::UnboundedReceiver<jabber::Event>>,
) -> Option<jabber::Event> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

fn load_jump_bridges(universe: &mut Universe, paths: &AppPaths, config: &Config) -> Result<()> {
    if !config.intel.use_jump_bridges {
        return Ok(());
    }
    let bridges = JumpBridges::load(&paths.jump_bridges_file)?;
    for warning in universe.set_jump_bridges(&bridges.links) {
        warn!(warning, "Jump bridge warning");
    }
    Ok(())
}

fn name_presence_systems(universe: &Universe, presence: &mut [CharacterPresence]) {
    for character in presence {
        let system = character.solar_system_id.and_then(|id| universe.system(id));
        character.solar_system_name = system.map(|system| system.name.clone());
        character.region_name = system.map(|system| system.region_name.clone());
        if let Some(ship) = &mut character.ship {
            ship.type_name = universe.item_type_name(ship.type_id).map(str::to_owned);
        }
    }
}

/// What each character is training, from the stored queue, so it shows before
/// ESI answers and moves with the clock.
fn attach_skill_queues(store: &Store, presence: &mut [CharacterPresence]) {
    let now = Utc::now();
    for character in presence {
        character.skill_queue = match store.skill_queue_known(character.character_id) {
            Ok(true) => store
                .skill_queue(character.character_id)
                .ok()
                .map(|entries| SkillQueue::summarize(&entries, now)),
            _ => None,
        };
    }
}

fn name_skills(universe: &Universe, entries: &mut [SkillQueueEntry]) {
    for entry in entries {
        if let Some(name) = universe.item_type_name(entry.skill_id) {
            entry.skill_name = name.to_owned();
        }
    }
}

fn deliver_due_skill_notifications(store: &Store, notifier: &Notifier<Desktop>) -> Result<()> {
    let due = store.due_skill_notifications(Utc::now())?;
    if due.is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let queue_for = |character_id: u64| {
        store
            .skill_queue(character_id)
            .ok()
            .map(|entries| SkillQueue::summarize(&entries, now))
    };
    match notifier.skill_group(&due, queue_for) {
        Ok(()) => {
            for completion in &due {
                store.mark_skill_notified(
                    completion.character_id,
                    completion.skill_id,
                    completion.level,
                )?;
            }
        }
        Err(error) => warn!(%error, "Could not show the skill notification"),
    }
    Ok(())
}

fn persist_log_checkpoints(store: &Store, log_monitor: &ChatLogMonitor) {
    let checkpoints = log_monitor.checkpoints();
    if checkpoints.is_empty() {
        return;
    }
    if let Err(error) = store.save_log_checkpoints(&checkpoints) {
        warn!(%error, "Could not save chat-log checkpoints");
    }
}
