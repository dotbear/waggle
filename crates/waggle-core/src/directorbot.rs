//! Reading Directorbot: fleet broadcasts and PAP replies.
//!
//! Pure text parsing, separate from the Jabber connection that delivers the
//! messages, so it can be tested against the synthetic corpus without a server
//! and shared by every front end.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use sha2::{Digest, Sha256};

/// Invisible characters stripped before a broadcast is parsed or fingerprinted.
///
/// Zero-width padding is both an ordinary artefact of copy-pasted broadcast
/// text and an easy way to make the same message look new. Leaving any of these
/// in would leak into a parsed field and change a ping's identity.
pub const INVISIBLE_CHARACTERS: [char; 7] = [
    '\u{00ad}', // soft hyphen
    '\u{200b}', // zero-width space
    '\u{200c}', // zero-width non-joiner
    '\u{200d}', // zero-width joiner
    '\u{2060}', // word joiner
    '\u{feff}', // zero-width no-break space / BOM
    '\u{2028}', // line separator, which is not a line break to `lines()`
];

/// Directorbot wraps its broadcast footer in these. The exact wording after
/// them varies, so the marker is what identifies a broadcast, not the sentence.
pub const FOOTER_MARKER: &str = "~~~";

#[derive(Debug, Clone)]
pub struct NewJabberPing {
    pub fingerprint: String,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub from: String,
    pub source: Option<String>,
    pub sender: Option<String>,
    pub target: Option<String>,
    pub summary: String,
    pub description: String,
    pub fc: Option<String>,
    pub fleet: Option<String>,
    pub formup: Option<String>,
    pub pap_type: Option<String>,
    pub comms: Option<String>,
    /// A joinable link from the comms field, when it carried one.
    ///
    /// Directorbot writes comms as a channel name followed by a URL. The URL
    /// is an HTML shim that redirects to a `mumble://` address, so it is
    /// resolved at join time rather than stored resolved: the shim is what the
    /// broadcaster published, and it can change behind the same link.
    pub comms_link: Option<String>,
    pub doctrine: Option<String>,
    pub body: String,
    /// Which fleet of the broadcast this is, from 1, and how many it
    /// announced. A single-fleet broadcast is 1 of 1.
    pub fleet_index: u32,
    pub fleet_count: u32,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PapStats {
    pub strategic: u32,
    pub peacetime: u32,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PapSnapshot {
    pub updated_at: DateTime<Utc>,
    pub month: PapStats,
    pub last_30_days: PapStats,
    pub last_90_days: PapStats,
    pub last_strat_at: Option<String>,
    pub last_strat_character: Option<String>,
}

/// Labels this parser knows how to read, for the diagnostic below.
const KNOWN_LABELS: [&str; 14] = [
    "account info",
    "comms",
    "doctrine",
    "fc",
    "fc name",
    "fleet",
    "fleet name",
    "formup",
    "formup location",
    "last 30 days",
    "last 90 days",
    "last strat-op",
    "pap type",
    "this month",
];

/// Which recognised labels a message used, and how many it used that this
/// parser does not know.
///
/// Only labels from the known list are named. A prose line such as
/// "Undock now: everyone" is shaped exactly like a label, so emitting
/// unrecognised ones would leak alliance communication into the journal in
/// order to diagnose a parser bug. The unknown *count* is enough to tell a
/// format change from an unremarkable chat message, and a sample can then be
/// supplied deliberately.
pub fn label_summary(body: &str) -> (Vec<&'static str>, usize) {
    let mut known: Vec<&'static str> = Vec::new();
    let mut unknown = 0;

    for line in body.lines() {
        let Some((label, _)) = line.trim().split_once(':') else {
            continue;
        };
        let label = label.trim().to_ascii_lowercase();
        if label.is_empty() {
            continue;
        }
        match KNOWN_LABELS.iter().find(|candidate| **candidate == label) {
            Some(matched) if !known.contains(matched) => known.push(matched),
            Some(_) => {}
            None => unknown += 1,
        }
    }
    (known, unknown)
}

/// The first fleet a broadcast announces, or `None` when it is not a ping.
///
/// A broadcast can announce several fleets; [`parse_pings`] returns one ping
/// for each. This is the first of them, which for the usual single-fleet
/// broadcast is the whole thing.
pub fn parse_ping(
    from: &str,
    body: &str,
    occurred_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
) -> Option<NewJabberPing> {
    parse_pings(from, body, occurred_at, received_at)
        .into_iter()
        .next()
}

/// The labels that describe one fleet.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FleetLabel {
    Fc,
    Fleet,
    Formup,
    PapType,
    Comms,
    Doctrine,
}

impl FleetLabel {
    fn of(label: &str) -> Option<Self> {
        Some(match label {
            "fc" | "fc name" => Self::Fc,
            "fleet" | "fleet name" => Self::Fleet,
            "formup" | "formup location" | "loc" => Self::Formup,
            "pap type" => Self::PapType,
            "comms" => Self::Comms,
            "doctrine" => Self::Doctrine,
            _ => return None,
        })
    }

