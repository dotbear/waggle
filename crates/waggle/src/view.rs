//! What the window shows, and how it is drawn from the engine's state.
//!
//! The engine builds a [`ViewModel`] from plain core types on its own thread;
//! [`apply`] runs on the UI thread and turns it into Slint models.

use std::rc::Rc;

use chrono::{DateTime, Utc};
use slint::{ModelRc, SharedString, VecModel};
use waggle_core::{
    directorbot::PapSnapshot,
    esi::CharacterPresence,
    mining,
    store::{IntelAlert, JabberPing},
};

use crate::{AppWindow, IntelRow, MiningInfo, OreRow, PilotRow, PingRow};

/// Everything the window shows, built by the engine.
#[derive(Debug, Clone, Default)]
pub struct ViewModel {
    pub now: DateTime<Utc>,
    pub paps: Option<PapSnapshot>,
    pub jabber_text: String,
    pub jabber_ok: bool,
    pub setup_hint: String,
    pub status: String,
    pub intel: Vec<IntelAlert>,
    pub pings: Vec<JabberPing>,
    pub pilots: Vec<CharacterPresence>,
    pub mining: Option<mining::View>,
}

impl ViewModel {
    /// One line for the tray icon's tooltip.
    pub fn tooltip(&self) -> String {
        match &self.paps {
            Some(paps) => format!(
                "Waggle · PAPs this month {}",
                pap_pair(paps.month.strategic, paps.month.peacetime)
            ),
            None => "Waggle".to_owned(),
        }
    }
}

pub fn apply(window: &AppWindow, view: &ViewModel) {
    let (month, thirty, ninety) = match &view.paps {
        Some(paps) => (
            pap_pair(paps.month.strategic, paps.month.peacetime),
            pap_pair(paps.last_30_days.strategic, paps.last_30_days.peacetime),
            pap_pair(paps.last_90_days.strategic, paps.last_90_days.peacetime),
        ),
        None => ("–".to_owned(), "–".to_owned(), "–".to_owned()),
    };
    window.set_paps_month(month.into());
    window.set_paps_30(thirty.into());
    window.set_paps_90(ninety.into());
    window.set_jabber_text(view.jabber_text.clone().into());
    window.set_jabber_ok(view.jabber_ok);
    window.set_setup_hint(view.setup_hint.clone().into());
    window.set_status(view.status.clone().into());
    window.set_intel(model(
        view.intel.iter().map(|alert| intel_row(alert, view.now)),
    ));
    window.set_pings(model(
        view.pings.iter().map(|ping| ping_row(ping, view.now)),
    ));
    window.set_pilots(model(
        view.pilots.iter().map(|pilot| pilot_row(pilot, view.now)),
    ));
    window.set_mining(mining_info(view.mining.as_ref()));
}

fn model<T: Clone + 'static>(rows: impl Iterator<Item = T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(rows.collect::<Vec<_>>())))
}

/// Strategic and peacetime participation as Directorbot counts them.
fn pap_pair(strategic: u32, peacetime: u32) -> String {
    format!("{strategic}+{peacetime}")
}

fn intel_row(alert: &IntelAlert, now: DateTime<Utc>) -> IntelRow {
    IntelRow {
        system: alert.system_name.clone().into(),
        jumps: match alert.distance {
            0 => "here".into(),
            jumps => format!("{jumps}j").into(),
        },
        age: age(alert.occurred_at, now).into(),
        message: alert.message.clone().into(),
        reporter: alert.author.clone().into(),
        pilot: alert.character_name.clone().into(),
        critical: alert.distance <= 1 && !alert.stale_presence,
        via_bridge: alert.via_bridge,
    }
}

