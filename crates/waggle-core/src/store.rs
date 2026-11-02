use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, params};

use crate::{
    directorbot::{NewJabberPing, PapSnapshot, PapStats, parse_ping, parse_pings},
    logs::LogCheckpoint,
    skills::{SkillCompletion, SkillQueueEntry},
};

pub const SCHEMA_VERSION: u32 = 14;

/// Every timestamp column is compared and ordered by SQLite as a plain string,
/// so all of them must share one fixed-width UTC representation. `to_rfc3339`
/// does not qualify: it emits a `+00:00` offset and a variable-length fractional
/// part, and a single value written in another shape would silently break
/// `end_at <= now` comparisons and retention pruning. Migration 6 rewrites any
/// pre-existing rows into this format.
pub fn sql_timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct LinkedCharacter {
    pub id: u64,
    pub name: String,
    pub scopes: Vec<String>,
    pub authorized_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct IntelAlert {
    pub id: u64,
    pub occurred_at: DateTime<Utc>,
    pub channel: String,
    pub author: String,
    pub message: String,
    pub system_id: u32,
    pub system_name: String,
    pub distance: u16,
    pub character_id: u64,
    pub character_name: String,
    pub stale_presence: bool,
    /// The report was only in range through a jump bridge.
    pub via_bridge: bool,
}

#[derive(Debug, Clone)]
pub struct NewIntelAlert {
    pub fingerprint: String,
    pub occurred_at: DateTime<Utc>,
    pub channel: String,
    pub author: String,
    pub message: String,
    pub system_id: u32,
    pub system_name: String,
    pub distance: u16,
    pub character_id: u64,
    pub character_name: String,
    pub stale_presence: bool,
    pub via_bridge: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct JabberPing {
    pub id: u64,
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
    /// Joinable comms link from the broadcast, resolved on demand.
    pub comms_link: Option<String>,
    /// The address the comms link resolved to, once it has been resolved.
    ///
    /// Cached because the redirect shim appears to be single-use: a second
    /// fetch of the same link answers 400. Resolving again on a second click
    /// would therefore fail, so the resolved address is kept and reused.
    pub comms_target: Option<String>,
    pub doctrine: Option<String>,
    pub body: String,
    /// Cache-relative path to the FC's portrait, when one is on disk.
    ///
    /// Derived rather than stored: a broadcast names its FC in words, and the
    /// ID behind that name is resolved by the ESI service and the portrait
    /// fetched by the image cache. Persisting a path would only create a value
    /// that goes stale when the cache is cleared.
    #[serde(default, skip_deserializing)]
    pub fc_portrait: Option<String>,
    /// Whether this broadcast was recorded silently, because its target is one
    /// the operator muted.
    ///
    /// Derived rather than stored, for the same reason as `fc_portrait`:
    /// muting is a configuration decision, so persisting it would freeze one
    /// day's configuration into every row and leave history disagreeing with
    /// the file the moment the list changed.
    #[serde(default, skip_deserializing)]
    pub muted: bool,
    /// Which fleet of its broadcast this is, from 1, and how many the
    /// broadcast announced. Directorbot sends several fleets as one message,
    /// and each is stored as a ping of its own; these say they belong
    /// together.
    #[serde(default = "first_fleet")]
    pub fleet_index: u32,
    #[serde(default = "first_fleet")]
    pub fleet_count: u32,
}

fn first_fleet() -> u32 {
    1
}

/// Tables an application keeps beyond the shared ones.
///
/// Industry tracking is EVEmarchy's and on its way to Nex Industries. Its
/// tables were created by the same numbered migrations as everything else, so
/// the steps stay in place and only run their industry statements when asked:
/// an existing EVEmarchy database migrates exactly as it always did, and a
/// Waggle database never gains tables it has no use for.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct Extras {
    pub industry: bool,
}

#[derive(Debug)]
pub struct Store {
    connection: Connection,
    extras: Extras,
}

impl Store {
    /// The open connection, for an application's own queries against its
    /// extra tables.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, Extras::default())
    }

    pub fn open_with(path: &Path, extras: Extras) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("could not open state database {}", path.display()))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("could not enable SQLite WAL mode")?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .context("could not enable SQLite foreign keys")?;
        // The CLI and the daemon both write, so a concurrent writer must wait
        // rather than immediately failing with SQLITE_BUSY.
        connection
            .pragma_update(None, "busy_timeout", 5_000)
            .context("could not set the SQLite busy timeout")?;

        let store = Self { connection, extras };
        store.migrate()?;
        Ok(store)
    }

    pub fn schema_version(&self) -> Result<u32> {
        self.connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("could not read SQLite schema version")
    }

    pub fn upsert_character(&self, character: &LinkedCharacter) -> Result<()> {
        let id = i64::try_from(character.id).context("character ID exceeds SQLite's range")?;
        let scopes = serde_json::to_string(&character.scopes)?;
        self.connection
            .execute(
                "INSERT INTO linked_characters (character_id, name, scopes_json, authorized_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(character_id) DO UPDATE SET
                   name = excluded.name,
                   scopes_json = excluded.scopes_json,
                   authorized_at = excluded.authorized_at",
                params![
                    id,
                    character.name,
                    scopes,
                    sql_timestamp(character.authorized_at)
                ],
            )
            .context("could not save linked character")?;
        Ok(())
    }

    pub fn linked_characters(&self) -> Result<Vec<LinkedCharacter>> {
        let mut statement = self.connection.prepare(
            "SELECT character_id, name, scopes_json, authorized_at
                 FROM linked_characters ORDER BY name COLLATE NOCASE, character_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        rows.map(|row| {
            let (id, name, scopes, authorized_at) = row?;
            Ok(LinkedCharacter {
                id: u64::try_from(id).context("stored character ID is negative")?,
                name,
                scopes: serde_json::from_str(&scopes)
                    .context("stored character scopes are invalid")?,
                authorized_at: DateTime::parse_from_rfc3339(&authorized_at)
                    .context("stored character authorization date is invalid")?
                    .with_timezone(&Utc),
            })
        })
        .collect()
    }

    pub fn find_character(&self, selector: &str) -> Result<Option<LinkedCharacter>> {
        let numeric_id = selector.parse::<u64>().ok();
        Ok(self.linked_characters()?.into_iter().find(|character| {
            numeric_id == Some(character.id) || character.name.eq_ignore_ascii_case(selector)
        }))
    }

    pub fn delete_character(&self, character_id: u64) -> Result<bool> {
        let id = i64::try_from(character_id).context("character ID exceeds SQLite's range")?;
        let initialized_key = format!("industry_initialized:{character_id}");
        let transaction = self.connection.unchecked_transaction()?;
        let deleted = transaction.execute(
            "DELETE FROM linked_characters WHERE character_id = ?1",
            [id],
        )? > 0;
        if self.extras.industry {
            transaction.execute("DELETE FROM industry_jobs WHERE character_id = ?1", [id])?;
            transaction.execute("DELETE FROM metadata WHERE key = ?1", [initialized_key])?;
        }
        transaction.commit()?;
        Ok(deleted)
    }

    /// Records a matched alert, or `None` when an identical one already exists.
    pub fn record_intel_alert(&self, alert: &NewIntelAlert) -> Result<Option<IntelAlert>> {
        let system_id = i64::from(alert.system_id);
        let character_id =
            i64::try_from(alert.character_id).context("character ID exceeds SQLite's range")?;
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO intel_alerts (
               fingerprint, occurred_at, channel, author, message, system_id, system_name,
               distance, character_id, character_name, stale_presence, created_at,
               via_bridge
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                alert.fingerprint,
                sql_timestamp(alert.occurred_at),
                alert.channel,
                alert.author,
                alert.message,
                system_id,
                alert.system_name,
                i64::from(alert.distance),
                character_id,
                alert.character_name,
                alert.stale_presence,
                sql_timestamp(Utc::now()),
                alert.via_bridge,
            ],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let id = self.connection.last_insert_rowid();
        Ok(Some(self.intel_alert_by_id(id)?))
    }

    pub fn recent_intel_alerts(&self, limit: usize) -> Result<Vec<IntelAlert>> {
        let limit = i64::try_from(limit).context("intel history limit is too large")?;
        let mut statement = self.connection.prepare(
            "SELECT id, occurred_at, channel, author, message, system_id, system_name,
                    distance, character_id, character_name, stale_presence, via_bridge
             FROM intel_alerts ORDER BY occurred_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], map_intel_alert)?;
        rows.map(|row| decode_intel_alert(row?)).collect()
    }
    pub fn mark_intel_notified(&self, id: u64) -> Result<()> {
        let id = i64::try_from(id).context("intel alert ID exceeds SQLite's range")?;
        self.connection.execute(
            "UPDATE intel_alerts SET notified_at = ?1 WHERE id = ?2",
            params![sql_timestamp(Utc::now()), id],
        )?;
        Ok(())
    }
    /// Stores one ping, if it is not already stored.
    ///
    /// `baseline` marks history the daemon is catching up on rather than news:
    /// it is recorded as already notified, so it never raises a notification.
    /// That used to be expressed as already *read*, which was the same idea
    /// wearing the flag that has since been removed.
    pub fn record_jabber_ping(
        &self,
        ping: &NewJabberPing,
        baseline: bool,
    ) -> Result<Option<JabberPing>> {
        let baseline_at = baseline.then(|| sql_timestamp(Utc::now()));
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO jabber_pings (
               fingerprint, occurred_at, received_at, sender_jid, source, sender, target,
               summary, description, fc, fleet, formup, pap_type, comms, doctrine, body,
               created_at, notified_at, comms_link, fleet_index, fleet_count
             ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
               ?16, ?17, ?18, ?19, ?20, ?21
             )",
            params![
                ping.fingerprint,
                sql_timestamp(ping.occurred_at),
                sql_timestamp(ping.received_at),
                ping.from,
                ping.source,
                ping.sender,
                ping.target,
                ping.summary,
                ping.description,
                ping.fc,
                ping.fleet,
                ping.formup,
                ping.pap_type,
                ping.comms,
                ping.doctrine,
                ping.body,
                sql_timestamp(Utc::now()),
                baseline_at,
                ping.comms_link,
                ping.fleet_index,
                ping.fleet_count,
            ],
        )?;
        if inserted == 0 {
            return Ok(None);
        }
        let id = self.connection.last_insert_rowid();
        Ok(Some(self.jabber_ping_by_id(id)?))
    }

    /// Durable chat-log resume points, newest session first.
    ///
    /// Checkpoints older than a week are dropped: EVE's own logs roll over, and
    /// a stale offset into a path that has since been reused would resume in
    /// the wrong place. The session-start check in the monitor is the second
    /// guard against that.
    pub fn log_checkpoints(&self) -> Result<Vec<LogCheckpoint>> {
        let cutoff = sql_timestamp(Utc::now() - TimeDelta::days(7));
        let mut statement = self.connection.prepare(
            "SELECT path, channel, character_id, session_started, offset_bytes
             FROM log_checkpoints WHERE session_started >= ?1
             ORDER BY session_started DESC",
        )?;
        let rows = statement.query_map([cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;

        rows.map(|row| {
            let (path, channel, character_id, session_started, offset) = row?;
            Ok(LogCheckpoint {
                path: PathBuf::from(path),
                channel,
                character_id: u64::try_from(character_id)
                    .context("stored checkpoint character ID is negative")?,
                session_started: DateTime::parse_from_rfc3339(&session_started)
                    .context("stored checkpoint session date is invalid")?
                    .with_timezone(&Utc),
                offset: u64::try_from(offset).context("stored checkpoint offset is negative")?,
            })
        })
        .collect()
    }

    /// Replaces the stored resume points and prunes anything a week old.
    pub fn save_log_checkpoints(&self, checkpoints: &[LogCheckpoint]) -> Result<()> {
        let now = sql_timestamp(Utc::now());
        let cutoff = sql_timestamp(Utc::now() - TimeDelta::days(7));
        let transaction = self.connection.unchecked_transaction()?;
        for checkpoint in checkpoints {
            let path = checkpoint
                .path
                .to_str()
                .context("chat log paths must be valid UTF-8 to be checkpointed")?;
            transaction.execute(
                "INSERT INTO log_checkpoints (
                   path, channel, character_id, session_started, offset_bytes, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET
                   channel = excluded.channel,
                   character_id = excluded.character_id,
                   session_started = excluded.session_started,
                   offset_bytes = excluded.offset_bytes,
                   updated_at = excluded.updated_at",
                params![
                    path,
                    checkpoint.channel,
                    i64::try_from(checkpoint.character_id)
                        .context("character ID exceeds SQLite's range")?,
                    sql_timestamp(checkpoint.session_started),
                    i64::try_from(checkpoint.offset).context("offset exceeds SQLite's range")?,
                    now,
                ],
            )?;
        }
        transaction.execute(
            "DELETE FROM log_checkpoints WHERE session_started < ?1",
            [cutoff],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The newest Directorbot message the daemon has ever stored.
    ///
    /// This is the durable high-water mark used to decide whether a message is
    /// new. Using the process start time instead would silently swallow any
    /// ping that arrived shortly before a restart or an upgrade.
    pub fn latest_jabber_ping_at(&self) -> Result<Option<DateTime<Utc>>> {
        let newest: Option<String> =
            self.connection
                .query_row("SELECT MAX(occurred_at) FROM jabber_pings", [], |row| {
                    row.get(0)
                })?;
        newest
            .map(|value| {
                Ok(DateTime::parse_from_rfc3339(&value)
                    .context("stored Jabber ping date is invalid")?
                    .with_timezone(&Utc))
            })
            .transpose()
    }

    /// The newest stored ping that carries a joinable comms link, or one by ID.
    ///
    /// "Newest with a link" is the useful default: the reason to join comms is
    /// almost always the ping that just arrived.
    pub fn jabber_ping_for_comms(&self, id: Option<u64>) -> Result<Option<JabberPing>> {
        let raw = match id {
            Some(id) => self.connection.query_row(
                "SELECT id, occurred_at, received_at, sender_jid, source, sender, target,
                        summary, description, fc, fleet, formup, pap_type, comms, doctrine, body,
                        comms_link, comms_target, fleet_index, fleet_count
                 FROM jabber_pings WHERE id = ?1",
                [i64::try_from(id).context("ping ID exceeds SQLite's range")?],
                map_jabber_ping,
            ),
            None => self.connection.query_row(
                "SELECT id, occurred_at, received_at, sender_jid, source, sender, target,
                        summary, description, fc, fleet, formup, pap_type, comms, doctrine, body,
                        comms_link, comms_target, fleet_index, fleet_count
                 FROM jabber_pings WHERE comms_link IS NOT NULL
                 ORDER BY occurred_at DESC, fleet_index ASC, id DESC LIMIT 1",
                [],
                map_jabber_ping,
            ),
        };
        match raw {
            Ok(raw) => Ok(Some(decode_jabber_ping(raw)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Remembers what a ping's comms link resolved to.
    pub fn set_comms_target(&self, id: u64, target: &str) -> Result<()> {
        let id = i64::try_from(id).context("ping ID exceeds SQLite's range")?;
        self.connection.execute(
            "UPDATE jabber_pings SET comms_target = ?1 WHERE id = ?2",
            params![target, id],
        )?;
        Ok(())
    }

    pub fn recent_jabber_pings(&self, limit: usize) -> Result<Vec<JabberPing>> {
        let limit = i64::try_from(limit).context("Jabber ping history limit is too large")?;
        let mut statement = self.connection.prepare(
            "SELECT id, occurred_at, received_at, sender_jid, source, sender, target,
                    summary, description, fc, fleet, formup, pap_type, comms, doctrine, body,
                    comms_link, comms_target, fleet_index, fleet_count
             FROM jabber_pings ORDER BY occurred_at DESC, fleet_index ASC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], map_jabber_ping)?;
        rows.map(|row| decode_jabber_ping(row?)).collect()
    }
    pub fn mark_jabber_ping_notified(&self, id: u64) -> Result<()> {
        let id = i64::try_from(id).context("Jabber ping ID exceeds SQLite's range")?;
        self.connection.execute(
            "UPDATE jabber_pings SET notified_at = ?1 WHERE id = ?2",
            params![sql_timestamp(Utc::now()), id],
        )?;
        Ok(())
    }
    pub fn prune_jabber_pings_before(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        Ok(self.connection.execute(
            "DELETE FROM jabber_pings WHERE occurred_at < ?1",
            [sql_timestamp(cutoff)],
        )?)
    }

    pub fn record_paps(&self, paps: &PapSnapshot) -> Result<()> {
        self.connection.execute(
            "INSERT INTO pap_snapshots (
               updated_at, month_strategic, month_peacetime,
               days_30_strategic, days_30_peacetime,
               days_90_strategic, days_90_peacetime,
               last_strat_at, last_strat_character
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                sql_timestamp(paps.updated_at),
                i64::from(paps.month.strategic),
                i64::from(paps.month.peacetime),
                i64::from(paps.last_30_days.strategic),
                i64::from(paps.last_30_days.peacetime),
                i64::from(paps.last_90_days.strategic),
                i64::from(paps.last_90_days.peacetime),
                paps.last_strat_at,
                paps.last_strat_character,
            ],
        )?;
        Ok(())
    }

    pub fn latest_paps(&self) -> Result<Option<PapSnapshot>> {
        let result = self.connection.query_row(
            "SELECT updated_at, month_strategic, month_peacetime,
                    days_30_strategic, days_30_peacetime,
                    days_90_strategic, days_90_peacetime,
                    last_strat_at, last_strat_character
             FROM pap_snapshots ORDER BY updated_at DESC, id DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        );
        match result {
            Ok(raw) => Ok(Some(decode_paps(raw)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Replaces one character's skill queue with what ESI just listed.
    ///
    /// A level whose finish time has passed but whose notification has not gone
    /// out stays current even when the new listing has dropped it, for the same
    /// reason a finished industry job does: ESI stops listing a trained level
    /// within a couple of minutes, and the daemon may not have been looking.
    pub fn reconcile_skill_queue(
        &self,
        character_id: u64,
        entries: &[SkillQueueEntry],
    ) -> Result<()> {
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.character_id != character_id)
        {
            anyhow::bail!(
                "skill queue entry {} level {} does not belong to character {character_id}",
                entry.skill_id,
                entry.level
            );
        }
        let scope_id =
            i64::try_from(character_id).context("character ID exceeds SQLite's range")?;
        let initialized_key = format!("skills_initialized:{character_id}");
        let transaction = self.connection.unchecked_transaction()?;
        let first_sync = transaction.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM metadata WHERE key = ?1)",
            [&initialized_key],
            |row| row.get::<_, bool>(0),
        )?;
        let now = Utc::now();
        let now_text = sql_timestamp(now);

        transaction.execute(
            "UPDATE skill_queue SET current = 0
             WHERE character_id = ?1
               AND NOT (notified_at IS NULL AND finish_at IS NOT NULL AND finish_at <= ?2)",
            params![scope_id, now_text],
        )?;

        for entry in entries {
            // A level already finished on the very first read was trained
            // before anything was watching; announcing it would be news
            // about the past.
            let baseline_at = (first_sync && entry.is_finished(now)).then_some(now_text.as_str());
            transaction.execute(
                "INSERT INTO skill_queue (
                   character_id, skill_id, level, character_name, skill_name, position,
                   start_at, finish_at, level_start_sp, level_end_sp, training_start_sp,
                   last_seen_at, current, notified_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13)
                 ON CONFLICT(character_id, skill_id, level) DO UPDATE SET
                   character_name = excluded.character_name,
                   skill_name = excluded.skill_name,
                   position = excluded.position,
                   start_at = excluded.start_at,
                   finish_at = excluded.finish_at,
                   level_start_sp = excluded.level_start_sp,
                   level_end_sp = excluded.level_end_sp,
                   training_start_sp = excluded.training_start_sp,
                   last_seen_at = excluded.last_seen_at,
                   current = 1",
                params![
                    scope_id,
                    i64::from(entry.skill_id),
                    i64::from(entry.level),
                    entry.character_name,
                    entry.skill_name,
                    i64::from(entry.position),
                    entry.start_at.map(sql_timestamp),
                    entry.finish_at.map(sql_timestamp),
                    entry.level_start_sp.map(i64::from),
                    entry.level_end_sp.map(i64::from),
                    entry.training_start_sp.map(i64::from),
                    now_text,
                    baseline_at,
                ],
            )?;
        }
        transaction.execute(
            "INSERT OR IGNORE INTO metadata (key, value) VALUES (?1, ?2)",
            params![initialized_key, now_text],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Whether a character's queue has ever been read, which is what separates
    /// "nothing is training" from "nothing is known".
    pub fn skill_queue_known(&self, character_id: u64) -> Result<bool> {
        let known = self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM metadata WHERE key = ?1)",
            [format!("skills_initialized:{character_id}")],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(known)
    }

    /// Every level ESI last listed for one character, in queue order.
    pub fn skill_queue(&self, character_id: u64) -> Result<Vec<SkillQueueEntry>> {
        let scope_id =
            i64::try_from(character_id).context("character ID exceeds SQLite's range")?;
        let mut statement = self.connection.prepare(
            "SELECT character_id, character_name, skill_id, skill_name, level, position,
                    start_at, finish_at, level_start_sp, level_end_sp, training_start_sp
             FROM skill_queue
             WHERE current = 1 AND character_id = ?1
             ORDER BY position, finish_at",
        )?;
        let rows = statement.query_map([scope_id], map_skill_entry)?;
        rows.map(|row| decode_skill_entry(row?)).collect()
    }

    /// Levels whose finish time has passed without a notification going out.
    pub fn due_skill_notifications(&self, now: DateTime<Utc>) -> Result<Vec<SkillCompletion>> {
        let mut statement = self.connection.prepare(
            "SELECT character_id, character_name, skill_id, skill_name, level, finish_at,
                    notified_at IS NOT NULL
             FROM skill_queue
             WHERE current = 1
               AND notified_at IS NULL
               AND finish_at IS NOT NULL
               AND finish_at <= ?1
             ORDER BY finish_at, character_id, skill_id",
        )?;
        let rows = statement.query_map([sql_timestamp(now)], map_skill_completion)?;
        rows.map(|row| decode_skill_completion(row?)).collect()
    }

    pub fn mark_skill_notified(&self, character_id: u64, skill_id: u32, level: u8) -> Result<()> {
        let scope_id =
            i64::try_from(character_id).context("character ID exceeds SQLite's range")?;
        self.connection.execute(
            "UPDATE skill_queue SET notified_at = ?1
             WHERE character_id = ?2 AND skill_id = ?3 AND level = ?4",
            params![
                sql_timestamp(Utc::now()),
                scope_id,
                i64::from(skill_id),
                i64::from(level)
            ],
        )?;
        Ok(())
    }

    /// Levels that finished since `since`, newest first.
    ///
    /// A level removed from the queue before it finished is excluded: it was
    /// never current when its finish time passed and was never announced, so
    /// the clock passing that time later does not make it trained.
    pub fn recent_skill_completions(
        &self,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<SkillCompletion>> {
        let mut statement = self.connection.prepare(
            "SELECT character_id, character_name, skill_id, skill_name, level, finish_at,
                    notified_at IS NOT NULL
             FROM skill_queue
             WHERE finish_at IS NOT NULL
               AND finish_at <= ?1
               AND finish_at >= ?2
               AND (current = 1 OR notified_at IS NOT NULL)
             ORDER BY finish_at DESC, character_id, skill_id",
        )?;
        let rows = statement.query_map(
            [sql_timestamp(now), sql_timestamp(since)],
            map_skill_completion,
        )?;
        rows.map(|row| decode_skill_completion(row?)).collect()
    }
    fn intel_alert_by_id(&self, id: i64) -> Result<IntelAlert> {
        let raw = self.connection.query_row(
            "SELECT id, occurred_at, channel, author, message, system_id, system_name,
                    distance, character_id, character_name, stale_presence, via_bridge
             FROM intel_alerts WHERE id = ?1",
            [id],
            map_intel_alert,
        )?;
        decode_intel_alert(raw)
    }

    fn jabber_ping_by_id(&self, id: i64) -> Result<JabberPing> {
        let raw = self.connection.query_row(
            "SELECT id, occurred_at, received_at, sender_jid, source, sender, target,
                    summary, description, fc, fleet, formup, pap_type, comms, doctrine, body,
                    comms_link, comms_target, fleet_index, fleet_count
             FROM jabber_pings WHERE id = ?1",
            [id],
            map_jabber_ping,
        )?;
        decode_jabber_ping(raw)
    }

    fn migrate(&self) -> Result<()> {
        // Take the write lock before reading the schema version. The installer
        // starts the daemon and then runs `doctor`, so two processes open the
        // database within the same moment; without this they both read the old
        // version and the loser fails trying to create a table the winner just
        // made. BEGIN IMMEDIATE makes the read-decide-write sequence atomic,
        // and each step below re-reads the version inside that lock.
        let guard = self
            .connection
            .prepare("BEGIN IMMEDIATE")
            .and_then(|mut statement| statement.raw_execute());
        if let Err(error) = guard {
            return Err(anyhow::Error::new(error)
                .context("could not take the migration lock; another copy of the application may be starting"));
        }
        let outcome = self.apply_migrations();
        // Roll back on failure so a partly-applied step cannot be observed.
        let _ = self.connection.execute_batch(if outcome.is_ok() {
            "COMMIT"
        } else {
            "ROLLBACK"
        });
        outcome
    }

    /// Runs `sql` only for an application that keeps the industry tables.
    fn industry_batch(&self, sql: &str) -> rusqlite::Result<()> {
        if self.extras.industry {
            self.connection.execute_batch(sql)
        } else {
            Ok(())
        }
    }

    fn apply_migrations(&self) -> Result<()> {
        let current = self.schema_version()?;
        if current == 0 {
            self.connection
                .execute_batch(
                    "
                    CREATE TABLE metadata (
                        key TEXT PRIMARY KEY NOT NULL,
                        value TEXT NOT NULL
                    ) STRICT;
                    PRAGMA user_version = 1;
                    ",
                )
                .context("could not apply state database migration 1")?;
        }

        let current = self.schema_version()?;
        if current == 1 {
            self.connection
                .execute_batch(
                    "
                    CREATE TABLE linked_characters (
                        character_id INTEGER PRIMARY KEY NOT NULL,
                        name TEXT NOT NULL,
                        scopes_json TEXT NOT NULL,
                        authorized_at TEXT NOT NULL
                    ) STRICT;
                    PRAGMA user_version = 2;
                    ",
                )
                .context("could not apply state database migration 2")?;
        }

        let current = self.schema_version()?;
        if current == 2 {
            self.connection
                .execute_batch(
                    "
                    CREATE TABLE intel_alerts (
                        id INTEGER PRIMARY KEY NOT NULL,
                        fingerprint TEXT UNIQUE NOT NULL,
                        occurred_at TEXT NOT NULL,
                        channel TEXT NOT NULL,
                        author TEXT NOT NULL,
                        message TEXT NOT NULL,
                        system_id INTEGER NOT NULL,
                        system_name TEXT NOT NULL,
                        distance INTEGER NOT NULL,
                        character_id INTEGER NOT NULL,
                        character_name TEXT NOT NULL,
                        stale_presence INTEGER NOT NULL,
                        created_at TEXT NOT NULL,
                        notified_at TEXT,
                        read_at TEXT
                    ) STRICT;
                    CREATE INDEX intel_alerts_occurred_at ON intel_alerts (occurred_at DESC);
                    CREATE INDEX intel_alerts_unread ON intel_alerts (read_at) WHERE read_at IS NULL;
                    PRAGMA user_version = 3;
                    ",
                )
                .context("could not apply state database migration 3")?;
        }

        let current = self.schema_version()?;
        if current == 3 {
            self.industry_batch(
                "
                    CREATE TABLE industry_jobs (
                        job_id INTEGER PRIMARY KEY NOT NULL,
                        character_id INTEGER NOT NULL,
                        character_name TEXT NOT NULL,
                        activity_id INTEGER NOT NULL,
                        item_type_id INTEGER NOT NULL,
                        item_name TEXT NOT NULL,
                        runs INTEGER NOT NULL,
                        status TEXT NOT NULL,
                        start_at TEXT NOT NULL,
                        end_at TEXT NOT NULL,
                        last_seen_at TEXT NOT NULL,
                        current INTEGER NOT NULL,
                        ready_notified_at TEXT,
                        ready_read_at TEXT
                    ) STRICT;
                    CREATE INDEX industry_jobs_current_end ON industry_jobs (current, end_at);
                    CREATE INDEX industry_jobs_unread ON industry_jobs (ready_read_at)
                      WHERE ready_notified_at IS NOT NULL AND ready_read_at IS NULL;
                    ",
            )
            .context("could not apply state database migration 4")?;
            self.connection
                .execute_batch("PRAGMA user_version = 4;")
                .context("could not apply state database migration 4")?;
        }

        let current = self.schema_version()?;
        if current == 4 {
            self.connection
                .execute_batch(
                    "
                    CREATE TABLE jabber_pings (
                        id INTEGER PRIMARY KEY NOT NULL,
                        fingerprint TEXT UNIQUE NOT NULL,
                        occurred_at TEXT NOT NULL,
                        received_at TEXT NOT NULL,
                        sender_jid TEXT NOT NULL,
                        source TEXT,
                        sender TEXT,
                        target TEXT,
                        summary TEXT NOT NULL,
                        description TEXT NOT NULL,
                        fc TEXT,
                        fleet TEXT,
                        formup TEXT,
                        pap_type TEXT,
                        comms TEXT,
                        doctrine TEXT,
                        body TEXT NOT NULL,
                        created_at TEXT NOT NULL,
                        notified_at TEXT,
                        read_at TEXT
                    ) STRICT;
                    CREATE INDEX jabber_pings_occurred_at ON jabber_pings (occurred_at DESC);
                    CREATE INDEX jabber_pings_unread ON jabber_pings (read_at)
                      WHERE notified_at IS NOT NULL AND read_at IS NULL;
                    CREATE TABLE pap_snapshots (
                        id INTEGER PRIMARY KEY NOT NULL,
                        updated_at TEXT NOT NULL,
                        month_strategic INTEGER NOT NULL,
                        month_peacetime INTEGER NOT NULL,
                        days_30_strategic INTEGER NOT NULL,
                        days_30_peacetime INTEGER NOT NULL,
                        days_90_strategic INTEGER NOT NULL,
                        days_90_peacetime INTEGER NOT NULL,
                        last_strat_at TEXT,
                        last_strat_character TEXT
                    ) STRICT;
                    CREATE INDEX pap_snapshots_updated_at ON pap_snapshots (updated_at DESC);
                    PRAGMA user_version = 5;
                    ",
                )
                .context("could not apply state database migration 5")?;
        }

        let current = self.schema_version()?;
        if current == 5 {
            // Rewrite every timestamp into the canonical fixed-width form so
            // string comparisons order correctly, and count an unread ping by
            // whether it was read rather than whether its notification was
            // delivered: a failed notify-send must not hide the ping.
            self.connection
                .execute_batch(
                    "
                    UPDATE linked_characters SET
                      authorized_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', authorized_at), authorized_at);
                    UPDATE intel_alerts SET
                      occurred_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', occurred_at), occurred_at),
                      created_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', created_at), created_at),
                      notified_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', notified_at), notified_at),
                      read_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', read_at), read_at);
                    UPDATE jabber_pings SET
                      occurred_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', occurred_at), occurred_at),
                      received_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', received_at), received_at),
                      created_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', created_at), created_at),
                      notified_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', notified_at), notified_at),
                      read_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', read_at), read_at);
                    UPDATE pap_snapshots SET
                      updated_at = COALESCE(
                        strftime('%Y-%m-%dT%H:%M:%fZ', updated_at), updated_at);
                    DROP INDEX jabber_pings_unread;
                    CREATE INDEX jabber_pings_unread ON jabber_pings (read_at)
                      WHERE read_at IS NULL;
                    PRAGMA user_version = 6;
                    ",
                )
                .context("could not apply state database migration 6")?;
            self.industry_batch(
                "
                UPDATE industry_jobs SET
                  start_at = COALESCE(
                    strftime('%Y-%m-%dT%H:%M:%fZ', start_at), start_at),
                  end_at = COALESCE(
                    strftime('%Y-%m-%dT%H:%M:%fZ', end_at), end_at),
                  last_seen_at = COALESCE(
                    strftime('%Y-%m-%dT%H:%M:%fZ', last_seen_at), last_seen_at),
                  ready_notified_at = COALESCE(
                    strftime('%Y-%m-%dT%H:%M:%fZ', ready_notified_at), ready_notified_at),
                  ready_read_at = COALESCE(
                    strftime('%Y-%m-%dT%H:%M:%fZ', ready_read_at), ready_read_at);
                ",
            )
            .context("could not apply state database migration 6")?;
        }

        let current = self.schema_version()?;
        if current == 6 {
            self.connection
                .execute_batch(
                    "
                    ALTER TABLE intel_alerts ADD COLUMN via_bridge INTEGER NOT NULL DEFAULT 0;
                    ALTER TABLE jabber_pings ADD COLUMN comms_link TEXT;
                    CREATE TABLE log_checkpoints (
                        path TEXT PRIMARY KEY NOT NULL,
                        channel TEXT NOT NULL,
                        character_id INTEGER NOT NULL,
                        session_started TEXT NOT NULL,
                        offset_bytes INTEGER NOT NULL,
                        updated_at TEXT NOT NULL
                    ) STRICT;
                    PRAGMA user_version = 7;
                    ",
                )
                .context("could not apply state database migration 7")?;
            self.industry_batch(
                "
                ALTER TABLE industry_jobs ADD COLUMN owner TEXT NOT NULL DEFAULT 'personal';
                ALTER TABLE industry_jobs ADD COLUMN corporation_id INTEGER;
                CREATE INDEX industry_jobs_owner ON industry_jobs (owner, corporation_id);
                ",
            )
            .context("could not apply state database migration 7")?;
        }

        let current = self.schema_version()?;
        if current == 7 {
            self.connection
                .execute_batch(
                    "
                    ALTER TABLE jabber_pings ADD COLUMN comms_target TEXT;
                    PRAGMA user_version = 8;
                    ",
                )
                .context("could not apply state database migration 8")?;
        }

        let current = self.schema_version()?;
        if current == 8 {
            // Read state is gone. A ready job, a hostile report and a fleet
            // ping are all shown until the thing itself stops being true —
            // the job is collected, the report ages out, the ping's formup
            // passes — so "have you looked at this" was a second, weaker
            // answer to a question the state already answered, and the badge
            // it drove could be cleared without anything changing.
            //
            // The notified columns stay: they stop one event raising two
            // notifications, which is a different job.
            //
            // The indexes go first. SQLite refuses to drop a column that an
            // index still refers to, and all three of these existed only to
            // make the unread counts cheap.
            self.connection
                .execute_batch(
                    "
                    DROP INDEX IF EXISTS intel_alerts_unread;
                    DROP INDEX IF EXISTS jabber_pings_unread;
                    ALTER TABLE intel_alerts DROP COLUMN read_at;
                    ALTER TABLE jabber_pings DROP COLUMN read_at;
                    PRAGMA user_version = 9;
                    ",
                )
                .context("could not apply state database migration 9")?;
            self.industry_batch(
                "
                DROP INDEX IF EXISTS industry_jobs_unread;
                ALTER TABLE industry_jobs DROP COLUMN ready_read_at;
                ",
            )
            .context("could not apply state database migration 9")?;
        }

        let current = self.schema_version()?;
        if current == 9 {
            // `summary` and `description` are not facts about a broadcast;
            // they are the parser's reading of `body`, which is the fact. The
            // parser used to discard every blank line, so a broadcast written
            // as paragraphs was stored as one unreadable block and the panel
            // had nothing to lay out with.
            //
            // Re-reading the bodies already on disk brings that history up to
            // what the parser now produces. Without it the panel would format
            // pings received from today differently from the ones above them,
            // which reads as a bug in the panel.
            self.reparse_ping_bodies()?;
            self.connection
                .execute_batch("PRAGMA user_version = 10;")
                .context("could not apply state database migration 10")?;
        }

        let current = self.schema_version()?;
        if current == 10 {
            // Skill training, kept the way industry jobs are: one row per
            // queued level, `current` for what the last read still listed,
            // and a notified stamp so a level announces itself exactly once
            // across restarts. The key is the level itself, because a level
            // is trained once and the queue has no ID of its own to offer.
            self.connection
                .execute_batch(
                    "
                    CREATE TABLE skill_queue (
                        character_id INTEGER NOT NULL,
                        skill_id INTEGER NOT NULL,
                        level INTEGER NOT NULL,
                        character_name TEXT NOT NULL,
                        skill_name TEXT NOT NULL,
                        position INTEGER NOT NULL,
                        start_at TEXT,
                        finish_at TEXT,
                        level_start_sp INTEGER,
                        level_end_sp INTEGER,
                        training_start_sp INTEGER,
                        last_seen_at TEXT NOT NULL,
                        current INTEGER NOT NULL DEFAULT 1,
                        notified_at TEXT,
                        PRIMARY KEY (character_id, skill_id, level)
                    ) STRICT;
                    CREATE INDEX skill_queue_finish ON skill_queue (finish_at);
                    PRAGMA user_version = 11;
                    ",
                )
                .context("could not apply state database migration 11")?;
        }

        if self.schema_version()? == 11 {
            self.connection
                .execute_batch(include_str!("mining/schema.sql"))?;
        }

        if self.schema_version()? == 12 {
            // Pilots switched off on the pilots tab: a character that does no
            // industry still has slots, and listing them as idle every time
            // the industry tab opens is noise. A row means off, so a pilot
            // linked tomorrow starts on.
            self.industry_batch(
                "
                    CREATE TABLE industry_off (
                        character_id INTEGER PRIMARY KEY NOT NULL
                    ) STRICT;
                ",
            )
            .context("could not apply state database migration 13")?;
            self.connection
                .execute_batch("PRAGMA user_version = 13;")
                .context("could not apply state database migration 13")?;
        }

        if self.schema_version()? == 13 {
            // Directorbot sends several fleets as one broadcast, and each is
            // now a ping of its own. The history already on disk holds those
            // broadcasts as one row apiece, with the second fleet's labels
            // pasted into the description and no way into its comms; this
            // splits them the way the parser now does.
            self.connection
                .execute_batch(
                    "
                    ALTER TABLE jabber_pings ADD COLUMN fleet_index INTEGER NOT NULL DEFAULT 1;
                    ALTER TABLE jabber_pings ADD COLUMN fleet_count INTEGER NOT NULL DEFAULT 1;
                    ",
                )
                .context("could not apply state database migration 14")?;
            self.split_multi_fleet_pings()
                .context("could not split multi-fleet pings for migration 14")?;
            self.connection
                .execute_batch("PRAGMA user_version = 14;")
                .context("could not apply state database migration 14")?;
        }

        let current = self.schema_version()?;
        if current != SCHEMA_VERSION {
            anyhow::bail!("unsupported SQLite schema version {current}; expected {SCHEMA_VERSION}");
        }

        Ok(())
    }

    /// Re-derives every stored ping's summary and description from its body.
    ///
    /// A row whose body no longer parses is left exactly as it is. An
    /// out-of-date description still says what the broadcast said; an empty
    /// one would lose it, and this runs for formatting alone.
    fn reparse_ping_bodies(&self) -> Result<usize> {
        let rows: Vec<(i64, String, String)> = {
            let mut statement = self
                .connection
                .prepare("SELECT id, sender_jid, body FROM jabber_pings")?;
            let mapped =
                statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        // The timestamps only feed the fingerprint, and the fingerprint is not
        // rewritten here — rewriting it would make every stored ping look new
        // and notify the whole history again.
        let at = Utc::now();
        let mut updated = 0;
        for (id, from, body) in rows {
            let Some(parsed) = parse_ping(&from, &body, at, at) else {
                continue;
            };
            updated += self.connection.execute(
                "UPDATE jabber_pings SET summary = ?1, description = ?2
                 WHERE id = ?3 AND (summary <> ?1 OR description <> ?2)",
                params![parsed.summary, parsed.description, id],
            )?;
        }
        Ok(updated)
    }

    /// Splits every stored broadcast that announced several fleets into one
    /// row per fleet.
    ///
    /// The stored row becomes the first fleet, keeping its ID, fingerprint and
    /// notified stamp, so nothing about it reads as new. The others are
    /// inserted beside it carrying the same stamps: the broadcast was already
    /// announced, and a restart must not announce half of it again.
    fn split_multi_fleet_pings(&self) -> Result<usize> {
        let rows: Vec<(i64, String, String, String)> = {
            let mut statement = self
                .connection
                .prepare("SELECT id, sender_jid, body, occurred_at FROM jabber_pings")?;
            let mapped = statement.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut split = 0;
        for (id, from, body, occurred_at) in rows {
            // The fingerprint hashes the time the broadcast was sent, so the
            // new rows' fingerprints come from the stored time, the same one a
            // replay of this broadcast would carry.
            let Ok(at) = DateTime::parse_from_rfc3339(&occurred_at) else {
                continue;
            };
            let pings = parse_pings(&from, &body, at.with_timezone(&Utc), Utc::now());
            let Some(first) = pings.first() else {
                continue;
            };
            // The footer parser used to need a word before "broadcast", so the
            // commonest footer of all left every ping without its sender.
            self.connection.execute(
                "UPDATE jabber_pings SET
                   source = COALESCE(source, ?1), sender = COALESCE(sender, ?2),
                   target = COALESCE(target, ?3)
                 WHERE id = ?4",
                params![first.source, first.sender, first.target, id],
            )?;
            if pings.len() < 2 {
                continue;
            }
            self.connection.execute(
                "UPDATE jabber_pings SET
                   summary = ?1, description = ?2, fc = ?3, fleet = ?4, formup = ?5,
                   pap_type = ?6, comms = ?7, doctrine = ?8,
                   comms_target = CASE WHEN comms_link IS ?9 THEN comms_target ELSE NULL END,
                   comms_link = ?9, fleet_index = ?10, fleet_count = ?11
                 WHERE id = ?12",
                params![
                    first.summary,
                    first.description,
                    first.fc,
                    first.fleet,
                    first.formup,
                    first.pap_type,
                    first.comms,
                    first.doctrine,
                    first.comms_link,
                    first.fleet_index,
                    first.fleet_count,
                    id,
                ],
            )?;
            for ping in &pings[1..] {
                // The fingerprint of a later fleet is the stored one plus a
                // suffix, so it is derived from the row rather than the parse.
                self.connection.execute(
                    "INSERT OR IGNORE INTO jabber_pings (
                       fingerprint, occurred_at, received_at, sender_jid, source, sender, target,
                       summary, description, fc, fleet, formup, pap_type, comms, doctrine, body,
                       created_at, notified_at, comms_link, fleet_index, fleet_count
                     )
                     SELECT fingerprint || ?1, occurred_at, received_at, sender_jid, source,
                            sender, target, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, body,
                            created_at, notified_at, ?10, ?11, ?12
                     FROM jabber_pings WHERE id = ?13",
                    params![
                        format!(":fleet-{}", ping.fleet_index),
                        ping.summary,
                        ping.description,
                        ping.fc,
                        ping.fleet,
                        ping.formup,
                        ping.pap_type,
                        ping.comms,
                        ping.doctrine,
                        ping.comms_link,
                        ping.fleet_index,
                        ping.fleet_count,
                        id,
                    ],
                )?;
            }
            split += 1;
        }
        Ok(split)
    }
}

type RawIntelAlert = (
    i64,
    String,
    String,
    String,
    String,
    i64,
    String,
    i64,
    i64,
    String,
    bool,
    bool,
);

fn map_intel_alert(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawIntelAlert> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

fn decode_intel_alert(raw: RawIntelAlert) -> Result<IntelAlert> {
    Ok(IntelAlert {
        id: u64::try_from(raw.0).context("stored intel alert ID is negative")?,
        occurred_at: DateTime::parse_from_rfc3339(&raw.1)
            .context("stored intel alert date is invalid")?
            .with_timezone(&Utc),
        channel: raw.2,
        author: raw.3,
        message: raw.4,
        system_id: u32::try_from(raw.5).context("stored intel system ID is invalid")?,
        system_name: raw.6,
        distance: u16::try_from(raw.7).context("stored intel distance is invalid")?,
        character_id: u64::try_from(raw.8).context("stored intel character ID is negative")?,
        character_name: raw.9,
        stale_presence: raw.10,
        via_bridge: raw.11,
    })
}

type RawJabberPing = (
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
);

fn map_jabber_ping(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawJabberPing> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
        row.get(19)?,
    ))
}

fn decode_jabber_ping(raw: RawJabberPing) -> Result<JabberPing> {
    Ok(JabberPing {
        id: u64::try_from(raw.0).context("stored Jabber ping ID is negative")?,
        occurred_at: DateTime::parse_from_rfc3339(&raw.1)
            .context("stored Jabber ping date is invalid")?
            .with_timezone(&Utc),
        received_at: DateTime::parse_from_rfc3339(&raw.2)
            .context("stored Jabber ping received date is invalid")?
            .with_timezone(&Utc),
        from: raw.3,
        source: raw.4,
        sender: raw.5,
        target: raw.6,
        summary: raw.7,
        description: raw.8,
        fc: raw.9,
        fleet: raw.10,
        formup: raw.11,
        pap_type: raw.12,
        comms: raw.13,
        doctrine: raw.14,
        body: raw.15,
        comms_link: raw.16,
        comms_target: raw.17,
        fc_portrait: None,
        muted: false,
        fleet_index: u32::try_from(raw.18).unwrap_or(1).max(1),
        fleet_count: u32::try_from(raw.19).unwrap_or(1).max(1),
    })
}

type RawPaps = (
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<String>,
    Option<String>,
);

fn decode_paps(raw: RawPaps) -> Result<PapSnapshot> {
    Ok(PapSnapshot {
        updated_at: DateTime::parse_from_rfc3339(&raw.0)
            .context("stored PAP update date is invalid")?
            .with_timezone(&Utc),
        month: PapStats {
            strategic: u32::try_from(raw.1).context("stored monthly strategic PAP is invalid")?,
            peacetime: u32::try_from(raw.2).context("stored monthly peacetime PAP is invalid")?,
        },
        last_30_days: PapStats {
            strategic: u32::try_from(raw.3).context("stored 30-day strategic PAP is invalid")?,
            peacetime: u32::try_from(raw.4).context("stored 30-day peacetime PAP is invalid")?,
        },
        last_90_days: PapStats {
            strategic: u32::try_from(raw.5).context("stored 90-day strategic PAP is invalid")?,
            peacetime: u32::try_from(raw.6).context("stored 90-day peacetime PAP is invalid")?,
        },
        last_strat_at: raw.7,
        last_strat_character: raw.8,
    })
}

type RawSkillEntry = (
    i64,
    String,
    i64,
    String,
    i64,
    i64,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

fn map_skill_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSkillEntry> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

pub fn parse_sql_timestamp(value: &str, what: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("stored {what} is invalid"))?
        .with_timezone(&Utc))
}

fn decode_skill_entry(raw: RawSkillEntry) -> Result<SkillQueueEntry> {
    let optional_sp = |value: Option<i64>, what: &str| -> Result<Option<u32>> {
        value
            .map(|sp| u32::try_from(sp).with_context(|| format!("stored {what} is invalid")))
            .transpose()
    };
    Ok(SkillQueueEntry {
        character_id: u64::try_from(raw.0).context("stored skill character ID is negative")?,
        character_name: raw.1,
        skill_id: u32::try_from(raw.2).context("stored skill ID is invalid")?,
        skill_name: raw.3,
        level: u8::try_from(raw.4).context("stored skill level is invalid")?,
        position: u32::try_from(raw.5).context("stored skill queue position is invalid")?,
        start_at: raw
            .6
            .as_deref()
            .map(|at| parse_sql_timestamp(at, "skill start date"))
            .transpose()?,
        finish_at: raw
            .7
            .as_deref()
            .map(|at| parse_sql_timestamp(at, "skill finish date"))
            .transpose()?,
        level_start_sp: optional_sp(raw.8, "skill level start SP")?,
        level_end_sp: optional_sp(raw.9, "skill level end SP")?,
        training_start_sp: optional_sp(raw.10, "skill training start SP")?,
    })
}

type RawSkillCompletion = (i64, String, i64, String, i64, String, bool);

fn map_skill_completion(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSkillCompletion> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn decode_skill_completion(raw: RawSkillCompletion) -> Result<SkillCompletion> {
    Ok(SkillCompletion {
        character_id: u64::try_from(raw.0).context("stored skill character ID is negative")?,
        character_name: raw.1,
        skill_id: u32::try_from(raw.2).context("stored skill ID is invalid")?,
        skill_name: raw.3,
        level: u8::try_from(raw.4).context("stored skill level is invalid")?,
        finished_at: parse_sql_timestamp(&raw.5, "skill finish date")?,
        notified: raw.6,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn creates_and_reopens_the_foundation_schema() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");

        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        drop(store);

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn upgrades_the_foundation_schema_without_recreating_the_database() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE metadata (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                 ) STRICT;
                 INSERT INTO metadata (key, value) VALUES ('preserved', 'yes');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let store = Store::open(&path).unwrap();

        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let preserved: String = store
            .connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'preserved'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, "yes");
        assert!(store.linked_characters().unwrap().is_empty());
        assert!(store.recent_intel_alerts(10).unwrap().is_empty());
    }

    #[test]
    fn persists_and_removes_linked_characters() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let store = Store::open(&path).unwrap();
        let character = LinkedCharacter {
            id: 2_112_345_678,
            name: "Example Pilot".to_owned(),
            scopes: vec!["esi-location.read_online.v1".to_owned()],
            authorized_at: "2026-09-03T09:00:00Z".parse().unwrap(),
        };

        store.upsert_character(&character).unwrap();
        assert_eq!(
            store.find_character("example pilot").unwrap(),
            Some(character.clone())
        );
        assert_eq!(store.find_character("2112345678").unwrap(), Some(character));
        assert!(store.delete_character(2_112_345_678).unwrap());
        assert!(store.linked_characters().unwrap().is_empty());
    }

    #[test]
    fn deduplicates_and_marks_intel_alerts_read() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        let alert = NewIntelAlert {
            fingerprint: "same-report".to_owned(),
            occurred_at: "2026-09-03T09:00:00Z".parse().unwrap(),
            channel: "example.intel".to_owned(),
            author: "Scout".to_owned(),
            message: "Q4C-S5 Sabre".to_owned(),
            system_id: 30_000_715,
            system_name: "Q4C-S5".to_owned(),
            distance: 1,
            character_id: 42,
            character_name: "Example Pilot".to_owned(),
            stale_presence: false,
            via_bridge: false,
        };

        let inserted = store.record_intel_alert(&alert).unwrap().unwrap();
        assert!(store.record_intel_alert(&alert).unwrap().is_none());
        assert_eq!(
            store.recent_intel_alerts(10).unwrap(),
            vec![inserted.clone()]
        );
    }

    fn jabber_ping() -> NewJabberPing {
        NewJabberPing {
            fingerprint: "example-ping".to_owned(),
            occurred_at: "2026-09-03T09:00:00Z".parse().unwrap(),
            received_at: "2026-09-03T09:00:01Z".parse().unwrap(),
            from: "directorbot@example.com".to_owned(),
            source: Some("coalition".to_owned()),
            sender: Some("Broadcaster".to_owned()),
            target: Some("pilots".to_owned()),
            summary: "Example fleet".to_owned(),
            description: "Form fleet".to_owned(),
            fc: Some("Space Boss".to_owned()),
            fleet: Some("Example fleet".to_owned()),
            formup: Some("Example Keepstar".to_owned()),
            pap_type: Some("Strategic".to_owned()),
            comms: Some("Fleet 1".to_owned()),
            comms_link: None,
            doctrine: Some("Fast ships".to_owned()),
            body: "Synthetic test ping".to_owned(),
            fleet_index: 1,
            fleet_count: 1,
        }
    }

    #[test]
    fn persists_and_deduplicates_jabber_pings() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        let pending = jabber_ping();

        let ping = store.record_jabber_ping(&pending, false).unwrap().unwrap();
        assert_eq!(ping.summary, "Example fleet");
        // The same broadcast arriving twice — a reconnect replaying history,
        // say — must not become two pings.
        assert!(store.record_jabber_ping(&pending, false).unwrap().is_none());

        store.mark_jabber_ping_notified(ping.id).unwrap();
        assert_eq!(store.recent_jabber_pings(10).unwrap().len(), 1);
    }

    #[test]
    fn reports_the_durable_ping_high_water_mark() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();

        assert!(store.latest_jabber_ping_at().unwrap().is_none());
        store.record_jabber_ping(&jabber_ping(), false).unwrap();

        assert_eq!(
            store.latest_jabber_ping_at().unwrap(),
            Some("2026-09-03T09:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn a_baselined_ping_still_joins_the_history() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();

        // Baselining is how the daemon catches up on history it missed while
        // it was down. The caller is what declines to notify; the ping is
        // still stored, because the panel shows it either way.
        store
            .record_jabber_ping(&jabber_ping(), true)
            .unwrap()
            .unwrap();

        assert_eq!(store.recent_jabber_pings(10).unwrap().len(), 1);
    }

    #[test]
    fn stores_the_latest_pap_snapshot() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        let paps = PapSnapshot {
            updated_at: "2026-09-03T09:00:00Z".parse().unwrap(),
            month: PapStats {
                strategic: 12,
                peacetime: 3,
            },
            last_30_days: PapStats {
                strategic: 20,
                peacetime: 4,
            },
            last_90_days: PapStats {
                strategic: 55,
                peacetime: 8,
            },
            last_strat_at: Some("2026-09-01 20:00".to_owned()),
            last_strat_character: Some("Example Pilot".to_owned()),
        };

        store.record_paps(&paps).unwrap();

        assert_eq!(store.latest_paps().unwrap(), Some(paps));
    }

    fn skill_entry(skill_id: u32, position: u32, finish_at: Option<&str>) -> SkillQueueEntry {
        SkillQueueEntry {
            character_id: 42,
            character_name: "Example Pilot".to_owned(),
            skill_id,
            skill_name: format!("Skill {skill_id}"),
            level: 3,
            position,
            start_at: finish_at.map(|_| "2026-09-01T00:00:00Z".parse().unwrap()),
            finish_at: finish_at.map(|at| at.parse().unwrap()),
            level_start_sp: Some(8_000),
            level_end_sp: Some(45_255),
            training_start_sp: Some(9_000),
        }
    }

    #[test]
    fn reconciles_a_skill_queue_and_notifies_a_finished_level_once() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        let later = "2100-01-01T00:00:00Z".parse().unwrap();

        assert!(!store.skill_queue_known(42).unwrap());
        let training = skill_entry(1, 0, Some("2099-01-01T00:00:00Z"));
        store
            .reconcile_skill_queue(42, std::slice::from_ref(&training))
            .unwrap();
        assert!(store.skill_queue_known(42).unwrap());
        assert_eq!(store.skill_queue(42).unwrap(), vec![training.clone()]);

        // Time passes the finish date: due exactly once.
        let due = store.due_skill_notifications(later).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].skill_id, 1);
        assert!(!due[0].notified);
        store.mark_skill_notified(42, 1, 3).unwrap();
        assert!(store.due_skill_notifications(later).unwrap().is_empty());

        // ESI drops the trained level; it stops being current but stays a
        // completion for the history.
        store.reconcile_skill_queue(42, &[]).unwrap();
        assert!(store.skill_queue(42).unwrap().is_empty());
        let recent = store
            .recent_skill_completions("2000-01-01T00:00:00Z".parse().unwrap(), later)
            .unwrap();
        assert_eq!(recent.len(), 1);
        assert!(recent[0].notified);
    }

    #[test]
    fn a_level_that_finishes_between_reads_is_still_notified() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();

        // Consume the first-sync baseline with a level still training.
        let training = skill_entry(1, 0, Some("2099-01-01T00:00:00Z"));
        store
            .reconcile_skill_queue(42, std::slice::from_ref(&training))
            .unwrap();
        // A second level, seen while training, whose finish time has passed.
        let finished = skill_entry(2, 1, Some("2000-01-01T00:00:00Z"));
        store
            .reconcile_skill_queue(42, &[training.clone(), finished])
            .unwrap();
        // ESI no longer lists it.
        store
            .reconcile_skill_queue(42, std::slice::from_ref(&training))
            .unwrap();

        let due = store.due_skill_notifications(Utc::now()).unwrap();
        assert_eq!(due.len(), 1, "a trained level must remain notifiable");
        assert_eq!(due[0].skill_id, 2);
    }

    #[test]
    fn the_first_skill_read_quietly_baselines_levels_already_trained() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();

        store
            .reconcile_skill_queue(42, &[skill_entry(1, 0, Some("2000-01-01T00:00:00Z"))])
            .unwrap();

        assert!(
            store
                .due_skill_notifications(Utc::now())
                .unwrap()
                .is_empty()
        );
        let recent = store
            .recent_skill_completions("1999-01-01T00:00:00Z".parse().unwrap(), Utc::now())
            .unwrap();
        assert_eq!(recent.len(), 1, "a baselined level is still history");
        assert!(recent[0].notified);
    }

    #[test]
    fn a_level_removed_from_the_queue_never_becomes_a_completion() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        let training = skill_entry(1, 0, Some("2099-01-01T00:00:00Z"));
        // Must stay unfinished at the moment it is removed, so the date has
        // to be beyond any plausible run of this test rather than merely
        // beyond the day it was written.
        let planned = skill_entry(2, 1, Some("2099-06-01T00:00:00Z"));
        store
            .reconcile_skill_queue(42, &[training.clone(), planned])
            .unwrap();
        // The player takes it out of the queue before it finishes.
        store
            .reconcile_skill_queue(42, std::slice::from_ref(&training))
            .unwrap();

        let later = "2100-01-01T00:00:00Z".parse().unwrap();
        assert!(
            store
                .due_skill_notifications(later)
                .unwrap()
                .iter()
                .all(|c| c.skill_id != 2)
        );
        assert!(
            store
                .recent_skill_completions("2000-01-01T00:00:00Z".parse().unwrap(), later)
                .unwrap()
                .iter()
                .all(|c| c.skill_id != 2)
        );
    }

    #[test]
    fn a_paused_queue_round_trips_without_dates() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        let paused = skill_entry(1, 0, None);
        store
            .reconcile_skill_queue(42, std::slice::from_ref(&paused))
            .unwrap();

        assert_eq!(store.skill_queue(42).unwrap(), vec![paused]);
        assert!(
            store
                .due_skill_notifications(Utc::now())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_skill_entries_for_the_wrong_character() {
        let temp = tempdir().unwrap();
        let store = Store::open(&temp.path().join("state.sqlite3")).unwrap();
        let entry = skill_entry(1, 0, None);
        assert!(store.reconcile_skill_queue(7, &[entry]).is_err());
    }

    #[test]
    fn only_an_application_that_asks_gets_the_industry_tables() {
        let temp = tempdir().unwrap();
        let plain = Store::open(&temp.path().join("waggle.sqlite3")).unwrap();
        let with_industry = Store::open_with(
            &temp.path().join("evemarchy.sqlite3"),
            Extras { industry: true },
        )
        .unwrap();
        let industry_tables = |store: &Store| -> Vec<String> {
            store
                .connection()
                .prepare(
                    "SELECT name FROM sqlite_master \
                     WHERE type = 'table' AND name LIKE 'industry%' ORDER BY name",
                )
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };

        assert!(industry_tables(&plain).is_empty());
        assert_eq!(
            industry_tables(&with_industry),
            ["industry_jobs", "industry_off"]
        );
        assert_eq!(plain.schema_version().unwrap(), SCHEMA_VERSION);
        // Removing a pilot must not reach for a table this database never had.
        assert!(!plain.delete_character(1).unwrap());
    }
}