    /// Whether this label coming round again means a second fleet.
    ///
    /// Comms is the exception: one fleet listing two channels is a fleet with
    /// two channels, not a second fleet with nothing but a channel.
    fn starts_a_fleet_when_repeated(self) -> bool {
        self != Self::Comms
    }
}

/// One fleet's fields, as a broadcast block lists them.
#[derive(Debug, Default, Clone)]
struct FleetBlock {
    fc: Option<String>,
    fleet: Option<String>,
    formup: Option<String>,
    pap_type: Option<String>,
    comms: Option<String>,
    comms_link: Option<String>,
    doctrine: Vec<String>,
}

impl FleetBlock {
    fn has(&self, label: FleetLabel) -> bool {
        match label {
            FleetLabel::Fc => self.fc.is_some(),
            FleetLabel::Fleet => self.fleet.is_some(),
            FleetLabel::Formup => self.formup.is_some(),
            FleetLabel::PapType => self.pap_type.is_some(),
            FleetLabel::Comms => self.comms.is_some(),
            FleetLabel::Doctrine => !self.doctrine.is_empty(),
        }
    }

    /// Records one label. Returns false when the label was already set, which
    /// only happens for one that does not start a new fleet.
    fn set(&mut self, label: FleetLabel, value: String) -> bool {
        let mut duplicate = false;
        match label {
            FleetLabel::Fc => keep_first(&mut self.fc, value, &mut duplicate),
            FleetLabel::Fleet => keep_first(&mut self.fleet, value, &mut duplicate),
            FleetLabel::Formup => keep_first(&mut self.formup, value, &mut duplicate),
            FleetLabel::PapType => keep_first(&mut self.pap_type, value, &mut duplicate),
            FleetLabel::Comms => {
                let (channel, link) = split_comms(&value);
                if self.comms_link.is_none() {
                    self.comms_link = link;
                }
                keep_first(&mut self.comms, channel, &mut duplicate)
            }
            FleetLabel::Doctrine => {
                if !value.is_empty() {
                    self.doctrine.push(value);
                }
                true
            }
        };
        !duplicate
    }
}

/// Whose a line of prose is: the broadcast's as a whole, or one fleet's.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Owner {
    Broadcast,
    Fleet(usize),
}

