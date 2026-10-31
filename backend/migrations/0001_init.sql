-- GW2 Translated backend schema (Cloudflare D1 / SQLite).
-- Times are RFC 3339 UTC text unless named *_ms (Unix milliseconds).
-- No raw IP is ever stored: ip_hash / net_hash are HMAC-SHA256(IP_SALT, ...).

-- English texts, keyed by the first 128 bits of SHA-256(en), hex.
CREATE TABLE texts (
  key         TEXT PRIMARY KEY,
  en          TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'trusted', 'rejected')),
  reporters   INTEGER NOT NULL DEFAULT 0,          -- distinct IP hashes that sent it
  review_note TEXT,
  created_at  TEXT NOT NULL,
  reviewed_at TEXT
);
CREATE INDEX texts_state ON texts (state);

CREATE TABLE text_reporters (
  text_key TEXT NOT NULL,
  ip_hash  TEXT NOT NULL,
  PRIMARY KEY (text_key, ip_hash)
);

-- Game text ID <-> English text pairs, with trust state.
CREATE TABLE id_pairs (
  game_id     INTEGER NOT NULL,
  text_key    TEXT NOT NULL,
  state       TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'trusted', 'rejected')),
  reporters   INTEGER NOT NULL DEFAULT 0,          -- distinct IP hashes that sent this exact pair
  first_build INTEGER,
  last_build  INTEGER,
  created_at  TEXT NOT NULL,
  reviewed_at TEXT,
  PRIMARY KEY (game_id, text_key)
);
CREATE INDEX id_pairs_text ON id_pairs (text_key, state);

CREATE TABLE id_pair_reporters (
  game_id  INTEGER NOT NULL,
  text_key TEXT NOT NULL,
  ip_hash  TEXT NOT NULL,
  PRIMARY KEY (game_id, text_key, ip_hash)
);

-- Translations. id = first 128 bits of SHA-256(text_key \0 lang \0 tr \0), hex,
-- so the same translation sent by many players is one row.
CREATE TABLE translations (
  id          TEXT PRIMARY KEY,
  text_key    TEXT NOT NULL,
  lang        TEXT NOT NULL,
  tr          TEXT NOT NULL,
  source      TEXT NOT NULL CHECK (source IN ('ai', 'argos', 'user')),
  review      TEXT NOT NULL DEFAULT 'pending' CHECK (review IN ('pending', 'passed', 'flagged', 'hidden')),
  review_note TEXT,
  up          INTEGER NOT NULL DEFAULT 0,
  down        INTEGER NOT NULL DEFAULT 0,
  score       REAL NOT NULL DEFAULT 0,             -- Wilson lower bound of up/down
  reporters   INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL,
  passed_at   TEXT                                 -- when it passed review (tie-break: first wins)
);
CREATE INDEX translations_text ON translations (text_key, lang, review);
CREATE INDEX translations_review ON translations (review);

CREATE TABLE translation_reporters (
  translation_id TEXT NOT NULL,
  ip_hash        TEXT NOT NULL,
  PRIMARY KEY (translation_id, ip_hash)
);

-- Current translation per text and language (materialized by the server).
CREATE TABLE current (
  text_key       TEXT NOT NULL,
  lang           TEXT NOT NULL,
  translation_id TEXT NOT NULL,
  score          REAL NOT NULL DEFAULT 0,
  up             INTEGER NOT NULL DEFAULT 0,
  down           INTEGER NOT NULL DEFAULT 0,
  updated_at     TEXT NOT NULL,
  PRIMARY KEY (text_key, lang)
);
CREATE INDEX current_score ON current (lang, score);
CREATE INDEX current_updated ON current (lang, updated_at);

-- One vote per IP hash per translation.
CREATE TABLE votes (
  translation_id TEXT NOT NULL,
  ip_hash        TEXT NOT NULL,
  value          INTEGER NOT NULL CHECK (value IN (-1, 1)),
  created_at     TEXT NOT NULL,
  UNIQUE (translation_id, ip_hash)
);
CREATE INDEX votes_ip ON votes (ip_hash);

-- Review queue (drained in batches by the cron job).
CREATE TABLE review_queue (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  kind            TEXT NOT NULL CHECK (kind IN ('text', 'pair', 'translation')),
  ref             TEXT NOT NULL,                   -- text key | "game_id:text_key" | translation id
  enqueued_at     TEXT NOT NULL,
  attempts        INTEGER NOT NULL DEFAULT 0,
  not_before_ms   INTEGER NOT NULL DEFAULT 0,
  UNIQUE (kind, ref)
);
CREATE INDEX review_queue_next ON review_queue (not_before_ms, id);

-- Published dictionary releases (files in R2 under releases/{lang}/{version}/).
CREATE TABLE releases (
  lang         TEXT NOT NULL,
  version      INTEGER NOT NULL,
  created_at   TEXT NOT NULL,
  files        TEXT NOT NULL,                      -- JSON array of {name, sha256, size}
  manifest     TEXT NOT NULL,                      -- signed manifest JSON, as served
  content_hash TEXT NOT NULL,
  records      INTEGER NOT NULL,
  active       INTEGER NOT NULL DEFAULT 0,         -- 1 = served as "latest"
  PRIMARY KEY (lang, version)
);
CREATE INDEX releases_active ON releases (lang, active);

-- Named counters (mod downloads).
CREATE TABLE counters (
  name  TEXT PRIMARY KEY,
  value INTEGER NOT NULL DEFAULT 0
);
INSERT INTO counters (name, value) VALUES ('downloads', 0);

-- Token buckets per IP hash and limit kind.
CREATE TABLE rate_limits (
  bucket     TEXT PRIMARY KEY,                     -- "{kind}:{ip_hash}"
  tokens     REAL NOT NULL,
  updated_ms INTEGER NOT NULL
);

-- Banned IP hashes or network-prefix hashes.
CREATE TABLE bans (
  hash       TEXT PRIMARY KEY,
  kind       TEXT NOT NULL CHECK (kind IN ('ip', 'net')),
  reason     TEXT,
  created_at TEXT NOT NULL
);

-- Small cached values (site stats computed by the cron job).
CREATE TABLE kv (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
