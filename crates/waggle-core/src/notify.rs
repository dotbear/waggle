//! What a notification says, decided once for every front end.
//!
//! Builders here turn an intel alert, a fleet ping or a trained skill into a
//! [`Notification`], applying privacy redaction; a [`NotificationSink`] puts
//! it on the desktop the platform's way.

use std::path::PathBuf;

use anyhow::Result;

use crate::{
    config::PrivacyMode,
    skills::{SkillCompletion, SkillQueue},
    store::{IntelAlert, JabberPing},
};

/// The three shapes the build spec defines, chosen by what happened.
///
/// Urgency is not decoration: it decides whether the notification expires on
/// its own, and Omarchy draws the critical shape so that it waits for you.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Urgency {
    /// A job is ready. One line, expires.
    Low,
    /// A fleet ping, or hostile intel with everyone docked. Two lines, expires.
    Medium,
    /// Hostile intel while a pilot is undocked, or a strategic op. Stays until
    /// dismissed.
    High,
}

impl Urgency {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "normal",
            Self::High => "critical",
        }
    }

    /// Zero means "until dismissed". The spec is explicit that critical never
    /// auto-dismisses: a missed hostile is the failure this exists to prevent.
    pub fn expire_ms(self) -> u32 {
        match self {
            Self::Low => 8_000,
            Self::Medium => 15_000,
            Self::High => 0,
        }
    }
}

/// Something a notification can do when it is clicked or one of its buttons
/// is pressed.
///
/// Described rather than spelled as a command line, so the builders stay pure
/// and the tests can say "this opens the intel tab" without knowing where the
/// binary is installed. Each front end decides what an intent does.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Intent {
    /// Open the application on one of its tabs, by the name the tab strip
    /// uses (`"PINGS"`, `"INTEL"`, ...).
    OpenTab(&'static str),
    /// Open the comms link a ping carried.
    JoinComms { ping: u64 },
}

/// A button on a notification.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Action {
    pub label: &'static str,
    pub intent: Intent,
}

/// One delivered desktop notification.
///
/// Notifications are described before they are delivered so tests can assert on
/// exactly what would reach the screen, including whether privacy redaction
/// applied, without needing a notification daemon.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub category: &'static str,
    pub urgency: Urgency,
    /// The mark Omarchy draws in the card's icon slot when there is no image:
    /// a Nerd Font glyph, which follows the theme the way a raster icon cannot.
    pub glyph: &'static str,
    /// A freedesktop icon name, for a notification daemon that is not Omarchy's.
    pub icon: &'static str,
    /// A cached portrait or item icon, when one is on disk. The spec is
    /// explicit that a missing image means firing without one rather than
    /// delaying.
    pub image: Option<PathBuf>,
    /// Notifications sharing a tag replace each other in place rather than
    /// stacking. Only intel uses one, keyed by system: a gate camp produces a
    /// dozen reports about one place, and a dozen cards would bury the other
    /// system that also just lit up. Everything else stacks, and Omarchy
    /// groups the stack under the application.
    pub replace_tag: Option<String>,
    /// What clicking the card does when it has no buttons. A card with buttons
    /// runs the first one on click, which is how Omarchy defines it.
    pub click: Option<Intent>,
    pub actions: Vec<Action>,
    pub redacted: bool,
}

/// Anything that can deliver a notification.
pub trait NotificationSink {
    fn deliver(&self, notification: &Notification) -> Result<()>;

    /// Whether a lock screen covers the desktop right now, for
    /// `PrivacyMode::Locked`. Only the platform can tell; a sink that cannot
    /// says no, and `PrivacyMode::Always` exists for anyone who would rather
    /// not depend on detection.
    fn screen_locked(&self) -> bool {
        false
    }
}