fn ping_row(ping: &JabberPing, now: DateTime<Utc>) -> PingRow {
    let title = ping.fleet.clone().unwrap_or_else(|| ping.summary.clone());
    let pap_type = ping.pap_type.clone().unwrap_or_default();
    PingRow {
        title: if ping.fleet_count > 1 {
            format!("{title} ({}/{})", ping.fleet_index, ping.fleet_count).into()
        } else {
            title.into()
        },
        fc: ping.fc.clone().unwrap_or_default().into(),
        formup: ping.formup.clone().unwrap_or_default().into(),
        strategic: pap_type.to_ascii_lowercase().contains("strat"),
        pap_type: pap_type.into(),
        comms: ping.comms.clone().unwrap_or_default().into(),
        // Only web links: Directorbot's comms link is an https page that
        // redirects to Mumble, and nothing else from a broadcast is opened.
        link: ping
            .comms_link
            .clone()
            .filter(|link| link.starts_with("https://"))
            .unwrap_or_default()
            .into(),
        age: age(ping.occurred_at, now).into(),
        muted: ping.muted,
    }
}

fn pilot_row(pilot: &CharacterPresence, now: DateTime<Utc>) -> PilotRow {
    let location = match (&pilot.solar_system_name, pilot.docked) {
        (Some(system), Some(true)) => format!("{system} (docked)"),
        (Some(system), _) => system.clone(),
        (None, _) => String::new(),
    };
    let ship = pilot
        .ship
        .as_ref()
        .map(|ship| ship.type_name.clone().unwrap_or_else(|| ship.name.clone()))
        .unwrap_or_default();
    let training = match &pilot.skill_queue {
        Some(queue) => match &queue.training {
            Some(entry) => {
                let left = entry
                    .finish_at
                    .map(|finish| format!(" · {}", remaining(finish, now)))
                    .unwrap_or_else(|| " · paused".to_owned());
                format!("{} {}{left}", entry.skill_name, roman(entry.level))
            }
            None => "Skill queue empty".to_owned(),
        },
        None => String::new(),
    };
    let problem = [&pilot.error, &pilot.skill_queue_error, &pilot.ship_error]
        .into_iter()
        .flatten()
        .next()
        .cloned()
        .unwrap_or_default();
    PilotRow {
        name: pilot.character_name.clone().into(),
        online: pilot.online == Some(true),
        known: pilot.online.is_some(),
        location: location.into(),
        ship: ship.into(),
        training: training.into(),
        problem: problem.into(),
    }
}

fn mining_info(view: Option<&mining::View>) -> MiningInfo {
    let Some(session) = view.and_then(|view| view.session.as_ref()) else {
        return MiningInfo {
            active: false,
            title: SharedString::new(),
            total: SharedString::new(),
            rate: SharedString::new(),
            hold_percent: 0.0,
            hold_text: SharedString::new(),
            ores: model(std::iter::empty()),
        };
    };
    let mut ores: Vec<(String, f64)> = Vec::new();
    for pilot in &session.pilots {
        for ore in &pilot.ores {
            match ores.iter_mut().find(|(name, _)| *name == ore.name) {
                Some((_, m3)) => *m3 += ore.m3,
                None => ores.push((ore.name.clone(), ore.m3)),
            }
        }
    }
    ores.sort_by(|a, b| b.1.total_cmp(&a.1));
    let hold = view
        .into_iter()
        .flat_map(|view| &view.live)
        .find_map(|pilot| pilot.hold.as_ref());
    let (hold_percent, hold_text) = match hold {
        Some(hold) if hold.capacity_m3 > 0.0 => match hold.used_m3 {
            Some(used) => (
                (used / hold.capacity_m3) as f32,
                format!(
                    "Hold {} / {} m³",
                    thousands(used),
                    thousands(hold.capacity_m3)
                ),
            ),
            None => (
                0.0,
                format!("Hold {} m³, fill unknown", thousands(hold.capacity_m3)),
            ),
        },
        _ => (0.0, String::new()),
    };
    MiningInfo {
        active: session.end.is_none(),
        title: if session.label.is_empty() {
            "SESSION".into()
        } else {
            session.label.to_uppercase().into()
        },
        total: format!("{} m³", thousands(session.totals.total_m3)).into(),
        // An hourly rate from a few seconds of yield is noise; wait for two
        // minutes of session before quoting one.
        rate: session
            .totals
            .session_rate
            .filter(|_| {
                let now = view.map_or(session.start, |view| view.now);
                session.end.unwrap_or(now) - session.start >= 120
            })
            .map(|per_second| format!("{} m³", thousands(per_second * 3600.0)))
            .unwrap_or_else(|| "–".to_owned())
            .into(),
        hold_percent,
        hold_text: hold_text.into(),
        ores: model(ores.into_iter().take(6).map(|(name, m3)| OreRow {
            name: name.into(),
            amount: format!("{} m³", thousands(m3)).into(),
        })),
    }
}

