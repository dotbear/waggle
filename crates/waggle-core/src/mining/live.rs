//! What each client is doing right now, read from its game log.
//!
//! The meter records yield; this watches everything else the log says about
//! the pilot so the previews can wear it: lasers that stopped and why, how
//! full the hold is, someone shooting or tackling the ship, a module out of
//! charges, a foreman burst that stopped landing. It is held in memory only —
//! a restart starts it fresh, which costs at most one hold calibration.
use super::logs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::LazyLock,
};

fn pattern(text: &str) -> Regex {
    Regex::new(text).expect("live-status pattern")
}
static HOLD_FULL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"^Your (.+) has completed operations\. Ship's cargo hold is full\.?$")
});
static DEPLETED: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"^(.+) deactivates as it finds the resource it was harvesting a pale shadow")
});
// EVE's spelling, not ours.
static STRAYED: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(.+) deactivates without transfering ore to your cargo hold"));
static TOO_FAR: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"is too far away to use your (.+?) on, it needs"));
static COMPRESSED: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^Successfully compressed (.+) into ([0-9][0-9,]*) (.+?)\.?$"));
static NO_CHARGES: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(.+) has run out of charges"));
static BURST: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^Your (.+) has applied bonuses to ([0-9]+) fleet members?\.?$"));
static HIT: LazyLock<Regex> = LazyLock::new(|| pattern(r"^[0-9][0-9,.]* from (.+)$"));
static MISS: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(.+?) misses you completely"));
static TACKLE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^Warp (scramble|disruption) attempt from (.+?) to you!?$"));

/// How long a combat or charges line stays in the published state. The
/// previews decide for themselves how recent is recent; this only keeps
/// stale entries out of the snapshot.
const FORGET_AFTER: i64 = 600;
/// A pilot whose last yield is older than this is not mining, rather than
/// mining with stopped lasers.
const MINING_LAPSES_AFTER: i64 = 900;
/// Yields kept for rates and stall detection.
const HISTORY: i64 = 600;
/// The window the fill rate is measured over.
const RATE_WINDOW: i64 = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Every laser stops at the end of its cycle.
    HoldFull,
    /// One laser's rock is gone.
    Depleted,
    /// One laser lost its cycle to the ship drifting away.
    OutOfRange,
    /// A laser was activated on a rock out of reach.
    TooFar,
    /// No yield for longer than the lasers' cycle explains, and no message
    /// saying why.
    Idle,
}

