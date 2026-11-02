//! EVE's single sign-on and the ESI reads every front end shares: who is
//! online, where, in which ship, and what they are training.

mod callback;
mod client;
mod sso;

pub use callback::LoopbackCallback;
pub use client::{
    CharacterPresence, EsiPoll, EsiService, IndustryPolling, ServerStatus, ShipPresence,
    TokenStore, presence_unchanged,
};
pub use sso::{
    AuthorizationRequest, CharacterIdentity, SsoClient, SsoMetadata, TokenBundle,
    ValidatedAccessToken,
};

/// The ESI compatibility date every request pins, so a route's shape cannot
/// change underneath a running client.
pub const COMPATIBILITY_DATE: &str = "2026-09-02";

/// Online status, which gates every intel alert.
pub const ONLINE_SCOPE: &str = "esi-location.read_online.v1";

/// Current solar system and docked state, the other half of intel proximity.
pub const LOCATION_SCOPE: &str = "esi-location.read_location.v1";

/// Personal industry jobs. EVEmarchy's; on its way to Nex Industries.
pub const CHARACTER_JOBS_SCOPE: &str = "esi-industry.read_character_jobs.v1";

/// Granted only by whichever character reads corporation industry jobs. The
/// character must also hold the in-game Factory_Manager role; ESI answers 403
/// when it does not, which is reported as an explicit health state rather than
/// a generic refresh failure.
pub const CORPORATION_JOBS_SCOPE: &str = "esi-industry.read_corporation_jobs.v1";

/// The skill queue. Optional: a character linked before it was asked for is
/// told to link again rather than degrading ESI over a queue it was never
/// allowed to read.
pub const SKILL_QUEUE_SCOPE: &str = "esi-skills.read_skillqueue.v1";

/// Which ship a character is sitting in, so the mining meter knows how big the
/// hold it is filling is. Optional like the skill queue: a character linked
/// before it was asked for keeps working and shows no ship.
pub const SHIP_SCOPE: &str = "esi-location.read_ship_type.v1";

/// Trained skill levels. A mining hold grows with the hull's skill, so without
/// them the meter can only offer the unskilled base size.
pub const SKILLS_SCOPE: &str = "esi-skills.read_skills.v1";