/// Every fleet a broadcast announces, one ping each.
///
/// Directorbot sends two or three fleets as one message: an opening line, then
/// a block of labels per fleet, each starting again at `FC Name:`. Read as one
/// ping that was the first fleet's FC and comms beside a description holding
/// the second fleet's labels as text, and no way into the second fleet's
/// comms at all. A label that comes round again is the start of the next
/// fleet.
///
/// Prose is the broadcast's when it comes before the first fleet, and a
/// fleet's when it sits directly above that fleet's labels ("fatigue does not
/// matter") or after them. Every fleet carries the broadcast's prose, since
/// that is the part saying what the fleets are for.
pub fn parse_pings(
    from: &str,
    body: &str,
    occurred_at: DateTime<Utc>,
    received_at: DateTime<Utc>,
) -> Vec<NewJabberPing> {
    let normalized = body.replace(INVISIBLE_CHARACTERS, "");
    // A broadcast is recognised either by Directorbot's trailing signature or
    // by carrying at least one structured label. Requiring the signature alone
    // would discard the whole ping — history and notification — the first time
    // that footer's wording changes.
    let captures = ping_signature().captures(&normalized);
    let source = captures.as_ref().and_then(|it| capture(it, "source"));
    let sender = captures.as_ref().and_then(|it| capture(it, "sender"));
    let target = captures.as_ref().and_then(|it| capture(it, "target"));

    let mut blocks: Vec<FleetBlock> = Vec::new();
    // Prose in the order it was written, each line with its owner. A blank is
    // an empty line: paragraph breaks are part of what a broadcast says.
    let mut prose: Vec<(Owner, String)> = Vec::new();
    // Lines since the last label, not yet given to anyone: whose they are
    // depends on what comes next.
    let mut pending: Vec<String> = Vec::new();
    let mut collecting_doctrine = false;

    // Hands the pending lines to the doctrine being collected, or to the
    // current fleet, or to the broadcast when no fleet has started.
    let settle = |pending: &mut Vec<String>,
                  blocks: &mut Vec<FleetBlock>,
                  prose: &mut Vec<(Owner, String)>,
                  collecting_doctrine: bool| {
        let lines = std::mem::take(pending);
        match blocks.last_mut() {
            Some(block) if collecting_doctrine => block.doctrine.extend(lines),
            Some(_) => {
                let owner = Owner::Fleet(blocks.len() - 1);
                prose.extend(lines.into_iter().map(|line| (owner, line)));
            }
            None => prose.extend(lines.into_iter().map(|line| (Owner::Broadcast, line))),
        }
    };

    for line in normalized.lines() {
        let trimmed = line.trim();
        // Stop at the footer however it is worded: everything from the marker
        // on is Directorbot's own attribution, not the broadcast.
        if trimmed.starts_with(FOOTER_MARKER) {
            break;
        }
        let labelled = trimmed.split_once(':').and_then(|(label, value)| {
            FleetLabel::of(&label.trim().to_ascii_lowercase()).map(|it| (it, value.trim()))
        });
        let Some((label, value)) = labelled else {
            pending.push(trimmed.to_owned());
            continue;
        };

        let new_fleet = blocks
            .last()
            .is_none_or(|block| label.starts_a_fleet_when_repeated() && block.has(label));
        if new_fleet {
            // The paragraph directly above a fleet's first label, with no blank
            // between them, is that fleet's own heading.
            let heading_from = pending
                .iter()
                .rposition(String::is_empty)
                .map_or(0, |blank| blank + 1);
            let heading = pending.split_off(heading_from);
            settle(&mut pending, &mut blocks, &mut prose, collecting_doctrine);
            blocks.push(FleetBlock::default());
            let owner = Owner::Fleet(blocks.len() - 1);
            prose.extend(heading.into_iter().map(|line| (owner, line)));
        } else {
            settle(&mut pending, &mut blocks, &mut prose, collecting_doctrine);
        }

        let block = blocks.last_mut().expect("a fleet was just started");
        if !block.set(label, value.to_owned()) {
            // A second comms channel for the same fleet is kept as written.
            prose.push((Owner::Fleet(blocks.len() - 1), trimmed.to_owned()));
        }
        collecting_doctrine = label == FleetLabel::Doctrine;
    }
    settle(&mut pending, &mut blocks, &mut prose, collecting_doctrine);

    // A broadcast is anything carrying Directorbot's footer marker, a
    // recognised label, or both. Requiring the *detailed* footer to parse
    // dropped a real broadcast whose wording differed from the one this parser
    // was written against — a missed fleet ping, which is the one failure this
    // whole path exists to prevent. The marker alone is enough to keep it; the
    // structured fields are a bonus.
    if captures.is_none() && blocks.is_empty() && !normalized.contains(FOOTER_MARKER) {
        return Vec::new();
    }

    let mut hasher = Sha256::new();
    hasher.update(from.as_bytes());
    hasher.update([0]);
    hasher.update(occurred_at.to_rfc3339().as_bytes());
    hasher.update([0]);
    hasher.update(normalized.as_bytes());
    let fingerprint = format!("{:x}", hasher.finalize());

    let fleet_count = blocks.len().max(1);
    // A broadcast with no labels at all is one ping with no fleet fields.
    if blocks.is_empty() {
        blocks.push(FleetBlock::default());
    }
    // A PAP type or formup said once, above every fleet, is said for all of
    // them. Who runs a fleet, what it flies and where it talks never are.
    let shared_pap = blocks.iter().find_map(|block| block.pap_type.clone());
    let shared_formup = blocks.iter().find_map(|block| block.formup.clone());

    blocks
        .into_iter()
        .enumerate()
        .map(|(index, block)| {
            let mut lines: Vec<String> = prose
                .iter()
                .filter(|(owner, _)| match owner {
                    Owner::Broadcast => true,
                    Owner::Fleet(of) => fleet_count == 1 || *of == index,
                })
                // "1st", "2nd": what the badge saying which fleet this is
                // already says.
                .filter(|(owner, line)| {
                    fleet_count == 1 || *owner == Owner::Broadcast || !is_fleet_ordinal(line)
                })
                .map(|(_, line)| line.clone())
                .collect();
            tidy_paragraphs(&mut lines);
            let mut doctrine = block.doctrine;
            while doctrine.first().is_some_and(String::is_empty) {
                doctrine.remove(0);
            }
            tidy_paragraphs(&mut doctrine);

            let (pap_type, formup) = if fleet_count == 1 {
                (block.pap_type, block.formup)
            } else {
                (
                    block.pap_type.or_else(|| shared_pap.clone()),
                    block.formup.or_else(|| shared_formup.clone()),
                )
            };
            let summary = block
                .fleet
                .clone()
                .or_else(|| lines.first().cloned())
                .or_else(|| block.fc.clone().map(|name| format!("Fleet with {name}")))
                .unwrap_or_else(|| "Directorbot broadcast".to_owned());

            NewJabberPing {
                // The first fleet keeps the broadcast's own fingerprint, so a
                // broadcast stored before fleets were split is still known
                // when the server replays it.
                fingerprint: if index == 0 {
                    fingerprint.clone()
                } else {
                    format!("{fingerprint}:fleet-{}", index + 1)
                },
                occurred_at,
                received_at,
                from: from.to_owned(),
                source: source.clone(),
                sender: sender.clone(),
                target: target.clone(),
                summary,
                description: lines.join("\n"),
                fc: block.fc,
                fleet: block.fleet,
                formup,
                pap_type,
                comms: block.comms,
                comms_link: block.comms_link,
                doctrine: (!doctrine.is_empty()).then(|| doctrine.join("\n")),
                body: normalized.clone(),
                fleet_index: u32::try_from(index + 1).unwrap_or(u32::MAX),
                fleet_count: u32::try_from(fleet_count).unwrap_or(u32::MAX),
            }
        })
        .collect()
}

