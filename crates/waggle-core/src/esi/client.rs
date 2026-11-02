use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::{error::Error, fmt};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::{StatusCode, header};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::{
    app::AppIdentity,
    industry::{IndustryJob, JobOwner, SlotCaps},
    secrets::RefreshTokenStore,
    skills::{SkillQueue, SkillQueueEntry},
    store::LinkedCharacter,
};

use super::{
    COMPATIBILITY_DATE, CORPORATION_JOBS_SCOPE, SHIP_SCOPE, SKILL_QUEUE_SCOPE, SKILLS_SCOPE,
    SsoClient, SsoMetadata, ValidatedAccessToken,
};
use crate::mining::holds::{MiningHold, mining_hold};

const ONLINE_DEFAULT_TTL: Duration = Duration::from_secs(60);
const LOCATION_DEFAULT_TTL: Duration = Duration::from_secs(5);
const INDUSTRY_DEFAULT_TTL: Duration = Duration::from_secs(300);
/// ESI caches a character's skill queue for two minutes.
const SKILL_QUEUE_DEFAULT_TTL: Duration = Duration::from_secs(120);
/// ESI caches the current ship for five seconds. Asked a little less often:
/// boarding another ship is rare, and a few seconds late is still long before
/// the first laser cycle finishes.
const SHIP_MIN_INTERVAL: Duration = Duration::from_secs(15);
/// Trained levels change when a skill finishes, which is at most a few times a
/// day; ESI's two-minute cache would re-ask for nothing.
const SKILLS_MIN_INTERVAL: Duration = Duration::from_secs(900);
/// Corporation membership rarely changes, and re-reading it is only needed to
/// notice a character that switched corporation.
const AFFILIATION_TTL: Duration = Duration::from_secs(3600);
/// Tranquility's own status endpoint caches for 30 seconds.
const SERVER_STATUS_TTL: Duration = Duration::from_secs(30);
/// A corporation's name changes about never; an hour is generous.
const CORPORATION_NAME_TTL: Duration = Duration::from_secs(3600);
const ERROR_RETRY_DELAY: Duration = Duration::from_secs(30);
/// How long a character that EVE refused the corporation job list to is passed
/// over. A missing Factory_Manager role is fixed in game by a person, never in
/// the next thirty seconds, so retrying at the generic error cadence only
/// spends the ESI error budget and fills the log.
const ROLE_RETRY_DELAY: Duration = Duration::from_secs(900);
const TOKEN_EXPIRY_MARGIN_SECONDS: i64 = 60;
/// How often a missing credential is looked up again. The keyring is often
/// still locked when the session's units start, so a single failed lookup must
/// not disable ESI for the rest of the login.
const SECRET_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Remaining error budget at which every request pauses until ESI's window
/// resets, well before CCP would start returning 420 responses.
const ERROR_LIMIT_FLOOR: u32 = 20;

/// Spreads retries so several characters failing at once do not resynchronise
/// into a thundering herd against ESI.
fn with_jitter(delay: Duration) -> Duration {
    let mut seed = [0_u8; 2];
    if getrandom::fill(&mut seed).is_err() {
        return delay;
    }
    let fraction = f64::from(u16::from_le_bytes(seed)) / f64::from(u16::MAX);
    delay.mul_f64(1.0 + fraction * 0.5)
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CharacterPresence {
    pub character_id: u64,
    pub character_name: String,
    pub online: Option<bool>,
    pub solar_system_id: Option<u32>,
    /// Resolved from the bundled dataset by the daemon, not by ESI.
    ///
    /// The panel shows pilots by where they are, and a system ID is not a
    /// place to a reader. Resolution happens once in the daemon so the plugin
    /// never has to carry a copy of the universe.
    #[serde(default)]
    pub solar_system_name: Option<String>,
    #[serde(default)]
    pub region_name: Option<String>,
    /// Cache-relative path to this pilot's portrait, set only when the file is
    /// actually on disk.
    ///
    /// The panel must never point an Image at a file that is not there: Qt
    /// logs a warning per attempt, and a pilot with no portrait would produce
    /// two of them on every panel open, forever.
    #[serde(default)]
    pub portrait: Option<String>,
    pub docked: Option<bool>,
    pub observed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// What the character is training, attached by the daemon from its durable
    /// copy of the queue. Absent until the queue has been read at least once.
    #[serde(default)]
    pub skill_queue: Option<SkillQueue>,
    /// Why the queue cannot be read, when it cannot. Usually a character
    /// linked before the skill scope was asked for, which the pilots view
    /// reports as its own line rather than as an ESI failure.
    #[serde(default)]
    pub skill_queue_error: Option<String>,
    /// The ship the character is sitting in, while it is online.
    #[serde(default)]
    pub ship: Option<ShipPresence>,
    /// Why the ship cannot be read, when it cannot: usually a character linked
    /// before the ship scope was asked for.
    #[serde(default)]
    pub ship_error: Option<String>,
    /// How many jobs this character's skills let it run, once its skill
    /// levels have been read. Absent for a character linked before the skills
    /// scope was asked for.
    #[serde(default)]
    pub industry_slots: Option<SlotCaps>,
}

impl CharacterPresence {
    /// Whether two readings say anything different about the character.
    ///
    /// `observed_at` moves on every poll that ESI answers, even with nothing
    /// new, so comparing it would make every poll a change.
    pub fn same_apart_from_observation(&self, other: &Self) -> bool {
        Self {
            observed_at: None,
            ..self.clone()
        } == Self {
            observed_at: None,
            ..other.clone()
        }
    }
}

/// [`CharacterPresence::same_apart_from_observation`] for a whole list, in
/// order.
pub fn presence_unchanged(before: &[CharacterPresence], after: &[CharacterPresence]) -> bool {
    before.len() == after.len()
        && before
            .iter()
            .zip(after)
            .all(|(before, after)| before.same_apart_from_observation(after))
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ShipPresence {
    pub type_id: u32,
    pub item_id: u64,
    /// What the pilot named it in game.
    pub name: String,
    /// The hull, resolved by the daemon from the bundled dataset.
    #[serde(default)]
    pub type_name: Option<String>,
    /// `None` for a ship with no mining hold.
    #[serde(default)]
    pub mining_hold: Option<MiningHold>,
}

#[derive(Debug, Clone, Default)]
pub struct EsiPoll {
    pub changed: bool,
    pub characters: Vec<CharacterPresence>,
    pub warnings: Vec<String>,
    pub industry_jobs: Vec<IndustryJob>,
    pub industry_refreshed_characters: Vec<u64>,
    pub industry_failures: usize,
    /// Characters whose skill queue was re-read this poll, with the queue.
    pub skill_queues_refreshed: Vec<(u64, Vec<SkillQueueEntry>)>,
    /// Corporations whose job listing was re-read this poll, with their jobs.
    pub corporation_refreshed: Vec<(u64, Vec<IndustryJob>)>,
    /// Set when the authorized character lacks the in-game role, so the panel
    /// can say what to fix rather than showing a generic failure.
    pub corporation_role_problem: Option<String>,
    /// Tranquility's player count, when it answered. `None` means the server
    /// is down or unreachable, which is itself worth showing.
    pub server: Option<ServerStatus>,
    /// The corporation the panel is titled with.
    pub corporation_name: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct ServerStatus {
    pub players: u64,
    pub version: String,
}

/// Which industry listings to poll. EVEmarchy's, on its way to Nex Industries;
/// the default polls nothing, which is what every other front end wants.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct IndustryPolling {
    pub enabled: bool,
    pub character_jobs: bool,
    pub corporation_jobs: bool,
}

/// Where refresh tokens live, shared by the service and every session.
pub type TokenStore = Arc<dyn RefreshTokenStore + Send + Sync>;

pub struct EsiService {
    app: AppIdentity,
    client_id: Option<String>,
    sso: SsoClient,
    esi: reqwest::Client,
    metadata: Option<SsoMetadata>,
    metadata_retry_at: Instant,
    industry_enabled: bool,
    character_jobs_enabled: bool,
    corporation_jobs_enabled: bool,
    sessions: Vec<CharacterSession>,
    /// ESI's error budget is per client, not per character, so exhausting it
    /// has to pause every session until the window resets.
    error_limit_reset_at: Option<Instant>,
    /// Both of these are unauthenticated and shared across characters, so they
    /// are cached on the service rather than per session.
    server: Option<ServerStatus>,
    server_etag: Option<String>,
    next_server_at: Instant,
    corporation_name: Option<String>,
    next_corporation_name_at: Instant,
    /// Character IDs by name, for fleet commanders named in a broadcast.
    ///
    /// A ping names its FC in words; a portrait is keyed by ID. Nothing else
    /// in the daemon has ever needed to go that direction, so this is the
    /// reverse of `character_names` rather than a lookup in it: an FC is
    /// usually not a linked character and usually not in the corporation.
    ///
    /// `None` records a name ESI could not resolve — a nickname, a typo, or a
    /// bot writing something that is not a character — so it is asked about
    /// once and not on every poll thereafter.
    character_ids: BTreeMap<String, Option<u64>>,
    /// Character names by ID, for the installers of corporation jobs.
    ///
    /// A corporation job is attributed to whoever installed it, which is
    /// usually not the one character authorized to read the listing. Without
    /// this every job in the factory carried the reader's name, so the panel
    /// said the wrong pilot should go and collect all of them.
    character_names: BTreeMap<u64, String>,
}

#[derive(Deserialize)]
struct RawServerStatus {
    players: u64,
    server_version: String,
}

#[derive(Deserialize)]
struct RawCorporation {
    name: String,
}

struct CharacterSession {
    character: LinkedCharacter,
    /// Where this session's refresh token is persisted when SSO rotates it.
    secrets: TokenStore,
    refresh_token: Option<SecretString>,
    /// False when a rotated refresh token could not be written to Secret
    /// Service. The token is still usable in memory, and the write is retried
    /// before the next refresh so a transient keyring failure cannot orphan the
    /// character.
    refresh_token_persisted: bool,
    /// The refresh token this session last read from or wrote to Secret
    /// Service. Compared before every write: `auth add` replaces the stored
    /// credential from another process, and a rotated token from the old grant
    /// written over it would silently discard the new one — along with every
    /// scope it was re-linked to gain.
    stored_refresh_token: Option<SecretString>,
    next_secret_retry_at: Instant,
    access_token: Option<ValidatedAccessToken>,
    presence: CharacterPresence,
    next_online_at: Instant,
    next_location_at: Instant,
    next_industry_at: Instant,
    online_etag: Option<String>,
    location_etag: Option<String>,
    industry_etag: Option<String>,
    industry_jobs: Vec<IndustryJob>,
    industry_error: Option<String>,
    next_skills_at: Instant,
    skills_etag: Option<String>,
    skill_queue: Vec<SkillQueueEntry>,
    next_ship_at: Instant,
    ship_etag: Option<String>,
    next_skill_levels_at: Instant,
    skill_levels_etag: Option<String>,
    /// Active level by skill, once read.
    skill_levels: Option<BTreeMap<u32, u8>>,
    /// Set only on the one session that reads corporation jobs.
    corporation_id: Option<u64>,
    next_affiliation_at: Instant,
    next_corporation_at: Instant,
    corporation_etag: Option<String>,
    corporation_jobs: Vec<IndustryJob>,
    corporation_error: Option<String>,
    /// When EVE last answered 403 for this character's corporation jobs. Kept
    /// apart from `corporation_error` because it outlives it: the error is the
    /// current reader's health, while this is why another character is reading.
    corporation_role_denied_at: Option<Instant>,
}

impl CharacterSession {
    /// Whether this character was authorized to read corporation jobs.
    fn may_read_corporation_jobs(&self) -> bool {
        self.character
            .scopes
            .iter()
            .any(|scope| scope == CORPORATION_JOBS_SCOPE)
    }

    /// Whether this character was authorized to read its skill queue.
    fn may_read_skill_queue(&self) -> bool {
        self.character
            .scopes
            .iter()
            .any(|scope| scope == SKILL_QUEUE_SCOPE)
    }

    fn holds(&self, wanted: &str) -> bool {
        self.character.scopes.iter().any(|scope| scope == wanted)
    }

    /// Recomputes the hold from the ship and the levels as they stand now.
    fn refresh_mining_hold(&mut self) {
        let levels = self.skill_levels.as_ref();
        if let Some(ship) = &mut self.presence.ship {
            ship.mining_hold = mining_hold(ship.type_id, levels);
        }
    }

    /// Whether EVE refused this character recently enough that another reader
    /// should be preferred. The refusal expires so a restored role is picked up
    /// without re-authorizing.
    fn corporation_role_denied(&self, now: Instant) -> bool {
        self.corporation_role_denied_at
            .is_some_and(|denied_at| now < denied_at + ROLE_RETRY_DELAY)
    }
}

#[derive(Debug, Deserialize)]
struct OnlineResponse {
    online: bool,
}

#[derive(Deserialize)]
struct LocationResponse {
    solar_system_id: u32,
    station_id: Option<u64>,
    structure_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawIndustryJob {
    job_id: u64,
    activity_id: u32,
    blueprint_type_id: u32,
    product_type_id: Option<u32>,
    runs: u32,
    status: String,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
    /// Present on corporation jobs, absent on the personal endpoint.
    #[serde(default)]
    installer_id: Option<u64>,
}

/// One queue entry as ESI lists it. A paused queue carries no dates.
#[derive(Debug, Deserialize)]
struct RawSkillQueueEntry {
    skill_id: u32,
    finished_level: u8,
    queue_position: u32,
    #[serde(default)]
    start_date: Option<DateTime<Utc>>,
    #[serde(default)]
    finish_date: Option<DateTime<Utc>>,
    #[serde(default)]
    level_start_sp: Option<u32>,
    #[serde(default)]
    level_end_sp: Option<u32>,
    #[serde(default)]
    training_start_sp: Option<u32>,
}

#[derive(Deserialize, Default)]
struct ResolvedIds {
    #[serde(default)]
    characters: Vec<ResolvedId>,
}

#[derive(Deserialize)]
struct ResolvedId {
    id: u64,
    name: String,
}

#[derive(Deserialize)]
struct ResolvedName {
    id: u64,
    name: String,
    category: String,
}

#[derive(Deserialize)]
struct CharacterAffiliation {
    corporation_id: u64,
}

#[derive(Deserialize)]
struct EsiErrorBody {
    error: Option<String>,
}

#[derive(Debug)]
struct EsiResponseError {
    status: StatusCode,
    detail: String,
    retry_after: Option<Duration>,
}

impl fmt::Display for EsiResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            write!(
                formatter,
                "ESI rate limited the request (retry after {} seconds): {}",
                self.retry_after.map_or(30, |delay| delay.as_secs()),
                self.detail
            )
        } else {
            write!(formatter, "ESI returned {}: {}", self.status, self.detail)
        }
    }
}

