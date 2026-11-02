CREATE TABLE mining_state (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE mining_sources (
 id INTEGER PRIMARY KEY, path TEXT NOT NULL, checkpoint TEXT NOT NULL
) STRICT;
CREATE INDEX mining_source_path ON mining_sources(path);
CREATE TABLE mining_sessions (
 id INTEGER PRIMARY KEY, label TEXT NOT NULL, start INTEGER NOT NULL,
 end INTEGER, policy TEXT NOT NULL, request_id TEXT UNIQUE,
 auto_started INTEGER NOT NULL DEFAULT 0,
 coverage_incomplete INTEGER NOT NULL DEFAULT 0,
 detail_available INTEGER NOT NULL DEFAULT 1
) STRICT;
CREATE TABLE mining_events (
 source INTEGER NOT NULL, offset INTEGER NOT NULL, at INTEGER NOT NULL,
 pilot TEXT NOT NULL, pilot_name TEXT NOT NULL, ore TEXT NOT NULL,
 units INTEGER NOT NULL, bonus INTEGER NOT NULL, volume INTEGER,
 type_id INTEGER, data_build TEXT, session INTEGER,
 PRIMARY KEY(source,offset), FOREIGN KEY(session) REFERENCES mining_sessions(id)
) STRICT;
CREATE INDEX mining_event_session_time ON mining_events(session,at);
CREATE INDEX mining_unresolved ON mining_events(ore) WHERE volume IS NULL;
CREATE TABLE mining_rollups (
 session INTEGER NOT NULL, pilot TEXT NOT NULL, pilot_name TEXT NOT NULL,
 ore TEXT NOT NULL, units INTEGER NOT NULL DEFAULT 0,
 bonus_units INTEGER NOT NULL DEFAULT 0, ordinary INTEGER NOT NULL DEFAULT 0,
 bonus INTEGER NOT NULL DEFAULT 0, crits INTEGER NOT NULL DEFAULT 0,
 unresolved INTEGER NOT NULL DEFAULT 0, last_yield INTEGER NOT NULL,
 PRIMARY KEY(session,pilot,ore), FOREIGN KEY(session) REFERENCES mining_sessions(id)
) STRICT;
PRAGMA user_version = 12;
