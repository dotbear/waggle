//! What the window shows, and how it is drawn from the engine's state.
//!
//! The engine builds a [`ViewModel`] from plain core types on its own thread;
//! [`apply`] runs on the UI thread and turns it into Slint models.

use std::rc::Rc;

use chrono::{DateTime, Local, TimeZone, Utc};
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
    pub update: Option<crate::updates::Update>,
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
    match &view.update {
        Some(update) => {
            window.set_update_text(
                format!("Waggle {} is out. Click to download it.", update.version).into(),
            );
            window.set_update_url(update.url.clone().into());
        }
        None => {
            window.set_update_text(SharedString::new());
            window.set_update_url(SharedString::new());
        }
    }
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

pub fn mining_info(view: Option<&mining::View>) -> MiningInfo {
    let Some(session) = view.and_then(|view| view.session.as_ref()) else {
        return MiningInfo {
            active: false,
            title: SharedString::new(),
            total: SharedString::new(),
            rate: SharedString::new(),
            rate_hour: SharedString::new(),
            hold_percent: 0.0,
            hold_text: SharedString::new(),
            ores: model(std::iter::empty()),
            graph: Graph::default().into(),
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
    // A rate from a few seconds of yield is noise; wait for two minutes of
    // session before quoting one.
    let rate = session.totals.session_rate.filter(|_| {
        let now = view.map_or(session.start, |view| view.now);
        session.end.unwrap_or(now) - session.start >= 120
    });
    MiningInfo {
        active: session.end.is_none(),
        title: if session.label.is_empty() {
            "SESSION".into()
        } else {
            session.label.to_uppercase().into()
        },
        total: format!("{} m³", thousands(session.totals.total_m3)).into(),
        rate: rate
            .map(|per_second| format!("{per_second:.2} m³/s"))
            .unwrap_or_else(|| "–".to_owned())
            .into(),
        rate_hour: rate
            .map(|per_second| format!("{} m³/h", thousands(per_second * 3600.0)))
            .unwrap_or_default()
            .into(),
        hold_percent,
        hold_text: hold_text.into(),
        ores: model(ores.into_iter().take(6).map(|(name, m3)| OreRow {
            name: name.into(),
            amount: format!("{} m³", thousands(m3)).into(),
        })),
        graph: {
            let mut graph = Graph::of(session);
            // The same two minutes the headline waits before quoting an average.
            if rate.is_none() {
                graph.average = None;
            }
            graph.into()
        },
    }
}

/// The yield graph as SVG paths in a 1000 × 100 box, y pointing down.
#[derive(Debug, Default, PartialEq)]
struct Graph {
    line: String,
    area: String,
    /// The top of the scale, in m³/s.
    top: f64,
    /// The session average as a fraction of the height from the bottom.
    average: Option<f64>,
    from: String,
    to: String,
}

impl Graph {
    fn of(session: &mining::SessionView) -> Self {
        let points = &session.graph;
        if points.len() < 2 {
            return Self::default();
        }
        let rates: Vec<f64> = points.iter().map(|p| p.ordinary + p.bonus).collect();
        let average = session.totals.session_rate.filter(|r| *r > 0.0);
        let peak = rates.iter().copied().chain(average).fold(0.0_f64, f64::max);
        if peak <= 0.0 {
            return Self::default();
        }
        let top = nice_ceiling(peak * 1.1);
        let last = (rates.len() - 1) as f64;
        let xy = |i: usize, rate: f64| (i as f64 * 1000.0 / last, 100.0 - rate / top * 100.0);
        let mut line = String::new();
        for (i, rate) in rates.iter().enumerate() {
            let (x, y) = xy(i, *rate);
            line += &format!("{}{x:.1} {y:.1} ", if i == 0 { "M" } else { "L" });
        }
        let area = format!("{line}L 1000 100 L 0 100 Z");
        let clock = |at: i64| {
            Local
                .timestamp_opt(at, 0)
                .single()
                .map(|t| t.format("%H:%M").to_string())
                .unwrap_or_default()
        };
        Self {
            line: line.trim_end().to_owned(),
            area,
            top,
            average: average.map(|a| a / top),
            from: clock(points[0].at),
            to: clock(points[points.len() - 1].at),
        }
    }
}

impl From<Graph> for crate::MiningGraph {
    fn from(graph: Graph) -> Self {
        Self {
            shown: !graph.line.is_empty(),
            line: graph.line.into(),
            area: graph.area.into(),
            top: if graph.top >= 10.0 {
                format!("{:.0} m³/s", graph.top)
            } else {
                format!("{:.1} m³/s", graph.top)
            }
            .into(),
            average: graph.average.map_or(-1.0, |a| a as f32),
            from: graph.from.into(),
            to: graph.to.into(),
        }
    }
}

/// The next of 1, 2, 2.5, 5 × 10ⁿ at or above `value`, so the scale's top
/// reads as a round number.
fn nice_ceiling(value: f64) -> f64 {
    let magnitude = 10_f64.powf(value.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|step| step * magnitude)
        .find(|candidate| *candidate >= value)
        .unwrap_or(value)
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

    #[test]
    fn the_scale_tops_out_at_a_round_number() {
        assert_eq!(nice_ceiling(0.73), 1.0);
        assert_eq!(nice_ceiling(1.3), 2.0);
        assert_eq!(nice_ceiling(2.2), 2.5);
        assert_eq!(nice_ceiling(37.0), 50.0);
        assert_eq!(nice_ceiling(120.0), 200.0);
    }

    #[test]
    fn the_graph_spans_the_box_and_marks_the_average() {
        let now = 1_790_000_000;
        let session = waggle_core::mining::demo(now).session.unwrap();
        let graph = Graph::of(&session);
        assert!(graph.line.starts_with("M0.0 "), "{}", graph.line);
        assert!(graph.line.contains("L1000.0 "), "ends at the right edge");
        assert_eq!(graph.line.matches(['M', 'L']).count(), session.graph.len());
        assert!(graph.area.ends_with("Z"));
        let average = graph.average.unwrap();
        assert!(average > 0.0 && average < 1.0);
        assert!(!graph.from.is_empty() && !graph.to.is_empty());

        let mut short = session.clone();
        short.graph.truncate(1);
        assert_eq!(Graph::of(&short), Graph::default(), "one point is no line");
    }
}