impl Error for EsiResponseError {}

impl EsiService {
    pub fn new(
        app: AppIdentity,
        client_id: Option<String>,
        industry: IndustryPolling,
        characters: Vec<LinkedCharacter>,
        secret_store: TokenStore,
    ) -> Result<Self> {
        let now = Instant::now();
        let sessions: Vec<CharacterSession> = characters
            .into_iter()
            .map(|character| {
                let (refresh_token, error) = match secret_store.lookup(character.id) {
                    Ok(Some(token)) => (Some(token), None),
                    Ok(None) => (
                        None,
                        Some("Refresh token is missing from Secret Service".to_owned()),
                    ),
                    Err(error) => (
                        None,
                        Some(format!("Could not access Secret Service: {error:#}")),
                    ),
                };
                // Said once here rather than on every poll: a character
                // linked before the skill scope existed cannot be asked
                // about its queue until it is linked again.
                let skill_queue_error = (!character
                    .scopes
                    .iter()
                    .any(|scope| scope == SKILL_QUEUE_SCOPE))
                .then(|| relink_for_skills(app, &character.name));
                let ship_error = (!character.scopes.iter().any(|scope| scope == SHIP_SCOPE))
                    .then(|| relink_for_ship(app, &character.name));
                let presence = CharacterPresence {
                    character_id: character.id,
                    character_name: character.name.clone(),
                    online: None,
                    solar_system_id: None,
                    solar_system_name: None,
                    region_name: None,
                    portrait: None,
                    docked: None,
                    observed_at: None,
                    error,
                    skill_queue: None,
                    skill_queue_error,
                    ship: None,
                    ship_error,
                    industry_slots: None,
                };
                CharacterSession {
                    character,
                    secrets: Arc::clone(&secret_store),
                    stored_refresh_token: refresh_token.clone(),
                    refresh_token,
                    refresh_token_persisted: true,
                    next_secret_retry_at: now + SECRET_RETRY_DELAY,
                    access_token: None,
                    presence,
                    next_online_at: now,
                    next_location_at: now,
                    next_industry_at: now,
                    online_etag: None,
                    location_etag: None,
                    industry_etag: None,
                    industry_jobs: Vec::new(),
                    industry_error: None,
                    next_skills_at: now,
                    skills_etag: None,
                    skill_queue: Vec::new(),
                    next_ship_at: now,
                    ship_etag: None,
                    next_skill_levels_at: now,
                    skill_levels_etag: None,
                    skill_levels: None,
                    corporation_id: None,
                    next_affiliation_at: now,
                    next_corporation_at: now,
                    corporation_etag: None,
                    corporation_jobs: Vec::new(),
                    corporation_error: None,
                    corporation_role_denied_at: None,
                }
            })
            .collect();
        // Seeded from the linked characters: their names are already known, so
        // an installer who is one of them never costs a request.
        let character_names: BTreeMap<u64, String> = sessions
            .iter()
            .map(|session| (session.character.id, session.character.name.clone()))
            .collect();
        let esi = reqwest::Client::builder()
            .user_agent(app.user_agent())
            .timeout(Duration::from_secs(20))
            .build()
            .context("could not build the ESI HTTP client")?;

        Ok(Self {
            app,
            client_id,
            sso: SsoClient::new(app)?,
            esi,
            metadata: None,
            metadata_retry_at: now,
            industry_enabled: industry.enabled,
            character_jobs_enabled: industry.character_jobs,
            corporation_jobs_enabled: industry.corporation_jobs,
            sessions,
            error_limit_reset_at: None,
            server: None,
            server_etag: None,
            next_server_at: now,
            corporation_name: None,
            next_corporation_name_at: now,
            character_ids: BTreeMap::new(),
            character_names,
        })
    }

    pub fn is_configured(&self) -> bool {
        self.client_id.is_some()
    }

    pub fn linked_character_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn matches_linked_characters(&self, characters: &[LinkedCharacter]) -> bool {
        self.sessions.len() == characters.len()
            && self
                .sessions
                .iter()
                .zip(characters)
                .all(|(session, character)| session.character == *character)
    }

    /// Brings every scheduled ESI request forward to now.
    ///
    /// This is safe to expose as a user action because each request carries an
    /// `If-None-Match`: an unchanged resource answers 304, which neither
    /// re-downloads anything nor spends the error budget. It also clears any
    /// error-limit pause, since the user is explicitly asking to try again.
    pub fn refresh_now(&mut self) {
        let now = Instant::now();
        self.error_limit_reset_at = None;
        self.metadata_retry_at = now;
        self.next_server_at = now;
        self.next_corporation_name_at = now;
        for session in &mut self.sessions {
            session.next_online_at = now;
            session.next_location_at = now;
            session.next_industry_at = now;
            session.next_skills_at = now;
            session.next_ship_at = now;
            session.next_skill_levels_at = now;
            session.next_secret_retry_at = now;
            session.next_corporation_at = now;
            session.next_affiliation_at = now;
        }
    }