/// Decides what a notification says, then hands it to a sink.
///
/// Privacy is resolved per notification rather than at startup, because
/// `PrivacyMode::Locked` depends on whether the screen is locked right now.
#[derive(Debug)]
pub struct Notifier<S> {
    sink: S,
    privacy: PrivacyMode,
    enabled: bool,
    /// The image cache, so a card can carry the portrait or item icon the
    /// daemon already fetched. Absent means cards go out with glyphs only.
    images: Option<PathBuf>,
}

impl<S: NotificationSink> Notifier<S> {
    pub fn with_sink(sink: S, privacy: PrivacyMode) -> Self {
        Self {
            sink,
            privacy,
            enabled: true,
            images: None,
        }
    }

    /// Whether anything reaches the desktop at all. Off means every
    /// notification is still built and reported delivered, so nothing is
    /// announced twice once it is turned back on.
    pub fn delivering(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The sink notifications are handed to, for a test to inspect.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    pub fn with_images(mut self, root: PathBuf) -> Self {
        self.images = Some(root);
        self
    }

    /// Reported as delivered either way: `never` is the operator saying they
    /// do not want desktop notifications, and re-notifying later because the
    /// daemon did not write it down would be worse than either.
    pub fn deliver(&self, notification: Notification) -> Result<()> {
        if !self.enabled {
            tracing::debug!(
                title = notification.title,
                "Skipping notification; desktop notifications are off"
            );
            return Ok(());
        }
        self.sink.deliver(&notification)
    }

    /// An image from the cache, by the cache-relative path the snapshot uses,
    /// when it is on disk.
    pub fn cached(&self, relative: &str) -> Option<PathBuf> {
        let path = self.images.as_ref()?.join(relative);
        path.is_file().then_some(path)
    }

    pub fn portrait(&self, character_id: u64) -> Option<PathBuf> {
        self.cached(&format!("characters/{character_id}.img"))
    }

    pub fn type_icon(&self, type_id: u32) -> Option<PathBuf> {
        self.cached(&format!("types/{type_id}.img"))
    }

    /// True when message text must be withheld from this notification.
    pub fn must_redact(&self) -> bool {
        match self.privacy {
            PrivacyMode::Off => false,
            PrivacyMode::Always => true,
            PrivacyMode::Locked => self.sink.screen_locked(),
        }
    }

    /// `exposed` is whether a pilot was in space when the report landed, which
    /// is what separates a critical alert from an informational one.
    pub fn intel(&self, alert: &IntelAlert, exposed: bool) -> Result<()> {
        // No portrait: the warning mark is the point of an intel card, and the
        // pilot it is about is named in its first line.
        self.deliver(intel_notification(alert, self.must_redact(), exposed))
    }

    pub fn ping(&self, ping: &JabberPing) -> Result<()> {
        let mut notification = ping_notification(ping, self.must_redact());
        // The FC's face is the fastest way to tell whose fleet this is. A
        // redacted card withholds the FC's name, and a face would give it
        // straight back, so that card goes without.
        if !notification.redacted {
            notification.image = ping
                .fc_portrait
                .as_deref()
                .and_then(|path| self.cached(path));
        }
        self.deliver(notification)
    }

    /// Announces a trained skill level. `queue` is what that character is left
    /// training, so the notification can say what comes next — or that
    /// nothing does, which is the one thing about a finished skill worth
    /// acting on.
    pub fn skill(&self, completion: &SkillCompletion, queue: Option<&SkillQueue>) -> Result<()> {
        // A skill name is not an alliance communication; never redacted.
        let mut notification = skill_notification(completion, queue);
        notification.image = self.portrait(completion.character_id);
        self.deliver(notification)
    }

    /// Announces several levels that finished together as one notification.
    pub fn skill_group(
        &self,
        completions: &[SkillCompletion],
        queue_for: impl Fn(u64) -> Option<SkillQueue>,
    ) -> Result<()> {
        match completions {
            [] => Ok(()),
            [single] => self.skill(single, queue_for(single.character_id).as_ref()),
            many => self.deliver(grouped_skill_notification(many, queue_for)),
        }
    }
}

/// Collects notifications instead of delivering them, for tests.
#[derive(Debug, Default)]
pub struct RecordingSink {
    delivered: std::sync::Mutex<Vec<Notification>>,
}

impl RecordingSink {
    pub fn delivered(&self) -> Vec<Notification> {
        self.delivered.lock().expect("sink is not poisoned").clone()
    }
}

impl NotificationSink for RecordingSink {
    fn deliver(&self, notification: &Notification) -> Result<()> {
        self.delivered
            .lock()
            .expect("sink is not poisoned")
            .push(notification.clone());
        Ok(())
    }
}

impl<T: NotificationSink> NotificationSink for &T {
    fn deliver(&self, notification: &Notification) -> Result<()> {
        (*self).deliver(notification)
    }

