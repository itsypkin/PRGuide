//! The complete v1 SQLite schema.

/// Schema v1. An event records its result on its own `result_kind` and
/// `result_id` fields so a repeated command can find what it produced first.
pub(crate) const V1: &str = r#"-- Schema v1. TEXT timestamps are RFC 3339 UTC; ids are ULIDs unless stated.

CREATE TABLE store_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
  -- keys: host, owner, repo, number, git_common_dir

CREATE TABLE runs(id TEXT PRIMARY KEY,          -- "r_" + 12 base32 chars
     state TEXT NOT NULL CHECK (state IN ('active','stopped','expired')),
     created_at TEXT NOT NULL, lease_until TEXT NOT NULL,
     ended_at TEXT,
     end_reason TEXT CHECK (end_reason IN ('quit','end','takeover','expired')));  -- NULL while active
CREATE UNIQUE INDEX one_active_run ON runs(state) WHERE state = 'active';

CREATE TABLE guide_versions(version INTEGER PRIMARY KEY,
     state TEXT NOT NULL CHECK (state IN ('staging','ready')),
     caused_by TEXT UNIQUE REFERENCES events(id),
     snapshot_base TEXT NOT NULL, snapshot_head TEXT NOT NULL,
     selection_kind TEXT NOT NULL CHECK (selection_kind IN ('full','commits')),
     selection_base TEXT NOT NULL, selection_head TEXT NOT NULL,
     commits_json TEXT NOT NULL,                 -- ordered SHAs
     refs_json TEXT NOT NULL,                    -- [{name, sha}] the version pins
     created_at TEXT NOT NULL, ready_at TEXT);

CREATE TABLE drafts(id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id),
     path TEXT NOT NULL, provenance_json TEXT NOT NULL, created_at TEXT NOT NULL);

CREATE TABLE threads(id TEXT PRIMARY KEY, created_at TEXT NOT NULL,
     origin_version INTEGER NOT NULL REFERENCES guide_versions(version),
     slide_id TEXT NOT NULL,
     anchor_json TEXT);                          -- StoredAnchor (T2-5); NULL = whole slide

CREATE TABLE messages(id TEXT PRIMARY KEY, thread_id TEXT NOT NULL REFERENCES threads(id),
     author TEXT NOT NULL CHECK (author IN ('reviewer','agent')),
     body TEXT NOT NULL, created_at TEXT NOT NULL);

CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
     type TEXT NOT NULL CHECK (type IN ('comment','deepdive_request','generate_request')),
     state TEXT NOT NULL CHECK (state IN ('pending','claimed','done')),
     claimed_by TEXT REFERENCES runs(id), claimed_at TEXT,
     thread_id TEXT REFERENCES threads(id),      -- comment, deepdive_request
     message_id TEXT REFERENCES messages(id),    -- comment
     range_first TEXT, range_last TEXT,          -- generate_request; both NULL = full
     result_kind TEXT CHECK (result_kind IN
         ('message','deepdive_section','outbox_item','guide_version','ack')),  -- NULL until done
     result_id TEXT, done_at TEXT,
     created_at TEXT NOT NULL);
CREATE INDEX events_open ON events(state, seq);

CREATE TABLE deepdives(id TEXT PRIMARY KEY,
     version INTEGER NOT NULL REFERENCES guide_versions(version),
     slide_id TEXT NOT NULL, created_at TEXT NOT NULL,
     UNIQUE (version, slide_id));
CREATE TABLE deepdive_sections(id TEXT PRIMARY KEY,
     deepdive_id TEXT NOT NULL REFERENCES deepdives(id),
     ord INTEGER NOT NULL, body TEXT NOT NULL,
     caused_by TEXT NOT NULL UNIQUE REFERENCES events(id),
     created_at TEXT NOT NULL, UNIQUE (deepdive_id, ord));

CREATE TABLE outbox_items(id TEXT PRIMARY KEY,
     kind TEXT NOT NULL CHECK (kind IN ('author_question','promoted_answer')),
     thread_id TEXT NOT NULL REFERENCES threads(id),
     message_id TEXT REFERENCES messages(id),    -- the promoted agent message
     body TEXT NOT NULL,
     state TEXT NOT NULL CHECK (state IN ('draft','dropped','posting','posted')),
     post_id TEXT REFERENCES posts(id), gh_comment_id INTEGER,
     created_at TEXT NOT NULL, updated_at TEXT NOT NULL);

CREATE TABLE posts(id TEXT PRIMARY KEY,
     state TEXT NOT NULL CHECK (state IN ('in_flight','unknown','done','failed')),
     head_sha TEXT NOT NULL, plan_json TEXT NOT NULL,   -- T7-3's placement plan
     gh_review_id INTEGER, error TEXT,
     created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE UNIQUE INDEX one_open_post ON posts((1)) WHERE state IN ('in_flight','unknown');

CREATE TABLE notices(seq INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL,   -- guide_ready
     ref TEXT NOT NULL, created_at TEXT NOT NULL);

CREATE TABLE gh_threads(id TEXT PRIMARY KEY, updated_at TEXT NOT NULL, json TEXT NOT NULL,
     fetched_at TEXT NOT NULL);
"#;