    /// Reads Tranquility's status and, once, the corporation to title the
    /// panel with.
    ///
    /// Both endpoints are public, so neither needs a token and neither is
    /// worth failing a poll over: a warning and the previous value is the
    /// right outcome, because the panel is meant to render from the last good
    /// sync.
    async fn refresh_server_status(
        &mut self,
        error_limit_reset: &mut Option<Duration>,
        warnings: &mut Vec<String>,
    ) {
        if Instant::now() >= self.next_server_at {
            let url = "https://esi.evetech.net/status";
            match get_public::<RawServerStatus>(
                &self.esi,
                url,
                SERVER_STATUS_TTL,
                self.server_etag.as_deref(),
                error_limit_reset,
            )
            .await
            {
                Ok(fetched) => {
                    self.next_server_at = Instant::now() + fetched.ttl();
                    if let EsiFetch::Fresh { value, etag, .. } = fetched {
                        self.server_etag = etag;
                        self.server = Some(ServerStatus {
                            players: value.players,
                            version: value.server_version,
                        });
                    }
                }
                Err(error) => {
                    // Tranquility being down is normal during downtime, and
                    // saying so is more use than an empty header.
                    self.server = None;
                    self.next_server_at = Instant::now() + retry_delay_for(&error);
                    warnings.push(format!("EVE server status unavailable: {error:#}"));
                }
            }
        }

        // The corporation only needs resolving once an hour, and only if a
        // character is linked to resolve it from.
        if self.corporation_name.is_none() || Instant::now() >= self.next_corporation_name_at {
            // Prefer a corporation already resolved for corporation jobs.
            // Otherwise resolve one publicly: a character's corporation is not
            // private, so this needs no scope and works with corporation-job
            // tracking switched off.
            let mut corporation_id = self
                .sessions
                .iter()
                .find_map(|session| session.corporation_id);
            if corporation_id.is_none()
                && let Some(character_id) = self.sessions.first().map(|s| s.character.id)
            {
                let url = format!("https://esi.evetech.net/characters/{character_id}");
                match get_public::<CharacterAffiliation>(
                    &self.esi,
                    &url,
                    AFFILIATION_TTL,
                    None,
                    error_limit_reset,
                )
                .await
                {
                    Ok(EsiFetch::Fresh { value, .. }) => {
                        corporation_id = Some(value.corporation_id);
                    }
                    Ok(EsiFetch::NotModified { .. }) => {}
                    Err(error) => {
                        self.next_corporation_name_at = Instant::now() + retry_delay_for(&error);
                        warnings.push(format!("corporation membership unavailable: {error:#}"));
                        return;
                    }
                }
            }
            let Some(corporation_id) = corporation_id else {
                return;
            };
            let url = format!("https://esi.evetech.net/corporations/{corporation_id}");
            match get_public::<RawCorporation>(
                &self.esi,
                &url,
                CORPORATION_NAME_TTL,
                None,
                error_limit_reset,
            )
            .await
            {
                Ok(EsiFetch::Fresh { value, .. }) => {
                    self.corporation_name = Some(value.name);
                    self.next_corporation_name_at = Instant::now() + CORPORATION_NAME_TTL;
                }
                Ok(EsiFetch::NotModified { ttl }) => {
                    self.next_corporation_name_at = Instant::now() + ttl;
                }
                Err(error) => {
                    self.next_corporation_name_at = Instant::now() + retry_delay_for(&error);
                    warnings.push(format!("corporation name unavailable: {error:#}"));
                }
            }
        }
    }

