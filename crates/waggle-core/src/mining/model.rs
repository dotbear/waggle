use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub revision: u64,
    pub enabled: bool,
    pub pilots: Vec<String>,
    pub auto_include: bool,
    pub auto_start: bool,
    pub window_seconds: u32,
    pub graph_minutes: u32,
    pub graph_series: String,
    pub crit_markers: bool,
    pub breakdown: bool,
    pub visible: String,
    pub aggregate_pilots: Vec<String>,
    pub sort: String,
    pub main_rate: String,
    pub compact: bool,
    pub show_graph: bool,
    pub show_rows: bool,
    pub opacity: f64,
    pub monitor: String,
    pub corner: String,
    pub meter_width: u32,
    pub graph_pilots: Vec<String>,
    pub inactive_seconds: u32,
    pub pilot_inactive: BTreeMap<String, u32>,
    pub baselines: BTreeMap<String, f64>,
    pub retention_days: u32,
    pub roots: Vec<String>,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: true,
            pilots: vec![],
            auto_include: true,
            auto_start: false,
            window_seconds: 60,
            graph_minutes: 15,
            graph_series: "combined".into(),
            crit_markers: true,
            breakdown: false,
            visible: "all".into(),
            aggregate_pilots: vec![],
            sort: "volume".into(),
            main_rate: "session".into(),
            compact: false,
            show_graph: true,
            show_rows: true,
            opacity: 1.0,
            monitor: String::new(),
            corner: "bar".into(),
            meter_width: 600,
            graph_pilots: vec![],
            inactive_seconds: 120,
            pilot_inactive: BTreeMap::new(),
            baselines: BTreeMap::new(),
            retention_days: 30,
            roots: vec![],
        }
    }
}
impl Preferences {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (10..=600).contains(&self.window_seconds),
            "window_seconds must be 10–600"
        );
        ensure!(
            [5, 15, 30, 60].contains(&self.graph_minutes),
            "graph_minutes must be 5, 15, 30 or 60"
        );
        ensure!(
            ["combined", "pilots", "both"].contains(&self.graph_series.as_str()),
            "invalid graph_series"
        );
        ensure!(
            ["all", "online", "recent"].contains(&self.visible.as_str()),
            "invalid visible filter"
        );
        ensure!(
            ["rate", "volume", "name"].contains(&self.sort.as_str()),
            "invalid sort"
        );
        ensure!(
            ["recent", "session"].contains(&self.main_rate.as_str()),
            "invalid main_rate"
        );
        ensure!(
            self.opacity.is_finite() && (0.4..=1.0).contains(&self.opacity),
            "opacity must be 0.4–1"
        );
        ensure!(
            [
                "bar",
                "top-left",
                "top-right",
                "bottom-left",
                "bottom-right"
            ]
            .contains(&self.corner.as_str()),
            "invalid meter corner"
        );
        ensure!(
            (400..=900).contains(&self.meter_width),
            "meter_width must be 400–900"
        );
        ensure!(
            self.monitor.len() <= 128 && self.graph_pilots.len() <= 20,
            "invalid monitor or graph pilot selection"
        );
        ensure!(
            (30..=1800).contains(&self.inactive_seconds),
            "inactive_seconds must be 30–1800"
        );
        ensure!(
            self.pilot_inactive
                .values()
                .all(|v| (30..=1800).contains(v)),
            "pilot inactivity must be 30–1800 seconds"
        );
        ensure!(
            self.baselines
                .values()
                .all(|v| v.is_finite() && *v > 0.0 && *v <= 1e9),
            "baselines must be positive m³/s (maximum 1 billion)"
        );
        ensure!(
            [7, 30, 90].contains(&self.retention_days),
            "retention_days must be 7, 30 or 90"
        );
        ensure!(
            self.roots.len() <= 20
                && self
                    .roots
                    .iter()
                    .all(|v| v.starts_with('/') && v.len() <= 4096),
            "roots must be absolute paths (maximum 20)"
        );
        ensure!(
            self.pilots.len() <= 100 && self.aggregate_pilots.len() <= 100,
            "maximum 100 selected pilots"
        );
        Ok(())
    }
    pub fn includes(&self, id: &str) -> bool {
        self.pilots.is_empty() || self.pilots.iter().any(|p| p == id)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Totals {
    pub ordinary_m3: f64,
    pub bonus_m3: f64,
    pub total_m3: f64,
    pub crits: u64,
    pub unresolved: u64,
    pub recent_rate: f64,
    pub session_rate: Option<f64>,
    pub uplift: Option<f64>,
}
impl Totals {
    pub fn finish(&mut self, seconds: i64) {
        self.total_m3 = self.ordinary_m3 + self.bonus_m3;
        self.session_rate = (seconds > 0).then(|| self.total_m3 / seconds as f64);
        self.uplift = (self.ordinary_m3 > 0.0).then(|| self.bonus_m3 * 100.0 / self.ordinary_m3);
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OreView {
    pub name: String,
    pub units: u64,
    pub bonus_units: u64,
    pub m3: f64,
    pub unresolved: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PilotView {
    pub id: String,
    pub name: String,
    pub last_yield: Option<i64>,
    /// Time the lasers sat stopped this session: every gap between yields
    /// longer than their cycle explains, less the cycle itself.
    #[serde(default)]
    pub idle_seconds: i64,
    pub totals: Totals,
    pub ores: Vec<OreView>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub id: i64,
    pub label: String,
    pub start: i64,
    pub end: Option<i64>,
    pub policy: Preferences,
    pub totals: Totals,
    pub pilots: Vec<PilotView>,
    pub graph: Vec<GraphPoint>,
    pub detail_available: bool,
    #[serde(default)]
    pub coverage_incomplete: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphPoint {
    pub at: i64,
    pub ordinary: f64,
    pub bonus: f64,
    pub crits: u64,
    pub pilots: BTreeMap<String, f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub preferences: Preferences,
    pub defaults: Preferences,
    pub now: i64,
    pub armed: bool,
    pub session: Option<SessionView>,
    pub history: Vec<History>,
    pub discovered: Vec<Identity>,
    pub problems: Vec<String>,
    pub sources: usize,
    /// What each client is doing right now, from its game log.
    #[serde(default)]
    pub live: Vec<super::live::PilotStatus>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct History {
    pub id: i64,
    pub label: String,
    pub start: i64,
    pub end: Option<i64>,
    pub m3: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Command {
    Status,
    Start {
        label: Option<String>,
    },
    Stop,
    New {
        label: Option<String>,
        request_id: String,
    },
    Rename {
        id: i64,
        label: String,
    },
    Preferences {
        value: Box<Preferences>,
    },
    Session {
        id: i64,
    },
    Rescan,
}