    fn screen_locked(&self) -> bool {
        (*self).screen_locked()
    }
}

// Nerd Font Material Design marks, drawn by Omarchy in the card's icon slot
// when there is no picture.
/// md-alert
pub const GLYPH_INTEL: &str = "\u{F0026}";
/// md-bullhorn
pub const GLYPH_PING: &str = "\u{F00E6}";
/// md-factory
pub const GLYPH_INDUSTRY: &str = "\u{F020F}";
/// md-school
pub const GLYPH_SKILL: &str = "\u{F0474}";

pub fn intel_notification(alert: &IntelAlert, redacted: bool, exposed: bool) -> Notification {
    let jump_label = if alert.distance == 1 { "jump" } else { "jumps" };
    let title = format!(
        "Intel: {} · {} {jump_label}",
        alert.system_name, alert.distance
    );
    let stale = if alert.stale_presence {
        " · stale location"
    } else {
        ""
    };
    // Hostiles generally cannot use your bridges, so saying how the report got
    // into range is the difference between "they are on top of you" and "they
    // are next to your infrastructure".
    let bridge = if alert.via_bridge {
        " · via bridge"
    } else {
        ""
    };
    let body = if redacted {
        format!(
            "Near {}{stale} · unlock to read the report",
            alert.character_name
        )
    } else {
        format!(
            "Near {}{stale}{bridge}\n{}: {}\n{}",
            alert.character_name,
            alert.author,
            truncate(&alert.message, 240),
            alert.channel
        )
    };

    Notification {
        title,
        body,
        category: "im.received",
        // A hostile report while a pilot is in space is the one thing that
        // must not disappear on a timer. With everyone docked it is news, not
        // an emergency.
        urgency: if exposed {
            Urgency::High
        } else {
            Urgency::Medium
        },
        glyph: GLYPH_INTEL,
        icon: "dialog-warning",
        image: None,
        // One card per system, updated as reports come in.
        replace_tag: Some(format!("intel:{}", alert.system_id)),
        click: Some(Intent::OpenTab("INTEL")),
        actions: Vec::new(),
        redacted,
    }
}

/// What a character is left training once a level has finished, in one line.
pub fn queue_line(queue: Option<&SkillQueue>) -> String {
    match queue {
        None => String::new(),
        Some(queue) if queue.is_empty() => "Skill queue is empty".to_owned(),
        Some(queue) if queue.paused => "Skill queue is paused".to_owned(),
        Some(queue) => match &queue.training {
            Some(next) => format!(
                "Now training {}{}",
                next.display_name(),
                match queue.remaining {
                    0 | 1 => String::new(),
                    more => format!(" · {} more queued", more - 1),
                }
            ),
            None => String::new(),
        },
    }
}

pub fn skill_notification(
    completion: &SkillCompletion,
    queue: Option<&SkillQueue>,
) -> Notification {
    let queue_line = queue_line(queue);
    // An empty queue wastes training time from this moment on, so it is worth
    // the medium shape: it stays up longer and reads as something to do.
    let urgency = if queue.is_some_and(SkillQueue::is_empty) {
        Urgency::Medium
    } else {
        Urgency::Low
    };
    let body = if queue_line.is_empty() {
        completion.character_name.clone()
    } else {
        format!("{}\n{queue_line}", completion.character_name)
    };
    Notification {
        title: format!("Skill trained: {}", completion.display_name()),
        body,
        category: "transfer.complete",
        urgency,
        glyph: GLYPH_SKILL,
        icon: "emblem-default",
        image: None,
        replace_tag: None,
        click: Some(Intent::OpenTab("PILOTS")),
        actions: Vec::new(),
        redacted: false,
    }
}

/// One notification for several levels that finished together, which only
/// happens when the daemon was not running as they did.
pub fn grouped_skill_notification(
    completions: &[SkillCompletion],
    queue_for: impl Fn(u64) -> Option<SkillQueue>,
) -> Notification {
    let mut lines: Vec<String> = completions
        .iter()
        .map(|completion| {
            format!(
                "{} · {}",
                completion.display_name(),
                completion.character_name
            )
        })
        .collect();
    let mut pilots: Vec<u64> = completions.iter().map(|c| c.character_id).collect();
    pilots.sort_unstable();
    pilots.dedup();
    let mut empty = false;
    for pilot in pilots {
        if queue_for(pilot).is_some_and(|queue| queue.is_empty()) {
            empty = true;
            let name = completions
                .iter()
                .find(|c| c.character_id == pilot)
                .map_or("", |c| c.character_name.as_str());
            lines.push(format!("{name}'s skill queue is empty"));
        }
    }
    Notification {
        title: format!("{} skills trained", completions.len()),
        body: lines.join("\n"),
        category: "transfer.complete",
        urgency: if empty { Urgency::Medium } else { Urgency::Low },
        glyph: GLYPH_SKILL,
        icon: "emblem-default",
        image: None,
        replace_tag: None,
        click: Some(Intent::OpenTab("PILOTS")),
        actions: Vec::new(),
        redacted: false,
    }
}

pub fn ping_notification(ping: &JabberPing, redacted: bool) -> Notification {
    let mut details = Vec::new();
    if let Some(fc) = &ping.fc {
        details.push(format!("FC {fc}"));
    }
    if let Some(formup) = &ping.formup {
        details.push(format!("Formup {formup}"));
    }
    if let Some(pap_type) = &ping.pap_type {
        details.push(pap_type.clone());
    }
    // What to undock in and where to listen: the two things the old plugin
    // card carried that a title and a body would otherwise leave to the text.
    if let Some(doctrine) = &ping.doctrine {
        details.push(format!("Flying {doctrine}"));
    }
    if let Some(comms) = &ping.comms {
        details.push(format!("Comms {comms}"));
    }
    let context = details.join(" · ");

    // A redacted ping still says a ping arrived and what kind, because that is
    // the part that needs a decision; only the broadcast text is withheld.
    // Which fleet of a several-fleet broadcast this is. Each fleet is its own
    // card, and they share the broadcast's opening line.
    let of_fleets = if ping.fleet_count > 1 {
        format!(" ({}/{})", ping.fleet_index, ping.fleet_count)
    } else {
        String::new()
    };
    // A broadcast that is only words has no fleet in it, and its first line is
    // often the whole paragraph: as a title that is a wall cut off mid-word.
    let text_only = context.is_empty();
    let (title, body) = if redacted {
        (
            format!("Fleet ping received{of_fleets}"),
            ping.pap_type.clone().map_or_else(
                || "Unlock to read the broadcast".to_owned(),
                |pap_type| format!("{pap_type} · unlock to read the broadcast"),
            ),
        )
    } else {
        let body = if text_only {
            truncate(&ping.description, 360)
        } else if ping.description.is_empty() {
            context
        } else {
            format!("{context}\n{}", truncate(&ping.description, 360))
        };
        let title = if text_only {
            ping.sender.as_deref().map_or_else(
                || "Broadcast".to_owned(),
                |sender| format!("Broadcast from {sender}"),
            )
        } else {
            format!("Fleet ping{of_fleets}: {}", ping.summary)
        };
        (title, body)
    };

    // A strategic op is the one ping worth interrupting for, so it stays up
    // until it is answered. Anything else is time-critical but not a threat,
    // so it expires.
    let strategic = ping
        .pap_type
        .as_deref()
        .is_some_and(|pap_type| pap_type.trim().to_ascii_lowercase().starts_with("strat"));

    // Joining comms is the thing a ping asks of you, so when there is a link
    // it is the first button and the card's click.
    let actions = if ping.comms_link.is_some() {
        vec![
            Action {
                label: "Join comms",
                intent: Intent::JoinComms { ping: ping.id },
            },
            Action {
                label: "Open pings",
                intent: Intent::OpenTab("PINGS"),
            },
        ]
    } else {
        Vec::new()
    };

    Notification {
        title,
        body,
        category: "im.received",
        urgency: if strategic {
            Urgency::High
        } else {
            Urgency::Medium
        },
        glyph: GLYPH_PING,
        icon: "mail-unread-new",
        image: None,
        // Every ping is its own card: a second broadcast replacing the first
        // is how a strategic op goes unread.
        replace_tag: None,
        click: Some(Intent::OpenTab("PINGS")),
        actions,
        redacted,
    }
}

pub fn truncate(value: &str, max_characters: usize) -> String {
    let mut characters = value.chars();
    let shortened = characters.by_ref().take(max_characters).collect::<String>();
    if characters.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert() -> IntelAlert {
        IntelAlert {
            id: 1,
            occurred_at: "2026-09-03T09:00:00Z".parse().unwrap(),
            channel: "example.intel".to_owned(),
            author: "Scout".to_owned(),
            message: "Q4C-S5 Sabre on gate".to_owned(),
            system_id: 30_000_715,
            system_name: "Q4C-S5".to_owned(),
            distance: 1,
            character_id: 42,
            character_name: "Example Pilot".to_owned(),
            stale_presence: false,
            via_bridge: false,
        }
    }

    #[test]
    fn notification_content_contains_the_actionable_context() {
        let Notification { title, body, .. } = intel_notification(&alert(), false, true);
        assert_eq!(title, "Intel: Q4C-S5 · 1 jump");
        assert!(body.contains("Near Example Pilot"));
        assert!(body.contains("Scout: Q4C-S5 Sabre on gate"));
        assert!(body.contains("example.intel"));
    }

    #[test]
    fn privacy_withholds_the_report_text_but_keeps_the_decision() {
        let sink = RecordingSink::default();
        let notifier = Notifier::with_sink(&sink, PrivacyMode::Always);

        notifier.intel(&alert(), true).unwrap();
        let delivered = sink.delivered();
        let intel = &delivered[0];

        assert!(intel.redacted);
        // The actionable part survives: which system, how far, which character.
        assert_eq!(intel.title, "Intel: Q4C-S5 · 1 jump");
        assert!(intel.body.contains("Near Example Pilot"));
        // The alliance communication itself does not.
        assert!(!intel.body.contains("Q4C-S5 Sabre on gate"));
        assert!(!intel.body.contains("Scout"));
    }

    #[test]
    fn privacy_withholds_a_ping_body_but_keeps_the_pap_type() {
        let ping = JabberPing {
            id: 1,
            occurred_at: "2026-09-03T09:00:00Z".parse().unwrap(),
            received_at: "2026-09-03T09:00:00Z".parse().unwrap(),
            from: "directorbot@example.com".to_owned(),
            source: None,
            sender: None,
            target: None,
            summary: "Important Fleet".to_owned(),
            description: "Form up and bring tackle".to_owned(),
            fc: Some("Space Boss".to_owned()),
            fleet: None,
            formup: Some("Example Keepstar".to_owned()),
            pap_type: Some("Strategic".to_owned()),
            comms: None,
            comms_link: None,
            comms_target: None,
            doctrine: None,
            body: "Form up and bring tackle".to_owned(),
            fc_portrait: None,
            muted: false,
            fleet_index: 1,
            fleet_count: 1,
        };

        let redacted = ping_notification(&ping, true);
        assert!(redacted.redacted);
        assert!(redacted.body.contains("Strategic"));
        assert!(!redacted.body.contains("tackle"));
        assert!(!redacted.title.contains("Important Fleet"));
        assert!(!redacted.body.contains("Space Boss"));

        let plain = ping_notification(&ping, false);
        assert!(!plain.redacted);
        assert!(plain.title.contains("Important Fleet"));
        assert!(plain.body.contains("tackle"));
    }

    #[test]
    fn privacy_off_delivers_the_full_report() {
        let sink = RecordingSink::default();
        let notifier = Notifier::with_sink(&sink, PrivacyMode::Off);

        notifier.intel(&alert(), true).unwrap();

        assert!(sink.delivered()[0].body.contains("Q4C-S5 Sabre on gate"));
    }

    #[test]
    fn truncation_respects_unicode_boundaries() {
        assert_eq!(truncate("åäö", 2), "åä…");
        assert_eq!(truncate("åä", 2), "åä");
    }

    fn completion(skill: &str, level: u8) -> SkillCompletion {
        SkillCompletion {
            character_id: 42,
            character_name: "Example Pilot".to_owned(),
            skill_id: 3330,
            skill_name: skill.to_owned(),
            level,
            finished_at: "2026-09-07T10:00:00Z".parse().unwrap(),
            notified: false,
        }
    }

    fn training(skill: &str, level: u8, remaining: u32) -> SkillQueue {
        SkillQueue {
            training: Some(crate::skills::SkillQueueEntry {
                character_id: 42,
                character_name: "Example Pilot".to_owned(),
                skill_id: 3331,
                skill_name: skill.to_owned(),
                level,
                position: 1,
                start_at: Some("2026-09-07T10:00:00Z".parse().unwrap()),
                finish_at: Some("2026-09-09T10:00:00Z".parse().unwrap()),
                level_start_sp: None,
                level_end_sp: None,
                training_start_sp: None,
            }),
            next: Vec::new(),
            remaining,
            ends_at: Some("2026-09-09T10:00:00Z".parse().unwrap()),
            paused: false,
        }
    }

    fn skill_notifier() -> Notifier<RecordingSink> {
        Notifier::with_sink(RecordingSink::default(), PrivacyMode::Off)
    }

    #[test]
    fn a_trained_skill_says_what_trains_next() {
        let notifier = skill_notifier();
        notifier
            .skill(
                &completion("Caldari Frigate", 5),
                Some(&training("Caldari Destroyer", 4, 3)),
            )
            .unwrap();

        let delivered = notifier.sink.delivered().remove(0);
        assert_eq!(delivered.title, "Skill trained: Caldari Frigate V");
        assert!(delivered.body.contains("Example Pilot"));
        assert!(delivered.body.contains("Now training Caldari Destroyer IV"));
        assert!(delivered.body.contains("2 more queued"));
        assert_eq!(delivered.urgency, Urgency::Low);
        assert!(!delivered.redacted);
    }

    #[test]
    fn an_emptied_skill_queue_is_the_louder_shape() {
        let notifier = skill_notifier();
        notifier
            .skill(&completion("Cybernetics", 5), Some(&SkillQueue::default()))
            .unwrap();

        let delivered = notifier.sink.delivered().remove(0);
        assert!(delivered.body.contains("Skill queue is empty"));
        assert_eq!(delivered.urgency, Urgency::Medium);
    }

    #[test]
    fn several_trained_skills_are_one_notification() {
        let notifier = skill_notifier();
        notifier
            .skill_group(
                &[completion("Cybernetics", 4), completion("Cybernetics", 5)],
                |_| Some(training("Caldari Frigate", 5, 1)),
            )
            .unwrap();

        let delivered = notifier.sink.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].title, "2 skills trained");
        assert!(delivered[0].body.contains("Cybernetics IV · Example Pilot"));
        assert!(delivered[0].body.contains("Cybernetics V · Example Pilot"));
    }
}