    /// Looks up character IDs for names this service has not seen.
    ///
    /// `/universe/ids` is the mirror of `/universe/names`: unauthenticated,
    /// batched, and it answers by category, so a name that is a corporation or
    /// a system does not come back as a character. Returns the IDs it now
    /// knows for the names asked about.
    pub async fn learn_character_ids(&mut self, names: &[String]) -> Vec<u64> {
        let unknown: Vec<&String> = {
            let mut seen: Vec<&String> = names
                .iter()
                .filter(|name| !name.trim().is_empty() && !self.character_ids.contains_key(*name))
                .collect();
            seen.sort_unstable();
            seen.dedup();
            seen
        };

        if !unknown.is_empty() {
            let response = self
                .esi
                .post("https://esi.evetech.net/universe/ids")
                .json(&unknown)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    match response.json::<ResolvedIds>().await {
                        Ok(resolved) => {
                            for entry in resolved.characters {
                                self.character_ids.insert(entry.name, Some(entry.id));
                            }
                        }
                        Err(error) => {
                            debug!(error = %error, "Could not read resolved character IDs");
                        }
                    }
                }
                Ok(response) => {
                    debug!(status = %response.status(), "ID resolution was refused");
                }
                Err(error) => {
                    debug!(error = %error, "Could not resolve character IDs");
                }
            }
            // Whatever did not come back is not a character. Recording that
            // stops the same nickname being asked about on every poll.
            for name in unknown {
                self.character_ids.entry(name.clone()).or_insert(None);
            }
        }

        names
            .iter()
            .filter_map(|name| self.character_ids.get(name).copied().flatten())
            .collect()
    }

    /// The ID this service knows for one character name, if any.
    pub fn character_id_for(&self, name: &str) -> Option<u64> {
        self.character_ids.get(name).copied().flatten()
    }

    /// Learns the names of any job installers this service has not seen.
    ///
    /// `/universe/names` is unauthenticated and takes a batch, so a factory
    /// full of jobs from four alts costs one request once. A failure leaves the
    /// names unresolved and is retried on the next poll rather than reported:
    /// a job with a placeholder name is still a job, and the panel says so.
    async fn learn_installer_names(&mut self, jobs: &[IndustryJob]) {
        let mut unknown: Vec<u64> = jobs
            .iter()
            .map(|job| job.character_id)
            .filter(|id| *id != 0 && !self.character_names.contains_key(id))
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        if unknown.is_empty() {
            return;
        }

        let response = self
            .esi
            .post("https://esi.evetech.net/universe/names")
            .json(&unknown)
            .send()
            .await;
        let named: Vec<ResolvedName> = match response {
            Ok(response) if response.status().is_success() => match response.json().await {
                Ok(named) => named,
                Err(error) => {
                    debug!(error = %error, "Could not read resolved character names");
                    return;
                }
            },
            Ok(response) => {
                debug!(status = %response.status(), "Name resolution was refused");
                return;
            }
            Err(error) => {
                debug!(error = %error, "Could not resolve character names");
                return;
            }
        };

        for entry in named {
            if entry.category == "character" {
                self.character_names.insert(entry.id, entry.name);
            }
        }
    }

    /// Replaces each job's installer name with the resolved one, where it is
    /// known. Jobs whose installer is still unresolved keep their placeholder.
    fn name_installers(&self, jobs: &mut [IndustryJob]) {
        for job in jobs {
            if let Some(name) = self.character_names.get(&job.character_id) {
                job.character_name = name.clone();
            }
        }
    }

    /// Whether any linked character was authorized for corporation jobs.
    pub fn has_corporation_reader(&self) -> bool {
        self.sessions
            .iter()
            .any(CharacterSession::may_read_corporation_jobs)
    }

    pub fn characters(&self) -> Vec<CharacterPresence> {
        self.sessions
            .iter()
            .map(|session| session.presence.clone())
            .collect()
    }

    pub fn industry_jobs(&self) -> Vec<IndustryJob> {
        let mut jobs = self
            .sessions
            .iter()
            .flat_map(|session| {
                session
                    .industry_jobs
                    .iter()
                    .chain(session.corporation_jobs.iter())
                    .cloned()
            })
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| (job.end_at, job.job_id));
        jobs
    }

    pub async fn poll(&mut self) -> EsiPoll {
        let before = self.characters();
        let mut warnings = Vec::new();
        let mut industry_refreshed_characters = Vec::new();
        let mut skill_queues_refreshed = Vec::new();
        let Some(client_id) = self.client_id.clone() else {
            return EsiPoll {
                characters: before,
                ..EsiPoll::default()
            };
        };
        if self.sessions.is_empty() {
            return EsiPoll {
                characters: before,
                ..EsiPoll::default()
            };
        }

        if self
            .error_limit_reset_at
            .is_some_and(|reset| Instant::now() < reset)
        {
            return EsiPoll {
                characters: before,
                industry_jobs: self.industry_jobs(),
                ..EsiPoll::default()
            };
        }
        self.error_limit_reset_at = None;

        // A credential that was unavailable at startup, typically because the
        // keyring was still locked, is looked up again instead of disabling the
        // character for the rest of the session.
        for session in &mut self.sessions {
            if session.refresh_token.is_none() && Instant::now() >= session.next_secret_retry_at {
                session.next_secret_retry_at = Instant::now() + SECRET_RETRY_DELAY;
                match session.secrets.lookup(session.character.id) {
                    Ok(Some(token)) => {
                        session.stored_refresh_token = Some(token.clone());
                        session.refresh_token = Some(token);
                        session.refresh_token_persisted = true;
                        session.presence.error = None;
                        session.next_online_at = Instant::now();
                        session.next_location_at = Instant::now();
                        session.next_industry_at = Instant::now();
                        session.next_skills_at = Instant::now();
                        info!(
                            character = session.character.name,
                            "Recovered the EVE refresh token from Secret Service"
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        session.presence.error =
                            Some(format!("Could not access Secret Service: {error:#}"));
                    }
                }
            }
        }

        if self.metadata.is_none() && Instant::now() >= self.metadata_retry_at {
            match self.sso.metadata().await {
                Ok(metadata) => self.metadata = Some(metadata),
                Err(error) => {
                    let detail = format!("EVE SSO metadata unavailable: {error:#}");
                    warnings.push(detail.clone());
                    for session in &mut self.sessions {
                        session.presence.error = Some(detail.clone());
                        if self.industry_enabled && self.character_jobs_enabled {
                            session.industry_error = Some(detail.clone());
                        }
                    }
                    self.metadata_retry_at = Instant::now() + ERROR_RETRY_DELAY;
                }
            }
        }

        let mut error_limit_reset = None;
        // Unauthenticated and shared, so they run once per poll rather than
        // once per character. Both are conditional requests, so an unchanged
        // answer costs a 304.
        self.refresh_server_status(&mut error_limit_reset, &mut warnings)
            .await;
        let mut corporation_refreshed = Vec::new();
        let mut corporation_role_problem = None;
        // Only one character reads the corporation listing; several would fetch
        // identical data and spend the error budget for nothing. A character
        // EVE has refused is passed over while another authorized one is left,
        // so losing the role on one alt does not stop corporation tracking. If
        // every authorized character is refused the first one still reads, and
        // its refusal is what the health line goes on to report.
        let corporation_reader = select_corporation_reader(&self.sessions, Instant::now());
        if let Some(metadata) = self.metadata.clone() {
            let sso = self.sso.clone();
            let esi = self.esi.clone();
            for session in &mut self.sessions {
                if let Err(error) = poll_session(
                    &sso,
                    &esi,
                    &metadata,
                    &client_id,
                    session,
                    &mut error_limit_reset,
                )
                .await
                {
                    let detail = format!("{}: {error:#}", session.character.name);
                    session.presence.error = Some(detail.clone());
                    let delay = retry_delay_for(&error);
                    if is_unauthorized(&error) {
                        session.access_token = None;
                    }
                    session.next_online_at = Instant::now() + delay;
                    session.next_location_at = Instant::now() + delay;
                    warnings.push(detail);
                }
                if self.industry_enabled && self.character_jobs_enabled {
                    match poll_industry_session(
                        &sso,
                        &esi,
                        &metadata,
                        &client_id,
                        session,
                        &mut error_limit_reset,
                    )
                    .await
                    {
                        Ok(true) => industry_refreshed_characters.push(session.character.id),
                        Ok(false) => {}
                        Err(error) => {
                            let detail = format!("{} industry: {error:#}", session.character.name);
                            session.industry_error = Some(detail.clone());
                            let delay = retry_delay_for(&error);
                            if is_unauthorized(&error) {
                                session.access_token = None;
                            }
                            session.next_industry_at = Instant::now() + delay;
                            warnings.push(detail);
                        }
                    }
                }
                if session.may_read_skill_queue() {
                    match poll_skill_session(
                        &sso,
                        &esi,
                        &metadata,
                        &client_id,
                        session,
                        &mut error_limit_reset,
                    )
                    .await
                    {
                        Ok(true) => skill_queues_refreshed
                            .push((session.character.id, session.skill_queue.clone())),
                        Ok(false) => {}
                        Err(error) => {
                            // Best effort, the way nfa reads it: a queue that
                            // cannot be read must not hold up presence or
                            // industry, and is reported on the pilot's own
                            // line rather than against ESI as a whole.
                            // Both mean the token itself lacks the scope: ESI
                            // answers 401 "not valid for any required scope"
                            // for a grant that never had it, even though the
                            // refresh that produced the token succeeded. The
                            // access token is fine for everything else, so it
                            // is kept, and the queue is asked about again at
                            // the role cadence rather than every half minute.
                            let no_scope =
                                error
                                    .downcast_ref::<EsiResponseError>()
                                    .is_some_and(|error| {
                                        error.status == StatusCode::FORBIDDEN
                                            || error.status == StatusCode::UNAUTHORIZED
                                    });
                            session.presence.skill_queue_error = Some(if no_scope {
                                relink_for_skills(self.app, &session.character.name)
                            } else {
                                format!("Skill queue unavailable: {error:#}")
                            });
                            session.next_skills_at = Instant::now()
                                + if no_scope {
                                    with_jitter(ROLE_RETRY_DELAY)
                                } else {
                                    retry_delay_for(&error)
                                };
                            debug!(
                                character = session.character.name,
                                error = %error,
                                "Could not read the skill queue"
                            );
                        }
                    }
                }
            }

            for session in &mut self.sessions {
                if session.holds(SKILLS_SCOPE)
                    && let Err(error) = poll_skill_levels_session(
                        &sso,
                        &esi,
                        &metadata,
                        &client_id,
                        session,
                        &mut error_limit_reset,
                    )
                    .await
                {
                    // Best effort: without levels the hold is the base size and
                    // says so, which is still worth showing.
                    session.next_skill_levels_at = Instant::now()
                        + if lacks_scope(&error) {
                            with_jitter(ROLE_RETRY_DELAY)
                        } else {
                            retry_delay_for(&error)
                        };
                    debug!(
                        character = session.character.name,
                        error = %error,
                        "Could not read skill levels"
                    );
                }
                if session.holds(SHIP_SCOPE)
                    && let Err(error) = poll_ship_session(
                        &sso,
                        &esi,
                        &metadata,
                        &client_id,
                        session,
                        &mut error_limit_reset,
                    )
                    .await
                {
                    let no_scope = lacks_scope(&error);
                    session.presence.ship_error = Some(if no_scope {
                        relink_for_ship(self.app, &session.character.name)
                    } else {
                        format!("Ship unavailable: {error:#}")
                    });
                    session.next_ship_at = Instant::now()
                        + if no_scope {
                            with_jitter(ROLE_RETRY_DELAY)
                        } else {
                            retry_delay_for(&error)
                        };
                    debug!(
                        character = session.character.name,
                        error = %error,
                        "Could not read the current ship"
                    );
                }
            }

            if self.industry_enabled
                && self.corporation_jobs_enabled
                && let Some(index) = corporation_reader
            {
                let sso = self.sso.clone();
                let esi = self.esi.clone();
                let session = &mut self.sessions[index];
                match poll_corporation_session(
                    &sso,
                    &esi,
                    &metadata,
                    &client_id,
                    session,
                    &mut error_limit_reset,
                )
                .await
                {
                    Ok(true) => {
                        if let Some(corporation_id) = session.corporation_id {
                            corporation_refreshed
                                .push((corporation_id, session.corporation_jobs.clone()));
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        // A 403 here means the scope was granted but EVE has
                        // not given the character the Factory_Manager role.
                        let forbidden = error
                            .downcast_ref::<EsiResponseError>()
                            .is_some_and(|error| error.status == StatusCode::FORBIDDEN);
                        let detail = if forbidden {
                            format!(
                                "{} cannot read corporation industry jobs; EVE requires \
                                 the Factory_Manager corporation role",
                                session.character.name
                            )
                        } else {
                            format!("{} corporation industry: {error:#}", session.character.name)
                        };
                        session.corporation_error = Some(detail.clone());
                        if forbidden {
                            // Only the first refusal is worth a log line; the
                            // health line carries the standing state, and this
                            // repeats until a person fixes the role in game.
                            let already_denied = session.corporation_role_denied(Instant::now());
                            session.corporation_role_denied_at = Some(Instant::now());
                            session.next_corporation_at =
                                Instant::now() + with_jitter(ROLE_RETRY_DELAY);
                            if !already_denied {
                                warnings.push(detail);
                            }
                        } else {
                            session.next_corporation_at = Instant::now() + retry_delay_for(&error);
                            warnings.push(detail);
                        }
                    }
                }
            }
        }

        // The reader's standing refusal is what the health line reports, not
        // only the poll that happened to hit the 403. Between refusals the
        // request is in backoff and answers nothing, and a health line that
        // flickered back to a generic failure count would hide the role that
        // actually needs granting.
        if let Some(index) = corporation_reader
            && self.sessions[index].corporation_role_denied(Instant::now())
        {
            corporation_role_problem = self.sessions[index].corporation_error.clone();
        }
        // Only the reader polls, so only the reader can clear its own error. A
        // character passed over while still holding one would keep industry at
        // degraded for good, long after another character took over the read.
        for (index, session) in self.sessions.iter_mut().enumerate() {
            if Some(index) != corporation_reader {
                session.corporation_error = None;
            }
        }

        // One pass per poll over every job the service is holding, so a new
        // installer is named before its jobs reach the store.
        if !corporation_refreshed.is_empty() {
            let holding = self.industry_jobs();
            self.learn_installer_names(&holding).await;
            for (_, jobs) in &mut corporation_refreshed {
                self.name_installers(jobs);
            }
        }

        if self.corporation_jobs_enabled && !self.has_corporation_reader() {
            corporation_role_problem = Some(format!(
                "No linked character granted {CORPORATION_JOBS_SCOPE}; run \
                 `evemarchy auth add --corporation-jobs` for a character holding the \
                 Factory_Manager corporation role"
            ));
        }

        if let Some(reset) = error_limit_reset {
            self.error_limit_reset_at = Some(Instant::now() + reset);
            warnings.push(format!(
                "ESI error budget is nearly exhausted; pausing requests for {} seconds",
                reset.as_secs()
            ));
        }

        let characters = self.characters();
        let industry_jobs = self.industry_jobs();
        EsiPoll {
            changed: characters != before,
            characters,
            warnings,
            industry_jobs,
            industry_refreshed_characters,
            skill_queues_refreshed,
            industry_failures: self
                .sessions
                .iter()
                .filter(|session| {
                    session.industry_error.is_some() || session.corporation_error.is_some()
                })
                .count(),
            corporation_refreshed,
            corporation_role_problem,
            server: self.server.clone(),
            corporation_name: self.corporation_name.clone(),
        }
    }
}

/// The one sentence the pilots view shows for a queue it is not allowed to read.
fn relink_for_skills(app: AppIdentity, character_name: &str) -> String {
    format!("{} to see skill training", (app.relink)(character_name))
}

fn relink_for_ship(app: AppIdentity, character_name: &str) -> String {
    format!("{} to see the ship it flies", (app.relink)(character_name))
}

/// ESI answers 401 "not valid for any required scope" for a grant that never
/// had the scope, even though the refresh that produced the token succeeded,
/// and 403 for one it was withdrawn from.
fn lacks_scope(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<EsiResponseError>()
        .is_some_and(|error| {
            error.status == StatusCode::FORBIDDEN || error.status == StatusCode::UNAUTHORIZED
        })
}

#[derive(Deserialize)]
struct ShipResponse {
    ship_type_id: u32,
    ship_item_id: u64,
    ship_name: String,
}

/// Reads the ship an online character is sitting in. An offline character is
/// in no ship worth showing, so its last one is forgotten.
async fn poll_ship_session(
    sso: &SsoClient,
    esi: &reqwest::Client,
    metadata: &SsoMetadata,
    client_id: &str,
    session: &mut CharacterSession,
    error_limit_reset: &mut Option<Duration>,
) -> Result<()> {
    if session.presence.online != Some(true) {
        session.presence.ship = None;
        session.ship_etag = None;
        return Ok(());
    }
    if Instant::now() < session.next_ship_at {
        return Ok(());
    }
    ensure_access_token(sso, metadata, client_id, session).await?;
    let access = session
        .access_token
        .as_ref()
        .context("access token was not initialized")?;
    let url = format!(
        "https://esi.evetech.net/characters/{}/ship",
        session.character.id
    );
    let fetched = get_esi::<ShipResponse>(
        esi,
        &url,
        &access.secret,
        SHIP_MIN_INTERVAL,
        session.ship_etag.as_deref(),
        error_limit_reset,
    )
    .await
    .context("could not read the current ship")?;
    session.next_ship_at = Instant::now() + fetched.ttl().max(SHIP_MIN_INTERVAL);
    session.presence.ship_error = None;
    if let EsiFetch::Fresh { value, etag, .. } = fetched {
        session.ship_etag = etag;
        let type_name = session
            .presence
            .ship
            .as_ref()
            .filter(|ship| ship.type_id == value.ship_type_id)
            .and_then(|ship| ship.type_name.clone());
        session.presence.ship = Some(ShipPresence {
            type_id: value.ship_type_id,
            item_id: value.ship_item_id,
            name: value.ship_name,
            type_name,
            mining_hold: None,
        });
        session.refresh_mining_hold();
    }
    Ok(())
}

#[derive(Deserialize)]
struct SkillsResponse {
    skills: Vec<RawSkill>,
}

#[derive(Deserialize)]
struct RawSkill {
    skill_id: u32,
    /// What the clone can use, which for an Alpha is capped below what was
    /// trained.
    active_skill_level: u8,
}

async fn poll_skill_levels_session(
    sso: &SsoClient,
    esi: &reqwest::Client,
    metadata: &SsoMetadata,
    client_id: &str,
    session: &mut CharacterSession,
    error_limit_reset: &mut Option<Duration>,
) -> Result<()> {
    if Instant::now() < session.next_skill_levels_at {
        return Ok(());
    }
    ensure_access_token(sso, metadata, client_id, session).await?;
    let access = session
        .access_token
        .as_ref()
        .context("access token was not initialized")?;
    let url = format!(
        "https://esi.evetech.net/characters/{}/skills",
        session.character.id
    );
    let fetched = get_esi::<SkillsResponse>(
        esi,
        &url,
        &access.secret,
        SKILLS_MIN_INTERVAL,
        session.skill_levels_etag.as_deref(),
        error_limit_reset,
    )
    .await
    .context("could not read skill levels")?;
    session.next_skill_levels_at = Instant::now() + fetched.ttl().max(SKILLS_MIN_INTERVAL);
    if let EsiFetch::Fresh { value, etag, .. } = fetched {
        session.skill_levels_etag = etag;
        session.skill_levels = Some(
            value
                .skills
                .into_iter()
                .map(|skill| (skill.skill_id, skill.active_skill_level))
                .collect(),
        );
        session.refresh_mining_hold();
        session.presence.industry_slots = session.skill_levels.as_ref().map(SlotCaps::from_levels);
    }
    Ok(())
}

/// Reads one character's skill queue. Returns whether it changed.
async fn poll_skill_session(
    sso: &SsoClient,
    esi: &reqwest::Client,
    metadata: &SsoMetadata,
    client_id: &str,
    session: &mut CharacterSession,
    error_limit_reset: &mut Option<Duration>,
) -> Result<bool> {
    if Instant::now() < session.next_skills_at {
        return Ok(false);
    }
    ensure_access_token(sso, metadata, client_id, session).await?;
    let access = session
        .access_token
        .as_ref()
        .context("access token was not initialized")?;
    let url = format!(
        "https://esi.evetech.net/characters/{}/skillqueue",
        session.character.id
    );
    let fetched = get_esi::<Vec<RawSkillQueueEntry>>(
        esi,
        &url,
        &access.secret,
        SKILL_QUEUE_DEFAULT_TTL,
        session.skills_etag.as_deref(),
        error_limit_reset,
    )
    .await
    .context("could not refresh the skill queue")?;

    let ttl = fetched.ttl();
    session.presence.skill_queue_error = None;
    session.next_skills_at = Instant::now() + ttl;
    let EsiFetch::Fresh {
        value: raw, etag, ..
    } = fetched
    else {
        return Ok(false);
    };
    session.skills_etag = etag;
    session.skill_queue = raw
        .into_iter()
        .map(|entry| convert_skill(entry, session.character.id, &session.character.name))
        .collect();
    Ok(true)
}

fn convert_skill(
    entry: RawSkillQueueEntry,
    character_id: u64,
    character_name: &str,
) -> SkillQueueEntry {
    SkillQueueEntry {
        character_id,
        character_name: character_name.to_owned(),
        skill_id: entry.skill_id,
        // The placeholder stands until the daemon names it from the bundled
        // dataset, in the same shape as an industry item's.
        skill_name: format!("Skill {}", entry.skill_id),
        level: entry.finished_level,
        position: entry.queue_position,
        start_at: entry.start_date,
        finish_at: entry.finish_date,
        level_start_sp: entry.level_start_sp,
        level_end_sp: entry.level_end_sp,
        training_start_sp: entry.training_start_sp,
    }
}

async fn poll_industry_session(
    sso: &SsoClient,
    esi: &reqwest::Client,
    metadata: &SsoMetadata,
    client_id: &str,
    session: &mut CharacterSession,
    error_limit_reset: &mut Option<Duration>,
) -> Result<bool> {
    if Instant::now() < session.next_industry_at {
        return Ok(false);
    }
    ensure_access_token(sso, metadata, client_id, session).await?;
    let access = session
        .access_token
        .as_ref()
        .context("access token was not initialized")?;
    let url = format!(
        "https://esi.evetech.net/characters/{}/industry/jobs",
        session.character.id
    );
    let fetched = get_esi::<Vec<RawIndustryJob>>(
        esi,
        &url,
        &access.secret,
        INDUSTRY_DEFAULT_TTL,
        session.industry_etag.as_deref(),
        error_limit_reset,
    )
    .await
    .context("could not refresh industry jobs")?;

    let ttl = fetched.ttl();
    let EsiFetch::Fresh {
        value: raw_jobs,
        etag,
        ..
    } = fetched
    else {
        // Nothing changed, so there is nothing to reconcile.
        session.industry_error = None;
        session.next_industry_at = Instant::now() + ttl;
        return Ok(false);
    };
    session.industry_etag = etag;
    session.industry_jobs = raw_jobs
        .into_iter()
        .map(|job| {
            convert_job(
                job,
                session.character.id,
                session.character.name.clone(),
                JobOwner::Personal,
                None,
            )
        })
        .collect();
    session.industry_error = None;
    session.next_industry_at = Instant::now() + ttl;
    Ok(true)
}

fn convert_job(
    job: RawIndustryJob,
    character_id: u64,
    character_name: String,
    owner: JobOwner,
    corporation_id: Option<u64>,
) -> IndustryJob {
    let item_type_id = job.product_type_id.unwrap_or(job.blueprint_type_id);
    // A corporation job installed by someone else is attributed to its
    // installer, so the panel can say whose it is to collect.
    let installer = job.installer_id.unwrap_or(character_id);
    IndustryJob {
        character_id: installer,
        // Never the reader's name for someone else's job: that told the panel
        // the wrong pilot should collect every line in the factory. The
        // placeholder stands until `/universe/names` answers, in the same shape
        // as the item name's.
        character_name: if installer == character_id {
            character_name
        } else {
            format!("Character {installer}")
        },
        job_id: job.job_id,
        activity_id: job.activity_id,
        item_type_id,
        item_name: format!("Type {item_type_id}"),
        runs: job.runs,
        status: job.status,
        start_at: job.start_date,
        end_at: job.end_date,
        owner,
        corporation_id,
    }
}

/// Picks the character that reads the corporation job list this poll.
///
/// Preferring an authorized character EVE has not refused keeps corporation
/// tracking alive when one alt loses the Factory_Manager role. When every
/// authorized character is refused the first is still returned, so its refusal
/// reaches the health line instead of the read going quiet.
fn select_corporation_reader(sessions: &[CharacterSession], now: Instant) -> Option<usize> {
    sessions
        .iter()
        .position(|session| {
            session.may_read_corporation_jobs() && !session.corporation_role_denied(now)
        })
        .or_else(|| {
            sessions
                .iter()
                .position(CharacterSession::may_read_corporation_jobs)
        })
}

/// Reads corporation industry jobs through the one authorized character.
///
/// Returns whether the job list changed. A 403 means the character holds the
/// scope but not the in-game Factory_Manager role, which is reported as its own
/// health state instead of a generic refresh failure.
async fn poll_corporation_session(
    sso: &SsoClient,
    esi: &reqwest::Client,
    metadata: &SsoMetadata,
    client_id: &str,
    session: &mut CharacterSession,
    error_limit_reset: &mut Option<Duration>,
) -> Result<bool> {
    if Instant::now() < session.next_corporation_at {
        return Ok(false);
    }
    ensure_access_token(sso, metadata, client_id, session).await?;

    if session.corporation_id.is_none() || Instant::now() >= session.next_affiliation_at {
        let url = format!(
            "https://esi.evetech.net/characters/{}",
            session.character.id
        );
        let access = session
            .access_token
            .as_ref()
            .context("access token was not initialized")?;
        let fetched = get_esi::<CharacterAffiliation>(
            esi,
            &url,
            &access.secret,
            AFFILIATION_TTL,
            None,
            error_limit_reset,
        )
        .await
        .context("could not read corporation membership")?;
        session.next_affiliation_at = Instant::now() + fetched.ttl();
        if let EsiFetch::Fresh { value, .. } = fetched {
            // A corporation change invalidates the cached job list, and the
            // role has to be re-proven against the new corporation.
            if session.corporation_id != Some(value.corporation_id) {
                session.corporation_etag = None;
                session.corporation_jobs.clear();
            }
            session.corporation_id = Some(value.corporation_id);
        }
    }

    let corporation_id = session
        .corporation_id
        .context("corporation membership is unknown")?;
    let access = session
        .access_token
        .as_ref()
        .context("access token was not initialized")?;
    let url = format!("https://esi.evetech.net/corporations/{corporation_id}/industry/jobs");
    let fetched = get_esi::<Vec<RawIndustryJob>>(
        esi,
        &url,
        &access.secret,
        INDUSTRY_DEFAULT_TTL,
        session.corporation_etag.as_deref(),
        error_limit_reset,
    )
    .await
    .context("could not refresh corporation industry jobs")?;

    let ttl = fetched.ttl();
    let EsiFetch::Fresh { value, etag, .. } = fetched else {
        session.corporation_error = None;
        session.corporation_role_denied_at = None;
        session.next_corporation_at = Instant::now() + ttl;
        return Ok(false);
    };
    session.corporation_etag = etag;
    session.corporation_jobs = value
        .into_iter()
        .map(|job| {
            convert_job(
                job,
                session.character.id,
                session.character.name.clone(),
                JobOwner::Corporation,
                Some(corporation_id),
            )
        })
        .collect();
    session.corporation_error = None;
    session.corporation_role_denied_at = None;
    session.next_corporation_at = Instant::now() + ttl;
    Ok(true)
}

async fn poll_session(
    sso: &SsoClient,
    esi: &reqwest::Client,
    metadata: &SsoMetadata,
    client_id: &str,
    session: &mut CharacterSession,
    error_limit_reset: &mut Option<Duration>,
) -> Result<()> {
    let now = Instant::now();
    // Nothing is due yet. Checked before anything that can fail, so a permanent
    // failure such as a missing credential backs off instead of reporting once
    // per poll tick.
    if now < session.next_online_at && now < session.next_location_at {
        return Ok(());
    }
    if session.refresh_token.is_none() {
        bail!("refresh token is missing from Secret Service");
    }
    ensure_access_token(sso, metadata, client_id, session).await?;

    if now >= session.next_online_at {
        let access = session
            .access_token
            .as_ref()
            .context("access token was not initialized")?;
        let url = format!(
            "https://esi.evetech.net/characters/{}/online",
            session.character.id
        );
        let fetched = get_esi::<OnlineResponse>(
            esi,
            &url,
            &access.secret,
            ONLINE_DEFAULT_TTL,
            session.online_etag.as_deref(),
            error_limit_reset,
        )
        .await
        .context("could not refresh online state")?;

        session.next_online_at = Instant::now() + fetched.ttl();
        session.presence.error = None;
        if let EsiFetch::Fresh { value, etag, .. } = fetched {
            session.online_etag = etag;
            session.presence.online = Some(value.online);
        }
        // An offline character has no location to poll, so its presence
        // observation has to be stamped here to stay fresh.
        if session.presence.online == Some(false) {
            session.presence.observed_at = Some(Utc::now());
        }
    }

    if session.presence.online == Some(true) && now >= session.next_location_at {
        let access = session
            .access_token
            .as_ref()
            .context("access token was not initialized")?;
        let url = format!(
            "https://esi.evetech.net/characters/{}/location",
            session.character.id
        );
        let fetched = get_esi::<LocationResponse>(
            esi,
            &url,
            &access.secret,
            LOCATION_DEFAULT_TTL,
            session.location_etag.as_deref(),
            error_limit_reset,
        )
        .await
        .context("could not refresh character location")?;

        session.next_location_at = Instant::now() + fetched.ttl();
        if let EsiFetch::Fresh { value, etag, .. } = fetched {
            session.location_etag = etag;
            session.presence.solar_system_id = Some(value.solar_system_id);
            session.presence.docked =
                Some(value.station_id.is_some() || value.structure_id.is_some());
        }
        // A 304 confirms the cached location is still current, so it counts as
        // a fresh observation for the stale-presence policy.
        session.presence.observed_at = Some(Utc::now());
        session.presence.error = None;
    }

    Ok(())
}

async fn ensure_access_token(
    sso: &SsoClient,
    metadata: &SsoMetadata,
    client_id: &str,
    session: &mut CharacterSession,
) -> Result<()> {
    let still_valid = session.access_token.as_ref().is_some_and(|token| {
        token.expires_at > Utc::now() + chrono::Duration::seconds(TOKEN_EXPIRY_MARGIN_SECONDS)
    });
    if still_valid {
        return Ok(());
    }

    // A refresh token that was rotated but never written to Secret Service is
    // the only copy that exists, so persist it before spending it again.
    if !session.refresh_token_persisted {
        persist_refresh_token(session);
    }

    // Twice at most: once with the grant this session holds, and once more if
    // that grant turns out to have been replaced by `auth add` while it was
    // being refreshed. The second pass uses the newer grant, whose scopes are
    // the ones the store now records for the character.
    for attempt in 0..2 {
        let refresh_token = session
            .refresh_token
            .clone()
            .context("refresh token is missing from Secret Service")?;
        let (access, rotated_refresh) = sso
            .refresh(metadata, client_id, &refresh_token)
            .await
            .context("could not refresh EVE authorization")?;
        if access.character.id != session.character.id {
            bail!("refreshed token belongs to a different character");
        }
        if let Some(rotated_refresh) = rotated_refresh {
            // CCP has already invalidated the previous token, so the rotated
            // one is adopted in memory first and only then written out. A
            // failed write is retried rather than discarding the token and
            // orphaning the character.
            session.refresh_token = Some(rotated_refresh);
            session.refresh_token_persisted = false;
            if persist_refresh_token(session) == Persisted::Superseded && attempt == 0 {
                continue;
            }
        }
        session.access_token = Some(access);
        return Ok(());
    }
    bail!("the EVE authorization was replaced twice while refreshing")
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Persisted {
    /// Written, or nothing to write.
    Stored,
    /// Secret Service held a credential this session did not put there, so it
    /// was adopted instead of overwritten.
    Superseded,
    /// The write failed and will be retried before the next refresh.
    Failed,
}

fn persist_refresh_token(session: &mut CharacterSession) -> Persisted {
    let Some(token) = session.refresh_token.clone() else {
        return Persisted::Stored;
    };
    // Read before writing. `auth add` runs in another process and stores a new
    // grant for the same character; if this session's rotated token from the
    // old grant landed afterwards it would replace the new one, and the
    // character would keep the old scopes until someone noticed. When the
    // stored credential is not the one this session last saw, it is the newer
    // grant, and the right move is to take it rather than bury it.
    match session.secrets.lookup(session.character.id) {
        Ok(Some(existing))
            if session
                .stored_refresh_token
                .as_ref()
                .is_some_and(|known| known.expose_secret() != existing.expose_secret()) =>
        {
            info!(
                character = session.character.name,
                "A newer EVE authorization was stored for this character; using it"
            );
            session.stored_refresh_token = Some(existing.clone());
            session.refresh_token = Some(existing);
            session.refresh_token_persisted = true;
            session.access_token = None;
            return Persisted::Superseded;
        }
        Ok(_) => {}
        Err(error) => debug!(
            character = session.character.name,
            error = %error,
            "Could not read Secret Service before writing the rotated token"
        ),
    }
    match session
        .secrets
        .store(session.character.id, &session.character.name, &token)
    {
        Ok(()) => {
            session.refresh_token_persisted = true;
            session.stored_refresh_token = Some(token);
            Persisted::Stored
        }
        Err(error) => {
            warn!(
                character = session.character.name,
                error = %error,
                "Could not persist the rotated EVE refresh token; retrying on the next refresh"
            );
            Persisted::Failed
        }
    }
}

/// Fetches an unauthenticated ESI resource.
///
/// Separate from `get_esi` because sending a bearer token to a public endpoint
/// would spend a character's authorization for nothing, and these run even
/// when no character is linked.
async fn get_public<T>(
    client: &reqwest::Client,
    url: &str,
    default_ttl: Duration,
    etag: Option<&str>,
    error_limit_reset: &mut Option<Duration>,
) -> Result<EsiFetch<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut request = client
        .get(url)
        .header("X-Compatibility-Date", COMPATIBILITY_DATE);
    if let Some(etag) = etag {
        request = request.header(header::IF_NONE_MATCH, etag);
    }

    let response = request.send().await.context("ESI request failed")?;
    let status = response.status();
    let ttl = response_ttl(&response, default_ttl);
    if let Some(reset) = exhausted_error_budget(&response) {
        *error_limit_reset = Some(reset);
    }
    if status == StatusCode::NOT_MODIFIED {
        return Ok(EsiFetch::NotModified { ttl });
    }
    if !status.is_success() {
        bail!("ESI returned {status}");
    }

    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    Ok(EsiFetch::Fresh {
        value: response
            .json()
            .await
            .context("could not parse ESI response")?,
        etag,
        ttl,
    })
}

fn is_unauthorized(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<EsiResponseError>()
        .is_some_and(|error| error.status == StatusCode::UNAUTHORIZED)
}

fn retry_delay_for(error: &anyhow::Error) -> Duration {
    with_jitter(
        error
            .downcast_ref::<EsiResponseError>()
            .and_then(|error| error.retry_after)
            .unwrap_or(ERROR_RETRY_DELAY),
    )
}

/// The result of one conditional ESI request.
#[derive(Debug)]
enum EsiFetch<T> {
    /// The resource was returned in full.
    Fresh {
        value: T,
        etag: Option<String>,
        ttl: Duration,
    },
    /// The cached copy is still current. ESI does not charge a 304 against the
    /// error budget, which is why every poll sends `If-None-Match`.
    NotModified { ttl: Duration },
}

impl<T> EsiFetch<T> {
    fn ttl(&self) -> Duration {
        match self {
            Self::Fresh { ttl, .. } | Self::NotModified { ttl } => *ttl,
        }
    }
}

async fn get_esi<T>(
    client: &reqwest::Client,
    url: &str,
    access_token: &SecretString,
    default_ttl: Duration,
    etag: Option<&str>,
    error_limit_reset: &mut Option<Duration>,
) -> Result<EsiFetch<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut request = client
        .get(url)
        .bearer_auth(access_token.expose_secret())
        .header("X-Compatibility-Date", COMPATIBILITY_DATE);
    if let Some(etag) = etag {
        request = request.header(header::IF_NONE_MATCH, etag);
    }

    let response = request.send().await.context("ESI request failed")?;
    let status = response.status();
    let ttl = response_ttl(&response, default_ttl);

    if let Some(reset) = exhausted_error_budget(&response) {
        *error_limit_reset = Some(reset);
    }

    if status == StatusCode::NOT_MODIFIED {
        return Ok(EsiFetch::NotModified { ttl });
    }
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let detail = response
            .json::<EsiErrorBody>()
            .await
            .ok()
            .and_then(|body| body.error)
            .unwrap_or_else(|| "no safe error detail was returned".to_owned());
        return Err(EsiResponseError {
            status,
            detail,
            retry_after,
        }
        .into());
    }

    let etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    Ok(EsiFetch::Fresh {
        value: response
            .json()
            .await
            .context("could not parse ESI response")?,
        etag,
        ttl,
    })
}