impl StopReason {
    /// Whether the other lasers can keep going.
    fn partial(self) -> bool {
        matches!(self, Self::Depleted | Self::OutOfRange | Self::TooFar)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaserState {
    Running,
    /// Some lasers stopped and some still yield.
    Partial,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lasers {
    pub state: LaserState,
    pub reason: Option<StopReason>,
    /// When the reason was logged, or the last yield for an idle stop.
    pub since: Option<i64>,
    pub last_yield: i64,
    /// Ore volume per second over the last few minutes of yields.
    pub rate_m3: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hold {
    pub capacity_m3: f64,
    /// Absent once ore left the hold where the log cannot see it — moved to a
    /// fleet hangar or a can — until the next full hold says where it is.
    pub used_m3: Option<f64>,
    /// False until a full hold has been seen since the ship or the session
    /// began; before that the contents carried in are unknown and `used_m3`
    /// counts only what was mined and compressed since.
    pub calibrated: bool,
    /// When the hold fills at the current rate if nothing is compressed or
    /// moved out. Absent while the lasers are not running.
    pub full_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamp {
    pub at: i64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Burst {
    pub at: i64,
    pub members: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PilotStatus {
    pub id: String,
    pub name: String,
    pub lasers: Option<Lasers>,
    pub hold: Option<Hold>,
    /// Shot at, hit or missed; the detail is who.
    pub attacked: Option<Stamp>,
    /// Scrambled or disrupted; the detail is who.
    pub tackled: Option<Stamp>,
    /// The detail is the module.
    pub out_of_charges: Option<Stamp>,
    pub burst: Option<Burst>,
}

#[derive(Debug, Default)]
struct Pilot {
    name: String,
    yields: VecDeque<(i64, f64)>,
    last_yield: Option<i64>,
    reason: Option<(StopReason, i64)>,
    /// Raw units of each ore mined since tracking began and not compressed.
    raw: HashMap<String, i64>,
    used: f64,
    calibrated: bool,
    lost: bool,
    last_compress: Option<(i64, String)>,
    ship_item: Option<u64>,
    capacity: Option<f64>,
    attacked: Option<Stamp>,
    tackled: Option<Stamp>,
    out_of_charges: Option<Stamp>,
    burst: Option<Burst>,
}

impl Pilot {
    /// Forgets the hold's contents: a different ship, or a dock where it was
    /// probably emptied.
    fn reset_hold(&mut self) {
        self.raw.clear();
        self.used = 0.0;
        self.calibrated = false;
        self.lost = false;
    }

    /// The typical gap between yields, from the most recent run of them.
    fn median_gap(&self) -> Option<i64> {
        let times: Vec<i64> = self.yields.iter().rev().take(13).map(|y| y.0).collect();
        let mut gaps: Vec<i64> = times.windows(2).map(|w| w[0] - w[1]).collect();
        if gaps.len() < 3 {
            return None;
        }
        gaps.sort_unstable();
        Some(gaps[gaps.len() / 2])
    }

    /// How long without a yield means the lasers have stopped.
    fn stall_after(&self) -> i64 {
        self.median_gap().map_or(60, |gap| (gap * 3).clamp(20, 600))
    }

    fn rate(&self) -> Option<f64> {
        let last = self.last_yield?;
        let recent: Vec<&(i64, f64)> = self
            .yields
            .iter()
            .filter(|y| y.0 > last - RATE_WINDOW)
            .collect();
        let span = recent.last()?.0 - recent.first()?.0;
        if recent.len() < 3 || span < 20 {
            return None;
        }
        // The first yield's volume was mined before the span began.
        Some(recent.iter().skip(1).map(|y| y.1).sum::<f64>() / span as f64)
    }

    fn yielded(&mut self, at: i64, ore: &str, units: i64, volume: Option<f64>) {
        let m3 = volume.map_or(0.0, |v| v * units as f64);
        self.yields.push_back((at, m3));
        while self.yields.front().is_some_and(|y| y.0 < at - HISTORY) {
            self.yields.pop_front();
        }
        self.last_yield = Some(self.last_yield.map_or(at, |last| last.max(at)));
        *self.raw.entry(ore.to_owned()).or_default() += units;
        self.used += m3;
        if let Some(capacity) = self.capacity
            && self.used > capacity * 1.02
        {
            // Ore arriving in a hold the estimate says is full means some
            // left it unseen — moved to a fleet hangar or a can.
            self.used = capacity;
            self.calibrated = false;
            self.lost = true;
        }
        if let Some((reason, since)) = self.reason
            && at > since
        {
            let elapsed = at - since;
            let clear = if reason.partial() {
                // The stopped laser is back once the last minute yields as
                // often as the minute before the stop did.
                let window = elapsed.min(60);
                let count = |from: i64, to: i64| {
                    self.yields
                        .iter()
                        .filter(|y| y.0 > from && y.0 <= to)
                        .count()
                };
                let after = count(at - window, at);
                let before = count(since - window, since);
                elapsed > 300 || (elapsed >= 20 && after >= before.max(2))
            } else {
                true
            };
            if clear {
                self.reason = None;
            }
        }
    }

    fn compressed(
        &mut self,
        at: i64,
        text: &str,
        ore: &str,
        units: i64,
        volume: Option<f64>,
        compressed: Option<f64>,
    ) {
        // EVE sometimes logs one compression twice in the same second.
        if self
            .last_compress
            .as_ref()
            .is_some_and(|(when, line)| *when == at && line == text)
        {
            return;
        }
        self.last_compress = Some((at, text.to_owned()));
        let (Some(volume), Some(compressed)) = (volume, compressed) else {
            return;
        };
        // Compression is unit for unit; only the volume shrinks.
        let held = self.raw.entry(ore.to_owned()).or_default();
        *held = (*held - units).max(0);
        self.used = (self.used - units as f64 * (volume - compressed)).max(0.0);
    }

    fn view(&self, id: &str, now: i64) -> PilotStatus {
        let recent = |stamp: &Option<Stamp>| stamp.clone().filter(|s| now - s.at <= FORGET_AFTER);
        let lasers = self
            .last_yield
            .filter(|last| now - last <= MINING_LAPSES_AFTER)
            .map(|last_yield| {
                let stalled = now - last_yield > self.stall_after();
                let (state, reason, since) = match self.reason {
                    Some((StopReason::HoldFull, at)) => {
                        (LaserState::Stopped, Some(StopReason::HoldFull), Some(at))
                    }
                    Some((reason, at)) if stalled => (LaserState::Stopped, Some(reason), Some(at)),
                    Some((reason, at)) => (LaserState::Partial, Some(reason), Some(at)),
                    None if stalled => (
                        LaserState::Stopped,
                        Some(StopReason::Idle),
                        Some(last_yield),
                    ),
                    None => (LaserState::Running, None, None),
                };
                Lasers {
                    state,
                    reason,
                    since,
                    last_yield,
                    rate_m3: self.rate(),
                }
            });
        let hold = self.capacity.filter(|_| lasers.is_some()).map(|capacity| {
            let running = lasers
                .as_ref()
                .is_some_and(|l| l.state != LaserState::Stopped);
            let full_at = self
                .rate()
                .filter(|rate| *rate > 0.0 && running && !self.lost)
                .zip(self.last_yield)
                .map(|(rate, last)| last + ((capacity - self.used).max(0.0) / rate).round() as i64);
            Hold {
                capacity_m3: capacity,
                used_m3: (!self.lost).then_some(self.used.min(capacity)),
                calibrated: self.calibrated,
                full_at,
            }
        });
        PilotStatus {
            id: id.to_owned(),
            name: self.name.clone(),
            lasers,
            hold,
            attacked: recent(&self.attacked),
            tackled: recent(&self.tackled),
            out_of_charges: recent(&self.out_of_charges),
            burst: self.burst.clone().filter(|b| now - b.at <= FORGET_AFTER),
        }
    }
}

/// Who did it, without the ship, corporation and weapon EVE appends.
fn who(text: &str) -> String {
    text.split(" - ").next().unwrap_or(text).trim().to_owned()
}

#[derive(Debug, Default)]
pub struct Tracker {
    pilots: BTreeMap<String, Pilot>,
}

impl Tracker {
    /// Reads one game-log line written by pilot `id`. `volume` gives the m³
    /// of one unit of a named ore.
    pub fn observe(
        &mut self,
        id: &str,
        name: &str,
        line: &str,
        volume: impl Fn(&str) -> Option<f64>,
    ) {
        let Some((at, kind, text)) = logs::entry(line) else {
            return;
        };
        let pilot = self.pilots.entry(id.to_owned()).or_default();
        if pilot.name != name {
            pilot.name = name.to_owned();
        }
        match kind {
            "mining" => {
                if let Some(event) = logs::parse(line) {
                    pilot.yielded(at, &event.ore, event.units, volume(&event.ore));
                }
            }
            "notify" => {
                if HOLD_FULL.is_match(&text) {
                    pilot.reason = Some((StopReason::HoldFull, at));
                    if let Some(capacity) = pilot.capacity {
                        pilot.used = capacity;
                        pilot.calibrated = true;
                        pilot.lost = false;
                    }
                } else if DEPLETED.is_match(&text) {
                    pilot.reason = Some((StopReason::Depleted, at));
                } else if STRAYED.is_match(&text) {
                    pilot.reason = Some((StopReason::OutOfRange, at));
                } else if TOO_FAR.is_match(&text) {
                    pilot.reason = Some((StopReason::TooFar, at));
                } else if let Some(c) = COMPRESSED.captures(&text) {
                    let units = c[2].replace(',', "").parse().unwrap_or(0);
                    pilot.compressed(at, &text, &c[1], units, volume(&c[1]), volume(&c[3]));
                } else if let Some(c) = NO_CHARGES.captures(&text) {
                    pilot.out_of_charges = Some(Stamp {
                        at,
                        detail: c[1].trim().to_owned(),
                    });
                } else if let Some(c) = BURST.captures(&text) {
                    pilot.burst = Some(Burst {
                        at,
                        members: c[2].parse().unwrap_or(0),
                    });
                }
            }
            "combat" => {
                if let Some(c) = TACKLE.captures(&text) {
                    pilot.tackled = Some(Stamp {
                        at,
                        detail: who(&c[2]),
                    });
                } else if let Some(c) = HIT.captures(&text).or_else(|| MISS.captures(&text)) {
                    pilot.attacked = Some(Stamp {
                        at,
                        detail: who(&c[1]),
                    });
                }
            }
            "None" if text.starts_with("Undocking from ") => pilot.reset_hold(),
            _ => {}
        }
    }

    /// Tells the tracker which ship each pilot sits in and how big its hold
    /// is, as ESI last said. Boarding another ship forgets the old hold.
    pub fn set_ship(&mut self, id: &str, item: Option<u64>, capacity: Option<f64>) {
        let Some(pilot) = self.pilots.get_mut(id) else {
            return;
        };
        if item.is_some() && pilot.ship_item.is_some() && item != pilot.ship_item {
            pilot.reset_hold();
        }
        if item.is_some() {
            pilot.ship_item = item;
        }
        pilot.capacity = capacity.or(pilot.capacity.filter(|_| item.is_none()));
    }

    pub fn view(&self, now: i64) -> Vec<PilotStatus> {
        self.pilots
            .iter()
            .map(|(id, pilot)| pilot.view(id, now))
            .filter(|status| {
                status.lasers.is_some()
                    || status.attacked.is_some()
                    || status.tackled.is_some()
                    || status.out_of_charges.is_some()
                    || status.burst.is_some()
            })
            .collect()
    }
}

/// Something about a mining pilot worth interrupting them for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    HoldFull,
    Depleted,
    OutOfRange,
    TooFar,
    /// The lasers stopped yielding with no message saying why.
    Stopped,
    OutOfCharges,
    Tackled,
    Attacked,
}

impl AlertKind {
    fn from_reason(reason: StopReason) -> Self {
        match reason {
            StopReason::HoldFull => Self::HoldFull,
            StopReason::Depleted => Self::Depleted,
            StopReason::OutOfRange => Self::OutOfRange,
            StopReason::TooFar => Self::TooFar,
            StopReason::Idle => Self::Stopped,
        }
    }

    /// The shortest gap between two alerts of this kind for one pilot. Being
    /// shot logs a line per hit.
    fn quiet(self) -> i64 {
        match self {
            Self::Attacked => 120,
            Self::Tackled => 60,
            _ => 0,
        }
    }

    /// What the alert says happened, in a few words.
    pub fn headline(self) -> &'static str {
        match self {
            Self::HoldFull => "Ore hold full",
            Self::Depleted => "Asteroid depleted",
            Self::OutOfRange => "Drifted out of range",
            Self::TooFar => "Asteroid too far away",
            Self::Stopped => "Lasers stopped",
            Self::OutOfCharges => "Out of charges",
            Self::Tackled => "Tackled",
            Self::Attacked => "Under fire",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub pilot: String,
    pub name: String,
    pub kind: AlertKind,
    pub at: i64,
    /// Who shot or tackled, or which module ran dry.
    pub detail: Option<String>,
}

/// Turns the tracker's state into one alert per new event, by comparing
/// each view with the last. The first view only sets the baseline, so
/// starting up next to a log full of old stops says nothing.
#[derive(Debug, Default)]
pub struct Alerts {
    started: bool,
    /// (pilot, kind) → the event last seen, and when it was last announced.
    seen: HashMap<(String, AlertKind), (i64, i64)>,
}

/// An event older than this when first seen is history, not news.
const ALERT_FRESH: i64 = 120;

impl Alerts {
    pub fn check(&mut self, statuses: &[PilotStatus], now: i64) -> Vec<Alert> {
        let mut out = Vec::new();
        for status in statuses {
            // Combat only matters to a ship that is mining; a pilot out
            // ratting or in a fleet fight knows they are being shot.
            let mining = status.lasers.is_some();
            let mut found: Vec<(AlertKind, i64, Option<String>)> = Vec::new();
            if let Some(lasers) = &status.lasers
                && let (Some(reason), Some(since)) = (lasers.reason, lasers.since)
            {
                found.push((AlertKind::from_reason(reason), since, None));
            }
            let stamped = [
                (AlertKind::OutOfCharges, &status.out_of_charges, true),
                (AlertKind::Tackled, &status.tackled, mining),
                (AlertKind::Attacked, &status.attacked, mining),
            ];
            for (kind, stamp, wanted) in stamped {
                if let Some(stamp) = stamp.as_ref().filter(|_| wanted) {
                    found.push((kind, stamp.at, Some(stamp.detail.clone())));
                }
            }
            for (kind, at, detail) in found {
                let key = (status.id.clone(), kind);
                let previous = self.seen.get(&key).copied();
                if previous.is_some_and(|(seen, _)| seen == at) {
                    continue;
                }
                let announced = previous.map_or(i64::MIN / 2, |(_, told)| told);
                // An idle stop is dated by the last yield, so its age says
                // nothing about when it was noticed.
                let fresh = kind == AlertKind::Stopped || now - at <= ALERT_FRESH;
                let due = self.started && fresh && now - announced >= kind.quiet();
                self.seen
                    .insert(key, (at, if due { now } else { announced }));
                if due {
                    out.push(Alert {
                        pilot: status.id.clone(),
                        name: status.name.clone(),
                        kind,
                        at,
                        detail,
                    });
                }
            }
        }
        self.started = true;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "768661391";

    fn line(at: i64, kind: &str, text: &str) -> String {
        let stamp = chrono::DateTime::from_timestamp(at, 0)
            .unwrap()
            .format("%Y.%m.%d %H:%M:%S");
        format!("[ {stamp} ] ({kind}) {text}")
    }

    fn volume(ore: &str) -> Option<f64> {
        match ore {
            "Glistening Zeolites" => Some(10.0),
            "Compressed Glistening Zeolites" => Some(0.1),
            _ => None,
        }
    }

    fn feed(tracker: &mut Tracker, at: i64, kind: &str, text: &str) {
        tracker.observe(ID, "Example Pilot", &line(at, kind, text), volume);
    }

    fn mine(tracker: &mut Tracker, from: i64, to: i64, every: i64) {
        let mut at = from;
        while at <= to {
            feed(
                tracker,
                at,
                "mining",
                "You mined 50 units of Glistening Zeolites",
            );
            at += every;
        }
    }

    const T: i64 = 1_790_000_000;

    #[test]
    fn a_full_hold_stops_the_lasers_and_calibrates_the_estimate() {
        let mut tracker = Tracker::default();
        mine(&mut tracker, T, T + 120, 6);
        tracker.set_ship(ID, Some(1), Some(10_000.0));
        let view = &tracker.view(T + 121)[0];
        assert_eq!(view.lasers.as_ref().unwrap().state, LaserState::Running);
        let hold = view.hold.as_ref().unwrap();
        assert!(!hold.calibrated);
        // 21 yields of 500 m³ at 500 m³ per 6 s.
        assert_eq!(hold.used_m3, Some(10_000.0_f64.min(21.0 * 500.0)));

        feed(
            &mut tracker,
            T + 125,
            "notify",
            "Your Abyssal Strip Miner has completed operations. Ship's cargo hold is full.",
        );
        let view = &tracker.view(T + 130)[0];
        let lasers = view.lasers.as_ref().unwrap();
        assert_eq!(lasers.state, LaserState::Stopped);
        assert_eq!(lasers.reason, Some(StopReason::HoldFull));
        assert_eq!(lasers.since, Some(T + 125));
        assert!(view.hold.as_ref().unwrap().calibrated);

        // Compressing frees the raw volume; mining again clears the stop.
        feed(
            &mut tracker,
            T + 140,
            "notify",
            "Successfully compressed Glistening Zeolites into 1,000 Compressed Glistening Zeolites.",
        );
        feed(
            &mut tracker,
            T + 140,
            "notify",
            "Successfully compressed Glistening Zeolites into 1,000 Compressed Glistening Zeolites.",
        );
        let hold = tracker.view(T + 141)[0].hold.clone().unwrap();
        assert!((hold.used_m3.unwrap() - (10_000.0 - 1_000.0 * 9.9)).abs() < 1e-6);
        mine(&mut tracker, T + 150, T + 210, 6);
        let view = &tracker.view(T + 211)[0];
        assert_eq!(view.lasers.as_ref().unwrap().state, LaserState::Running);
        let full_at = view.hold.as_ref().unwrap().full_at.unwrap();
        assert!(full_at > T + 211);
    }

    #[test]
    fn silence_longer_than_the_cycle_is_an_idle_stop() {
        let mut tracker = Tracker::default();
        mine(&mut tracker, T, T + 60, 6);
        let lasers = tracker.view(T + 70)[0].lasers.clone().unwrap();
        assert_eq!(lasers.state, LaserState::Running);
        let lasers = tracker.view(T + 81)[0].lasers.clone().unwrap();
        assert_eq!(lasers.state, LaserState::Stopped);
        assert_eq!(lasers.reason, Some(StopReason::Idle));
        assert_eq!(lasers.since, Some(T + 60));
        // Long enough and the pilot has simply stopped mining.
        assert!(tracker.view(T + 60 + MINING_LAPSES_AFTER + 1).is_empty());
    }

    #[test]
    fn one_depleted_rock_is_partial_until_the_rate_recovers() {
        let mut tracker = Tracker::default();
        mine(&mut tracker, T, T + 60, 6);
        feed(
            &mut tracker,
            T + 61,
            "notify",
            "Abyssal Strip Miner deactivates as it finds the resource it was harvesting a pale shadow of its former glory.",
        );
        // The other laser carries on at half the rate.
        mine(&mut tracker, T + 66, T + 90, 12);
        let lasers = tracker.view(T + 91)[0].lasers.clone().unwrap();
        assert_eq!(lasers.state, LaserState::Partial);
        assert_eq!(lasers.reason, Some(StopReason::Depleted));
        // Back to both lasers.
        mine(&mut tracker, T + 96, T + 150, 6);
        let lasers = tracker.view(T + 151)[0].lasers.clone().unwrap();
        assert_eq!(lasers.state, LaserState::Running);
    }

    #[test]
    fn combat_names_the_attacker_and_tackle_only_counts_when_it_is_on_you() {
        let mut tracker = Tracker::default();
        feed(
            &mut tracker,
            T,
            "combat",
            "312 from Angel Tribuni - Republic Fleet Valkyrie - Hits",
        );
        feed(
            &mut tracker,
            T + 1,
            "combat",
            "Warp scramble attempt from Infesting Obstructor Xi to  Example Pilot - Phoenix Navy Issue",
        );
        let view = &tracker.view(T + 2)[0];
        assert_eq!(view.attacked.as_ref().unwrap().detail, "Angel Tribuni");
        assert!(view.tackled.is_none());
        feed(
            &mut tracker,
            T + 3,
            "combat",
            "Warp disruption attempt from  Example Hostile - Griffin - EXMPL - EXA - Puffin to you!",
        );
        feed(&mut tracker, T + 4, "combat", "Dagan misses you completely");
        let view = &tracker.view(T + 5)[0];
        assert_eq!(view.tackled.as_ref().unwrap().detail, "Example Hostile");
        assert_eq!(view.attacked.as_ref().unwrap().detail, "Dagan");
        assert!(tracker.view(T + 4 + FORGET_AFTER + 1).is_empty());
    }

    #[test]
    fn boarding_another_ship_forgets_the_hold() {
        let mut tracker = Tracker::default();
        mine(&mut tracker, T, T + 30, 6);
        tracker.set_ship(ID, Some(1), Some(10_000.0));
        assert!(tracker.view(T + 31)[0].hold.as_ref().unwrap().used_m3 > Some(0.0));
        tracker.set_ship(ID, Some(2), Some(5_000.0));
        let hold = tracker.view(T + 31)[0].hold.clone().unwrap();
        assert_eq!(hold.used_m3, Some(0.0));
        assert_eq!(hold.capacity_m3, 5_000.0);
    }

    #[test]
    fn ore_arriving_in_a_full_hold_makes_the_contents_unknown() {
        let mut tracker = Tracker::default();
        mine(&mut tracker, T, T + 6, 6);
        tracker.set_ship(ID, Some(1), Some(1_000.0));
        // 500 m³ a yield into a 1,000 m³ hold, and no full-hold message.
        mine(&mut tracker, T + 12, T + 30, 6);
        let hold = tracker.view(T + 31)[0].hold.clone().unwrap();
        assert_eq!(hold.used_m3, None);
        assert_eq!(hold.full_at, None);
        feed(
            &mut tracker,
            T + 32,
            "notify",
            "Your Abyssal Strip Miner has completed operations. Ship's cargo hold is full.",
        );
        assert_eq!(
            tracker.view(T + 33)[0].hold.clone().unwrap().used_m3,
            Some(1_000.0)
        );
    }

    fn kinds(alerts: Vec<Alert>) -> Vec<AlertKind> {
        alerts.into_iter().map(|a| a.kind).collect()
    }

    #[test]
    fn each_stop_is_announced_once_and_old_ones_never() {
        let mut tracker = Tracker::default();
        let mut alerts = Alerts::default();
        mine(&mut tracker, T, T + 60, 6);
        feed(
            &mut tracker,
            T + 61,
            "notify",
            "Your Miner II deactivates as it finds the resource it was harvesting a pale shadow of its former glory.",
        );
        // Starting up next to that stop says nothing about it.
        assert!(alerts.check(&tracker.view(T + 62), T + 62).is_empty());

        mine(&mut tracker, T + 66, T + 400, 6);
        assert!(alerts.check(&tracker.view(T + 400), T + 400).is_empty());
        feed(
            &mut tracker,
            T + 401,
            "notify",
            "Your Miner II deactivates as it finds the resource it was harvesting a pale shadow of its former glory.",
        );
        let found = alerts.check(&tracker.view(T + 402), T + 402);
        assert_eq!(kinds(found), [AlertKind::Depleted]);
        assert!(
            alerts.check(&tracker.view(T + 403), T + 403).is_empty(),
            "once"
        );

        feed(
            &mut tracker,
            T + 404,
            "notify",
            "Your Miner II has completed operations. Ship's cargo hold is full.",
        );
        assert_eq!(
            kinds(alerts.check(&tracker.view(T + 405), T + 405)),
            [AlertKind::HoldFull]
        );
    }

    #[test]
    fn lasers_that_simply_stop_are_announced() {
        let mut tracker = Tracker::default();
        let mut alerts = Alerts::default();
        mine(&mut tracker, T, T + 60, 6);
        alerts.check(&tracker.view(T + 61), T + 61);
        // Three missed cycles, and no message saying why.
        let found = alerts.check(&tracker.view(T + 90), T + 90);
        assert_eq!(kinds(found), [AlertKind::Stopped]);
        assert!(alerts.check(&tracker.view(T + 120), T + 120).is_empty());
    }

    #[test]
    fn fire_on_a_miner_is_announced_sparingly() {
        let mut tracker = Tracker::default();
        let mut alerts = Alerts::default();
        alerts.check(&tracker.view(T), T);
        // Shot while not mining: none of our business.
        feed(
            &mut tracker,
            T + 1,
            "combat",
            "120 from Example Hostile - Hobgoblin I - Hits",
        );
        assert!(alerts.check(&tracker.view(T + 2), T + 2).is_empty());

        mine(&mut tracker, T + 10, T + 100, 6);
        feed(
            &mut tracker,
            T + 101,
            "combat",
            "120 from Example Hostile - Hobgoblin I - Hits",
        );
        let found = alerts.check(&tracker.view(T + 101), T + 101);
        assert_eq!(found[0].kind, AlertKind::Attacked);
        assert_eq!(found[0].detail.as_deref(), Some("Example Hostile"));
        // Every hit logs a line; two minutes pass before the next alert.
        mine(&mut tracker, T + 102, T + 300, 6);
        feed(
            &mut tracker,
            T + 150,
            "combat",
            "120 from Example Hostile - Hobgoblin I - Hits",
        );
        assert!(alerts.check(&tracker.view(T + 150), T + 150).is_empty());
        feed(
            &mut tracker,
            T + 230,
            "combat",
            "120 from Example Hostile - Hobgoblin I - Hits",
        );
        assert_eq!(
            kinds(alerts.check(&tracker.view(T + 230), T + 230)),
            [AlertKind::Attacked]
        );
    }
}
