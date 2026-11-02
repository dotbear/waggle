//! The Directorbot parser against a corpus of message shapes.
//!
//! Directorbot's output is not a documented contract, so the risk is format
//! drift silently turning a fleet ping into nothing at all. Each fixture pins
//! one shape; all of them are synthetic (see `fixtures/directorbot/README.md`).

use chrono::{DateTime, TimeDelta, Utc};
use waggle_core::directorbot::{parse_paps, parse_ping, parse_pings};

const FROM: &str = "directorbot@example.invalid";

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/directorbot")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn at() -> DateTime<Utc> {
    "2026-09-03T12:34:56Z".parse().unwrap()
}

#[test]
fn a_standard_broadcast_yields_every_structured_field() {
    let ping = parse_ping(FROM, &fixture("standard_ping.txt"), at(), at()).unwrap();

    assert_eq!(ping.summary, "Synthetic Home Defence");
    assert_eq!(ping.fc.as_deref(), Some("Synthetic Boss"));
    assert_eq!(ping.formup.as_deref(), Some("Synthetic Keepstar"));
    assert_eq!(ping.pap_type.as_deref(), Some("Strategic"));
    assert_eq!(ping.comms.as_deref(), Some("Synthetic 1"));
    assert_eq!(
        ping.doctrine.as_deref(),
        Some("Fast tackle\nLogistics welcome")
    );
    assert_eq!(ping.source.as_deref(), Some("coalition"));
    assert_eq!(ping.sender.as_deref(), Some("Synthetic Broadcaster"));
    assert_eq!(ping.target.as_deref(), Some("pilots"));
    assert_eq!(ping.description, "Undock and bring tackle.");
    // The raw text is always retained so nothing is lost to a parse decision.
    assert!(ping.body.contains("Undock and bring tackle."));
}

#[test]
fn a_broadcast_without_a_signature_is_still_a_ping() {
    let ping = parse_ping(FROM, &fixture("no_signature.txt"), at(), at())
        .expect("labelled broadcasts must not be discarded when the footer changes");

    assert_eq!(ping.fc.as_deref(), Some("Synthetic Boss"));
    assert_eq!(ping.formup.as_deref(), Some("Synthetic Keepstar"));
    assert!(ping.source.is_none());
    assert!(ping.body.contains("Undock now"));
}

#[test]
fn a_signature_split_across_lines_still_identifies_its_sender() {
    let ping = parse_ping(FROM, &fixture("wrapped_signature.txt"), at(), at()).unwrap();

    assert_eq!(ping.source.as_deref(), Some("alliance"));
    assert_eq!(ping.sender.as_deref(), Some("Synthetic Broadcaster"));
    assert_eq!(ping.target.as_deref(), Some("pilots"));
}

#[test]
fn a_two_fleet_broadcast_is_two_pings() {
    let pings = parse_pings(FROM, &fixture("multi_fleet.txt"), at(), at());

    // Merging both fleets into one set of fields would produce a plausible
    // looking ping with the wrong FC against the wrong fleet.
    assert_eq!(pings.len(), 2);
    assert_eq!(pings[0].fc.as_deref(), Some("First Synthetic Boss"));
    assert_eq!(pings[0].fleet.as_deref(), Some("First Synthetic Fleet"));
    assert_eq!(pings[0].formup.as_deref(), Some("Synthetic Keepstar"));
    assert_eq!(pings[1].fc.as_deref(), Some("Second Synthetic Boss"));
    assert_eq!(pings[1].fleet.as_deref(), Some("Second Synthetic Fleet"));
    assert_eq!(pings[1].formup.as_deref(), Some("Other Synthetic Keepstar"));
    // Only the first fleet states a PAP type, and it is the broadcast's.
    assert_eq!(pings[1].pap_type.as_deref(), Some("Strategic"));
    assert!(!pings[0].description.contains("Second Synthetic"));
}