/// How long ago, in the fewest characters that still read naturally.
fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - at).num_seconds().max(0);
    match seconds {
        0..10 => "now".to_owned(),
        10..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

/// Time left until `at`, such as `2h 15m` or `40m`.
fn remaining(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (at - now).num_minutes().max(0);
    match minutes {
        0 => "under a minute".to_owned(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h {}m", minutes / 60, minutes % 60),
        _ => format!("{}d {}h", minutes / 1440, (minutes % 1440) / 60),
    }
}

fn roman(level: u8) -> &'static str {
    match level {
        1 => "I",
        2 => "II",
        3 => "III",
        4 => "IV",
        5 => "V",
        _ => "",
    }
}

/// A whole number with thousands separators, the way EVE writes volumes.
fn thousands(value: f64) -> String {
    let rounded = value.round().max(0.0) as u64;
    let digits = rounded.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    #[test]
    fn ages_read_naturally() {
        let now = Utc::now();
        assert_eq!(age(now, now), "now");
        assert_eq!(age(now - TimeDelta::seconds(45), now), "45s");
        assert_eq!(age(now - TimeDelta::minutes(3), now), "3m");
        assert_eq!(age(now - TimeDelta::hours(2), now), "2h");
        assert_eq!(age(now + TimeDelta::seconds(5), now), "now");
    }

    #[test]
    fn volumes_get_thousands_separators() {
        assert_eq!(thousands(0.0), "0");
        assert_eq!(thousands(999.4), "999");
        assert_eq!(thousands(12_345.6), "12,346");
        assert_eq!(thousands(1_234_567.0), "1,234,567");
    }

    #[test]
    fn time_left_is_compact() {
        let now = Utc::now();
        assert_eq!(remaining(now + TimeDelta::minutes(40), now), "40m");
        assert_eq!(remaining(now + TimeDelta::minutes(135), now), "2h 15m");
        assert_eq!(remaining(now + TimeDelta::hours(50), now), "2d 2h");
    }

    fn ping_with_link(link: Option<&str>) -> JabberPing {
        let now = Utc::now();
        JabberPing {
            id: 1,
            occurred_at: now,
            received_at: now,
            from: "directorbot@example.invalid".to_owned(),
            source: None,
            sender: None,
            target: None,
            summary: "Fleet".to_owned(),
            description: String::new(),
            fc: None,
            fleet: Some("Home Defence".to_owned()),
            formup: None,
            pap_type: Some("Strategic".to_owned()),
            comms: Some("Op 1".to_owned()),
            comms_link: link.map(str::to_owned),
            comms_target: None,
            doctrine: None,
            body: String::new(),
            fc_portrait: None,
            muted: false,
            fleet_index: 1,
            fleet_count: 1,
        }
    }

    #[test]
    fn only_web_links_from_a_ping_are_clickable() {
        let now = Utc::now();
        assert_eq!(
            ping_row(
                &ping_with_link(Some("https://example.invalid/c/op.html")),
                now
            )
            .link,
            "https://example.invalid/c/op.html"
        );
        for unsafe_link in [
            "mumble://example.invalid",
            "file:///C:/Windows",
            "javascript:alert(1)",
        ] {
            assert_eq!(ping_row(&ping_with_link(Some(unsafe_link)), now).link, "");
        }
        assert_eq!(ping_row(&ping_with_link(None), now).link, "");
    }

    #[test]
    fn a_strategic_ping_is_marked_strategic() {
        let row = ping_row(&ping_with_link(None), Utc::now());
        assert!(row.strategic);
        assert_eq!(row.pap_type, "Strategic");
        assert_eq!(row.title, "Home Defence");
    }
}
