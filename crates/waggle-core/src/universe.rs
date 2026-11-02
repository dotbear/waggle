use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const BUNDLED_DATA: &str = include_str!("../assets/universe-v1.tsv");

/// Classification of a blueprint's manufactured product from CCP's SDE.
/// Rigs take precedence, including rigs whose blueprints come from invention.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ManufacturingProductKind {
    Invented,
    Rig,
    Other,
}

impl ManufacturingProductKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Invented => "invented",
            Self::Rig => "rig",
            Self::Other => "other",
        }
    }
}

/// Which kind of capital a hull is, from its SDE group.
///
/// Only the hulls a home-defence umbrella distinguishes between: what a
/// dreadnought is told to do at a given status differs from what a Rorqual is
/// told, and ratting carriers are not saved at all. Freighters are capital-sized
/// but nobody drops to rescue one, so they are not here.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapitalClass {
    Carrier,
    Dreadnought,
    ForceAuxiliary,
    Supercarrier,
    Titan,
    Rorqual,
}

impl CapitalClass {
    pub fn label(self) -> &'static str {
        match self {
            Self::Carrier => "carrier",
            Self::Dreadnought => "dreadnought",
            Self::ForceAuxiliary => "force-auxiliary",
            Self::Supercarrier => "supercarrier",
            Self::Titan => "titan",
            Self::Rorqual => "rorqual",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        [
            Self::Carrier,
            Self::Dreadnought,
            Self::ForceAuxiliary,
            Self::Supercarrier,
            Self::Titan,
            Self::Rorqual,
        ]
        .into_iter()
        .find(|class| class.label() == label)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct SolarSystem {
    pub id: u32,
    pub name: String,
    pub region_id: u32,
    pub region_name: String,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ShipType {
    pub id: u32,
    pub name: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SystemResolution {
    Exact(SolarSystem),
    UniquePrefix(SolarSystem),
}

impl SystemResolution {
    pub fn system(&self) -> &SolarSystem {
        match self {
            Self::Exact(system) | Self::UniquePrefix(system) => system,
        }
    }
}

/// How far apart two systems are, and how that distance was reached.
///
/// Whether a bridge was involved matters to the reader: hostiles generally
/// cannot use your alliance's jump bridges, so a bridge-adjacent report is a
/// different kind of news from a gate-adjacent one even at the same hop count.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Route {
    pub jumps: u16,
    pub via_bridge: bool,
}

#[derive(Debug)]
pub struct Universe {
    build_number: u64,
    systems: HashMap<u32, SolarSystem>,
    systems_by_name: HashMap<String, u32>,
    ship_types: HashMap<String, ShipType>,
    item_types: HashMap<u32, String>,
    manufacturing_products: HashMap<u32, ManufacturingProductKind>,
    container_types: HashSet<u32>,
    capital_classes: HashMap<u32, CapitalClass>,
    gates: HashMap<u32, Vec<u32>>,
    /// Alliance jump bridges. Not part of the generated CCP dataset: these are
    /// the operator's own infrastructure, supplied by configuration.
    bridges: HashMap<u32, Vec<u32>>,
}

impl Universe {
    pub fn bundled() -> Result<Self> {
        Self::from_tsv(BUNDLED_DATA)
    }

    pub fn from_tsv(source: &str) -> Result<Self> {
        let mut build_number = None;
        let mut regions = HashMap::<u32, String>::new();
        let mut raw_systems = Vec::<(u32, u32, String)>::new();
        let mut edges = HashSet::<(u32, u32)>::new();
        let mut ships = Vec::<ShipType>::new();
        let mut item_types = HashMap::<u32, String>::new();
        let mut manufacturing_products = HashMap::new();
        let mut container_types = HashSet::new();
        let mut capital_classes = HashMap::new();

        for (index, line) in source.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields = line.split('\t').collect::<Vec<_>>();
            let line_number = index + 1;
            match fields.first().copied() {
                Some("B") if fields.len() == 2 => {
                    build_number = Some(parse(fields[1], line_number, "build number")?);
                }
                Some("R") if fields.len() == 3 => {
                    let id = parse(fields[1], line_number, "region id")?;
                    regions.insert(id, fields[2].to_owned());
                }
                Some("S") if fields.len() == 4 => {
                    raw_systems.push((
                        parse(fields[1], line_number, "system id")?,
                        parse(fields[2], line_number, "system region id")?,
                        fields[3].to_owned(),
                    ));
                }
                Some("G") if fields.len() == 3 => {
                    let first = parse(fields[1], line_number, "gate origin")?;
                    let second = parse(fields[2], line_number, "gate destination")?;
                    if first != second {
                        edges.insert(ordered_pair(first, second));
                    }
                }
                Some("T") if fields.len() == 3 => ships.push(ShipType {
                    id: parse(fields[1], line_number, "ship type id")?,
                    name: fields[2].to_owned(),
                }),
                Some("I") if fields.len() == 3 => {
                    item_types.insert(
                        parse(fields[1], line_number, "item type id")?,
                        fields[2].to_owned(),
                    );
                }
                Some("C") if fields.len() == 2 => {
                    container_types.insert(parse(fields[1], line_number, "container type id")?);
                }
                Some("K") if fields.len() == 3 => {
                    let class = CapitalClass::from_label(fields[2])
                        .with_context(|| format!("invalid capital class at line {line_number}"))?;
                    capital_classes
                        .insert(parse(fields[1], line_number, "capital type id")?, class);
                }
                Some("P") if fields.len() == 3 => {
                    let kind = match fields[2] {
                        "invented" => ManufacturingProductKind::Invented,
                        "rig" => ManufacturingProductKind::Rig,
                        "other" => ManufacturingProductKind::Other,
                        _ => bail!("invalid manufacturing product kind at line {line_number}"),
                    };
                    manufacturing_products
                        .insert(parse(fields[1], line_number, "blueprint type id")?, kind);
                }
                _ => bail!("invalid universe data at line {line_number}"),
            }
        }

        let build_number = build_number.context("universe data has no build number")?;
        let systems = raw_systems
            .into_iter()
            .map(|(id, region_id, name)| {
                let region_name = regions
                    .get(&region_id)
                    .with_context(|| format!("system {id} references unknown region {region_id}"))?
                    .clone();
                Ok((
                    id,
                    SolarSystem {
                        id,
                        name,
                        region_id,
                        region_name,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let systems_by_name = systems
            .values()
            .map(|system| (system.name.to_lowercase(), system.id))
            .collect();
        let ship_types = ships
            .into_iter()
            .map(|ship| (ship.name.to_lowercase(), ship))
            .collect();
        let mut gates = HashMap::<u32, Vec<u32>>::new();
        for (first, second) in edges {
            gates.entry(first).or_default().push(second);
            gates.entry(second).or_default().push(first);
        }
        for destinations in gates.values_mut() {
            destinations.sort_unstable();
        }

        Ok(Self {
            build_number,
            systems,
            systems_by_name,
            ship_types,
            item_types,
            manufacturing_products,
            container_types,
            capital_classes,
            gates,
            bridges: HashMap::new(),
        })
    }

    pub fn build_number(&self) -> u64 {
        self.build_number
    }

    pub fn manufacturing_product_kind(&self, blueprint: u32) -> Option<ManufacturingProductKind> {
        self.manufacturing_products.get(&blueprint).copied()
    }

    pub fn is_container_type(&self, type_id: u32) -> bool {
        self.container_types.contains(&type_id)
    }

    /// The kind of capital a ship type is, or `None` for anything smaller.
    pub fn capital_class(&self, type_id: u32) -> Option<CapitalClass> {
        self.capital_classes.get(&type_id).copied()
    }

    pub fn system_count(&self) -> usize {
        self.systems.len()
    }

    pub fn ship_type_count(&self) -> usize {
        self.ship_types.len()
    }

    pub fn item_type_count(&self) -> usize {
        self.item_types.len()
    }

    /// One solar system by ID, for turning an ESI location into a place name.
    pub fn system(&self, id: u32) -> Option<&SolarSystem> {
        self.systems.get(&id)
    }

    pub fn item_type_name(&self, type_id: u32) -> Option<&str> {
        self.item_types.get(&type_id).map(String::as_str)
    }

    pub fn resolve_system(&self, input: &str, region_hints: &[String]) -> Option<SystemResolution> {
        let query = normalized_token(input);
        if query.is_empty() {
            return None;
        }

        if let Some(id) = self.systems_by_name.get(&query) {
            return self.systems.get(id).cloned().map(SystemResolution::Exact);
        }

        // Prefix matching only applies to tokens that plausibly abbreviate a
        // system name. Without this, a short ordinary word such as "in" or "up"
        // can uniquely prefix a real system and invent an intel location.
        if !is_plausible_system_abbreviation(&query) {
            return None;
        }
        let minimum_prefix = if region_hints.is_empty() { 3 } else { 2 };
        if query.len() < minimum_prefix {
            return None;
        }

        let candidates = self
            .systems
            .values()
            .filter(|system| system.name.to_lowercase().starts_with(&query))
            .filter(|system| {
                region_hints.is_empty()
                    || region_hints
                        .iter()
                        .any(|hint| hint.eq_ignore_ascii_case(&system.region_name))
            })
            .take(2)
            .cloned()
            .collect::<Vec<_>>();

        match candidates.as_slice() {
            [system] => Some(SystemResolution::UniquePrefix(system.clone())),
            _ => None,
        }
    }

    pub fn ships_in_text(&self, text: &str) -> Vec<ShipType> {
        let normalized = normalize_words(text);
        let haystack = format!(" {normalized} ");
        let mut matches = self
            .ship_types
            .iter()
            .filter(|(name, _)| haystack.contains(&format!(" {name} ")))
            .map(|(_, ship)| ship.clone())
            .collect::<Vec<_>>();
        matches.sort_by_key(|ship| std::cmp::Reverse(ship.name.len()));

        let mut retained = Vec::<ShipType>::new();
        for ship in matches {
            if !retained.iter().any(|existing| {
                existing
                    .name
                    .to_lowercase()
                    .contains(&ship.name.to_lowercase())
            }) {
                retained.push(ship);
            }
        }
        retained
    }

    /// Installs the operator's jump-bridge network, by system name.
    ///
    /// Returns a warning for every link that could not be resolved, so a
    /// renamed or misspelled system is reported rather than silently shrinking
    /// the network. Links are stored in both directions: a bridge is usable
    /// either way, and an asymmetric source list would otherwise produce
    /// distances that depend on which end you start from.
    pub fn set_jump_bridges<S: AsRef<str>>(&mut self, links: &[[S; 2]]) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut bridges = HashMap::<u32, Vec<u32>>::new();

        for link in links {
            let (left, right) = (link[0].as_ref(), link[1].as_ref());
            let resolve = |name: &str| {
                self.systems_by_name
                    .get(&name.trim().to_lowercase())
                    .copied()
            };
            match (resolve(left), resolve(right)) {
                (Some(first), Some(second)) if first == second => {
                    warnings.push(format!(
                        "jump bridge {left} <-> {right} links a system to itself"
                    ));
                }
                (Some(first), Some(second)) => {
                    bridges.entry(first).or_default().push(second);
                    bridges.entry(second).or_default().push(first);
                }
                (None, _) => warnings.push(format!("unknown jump bridge system {left:?}")),
                (_, None) => warnings.push(format!("unknown jump bridge system {right:?}")),
            }
        }

        for destinations in bridges.values_mut() {
            destinations.sort_unstable();
            destinations.dedup();
        }
        self.bridges = bridges;
        warnings
    }

    /// Number of one-way jump-bridge links currently installed.
    pub fn jump_bridge_count(&self) -> usize {
        self.bridges.values().map(Vec::len).sum::<usize>() / 2
    }

    /// Shortest route from `origin` to `destination`, at most `max_jumps` long.
    ///
    /// Gate-only and bridge-inclusive searches are run separately and the
    /// shorter wins. Doing it in one pass would need per-node tie-breaking to
    /// avoid labelling a purely-gate route as going via a bridge; two bounded
    /// searches are cheaper to read and, at these depths, cheaper to run.
    pub fn route(
        &self,
        origin: u32,
        destination: u32,
        max_jumps: u16,
        use_bridges: bool,
    ) -> Option<Route> {
        if origin == destination {
            return self.systems.contains_key(&origin).then_some(Route {
                jumps: 0,
                via_bridge: false,
            });
        }
        if !self.systems.contains_key(&origin) || !self.systems.contains_key(&destination) {
            return None;
        }

        let by_gate = self.search(origin, destination, max_jumps, false);
        if !use_bridges || self.bridges.is_empty() {
            return by_gate.map(|jumps| Route {
                jumps,
                via_bridge: false,
            });
        }
        let by_either = self.search(origin, destination, max_jumps, true);

        match (by_gate, by_either) {
            // A bridge only counts as "via bridge" when it actually got you
            // there sooner than the gates would have.
            (Some(gate), Some(either)) if either < gate => Some(Route {
                jumps: either,
                via_bridge: true,
            }),
            (Some(gate), _) => Some(Route {
                jumps: gate,
                via_bridge: false,
            }),
            (None, Some(either)) => Some(Route {
                jumps: either,
                via_bridge: true,
            }),
            (None, None) => None,
        }
    }

    /// Breadth-first search bounded by `max_jumps`.
    fn search(
        &self,
        origin: u32,
        destination: u32,
        max_jumps: u16,
        use_bridges: bool,
    ) -> Option<u16> {
        let mut queue = VecDeque::from([(origin, 0_u16)]);
        let mut visited = HashSet::from([origin]);

        while let Some((current, distance)) = queue.pop_front() {
            if distance >= max_jumps {
                continue;
            }
            let next = distance.checked_add(1)?;
            let gates = self.gates.get(&current).into_iter().flatten();
            let bridges = use_bridges
                .then(|| self.bridges.get(&current))
                .flatten()
                .into_iter()
                .flatten();
            for neighbour in gates.chain(bridges) {
                if *neighbour == destination {
                    return Some(next);
                }
                if visited.insert(*neighbour) {
                    queue.push_back((*neighbour, next));
                }
            }
        }
        None
    }
}

/// Nullsec and lowsec system names that get abbreviated in intel channels
/// almost always carry a digit or a hyphen ("q4c" for Q4C-S5). A purely
/// alphabetic token has to be long enough that it is unlikely to be an ordinary
/// English word, and must never be a known filler word.
fn is_plausible_system_abbreviation(query: &str) -> bool {
    const NEVER_A_SYSTEM: [&str; 24] = [
        "and", "any", "are", "back", "clear", "eyes", "for", "from", "gate", "gates", "hostile",
        "into", "just", "neut", "neuts", "not", "now", "out", "red", "reds", "seen", "the", "was",
        "with",
    ];

    if NEVER_A_SYSTEM.contains(&query) {
        return false;
    }
    query.chars().any(|character| character.is_ascii_digit())
        || query.contains('-')
        || query.len() >= 4
}

fn normalized_token(input: &str) -> String {
    input
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .to_lowercase()
}

fn normalize_words(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse<T>(value: &str, line: usize, field: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid {field} at universe data line {line}: {error}"))
}

fn ordered_pair(first: u32, second: u32) -> (u32, u32) {
    if first < second {
        (first, second)
    } else {
        (second, first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gate-only distance with a generous bound, for the plain graph tests.
    fn jumps(universe: &Universe, origin: u32, destination: u32) -> Option<u16> {
        universe
            .route(origin, destination, 20, false)
            .map(|route| route.jumps)
    }

    fn universe() -> Universe {
        Universe::from_tsv(
            "B\t123\n\
             R\t10\tExample Region\n\
             R\t11\tOther Region\n\
             S\t1\t10\tQ4C-S5\n\
             S\t2\t10\t4-CM8I\n\
             S\t3\t10\t1QZ-Y9\n\
             S\t4\t11\tQ4X-XX\n\
             G\t1\t2\n\
             G\t2\t3\n\
             T\t100\tSabre\n\
             T\t101\tFederation Navy Comet\n\
             I\t34\tTritanium\n",
        )
        .unwrap()
    }

    #[test]
    fn resolves_exact_names_case_insensitively() {
        let universe = universe();
        let resolved = universe.resolve_system("q4c-s5", &[]).unwrap();

        assert!(matches!(resolved, SystemResolution::Exact(_)));
        assert_eq!(resolved.system().id, 1);
    }

    #[test]
    fn resolves_a_unique_prefix_within_a_region_hint() {
        let universe = universe();
        let resolved = universe
            .resolve_system("q4", &["Example Region".to_owned()])
            .unwrap();

        assert!(matches!(resolved, SystemResolution::UniquePrefix(_)));
        assert_eq!(resolved.system().name, "Q4C-S5");
    }

    #[test]
    fn rejects_ambiguous_global_prefixes() {
        assert!(universe().resolve_system("q4", &[]).is_none());
    }

    #[test]
    fn ordinary_words_never_resolve_as_a_system_by_prefix() {
        let universe = Universe::from_tsv(
            "B\t123\n\
             R\t10\tExample Region\n\
             S\t1\t10\tINGA\n\
             S\t2\t10\tQ4C-S5\n\
             T\t100\tSabre\n",
        )
        .unwrap();
        let hint = ["Example Region".to_owned()];

        // "in" uniquely prefixes INGA, but it is a filler word, not intel.
        assert!(universe.resolve_system("in", &hint).is_none());
        assert!(universe.resolve_system("the", &hint).is_none());
        assert!(universe.resolve_system("reds", &hint).is_none());
        // A real abbreviation is still accepted.
        assert!(universe.resolve_system("q4c", &hint).is_some());
        assert!(universe.resolve_system("inga", &hint).is_some());
    }

    #[test]
    fn finds_multiword_ship_names_without_losing_the_longest_match() {
        let ships = universe().ships_in_text("two Federation Navy Comet, Sabre on gate");

        assert_eq!(
            ships
                .iter()
                .map(|ship| ship.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Federation Navy Comet", "Sabre"]
        );
    }

    #[test]
    fn calculates_shortest_gate_distance() {
        let universe = universe();

        assert_eq!(jumps(&universe, 1, 1), Some(0));
        assert_eq!(jumps(&universe, 1, 3), Some(2));
        assert_eq!(jumps(&universe, 1, 4), None);
    }

    /// A chain HOME-1 — NEAR-2 — MID-3 — FAR-4 plus an isolated OTHER-5, so a
    /// bridge can be the only way to reach somewhere.
    fn chain() -> Universe {
        Universe::from_tsv(
            "B\t123\n\
             R\t10\tExample Region\n\
             S\t1\t10\tHOME-1\n\
             S\t2\t10\tNEAR-2\n\
             S\t3\t10\tMID-3\n\
             S\t4\t10\tFAR-4\n\
             S\t5\t10\tOTHER-5\n\
             G\t1\t2\n\
             G\t2\t3\n\
             G\t3\t4\n\
             T\t100\tSabre\n",
        )
        .unwrap()
    }

    #[test]
    fn a_bridge_shortens_a_route_and_is_reported_as_such() {
        let mut universe = chain();
        universe.set_jump_bridges(&[["HOME-1", "FAR-4"]]);

        // Three gates the long way, one bridge the short way.
        assert_eq!(
            universe.route(1, 4, 3, false),
            Some(Route {
                jumps: 3,
                via_bridge: false
            })
        );
        assert_eq!(
            universe.route(1, 4, 3, true),
            Some(Route {
                jumps: 1,
                via_bridge: true
            })
        );
    }

    #[test]
    fn a_bridge_brings_an_out_of_range_system_into_range() {
        let mut universe = chain();
        universe.set_jump_bridges(&[["HOME-1", "FAR-4"]]);

        // This is the missed alert the feature exists to prevent: FAR-4 is
        // three gates away, so at max_jumps = 2 it is silent without bridges.
        assert_eq!(universe.route(1, 4, 2, false), None);
        assert_eq!(
            universe.route(1, 4, 2, true),
            Some(Route {
                jumps: 1,
                via_bridge: true
            })
        );
    }

    #[test]
    fn a_gate_route_is_never_mislabelled_as_going_via_a_bridge() {
        let mut universe = chain();
        // A bridge that parallels an existing gate hop.
        universe.set_jump_bridges(&[["HOME-1", "NEAR-2"]]);

        let route = universe.route(1, 2, 2, true).unwrap();

        assert_eq!(route.jumps, 1);
        assert!(
            !route.via_bridge,
            "the gates alone got there just as fast, so this is a gate route"
        );
    }

    #[test]
    fn a_bridge_is_usable_in_both_directions() {
        let mut universe = chain();
        universe.set_jump_bridges(&[["FAR-4", "HOME-1"]]);

        // Listed one way round; distance must not depend on which end starts.
        assert_eq!(universe.route(1, 4, 2, true), universe.route(4, 1, 2, true));
        assert_eq!(universe.route(1, 4, 2, true).unwrap().jumps, 1);
    }

    #[test]
    fn a_bridge_can_reach_a_system_with_no_gates_at_all() {
        let mut universe = chain();
        universe.set_jump_bridges(&[["HOME-1", "OTHER-5"]]);

        assert_eq!(universe.route(1, 5, 5, false), None);
        assert_eq!(
            universe.route(1, 5, 5, true),
            Some(Route {
                jumps: 1,
                via_bridge: true
            })
        );
    }

    #[test]
    fn bridges_are_ignored_when_the_operator_turns_them_off() {
        let mut universe = chain();
        universe.set_jump_bridges(&[["HOME-1", "FAR-4"]]);

        assert_eq!(universe.route(1, 4, 2, false), None);
    }

    #[test]
    fn an_unresolvable_link_is_reported_rather_than_dropped_silently() {
        let mut universe = chain();

        let warnings = universe.set_jump_bridges(&[
            ["HOME-1", "FAR-4"],
            ["HOME-1", "NOWHERE-9"],
            ["NOWHERE-9", "HOME-1"],
            // A link to itself is a data error, not a bridge.
            ["MID-3", "MID-3"],
        ]);

        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("NOWHERE-9")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("links a system to itself"))
        );
        // The good link still installed.
        assert_eq!(universe.jump_bridge_count(), 1);
    }

    #[test]
    fn bridge_names_resolve_case_insensitively_and_ignore_padding() {
        let mut universe = chain();

        let warnings = universe.set_jump_bridges(&[["  home-1 ", "Far-4"]]);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(universe.jump_bridge_count(), 1);
    }

    #[test]
    fn a_repeated_link_counts_once() {
        let mut universe = chain();

        universe.set_jump_bridges(&[
            ["HOME-1", "FAR-4"],
            ["FAR-4", "HOME-1"],
            ["HOME-1", "FAR-4"],
        ]);

        assert_eq!(universe.jump_bridge_count(), 1);
    }

    #[test]
    fn routing_respects_the_jump_limit() {
        let universe = chain();

        assert_eq!(universe.route(1, 4, 2, false), None);
        assert_eq!(universe.route(1, 4, 3, false).unwrap().jumps, 3);
        // A system is always zero jumps from itself, even at a limit of zero.
        assert_eq!(universe.route(1, 1, 0, false).unwrap().jumps, 0);
    }

    #[test]
    fn resolves_item_type_names_for_industry() {
        assert_eq!(universe().item_type_name(34), Some("Tritanium"));
        assert_eq!(universe().item_type_name(999), None);
    }

    #[test]
    fn bundled_data_recognizes_inventory_containers_without_their_contents() {
        let universe = Universe::bundled().unwrap();
        for type_id in [3297, 3467, 17363, 17366, 17367, 24445] {
            assert!(
                universe.is_container_type(type_id),
                "container type {type_id}"
            );
        }
        for type_id in [27, 34, 587, 2049, u32::MAX] {
            assert!(
                !universe.is_container_type(type_id),
                "non-container type {type_id}"
            );
        }
    }

    #[test]
    fn bundled_data_knows_which_hulls_are_capitals() {
        use CapitalClass::{Carrier, Dreadnought, ForceAuxiliary, Rorqual, Supercarrier, Titan};
        let universe = Universe::bundled().unwrap();
        for (type_id, expected) in [
            (73790, Dreadnought), // Revelation Navy Issue
            (77281, Dreadnought), // Hubris, a lancer
            (23757, Carrier),     // Archon
            (92822, Carrier),     // Salvation, a command carrier
            (37604, ForceAuxiliary),
            (23919, Supercarrier), // Aeon
            (671, Titan),          // Erebus
            (28352, Rorqual),
        ] {
            assert_eq!(universe.capital_class(type_id), Some(expected), "{type_id}");
        }
        // A Hulk, an Orca, a freighter and a jump freighter are not capitals
        // anybody drops to save.
        for type_id in [22544, 28606, 20183, 28844] {
            assert_eq!(universe.capital_class(type_id), None, "{type_id}");
        }
    }

    #[test]
    fn bundled_manufacturing_products_cover_invention_and_all_rigs() {
        use ManufacturingProductKind::{Invented, Other, Rig};
        let universe = Universe::bundled().unwrap();
        for (blueprint, expected) in [
            (22457, Invented), // Sabre: no "II" suffix to match.
            (24496, Invented), // Scourge Fury Light Missile.
            (2049, Invented),  // Damage Control II.
            (31789, Rig),      // Small Core Defense Field Extender I.
            (31795, Rig),      // Small Core Defense Field Extender II.
            (17355, Other),    // Fernite Carbide Composite Armor Plate: a T2 input.
            (691, Other),      // Rifter: inventable, but not itself an invention product.
            (1145, Other),     // Antimatter Charge M.
        ] {
            assert_eq!(
                universe.manufacturing_product_kind(blueprint),
                Some(expected),
                "blueprint {blueprint}"
            );
        }
        assert_eq!(universe.manufacturing_product_kind(u32::MAX), None);
    }

    #[test]
    fn bundled_data_is_a_real_generated_sde_subset() {
        let universe = Universe::bundled().unwrap();

        assert_eq!(universe.build_number(), 3_561_556);
        assert!(universe.system_count() > 8_000);
        assert!(universe.ship_type_count() > 400);
        assert!(universe.item_type_count() > 10_000);
        assert!(
            universe
                .resolve_system("Q4C-S5", &[])
                .is_some_and(|system| system.system().region_name == "Scalding Pass")
        );
        assert_eq!(universe.ships_in_text("Sabre on gate")[0].name, "Sabre");
    }
}