#[test]
fn an_ordinary_bot_reply_is_not_a_ping() {
    assert!(parse_ping(FROM, &fixture("not_a_ping.txt"), at(), at()).is_none());
}

#[test]
fn invisible_characters_do_not_change_a_pings_identity() {
    let obscured = parse_ping(FROM, &fixture("obscured_ping.txt"), at(), at()).unwrap();
    let clean = parse_ping(
        FROM,
        &fixture("obscured_ping.txt").replace(['\u{200b}', '\u{200d}', '\u{feff}'], ""),
        at(),
        at(),
    )
    .unwrap();

    assert_eq!(obscured.fc.as_deref(), Some("Synthetic Boss"));
    assert_eq!(obscured.fleet.as_deref(), Some("Synthetic Home Defence"));
    assert_eq!(
        obscured.fingerprint, clean.fingerprint,
        "zero-width padding must not defeat deduplication"
    );
}

#[test]
fn the_same_broadcast_replayed_after_a_reconnect_keeps_its_fingerprint() {
    let body = fixture("standard_ping.txt");
    let occurred = at();

    // A server replaying history stamps the original send time in a delay
    // element, so `occurred_at` is stable even though `received_at` is not.
    let live = parse_ping(FROM, &body, occurred, occurred).unwrap();
    let replayed = parse_ping(FROM, &body, occurred, occurred + TimeDelta::hours(2)).unwrap();

    assert_eq!(live.fingerprint, replayed.fingerprint);
    assert_ne!(live.received_at, replayed.received_at);
}

#[test]
fn pap_totals_parse_from_several_separator_styles() {
    let paps = parse_paps(&fixture("account_info.txt"), at()).unwrap();

    assert_eq!(paps.month.strategic, 12);
    assert_eq!(paps.month.peacetime, 3);
    assert_eq!(paps.last_30_days.strategic, 20);
    assert_eq!(paps.last_30_days.peacetime, 4);
    assert_eq!(paps.last_90_days.strategic, 55);
    assert_eq!(paps.last_90_days.peacetime, 8);
    assert_eq!(
        paps.last_strat_character.as_deref(),
        Some("Synthetic Pilot")
    );
    assert_eq!(paps.updated_at, at());
}

#[test]
fn pap_totals_and_last_operation_allow_directorbots_arrow_prefix() {
    let paps = parse_paps(&fixture("account_info_arrows.txt"), at()).unwrap();

    assert_eq!(paps.month.strategic, 12);
    assert_eq!(paps.month.peacetime, 13);
    assert_eq!(paps.last_30_days.strategic, 20);
    assert_eq!(paps.last_30_days.peacetime, 24);
    assert_eq!(paps.last_90_days.strategic, 55);
    assert_eq!(paps.last_90_days.peacetime, 108);
    assert_eq!(paps.last_strat_at.as_deref(), Some("2026-09-01 20:00"));
    assert_eq!(
        paps.last_strat_character.as_deref(),
        Some("Synthetic Pilot")
    );
}

#[test]
fn a_pilot_with_no_paps_parses_as_zero_rather_than_failing() {
    let paps = parse_paps(&fixture("account_info_zeroed.txt"), at()).unwrap();

    assert_eq!(paps.month.strategic, 0);
    assert_eq!(paps.last_90_days.peacetime, 0);
    // Absent rather than fabricated.
    assert!(paps.last_strat_at.is_none());
    assert!(paps.last_strat_character.is_none());
}

#[test]
fn a_fleet_ping_is_never_mistaken_for_a_pap_response() {
    assert!(parse_paps(&fixture("standard_ping.txt"), at()).is_none());
    assert!(parse_paps(&fixture("not_a_ping.txt"), at()).is_none());
}

#[test]
fn a_pap_response_is_never_mistaken_for_a_fleet_ping() {
    assert!(parse_ping(FROM, &fixture("account_info.txt"), at(), at()).is_none());
}