/// Collapses runs of blank lines to one and drops them from both ends. A
/// label line between two paragraphs leaves a blank on each side of it, and
/// the label is drawn as its own field, so the gap it left behind is not a
/// paragraph break the sender wrote.
fn tidy_paragraphs(lines: &mut Vec<String>) {
    lines.dedup_by(|next, previous| next.is_empty() && previous.is_empty());
    while lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
}

/// A line that only numbers a fleet: "1st", "2nd fleet", "Fleet 2", "#3".
fn is_fleet_ordinal(line: &str) -> bool {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(
                r"(?i)^\s*(#?\d+(st|nd|rd|th)?|first|second|third|fourth|fifth)(\s+fleet)?\s*[:.)\-]?\s*$|^\s*fleet\s+#?\d+\s*[:.)\-]?\s*$",
            )
            .expect("valid fleet ordinal regex")
        })
        .is_match(line)
}

/// Splits a comms field into its channel name and its joinable link.
///
/// Matched on "ends with an https URL" rather than on a particular host: the
/// shape is what Directorbot publishes, and hard-coding one alliance's domain
/// would both narrow the parser and put that domain in the repository.
fn split_comms(value: &str) -> (String, Option<String>) {
    let trimmed = value.trim();
    let Some(position) = trimmed.rfind("https://") else {
        return (trimmed.to_owned(), None);
    };
    let (channel, link) = trimmed.split_at(position);
    let link = link.trim();
    // A URL with whitespace in it is not a URL, and a comms field that is
    // nothing but a link still names no channel.
    if link.split_whitespace().count() != 1 {
        return (trimmed.to_owned(), None);
    }
    let channel = channel.trim();
    // Two URLs means the field is ambiguous about which one is the comms
    // link, and guessing wrong sends someone to the wrong channel.
    if channel.contains("https://") {
        return (trimmed.to_owned(), None);
    }
    (
        if channel.is_empty() {
            trimmed.to_owned()
        } else {
            channel.to_owned()
        },
        Some(link.to_owned()),
    )
}

pub fn parse_paps(body: &str, updated_at: DateTime<Utc>) -> Option<PapSnapshot> {
    if !body.contains("Account Info:") {
        return None;
    }
    Some(PapSnapshot {
        updated_at,
        month: parse_pap_stats(body, pap_month())?,
        last_30_days: parse_pap_stats(body, pap_30_days())?,
        last_90_days: parse_pap_stats(body, pap_90_days())?,
        last_strat_at: last_strat()
            .captures(body)
            .and_then(|captures| capture(&captures, "date")),
        last_strat_character: last_strat()
            .captures(body)
            .and_then(|captures| capture(&captures, "character")),
    })
}

/// Describes a PAP reply without retaining counts, names or message text.
#[derive(Debug, PartialEq)]
pub struct PapReplyShape {
    pub month_parsed: bool,
    pub days_30_parsed: bool,
    pub days_90_parsed: bool,
}

pub fn pap_reply_shape(body: &str) -> Option<PapReplyShape> {
    body.contains("Account Info:").then(|| PapReplyShape {
        month_parsed: parse_pap_stats(body, pap_month()).is_some(),
        days_30_parsed: parse_pap_stats(body, pap_30_days()).is_some(),
        days_90_parsed: parse_pap_stats(body, pap_90_days()).is_some(),
    })
}

fn parse_pap_stats(body: &str, regex: &Regex) -> Option<PapStats> {
    let captures = regex.captures(body)?;
    Some(PapStats {
        strategic: captures.name("str")?.as_str().parse().ok()?,
        peacetime: captures.name("pct")?.as_str().parse().ok()?,
    })
}

fn capture(captures: &regex::Captures<'_>, name: &str) -> Option<String> {
    captures
        .name(name)
        .map(|value| value.as_str().trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

/// Stores `value` only if the field is still unset, reporting a repeat so the
/// caller can preserve it instead of losing it.
fn keep_first(field: &mut Option<String>, value: String, duplicate: &mut bool) -> bool {
    match nonempty(value) {
        Some(value) if field.is_none() => *field = Some(value),
        Some(_) => *duplicate = true,
        None => {}
    }
    true
}

fn ping_signature() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        // `(?s)` lets the footer wrap across lines, which it does whenever a
        // client reflows a long broadcast, and the trailing `~~~` is optional so
        // a truncated footer still yields the sender and target. The source
        // ("coalition", "coord") is optional: a plain "This was a broadcast
        // from" is the commonest footer of all.
        Regex::new(
            r"(?is)~~~\s*This was an?\s+(?:(?P<source>.+?)\s+)?broadcast\s+from\s+(?P<sender>.+?)\s+to\s+(?P<target>.+?)\s+at\b",
        )
        .expect("valid Directorbot signature regex")
    })
}