fn response_ttl(response: &reqwest::Response, default_ttl: Duration) -> Duration {
    response
        .headers()
        .get(header::EXPIRES)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
        .and_then(|expires| expires.duration_since(SystemTime::now()).ok())
        .filter(|ttl| !ttl.is_zero())
        .unwrap_or(default_ttl)
}

/// ESI advertises the remaining error budget and the seconds until it resets on
/// every response. Honouring it is how a client avoids being throttled outright.
fn exhausted_error_budget(response: &reqwest::Response) -> Option<Duration> {
    let header_value = |name: &str| -> Option<u64> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
    };

    let remain = header_value("x-esi-error-limit-remain")?;
    if remain > u64::from(ERROR_LIMIT_FLOOR) {
        return None;
    }
    Some(Duration::from_secs(
        header_value("x-esi-error-limit-reset").unwrap_or(60).max(1),
    ))
}

/// A scripted HTTP server standing in for ESI.
///
/// Real ESI is HTTPS with signed tokens, so these tests target the fetch layer
/// where the cache, role and rate-limit contracts actually live. Each canned
/// response is served once, in order, and the server records what it received
/// so conditional-request behaviour can be asserted from both sides.
#[cfg(test)]
mod fake_esi {
    use std::sync::{Arc, Mutex};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    pub struct Canned {
        pub status: &'static str,
        pub headers: Vec<(&'static str, String)>,
        pub body: &'static str,
    }

    impl Canned {
        pub fn ok(body: &'static str) -> Self {
            Self {
                status: "200 OK",
                headers: Vec::new(),
                body,
            }
        }

        pub fn status(status: &'static str, body: &'static str) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body,
            }
        }

        pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
            self.headers.push((name, value.into()));
            self
        }
    }

    pub struct FakeEsi {
        pub base: String,
        pub requests: Arc<Mutex<Vec<String>>>,
        handle: JoinHandle<()>,
    }

    impl FakeEsi {
        pub async fn start(responses: Vec<Canned>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);

            let handle = tokio::spawn(async move {
                for response in responses {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    let mut buffer = vec![0_u8; 8192];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    recorded
                        .lock()
                        .expect("not poisoned")
                        .push(String::from_utf8_lossy(&buffer[..read]).into_owned());

                    let mut head = format!("HTTP/1.1 {}\r\n", response.status);
                    for (name, value) in &response.headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str(&format!(
                        "content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response.body.len()
                    ));
                    head.push_str(response.body);
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            });

            Self {
                base,
                requests,
                handle,
            }
        }

        pub fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("not poisoned").clone()
        }
    }

    impl Drop for FakeEsi {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What EVEmarchy's configuration asks for when it says nothing.
    const EVEMARCHY_DEFAULT_INDUSTRY: IndustryPolling = IndustryPolling {
        enabled: true,
        character_jobs: true,
        corporation_jobs: false,
    };

    fn raw_job(job_id: u64, installer_id: Option<u64>) -> RawIndustryJob {
        RawIndustryJob {
            job_id,
            activity_id: 1,
            blueprint_type_id: 1_000,
            product_type_id: Some(2_000),
            runs: 4,
            status: "active".to_owned(),
            start_date: "2026-09-04T00:00:00Z".parse().unwrap(),
            end_date: "2026-09-04T06:00:00Z".parse().unwrap(),
            installer_id,
        }
    }

    /// A session with only the fields corporation reader selection reads.
    fn corporation_session(
        id: u64,
        name: &str,
        scopes: &[&str],
        role_denied_at: Option<Instant>,
    ) -> CharacterSession {
        let now = Instant::now();
        CharacterSession {
            character: LinkedCharacter {
                id,
                name: name.to_owned(),
                scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
                authorized_at: "2026-09-04T00:00:00Z".parse().unwrap(),
            },
            secrets: Arc::new(crate::secrets::MemoryStore::default()),
            refresh_token: None,
            refresh_token_persisted: true,
            stored_refresh_token: None,
            next_secret_retry_at: now,
            access_token: None,
            presence: CharacterPresence {
                character_id: id,
                character_name: name.to_owned(),
                online: None,
                solar_system_id: None,
                solar_system_name: None,
                region_name: None,
                portrait: None,
                docked: None,
                observed_at: None,
                error: None,
                skill_queue: None,
                skill_queue_error: None,
                ship: None,
                ship_error: None,
                industry_slots: None,
            },
            next_online_at: now,
            next_location_at: now,
            next_industry_at: now,
            online_etag: None,
            location_etag: None,
            industry_etag: None,
            industry_jobs: Vec::new(),
            industry_error: None,
            next_skills_at: now,
            skills_etag: None,
            skill_queue: Vec::new(),
            next_ship_at: now,
            ship_etag: None,
            next_skill_levels_at: now,
            skill_levels_etag: None,
            skill_levels: None,
            corporation_id: None,
            next_affiliation_at: now,
            next_corporation_at: now,
            corporation_etag: None,
            corporation_jobs: Vec::new(),
            corporation_error: None,
            corporation_role_denied_at: role_denied_at,
        }
    }

    #[test]
    fn a_paused_queue_entry_converts_without_dates() {
        let entry = convert_skill(
            RawSkillQueueEntry {
                skill_id: 3330,
                finished_level: 5,
                queue_position: 0,
                start_date: None,
                finish_date: None,
                level_start_sp: Some(256_000),
                level_end_sp: Some(1_448_000),
                training_start_sp: Some(300_000),
            },
            42,
            "Example Pilot",
        );

        assert_eq!(entry.skill_name, "Skill 3330");
        assert_eq!(entry.level, 5);
        assert!(entry.finish_at.is_none());
        assert!(!entry.is_finished(Utc::now()));
    }

    #[test]
    fn a_character_eve_refused_hands_the_read_to_another_authorized_one() {
        // Swapping the corporation's CEO takes the implicit Factory_Manager
        // role away from the old one. Picking the first authorized character
        // regardless left the read on a character EVE answers 403 to, so
        // corporation tracking stayed dark with a working reader right there.
        let now = Instant::now();
        let sessions = vec![
            corporation_session(1, "Refused", &[CORPORATION_JOBS_SCOPE], Some(now)),
            corporation_session(2, "Allowed", &[CORPORATION_JOBS_SCOPE], None),
        ];

        assert_eq!(select_corporation_reader(&sessions, now), Some(1));
    }

    #[test]
    fn an_unauthorized_character_is_never_the_reader() {
        let now = Instant::now();
        let sessions = vec![
            corporation_session(1, "No scope", &["esi-location.read_online.v1"], None),
            corporation_session(2, "Authorized", &[CORPORATION_JOBS_SCOPE], None),
        ];

        assert_eq!(select_corporation_reader(&sessions, now), Some(1));
    }

    #[test]
    fn the_last_refused_character_still_reads_so_the_refusal_is_reported() {
        // With nobody left to hand the read to, the refusal has to keep
        // happening: it is what names the role the panel asks to be granted.
        let now = Instant::now();
        let sessions = vec![
            corporation_session(1, "Refused", &[CORPORATION_JOBS_SCOPE], Some(now)),
            corporation_session(2, "Also refused", &[CORPORATION_JOBS_SCOPE], Some(now)),
        ];

        assert_eq!(select_corporation_reader(&sessions, now), Some(0));
    }

    #[test]
    fn a_refusal_expires_so_a_restored_role_is_picked_up_on_its_own() {
        let denied_at = Instant::now();
        let sessions = [corporation_session(
            1,
            "Refused",
            &[CORPORATION_JOBS_SCOPE],
            Some(denied_at),
        )];

        assert!(sessions[0].corporation_role_denied(denied_at + ROLE_RETRY_DELAY / 2));
        // Past the window the character is eligible again, so granting the
        // role back in game is enough; no re-authorization is needed.
        assert!(!sessions[0].corporation_role_denied(denied_at + ROLE_RETRY_DELAY));
    }

    #[test]
    fn a_job_someone_else_installed_never_wears_the_readers_name() {
        // One character is authorized to read the corporation listing; every
        // job in it was installed by whoever had the blueprint. Attributing
        // them all to the reader told the panel the wrong pilot should collect
        // the entire factory.
        let job = convert_job(
            raw_job(1, Some(99)),
            42,
            "Reader".to_owned(),
            JobOwner::Corporation,
            Some(98_000_001),
        );

        assert_eq!(job.character_id, 99);
        assert_ne!(job.character_name, "Reader");
        assert!(job.character_name.contains("99"), "{}", job.character_name);
    }

    #[test]
    fn a_job_the_reader_installed_keeps_the_name_already_known() {
        let job = convert_job(
            raw_job(2, Some(42)),
            42,
            "Reader".to_owned(),
            JobOwner::Corporation,
            Some(98_000_001),
        );

        // No placeholder and no request: the reader's own name is already in
        // hand.
        assert_eq!(job.character_id, 42);
        assert_eq!(job.character_name, "Reader");
    }

    #[test]
    fn a_personal_job_carries_no_installer_and_keeps_its_character() {
        let job = convert_job(
            raw_job(3, None),
            42,
            "Reader".to_owned(),
            JobOwner::Personal,
            None,
        );

        assert_eq!(job.character_id, 42);
        assert_eq!(job.character_name, "Reader");
    }

    #[test]
    fn resolved_names_replace_the_placeholders() {
        let mut service = EsiService::new(
            crate::app::TEST_APP,
            None,
            EVEMARCHY_DEFAULT_INDUSTRY,
            Vec::new(),
            Arc::new(crate::secrets::MemoryStore::default()),
        )
        .unwrap();
        service
            .character_names
            .insert(99, "Real Installer".to_owned());
        let mut jobs = vec![
            convert_job(
                raw_job(1, Some(99)),
                42,
                "Reader".to_owned(),
                JobOwner::Corporation,
                Some(98_000_001),
            ),
            convert_job(
                raw_job(2, Some(100)),
                42,
                "Reader".to_owned(),
                JobOwner::Corporation,
                Some(98_000_001),
            ),
        ];

        service.name_installers(&mut jobs);

        assert_eq!(jobs[0].character_name, "Real Installer");
        // Still unresolved, so it keeps the placeholder rather than borrowing
        // the previous job's name or the reader's.
        assert!(jobs[1].character_name.contains("100"));
    }

    #[test]
    fn linked_characters_seed_the_name_cache() {
        // An installer who is one of your own alts must never cost a request.
        let service = EsiService::new(
            crate::app::TEST_APP,
            None,
            EVEMARCHY_DEFAULT_INDUSTRY,
            vec![LinkedCharacter {
                id: 7,
                name: "Own Alt".to_owned(),
                scopes: Vec::new(),
                authorized_at: "2026-09-03T09:00:00Z".parse().unwrap(),
            }],
            Arc::new(crate::secrets::MemoryStore::default()),
        )
        .unwrap();

        assert_eq!(
            service.character_names.get(&7).map(String::as_str),
            Some("Own Alt")
        );
    }

    #[test]
    fn unconfigured_service_does_not_require_characters() {
        let service = EsiService::new(
            crate::app::TEST_APP,
            None,
            EVEMARCHY_DEFAULT_INDUSTRY,
            Vec::new(),
            Arc::new(crate::secrets::MemoryStore::default()),
        )
        .unwrap();
        assert!(!service.is_configured());
        assert_eq!(service.linked_character_count(), 0);
        assert!(service.matches_linked_characters(&[]));
        assert!(!service.matches_linked_characters(&[LinkedCharacter {
            id: 42,
            name: "Example Pilot".to_owned(),
            scopes: Vec::new(),
            authorized_at: "2026-09-03T09:00:00Z".parse().unwrap(),
        }]));
        assert!(service.characters().is_empty());
    }

    #[tokio::test]
    async fn a_conditional_request_sends_the_stored_etag_and_honours_a_304() {
        use fake_esi::{Canned, FakeEsi};

        let server = FakeEsi::start(vec![
            Canned::ok(r#"{"online":true}"#)
                .header("etag", "\"first\"")
                .header("expires", "Thu, 03 Sep 2026 09:00:30 GMT"),
            Canned::status("304 Not Modified", ""),
        ])
        .await;
        let client = reqwest::Client::new();
        let token = SecretString::from("access-token".to_owned());
        let url = format!("{}/characters/42/online", server.base);
        let mut budget = None;

        let first =
            get_esi::<OnlineResponse>(&client, &url, &token, ONLINE_DEFAULT_TTL, None, &mut budget)
                .await
                .unwrap();
        let EsiFetch::Fresh { value, etag, .. } = first else {
            panic!("the first response must be fresh");
        };
        assert!(value.online);
        assert_eq!(etag.as_deref(), Some("\"first\""));

        let second = get_esi::<OnlineResponse>(
            &client,
            &url,
            &token,
            ONLINE_DEFAULT_TTL,
            etag.as_deref(),
            &mut budget,
        )
        .await
        .unwrap();
        assert!(matches!(second, EsiFetch::NotModified { .. }));

        let requests = server.requests();
        assert!(!requests[0].to_lowercase().contains("if-none-match"));
        assert!(
            requests[1]
                .to_lowercase()
                .contains("if-none-match: \"first\""),
            "{}",
            requests[1]
        );
        // Every request must identify the reviewed compatibility date.
        assert!(requests[0].contains(COMPATIBILITY_DATE));
    }

    #[tokio::test]
    async fn a_missing_corporation_role_surfaces_as_a_forbidden_response() {
        use fake_esi::{Canned, FakeEsi};

        let server = FakeEsi::start(vec![Canned::status(
            "403 Forbidden",
            r#"{"error":"character does not have required role(s)"}"#,
        )])
        .await;
        let client = reqwest::Client::new();
        let token = SecretString::from("access-token".to_owned());
        let mut budget = None;

        let error = get_esi::<Vec<RawIndustryJob>>(
            &client,
            &format!("{}/corporations/98000001/industry/jobs", server.base),
            &token,
            INDUSTRY_DEFAULT_TTL,
            None,
            &mut budget,
        )
        .await
        .unwrap_err();

        let response = error.downcast_ref::<EsiResponseError>().unwrap();
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert!(response.detail.contains("role"));
        // A role problem is not a rate-limit problem.
        assert!(budget.is_none());
    }

    #[tokio::test]
    async fn a_depleted_error_budget_reports_when_to_resume() {
        use fake_esi::{Canned, FakeEsi};

        let server = FakeEsi::start(vec![
            Canned::status("420 Error Limited", r#"{"error":"error limited"}"#)
                .header("x-esi-error-limit-remain", "0")
                .header("x-esi-error-limit-reset", "47"),
        ])
        .await;
        let client = reqwest::Client::new();
        let token = SecretString::from("access-token".to_owned());
        let mut budget = None;

        let outcome = get_esi::<OnlineResponse>(
            &client,
            &format!("{}/characters/42/online", server.base),
            &token,
            ONLINE_DEFAULT_TTL,
            None,
            &mut budget,
        )
        .await;

        assert!(outcome.is_err());
        assert_eq!(budget, Some(Duration::from_secs(47)));
    }

    #[tokio::test]
    async fn a_healthy_error_budget_does_not_pause_requests() {
        use fake_esi::{Canned, FakeEsi};

        let server = FakeEsi::start(vec![
            Canned::ok(r#"{"online":false}"#).header("x-esi-error-limit-remain", "100"),
        ])
        .await;
        let client = reqwest::Client::new();
        let token = SecretString::from("access-token".to_owned());
        let mut budget = None;

        get_esi::<OnlineResponse>(
            &client,
            &format!("{}/characters/42/online", server.base),
            &token,
            ONLINE_DEFAULT_TTL,
            None,
            &mut budget,
        )
        .await
        .unwrap();

        assert!(budget.is_none());
    }

    #[tokio::test]
    async fn a_rate_limited_response_carries_its_retry_after() {
        use fake_esi::{Canned, FakeEsi};

        let server = FakeEsi::start(vec![
            Canned::status("429 Too Many Requests", r#"{"error":"slow down"}"#)
                .header("retry-after", "12"),
        ])
        .await;
        let client = reqwest::Client::new();
        let token = SecretString::from("access-token".to_owned());
        let mut budget = None;

        let error = get_esi::<OnlineResponse>(
            &client,
            &format!("{}/characters/42/online", server.base),
            &token,
            ONLINE_DEFAULT_TTL,
            None,
            &mut budget,
        )
        .await
        .unwrap_err();

        assert!(retry_delay_for(&error).as_secs() >= 12);
        let response = error.downcast_ref::<EsiResponseError>().unwrap();
        assert_eq!(response.retry_after, Some(Duration::from_secs(12)));
    }

    #[tokio::test]
    async fn an_expires_header_overrides_the_default_cache_window() {
        use fake_esi::{Canned, FakeEsi};

        let expires = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(600));
        let server = FakeEsi::start(vec![
            Canned::ok(r#"{"online":true}"#).header("expires", expires),
        ])
        .await;
        let client = reqwest::Client::new();
        let token = SecretString::from("access-token".to_owned());
        let mut budget = None;

        let fetched = get_esi::<OnlineResponse>(
            &client,
            &format!("{}/characters/42/online", server.base),
            &token,
            ONLINE_DEFAULT_TTL,
            None,
            &mut budget,
        )
        .await
        .unwrap();

        // Well beyond the 60 second default, so the header was honoured.
        assert!(fetched.ttl() > Duration::from_secs(500));
    }

    #[test]
    fn docked_state_accepts_stations_or_structures() {
        let station = LocationResponse {
            solar_system_id: 30_000_142,
            station_id: Some(60_000_001),
            structure_id: None,
        };
        let structure = LocationResponse {
            solar_system_id: 30_000_142,
            station_id: None,
            structure_id: Some(1_000_000_000_001),
        };
        let space = LocationResponse {
            solar_system_id: 30_000_142,
            station_id: None,
            structure_id: None,
        };

        assert!(station.station_id.is_some() || station.structure_id.is_some());
        assert!(structure.station_id.is_some() || structure.structure_id.is_some());
        assert!(!(space.station_id.is_some() || space.structure_id.is_some()));
    }

    #[test]
    fn a_poll_that_only_moves_observed_at_is_not_a_change() {
        let before = corporation_session(1, "Pilot", &[], None).presence;
        let mut observed = before.clone();
        observed.observed_at = Some("2026-10-28T08:00:00Z".parse().unwrap());
        assert!(presence_unchanged(
            std::slice::from_ref(&before),
            std::slice::from_ref(&observed)
        ));

        let mut docked = observed;
        docked.docked = Some(true);
        assert!(!presence_unchanged(
            std::slice::from_ref(&before),
            &[docked]
        ));
        assert!(!presence_unchanged(&[before], &[]));
    }
}