fn pap_month() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| pap_regex("This Month"))
}

fn pap_30_days() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| pap_regex("Last 30 Days"))
}

fn pap_90_days() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| pap_regex("Last 90 Days"))
}

fn pap_regex(label: &str) -> Regex {
    // Directorbot prefixes these lines with "-> ". Like RIFT, find the
    // label within the line instead of requiring it at the start. Keep the
    // separator lazy so a multi-digit PCT count is captured in full, and
    // allow trailing notes after it.
    Regex::new(&format!(
        r"(?i)\b{}:\s*(?P<str>\d+)\s+STR[^\r\n]*?(?P<pct>\d+)\s+PCT",
        regex::escape(label)
    ))
    .expect("valid PAP statistics regex")
}

fn last_strat() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?im)\bLast strat-op:\s*(?P<date>.+?)\s+with\s+(?P<character>[^\r\n]+?)\s*$")
            .expect("valid last-strat regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_structured_directorbot_ping() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "Form fleet and bring tackle.\nFC Name: Space Boss\nFleet name: Important Fleet\nFormup Location: Example Keepstar\nPAP Type: Strategic\nComms: Fleet 1\nDoctrine: Fast ships\nLogistics welcome\n~~~ This was a coalition broadcast from Broadcaster to pilots at 2026-09-03 12:34:56 ~~~";

        let ping = parse_ping("directorbot@example.com", body, at, at).unwrap();

        assert_eq!(ping.summary, "Important Fleet");
        assert_eq!(ping.fc.as_deref(), Some("Space Boss"));
        assert_eq!(ping.formup.as_deref(), Some("Example Keepstar"));
        assert_eq!(ping.pap_type.as_deref(), Some("Strategic"));
        assert_eq!(
            ping.doctrine.as_deref(),
            Some("Fast ships\nLogistics welcome")
        );
        assert_eq!(ping.source.as_deref(), Some("coalition"));
        assert_eq!(ping.sender.as_deref(), Some("Broadcaster"));
        assert_eq!(ping.target.as_deref(), Some("pilots"));
    }

    #[test]
    fn a_comms_field_splits_into_a_channel_and_a_joinable_link() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "Comms: Synthetic 1 https://example.invalid/c/abc.html\n\
                    ~~~ This was a coalition broadcast from A to B at noon ~~~";

        let ping = parse_ping("directorbot@example.com", body, at, at).unwrap();

        assert_eq!(ping.comms.as_deref(), Some("Synthetic 1"));
        assert_eq!(
            ping.comms_link.as_deref(),
            Some("https://example.invalid/c/abc.html")
        );
    }

    #[test]
    fn a_comms_field_without_a_link_keeps_its_whole_text() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "Comms: Synthetic 1\n~~~ This was a coalition broadcast from A to B at noon ~~~";

        let ping = parse_ping("directorbot@example.com", body, at, at).unwrap();

        assert_eq!(ping.comms.as_deref(), Some("Synthetic 1"));
        assert!(ping.comms_link.is_none());
    }

    #[test]
    fn comms_text_that_merely_mentions_a_url_is_not_treated_as_a_link() {
        for value in [
            "see https://example.invalid/a b",
            "https://one.invalid https://two.invalid",
        ] {
            let (_, link) = split_comms(value);
            assert!(link.is_none(), "{value:?} should not yield a link");
        }
    }

    #[test]
    fn a_bare_link_still_yields_a_link_and_a_label() {
        let (channel, link) = split_comms("https://example.invalid/c/abc.html");

        assert_eq!(link.as_deref(), Some("https://example.invalid/c/abc.html"));
        assert!(!channel.is_empty());
    }

    #[test]
    fn ignores_plain_direct_messages() {
        let at = Utc::now();
        assert!(parse_ping("directorbot@example.com", "hello", at, at).is_none());
    }

    #[test]
    fn a_footer_this_parser_cannot_read_still_yields_a_ping() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        // Observed in the wild: three lines, a footer marker, and wording this
        // parser's signature regex does not match. It used to be dropped whole.
        let body = "Undock now, form on me\n\
                    Staging: Synthetic Keepstar\n\
                    ~~~ Sent by Synthetic Broadcaster via some other route ~~~";

        let ping = parse_ping("directorbot@example.com", body, at, at)
            .expect("a footer marker is enough to keep a broadcast");

        // The structured fields are absent, which is honest, but the broadcast
        // itself survives and can notify.
        assert!(ping.source.is_none());
        assert!(ping.sender.is_none());
        assert!(ping.body.contains("Undock now"));
        assert!(ping.description.contains("Undock now"));
        // The footer is attribution, not content, so it stays out of the
        // description regardless of its wording.
        assert!(!ping.description.contains("Sent by"));
    }

    #[test]
    fn keeps_a_broadcast_whose_signature_is_missing_or_reworded() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();

        // No trailing signature at all, but clearly a structured broadcast.
        let unsigned = parse_ping(
            "directorbot@example.com",
            "FC Name: Space Boss\nFormup Location: Example Keepstar\nUndock now",
            at,
            at,
        )
        .expect("a labelled broadcast must never be discarded");
        assert_eq!(unsigned.fc.as_deref(), Some("Space Boss"));
        assert!(unsigned.source.is_none());

        // Signature present but reflowed across lines and truncated.
        let wrapped = parse_ping(
            "directorbot@example.com",
            "Undock now\n~~~ This was an alliance broadcast\nfrom Broadcaster to pilots at",
            at,
            at,
        )
        .expect("a wrapped signature must still parse");
        assert_eq!(wrapped.source.as_deref(), Some("alliance"));
        assert_eq!(wrapped.target.as_deref(), Some("pilots"));
    }

    /// The shape a non-fleet broadcast arrives in: sent through the API to an
    /// opt-in group, with a Discord-style sender handle and none of the fleet
    /// labels. It carries no fields to key on, so the parser has to keep the
    /// prose intact — the prose is the entire message.
    #[test]
    fn an_opt_in_broadcast_keeps_its_paragraphs_and_names_its_target() {
        let at = "2026-09-04T18:21:12Z".parse().unwrap();
        let body = "Recruiting now!\n\
                    \n\
                    We are looking for people to fill these roles:\n\
                    Camera operator\n\
                    Commentator\n\
                    \n\
                    Reach out if that sounds like you.\n\
                    \n\
                    ~~~ This was a broadcast-api broadcast from Someone#0 to \
                    opt-group at 2026-09-04 18:21:12.878329 EVE ~~~";

        let ping = parse_ping("directorbot@example.com", body, at, at)
            .expect("an opt-in broadcast is still a broadcast");

        assert_eq!(ping.source.as_deref(), Some("broadcast-api"));
        assert_eq!(ping.sender.as_deref(), Some("Someone#0"));
        assert_eq!(ping.target.as_deref(), Some("opt-group"));
        // Not a fleet, so every fleet field is empty rather than guessed at.
        assert!(ping.fc.is_none());
        assert!(ping.formup.is_none());
        assert!(ping.pap_type.is_none());
        assert!(ping.comms.is_none());
        assert!(ping.doctrine.is_none());

        // The summary is only the first line, so everything after it has to
        // survive in the description or the message is lost.
        assert_eq!(ping.summary, "Recruiting now!");
        assert!(ping.description.contains("Camera operator"));
        assert!(ping.description.contains("Reach out"));
        // Written as paragraphs and kept as paragraphs.
        assert!(
            ping.description
                .contains("Recruiting now!\n\nWe are looking")
        );
        // The footer's own blank line is not a paragraph the sender wrote.
        assert!(!ping.description.ends_with('\n'));
    }

    /// A one-line fleet ping's description must stay exactly one line, or
    /// every card in the panel grows an empty paragraph under it.
    #[test]
    fn a_single_line_broadcast_gains_no_blank_lines() {
        let at = "2026-09-04T12:48:55Z".parse().unwrap();
        let body = "wheeee\n\
                    \n\
                    FC Name: Space Boss\n\
                    Formup Location: Example Keepstar\n\
                    \n\
                    ~~~ This was a skirmishbot broadcast from boss to all at now ~~~";

        let ping = parse_ping("directorbot@example.com", body, at, at)
            .expect("a labelled broadcast parses");

        assert_eq!(ping.description, "wheeee");
        assert_eq!(ping.summary, "wheeee");
    }

    #[test]
    fn a_second_fleet_is_a_second_ping() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "FC: First Boss\nFleet: First Fleet\nFC: Second Boss\nFleet: Second Fleet\n\
                    ~~~ This was a coalition broadcast from Broadcaster to pilots at noon ~~~";

        let pings = parse_pings("directorbot@example.com", body, at, at);

        assert_eq!(pings.len(), 2);
        assert_eq!(pings[0].fc.as_deref(), Some("First Boss"));
        assert_eq!(pings[0].fleet.as_deref(), Some("First Fleet"));
        assert_eq!(pings[1].fc.as_deref(), Some("Second Boss"));
        assert_eq!(pings[1].fleet.as_deref(), Some("Second Fleet"));
        // Neither fleet's labels leak into the other's text.
        assert!(!pings[0].description.contains("Second"));
        assert_eq!((pings[1].fleet_index, pings[1].fleet_count), (2, 2));
        // The first keeps the broadcast's fingerprint, so a broadcast stored
        // as one row before fleets were split is still recognised on replay.
        let whole = parse_ping("directorbot@example.com", body, at, at).unwrap();
        assert_eq!(whole.fingerprint, pings[0].fingerprint);
        assert_ne!(pings[0].fingerprint, pings[1].fingerprint);
    }

    /// The shape Directorbot actually sends: an opening line, then one block
    /// per fleet, each with its own FC, comms link and doctrine.
    const TWO_FLEETS: &str = "Two subcap fleets\n\
        \n\
        FC Name: Example Alpha\n\
        Formup Location: Example Staging\n\
        PAP Type: Strategic\n\
        Comms: Op 2 https://example.invalid/a.html\n\
        Doctrine: Example Battleships (Boosters > Logi > DPS)\n\
        \n\
        stay aligned\n\
        FC Name: Example Bravo\n\
        Formup Location: Example Staging\n\
        PAP Type: Strategic\n\
        Comms: Op 3 https://example.invalid/b.html\n\
        Doctrine: Example Cruisers\n\
        \n\
        ~~~ This was a broadcast from example_sender to all at 2026-10-31 19:06:16 EVE ~~~";

    #[test]
    fn each_fleet_has_its_own_comms_and_doctrine() {
        let at = "2026-10-31T19:06:16Z".parse().unwrap();
        let pings = parse_pings("directorbot@example.com", TWO_FLEETS, at, at);

        assert_eq!(pings.len(), 2);
        assert_eq!(pings[0].comms.as_deref(), Some("Op 2"));
        assert_eq!(
            pings[0].comms_link.as_deref(),
            Some("https://example.invalid/a.html")
        );
        assert_eq!(pings[1].comms.as_deref(), Some("Op 3"));
        assert_eq!(
            pings[1].comms_link.as_deref(),
            Some("https://example.invalid/b.html")
        );
        assert_eq!(
            pings[0].doctrine.as_deref(),
            Some("Example Battleships (Boosters > Logi > DPS)")
        );
        assert_eq!(pings[1].doctrine.as_deref(), Some("Example Cruisers"));
        // Both carry the opening line, which says what the fleets are for.
        assert_eq!(pings[0].summary, "Two subcap fleets");
        assert_eq!(pings[1].summary, "Two subcap fleets");
        assert_eq!(pings[0].sender.as_deref(), Some("example_sender"));
    }

    #[test]
    fn a_line_directly_above_a_fleet_is_that_fleets_not_the_doctrine_before_it() {
        let at = "2026-10-31T19:06:16Z".parse().unwrap();
        let pings = parse_pings("directorbot@example.com", TWO_FLEETS, at, at);

        // "stay aligned" sits between the first fleet's doctrine and the
        // second fleet's FC. Read as doctrine continuation it told the first
        // fleet something meant for the second.
        assert_eq!(pings[1].description, "Two subcap fleets\n\nstay aligned");
        assert!(!pings[0].description.contains("stay aligned"));
        assert!(
            !pings[0]
                .doctrine
                .as_deref()
                .unwrap()
                .contains("stay aligned")
        );
    }

    #[test]
    fn fleet_numbering_lines_are_left_to_the_badge() {
        let at = "2026-11-01T17:00:24Z".parse().unwrap();
        let body = "Cap fleets for the move\n\n1st\nFC Name: Example Alpha\nDoctrine: Carriers\n\n\
                    2nd\nFC Name: Example Bravo\nDoctrine: Dreads\n\n\
                    ~~~ This was a broadcast from example_sender to all at 2026-11-01 17:00:24 EVE ~~~";

        let pings = parse_pings("directorbot@example.com", body, at, at);

        assert_eq!(pings.len(), 2);
        assert_eq!(pings[0].description, "Cap fleets for the move");
        assert_eq!(pings[1].description, "Cap fleets for the move");
        assert_eq!(pings[0].doctrine.as_deref(), Some("Carriers"));
        assert_eq!(pings[1].doctrine.as_deref(), Some("Dreads"));
    }

    #[test]
    fn a_pap_type_said_once_is_said_for_every_fleet() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "FC: First Boss\nPAP Type: Strategic\nFC: Second Boss\n\
                    ~~~ This was a coalition broadcast from Broadcaster to pilots at noon ~~~";

        let pings = parse_pings("directorbot@example.com", body, at, at);

        assert_eq!(pings[1].pap_type.as_deref(), Some("Strategic"));
    }

    #[test]
    fn two_comms_lines_in_one_fleet_are_not_two_fleets() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "FC: Boss\nComms: Op 1 https://example.invalid/1.html\nComms: Op 2\n\
                    ~~~ This was a coalition broadcast from Broadcaster to pilots at noon ~~~";

        let pings = parse_pings("directorbot@example.com", body, at, at);

        assert_eq!(pings.len(), 1);
        assert_eq!(pings[0].comms.as_deref(), Some("Op 1"));
        assert!(pings[0].description.contains("Comms: Op 2"));
    }

    #[test]
    fn a_text_only_broadcast_is_one_ping() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "Big timers tonight, everyone there\n\n\
                    ~~~ This was a broadcast from example_sender to all at noon ~~~";

        let pings = parse_pings("directorbot@example.com", body, at, at);

        assert_eq!(pings.len(), 1);
        assert_eq!((pings[0].fleet_index, pings[0].fleet_count), (1, 1));
        assert_eq!(pings[0].fc, None);
        assert_eq!(pings[0].description, "Big timers tonight, everyone there");
    }

    #[test]
    fn normalizes_invisible_characters_before_fingerprinting() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let clean = "Fleet: Example\n~~~ This was a coalition broadcast from A to B at noon ~~~";
        let obscured =
            "Fleet:\u{200d} Example\n~~~ This was a coalition broadcast from A to B at noon ~~~";

        let clean = parse_ping("directorbot@example.com", clean, at, at).unwrap();
        let obscured = parse_ping("directorbot@example.com", obscured, at, at).unwrap();

        assert_eq!(clean.fingerprint, obscured.fingerprint);
    }

    #[test]
    fn every_invisible_character_is_stripped_before_parsing() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let footer = "~~~ This was a coalition broadcast from A to B at noon ~~~";
        let clean = parse_ping(
            "directorbot@example.com",
            &format!("Fleet: Example\n{footer}"),
            at,
            at,
        )
        .unwrap();

        for invisible in INVISIBLE_CHARACTERS {
            let padded = parse_ping(
                "directorbot@example.com",
                &format!("Fleet:{invisible} Exam{invisible}ple\n{footer}"),
                at,
                at,
            )
            .unwrap_or_else(|| panic!("U+{:04X} broke parsing", invisible as u32));

            assert_eq!(
                padded.fleet.as_deref(),
                Some("Example"),
                "U+{:04X} leaked into the field",
                invisible as u32
            );
            assert_eq!(
                padded.fingerprint, clean.fingerprint,
                "U+{:04X} defeated deduplication",
                invisible as u32
            );
        }
    }

    #[test]
    fn the_unrecognised_warning_names_only_labels_it_already_knows() {
        let body = "Account Info:\nThis Month: 4 STR\nOperation Sunrise: strike at 0300";

        let (known, unknown) = label_summary(body);

        assert_eq!(known, vec!["account info", "this month"]);
        // The prose line is counted, never quoted.
        assert_eq!(unknown, 1);
    }

    #[test]
    fn prose_shaped_like_a_label_never_reaches_the_diagnostic() {
        let body = "Undock now: everyone, and I mean everyone\nFC: Boss";

        let (known, unknown) = label_summary(body);

        assert_eq!(known, vec!["fc"]);
        assert_eq!(unknown, 1);
        assert!(!known.iter().any(|label| label.contains("undock")));
    }

    #[test]
    fn a_repeated_label_is_named_once() {
        let (known, _) = label_summary("FC: One\nFC: Two\nFleet: Three");

        assert_eq!(known, vec!["fc", "fleet"]);
    }

    #[test]
    fn pap_lines_parse_with_trailing_text_after_the_counts() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        // Anchoring the line end used to lose the whole reply over a suffix.
        let body = "Account Info:\n\
                    This Month: 12 STR / 3 PCT (top 10%)\n\
                    Last 30 Days: 20 STR and 4 PCT\n\
                    Last 90 Days: 55 STR, 8 PCT -- keep it up\n\
                    Last strat-op: 2026-09-01 20:00 with Example Pilot";

        let paps = parse_paps(body, at).unwrap();

        assert_eq!(paps.month.strategic, 12);
        assert_eq!(paps.month.peacetime, 3);
        assert_eq!(paps.last_90_days.peacetime, 8);
        assert_eq!(paps.last_strat_character.as_deref(), Some("Example Pilot"));
    }

    #[test]
    fn pap_lines_parse_when_indented() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "Account Info:\n  This Month: 1 STR / 2 PCT\n  Last 30 Days: 3 STR / 4 PCT\n  Last 90 Days: 5 STR / 6 PCT";

        let paps = parse_paps(body, at).unwrap();

        assert_eq!(paps.month.strategic, 1);
        assert_eq!(paps.last_90_days.peacetime, 6);
    }

    #[test]
    fn parses_pap_totals() {
        let at = "2026-09-03T12:34:56Z".parse().unwrap();
        let body = "Account Info:\nThis Month: 12 STR / 3 PCT\nLast 30 Days: 20 STR and 4 PCT\nLast 90 Days: 55 STR, 8 PCT\nLast strat-op: 2026-09-01 20:00 with Example Pilot";

        let paps = parse_paps(body, at).unwrap();

        assert_eq!(paps.month.strategic, 12);
        assert_eq!(paps.month.peacetime, 3);
        assert_eq!(paps.last_30_days.strategic, 20);
        assert_eq!(paps.last_90_days.peacetime, 8);
        assert_eq!(paps.last_strat_character.as_deref(), Some("Example Pilot"));
    }
}
