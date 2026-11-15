# GW2 Enhanced backend

Cloudflare Worker written in Rust ([workers-rs](https://github.com/cloudflare/workers-rs)).
It implements the contract in [`docs/api.md`](docs/api.md):
contributions from the mod, the voting/correction site, and signed dictionary releases.

- **D1** (`DB`): every record (schema in [`migrations/`](migrations/)).
- **R2** (`RELEASES`): release files (`releases/…`) and D1 backups (`backups/…`).
- **Cron** (Worker): review queue every 5 min.
- **GitHub Actions** (`backend-jobs.yml`, `src/bin/jobs.rs`): release build then backup, daily 06:00 UTC.
  They run outside the Worker so it fits the Free plan (10 ms CPU per run); they use D1 and R2 through the
  Cloudflare API with the same statements.
- **Site**: `GET /` ([`site/index.html`](site/index.html)) plus `/app.css`, `/app.js`, self-hosted
  fonts under `/fonts/` and images under `/img/` (all embedded in the Worker; Brazilian Portuguese UI; no
  inline script or style). It presents the three parts of the mod and hosts the translation dictionary.
- **Dictionary seed** (same workflow, daily until done): `jobs seed` loads the dictionary the mod ships
  (`translated/data/languages/pt-BR/dictionary.jsonl`) into D1 as trusted, reviewed translations, about 75k written
  rows per day (D1 Free allows 100k, and each index counts as a written row), resuming from `kv` `seed:pt-BR`. The
  release build of a language waits until its seed is `done`.

This crate builds on its own: it targets `wasm32-unknown-unknown`, unlike the mods.

## See the site locally

```sh
python dev/preview.py        # opens http://localhost:8799 with sample data
```

It serves `site/` and fakes the read endpoints, so no Cloudflare account is needed. Voting and corrections are not simulated. To run the real Worker, use `bun wrangler dev` (needs the wasm32 target and `worker-build`, and a local D1 with `migrations/` applied).

## Layout

| Path | What |
|---|---|
| `src/validate.rs` | Rule checks: markup signature / `[...]` fixups (ported from `translated/src/dict.rs`), links, control chars, length, gibberish |
| `src/api.rs` | Request parsing and limits (500 items, 256 KiB) |
| `src/hash.rs` | Text keys, translation IDs, salted IP hashes (HMAC-SHA256) |
| `src/ratelimit.rs` | Token-bucket math |
| `src/score.rs` | Wilson lower bound, current-translation choice |
| `src/trust.rs` | When texts and ID↔text pairs become trusted |
| `src/review.rs` | AI review prompt + verdict parsing |
| `src/chunk.rs`, `src/release.rs` | JSONL chunking + gzip, release records |
| `src/manifest.rs` | Canonical JSON, Ed25519 sign/verify |
| `src/queries.rs` | **Every SQL statement**, built with sea-query (SQLite dialect) as `(sql with ? placeholders, values)`; `src/queries/tests.rs` runs each one against the migration in SQLite |
| `src/headers.rs` | Security headers + CSP for HTML / JSON / asset responses |
| `src/app/` | Worker glue (wasm32 only): routing, D1/R2, Turnstile, NIM, jobs, admin |
| `site/` | `index.html`, `app.css`, `app.js`, `fonts/` (Cinzel, EB Garamond, Barlow Condensed; SIL OFL, see `fonts/OFL.txt`), `img/` (comparison images) |
| `src/images.rs` | The list of images served under `/img/` |
| `examples/keygen.rs` | Generates the release signing key pair |

Everything outside `src/app/` is pure Rust and tested on the host.

```sh
cargo test                                        # host unit tests
cargo check --target wasm32-unknown-unknown       # Worker build check
```

## API (as implemented)

Every error body is `{"error": "<code>"}`.

| Method & path | Notes |
|---|---|
| `POST /v1/contributions` | Contract body. `202 {"accepted","rejected"}` (`accepted + rejected` = items sent). `400` malformed / bad gzip / unsupported `lang`; `413` > 256 KiB (raw or gunzipped) or > 500 items; `429` + `Retry-After`; `403` banned. Gzip is detected by its magic bytes, so it works whether or not the runtime already decoded `Content-Encoding: gzip`. |
| `GET /v1/releases/{lang}/latest` | Signed manifest (see below), `404 no_release` before the first build. |
| `GET /v1/releases/{lang}/{version}/{name}` | `dictionary-NNN.jsonl.gz` or `manifest.json`, `cache-control: immutable`. |
| `GET /v1/public-key` | `{"algorithm":"ed25519","public_key":"<base64>"}` (convenience; the mod ships its own copy). |
| `GET /v1/texts?lang&sort=worst\|best\|recent&q&cursor` | `{"lang","items":[{"text_key","en","ids":[…],"current":T\|null,"alternatives":[T…]}],"next_cursor":"30"\|null}`, `T = {"id","tr","source","up","down","score"}`. 30 per page, cursor = offset (max 3000). Without `q` only texts with a current translation are listed; with `q` (searches English and current translation) untranslated texts appear too, so the site can propose a translation. |
| `POST /v1/votes` | Contract body. `200 {"translation_id","up","down","score","current"}`; `403 turnstile_failed`; `404`; `409 already_voted` / `not_votable` (not passed review). |
| `POST /v1/corrections` | Contract body. `202 {"translation_id","review"}`; `404 unknown_text` (unknown or untrusted text); `422 <rule>` (`markup`, `fixups`, `link`, `length`, …). |
| `GET /v1/stats?lang` | `{"lang","texts","translated","perfect","translated_pct","perfect_pct","review_queue","updated_at","downloads"}` (refreshed by the 5-min cron; downloads are live). |
| `GET /v1/download` | Counts (≤ 3 per IP hash, +1 per 8 h), then `302` to the latest release's installer (`DOWNLOAD_URL`/`GW2Enhanced-Installer-windows-x64.exe`; `?os=linux` the AppImage, `?os=deb` the .deb; without `os`, Linux browsers get the AppImage). |

### Release files

`releases/{lang}/{version}/dictionary-NNN.jsonl.gz`, one record per English text:

```json
{"en":"The Cost of Victory","tr":"O Custo da Vitória","src":"argos","ids":[1601]}
```

`src` is `nemotron` (stored source `ai`), `argos` or `user`; `ids` = trusted game text IDs
for that text, omitted when empty. Records are ordered by text key; files are cut when the
uncompressed JSONL would pass 2 MiB (so each `.gz` is well under 2 MB). An empty dictionary
still has `dictionary-000.jsonl.gz`. Records the mod would refuse (`unusable`) are never written.
Every version stays in R2; a build is skipped when its content hash equals the active release.

### Manifest signature (what the mod must verify)

```json
{"lang":"pt-BR","version":42,"created_at":"<RFC 3339 UTC time>",
 "files":[{"name":"dictionary-000.jsonl.gz","sha256":"<hex>","size":123456}],
 "signature":"<base64>"}
```

- **Signing input**: the UTF-8 bytes of the manifest object **without the `signature` key**,
  serialized as compact JSON with object keys sorted by their bytes at every level (array
  order kept), no whitespace, strings escaped like `serde_json` (only `"`, `\` and control
  characters escaped; non-ASCII written raw). For the current manifest that is exactly:

  ```
  {"created_at":"…","files":[{"name":"…","sha256":"…","size":N},…],"lang":"pt-BR","version":N}
  ```

- **Signature**: Ed25519 (RFC 8032, pure, no prehash) over those bytes; 64 bytes, standard
  base64 with padding.
- **Public key**: the raw 32-byte Ed25519 public key, standard base64 with padding
  (44 chars). Print it with `cargo run --example keygen -- <private base64>`.
- `sha256` is lowercase hex of each `.gz` file as served; `size` its byte length.

In Rust (serde_json **without** the `preserve_order` feature, whose `Value` map is sorted):

```rust
let mut v: serde_json::Value = serde_json::from_slice(&body)?;
let sig = v.as_object_mut().and_then(|m| m.remove("signature")); // base64 string
let msg = serde_json::to_vec(&v)?;                               // signing input
// ed25519_dalek::VerifyingKey::from_bytes(&pk)?.verify(&msg, &Signature::from_slice(&sig_bytes)?)?
```

`manifest::tests::exact_signing_input` pins this format.

## Anti-abuse

Nothing from clients is trusted; there is no login.

- **IP privacy**: only `HMAC-SHA256(IP_SALT, "ip:"+addr)` and `HMAC(IP_SALT, "net:"+prefix)`
  (IPv4 /24, IPv6 /48) are used and stored (first 128 bits, hex). Raw IPs never hit D1.
- **Bans**: `bans` holds IP or network hashes; banned callers get `403` on every write.
- **Rate limits** (token buckets in D1, per IP hash): contributions 3000 items burst then
  1 item/s; votes 30 then 1/10 s; corrections 10 then 1/min; `/v1/texts` 120 then 2/s.
- **Sizes**: Content-Length checked before reading; body ≤ 256 KiB raw and after gunzip
  (bounded decoder, zip bombs stop at the limit); ≤ 500 items; text ≤ 8 KiB.
- **Rule checks** (`validate.rs`) on every English text and translation, at ingest and again
  at review: markup signature equal (`<br>`, `<c=`, `</c>`, `%vars%`), no invented `[...]`
  fixups (only `[s]` allowed), no `[null]` over real text, no links (except game domains),
  no control/bidi characters, sane length ratio, not gibberish.
- **English texts and IDs** can be forged: a text is trusted only when the AI says it is
  plausible GW2 text, or 3 distinct IP hashes sent it. An ID↔text pair is trusted when
  ≥ 2 distinct IP hashes agree and it leads every competing text for that ID 2:1, or when
  it is the only candidate and its text is trusted. A pair conflicting with a trusted pair
  is rejected at ingest. Untrusted texts/pairs are never listed or released.
- **Translations** go through the review queue: rule checks, then the AI (NVIDIA NIM, batches
  of 20, untrusted text sent as JSON data with an instruction to ignore embedded
  instructions) must say it is a faithful translation, keeps markup, and is not a joke,
  insult, spam or link. No verdict (API down, no key, garbage answer) = stays queued, never
  passed. Mod-sent machine translations are only reviewed while the text has no passed
  translation (saves AI calls); site corrections are always reviewed.
- **Votes and corrections** need a Turnstile token verified server-side (`siteverify` with
  `remoteip`; optionally the hostname). One vote per IP hash per translation
  (`UNIQUE(translation_id, ip_hash)`), only on translations that passed review.
- **Current translation**: best Wilson lower bound (95%) among `passed` translations; ties go
  to the one that passed review first, so the first passing one stays until votes beat it.
  `flagged`/`hidden` ones and ones with ≥ 5 down votes and more than 2× as many down as up
  votes are never current.

## SQL injection and XSS

These are two separate problems with separate defences:

- **SQL injection** (data changing a query): every statement is built with
  [sea-query](https://crates.io/crates/sea-query) in `src/queries.rs`. The SQL text holds only
  identifiers (from `#[derive(Iden)]` schema enums) and `?` placeholders; every value is bound
  through D1's `prepare().bind()`. No SQL string is ever concatenated with data (the backup
  table name comes from a fixed enum). Rows map to structs with serde `Deserialize`.
  Migrations stay plain SQL files for `wrangler d1 migrations apply`.
- **XSS** (data becoming markup/script in a browser) is about output, not the database:
  - `site/app.js` writes API data only through `textContent`/DOM nodes; it never uses
    `innerHTML`, `insertAdjacentHTML`, `document.write` or builds attributes/URLs from API
    data. A host test (`headers::tests::site_has_no_inline_code_or_html_sinks`) enforces this.
  - The Worker renders no HTML from data; the only substitution in `index.html` is the
    Turnstile site key (an env var, filtered to `[A-Za-z0-9_-]` and HTML-escaped).
  - JSON responses always carry `content-type: application/json; charset=utf-8` and `nosniff`.
  - Translations (mod and site) are rejected when they contain control characters, bidi
    overrides/isolates, directional marks, zero-width characters or line separators.
  - A strict CSP blocks inline and third-party script even if a bug slipped through.

Headers on every HTML response (`src/headers.rs`):

```
Content-Security-Policy: default-src 'self'; script-src 'self' https://challenges.cloudflare.com; frame-src https://challenges.cloudflare.com; connect-src 'self'; img-src 'self' data:; font-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; upgrade-insecure-requests
X-Content-Type-Options: nosniff
Referrer-Policy: strict-origin-when-cross-origin
Strict-Transport-Security: max-age=63072000; includeSubDomains
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Resource-Policy: same-origin
Permissions-Policy: camera=(), microphone=(), geolocation=(), payment=(), usb=(), … (all disabled)
X-Frame-Options: DENY
```

JSON responses get the same hardening headers with `Content-Security-Policy: default-src 'none';
frame-ancestors 'none'; base-uri 'none'`. Turnstile needs only `challenges.cloudflare.com` in
`script-src` and `frame-src` (plus `connect-src 'self'`), per Cloudflare's Turnstile CSP reference.

## Admin

`Authorization: Bearer $ADMIN_TOKEN` (≥ 16 chars; admin is disabled when unset).

```sh
A="Authorization: Bearer $ADMIN_TOKEN"; H=https://gw2enhanced.wilssola.dev
curl -H "$A" $H/v1/admin/translations/<id>                                  # details + reporter/voter hashes
curl -H "$A" -X POST $H/v1/admin/translations/<id>/hide -d '{"reason":"troll"}'
curl -H "$A" -X POST $H/v1/admin/translations/<id>/unhide
curl -H "$A" -X POST $H/v1/admin/bans -d '{"ip_hash":"<hash>","purge":true,"reason":"vote spam"}'
curl -H "$A" -X POST $H/v1/admin/bans -d '{"range":"203.0.113.0/24"}'    # hashed, not stored raw
curl -H "$A" -X DELETE $H/v1/admin/bans/<hash>
curl -H "$A" $H/v1/admin/releases/pt-BR                                      # versions
curl -H "$A" -X POST $H/v1/admin/releases/pt-BR/build -d '{"force":true}'
curl -H "$A" -X POST $H/v1/admin/releases/pt-BR/rollback -d '{"version":41}'
curl -H "$A" -X POST $H/v1/admin/review                                      # run a review batch now
curl -H "$A" -X POST $H/v1/admin/backup
```

`purge` deletes that IP hash's votes (recounting scores) and hides pending site corrections
only it sent. A rollback **pins** the language: scheduled builds stop until an admin calls
`build` again (otherwise the next daily build would publish over the rollback).

## Configuration

Vars (`wrangler.toml`): `LANGS` (comma-separated, default `pt-BR`), `NIM_MODEL`
(default `nvidia/llama-3.3-nemotron-super-49b-v1`; `nvidia/llama-3.3-nemotron-super-49b-v1.5`
is also hosted on build.nvidia.com), `NIM_URL`, `REVIEW_BATCH` (queue items per cron run,
default 200), `TURNSTILE_SITE_KEY`, `TURNSTILE_HOSTNAME` (optional), `DOWNLOAD_URL`.

Secrets (`bun wrangler secret put NAME`; never commit them):

| Secret | Use |
|---|---|
| `NVIDIA_API_KEY` | NVIDIA NIM API key (build.nvidia.com). Without it nothing passes AI review (items wait). |
| `TURNSTILE_SECRET` | Turnstile secret key (votes/corrections return 503 without it). |
| `RELEASE_SIGNING_KEY` | Ed25519 private key: base64 of the 32-byte seed (a 64-byte seed‖public pair is accepted too). |
| `ADMIN_TOKEN` | Admin bearer token, ≥ 16 chars (e.g. `openssl rand -base64 32`). |
| `IP_SALT` | Secret salt for IP hashes (e.g. `openssl rand -base64 32`). Writes return 503 without it. **Never change it**: votes, bans and rate limits are keyed by it. |

For local dev put them in `.dev.vars` (git-ignored).

## Deploy

Needs Node.js (for `wrangler`) and Rust with `wasm32-unknown-unknown`. The Workers Free plan is enough: the
CPU-heavy daily jobs run in GitHub Actions (secrets `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`,
`RELEASE_SIGNING_KEY` in the `production` environment). Run them by hand with
`cargo run --release --features jobs --bin jobs -- release|backup` and the same env vars.

```sh
cd backend
rustup target add wasm32-unknown-unknown
bun install                  # wrangler, pinned in package.json
bun wrangler login

# 1. D1: create, paste the database_id into wrangler.toml, apply the schema
bun wrangler d1 create gw2enhanced
bun wrangler d1 migrations apply gw2enhanced --remote

# 2. R2 bucket for releases and backups
bun wrangler r2 bucket create gw2enhanced
#    (optional) expire old backups: lifecycle rule on prefix backups/ (e.g. 60 days)
bun wrangler r2 bucket lifecycle add gw2enhanced backups-expire backups/ --expire-days 60

# 3. Turnstile: create a widget in the dashboard for the Worker hostname,
#    put its site key in TURNSTILE_SITE_KEY (wrangler.toml)

# 4. Secrets
cargo run --example keygen        # prints RELEASE_SIGNING_KEY and the public key for the mod
bun wrangler secret put RELEASE_SIGNING_KEY
bun wrangler secret put NVIDIA_API_KEY
bun wrangler secret put TURNSTILE_SECRET
bun wrangler secret put ADMIN_TOKEN
bun wrangler secret put IP_SALT

# 5. Deploy (wrangler runs worker-build), then build the first release
bun wrangler deploy
curl -H "Authorization: Bearer $ADMIN_TOKEN" -X POST https://gw2enhanced.wilssola.dev/v1/admin/releases/pt-BR/build
```

The Worker (`gw2enhanced`) is served at `https://gw2enhanced.wilssola.dev` (custom domain in `wrangler.toml` `routes`) and at
`https://gw2enhanced.wilssola.workers.dev` (workers.dev hostnames are `<worker>.<account subdomain>.workers.dev`; the account
subdomain is `wilssola`).

Local run: `bun run migrate:local && bun wrangler dev`
(`bun wrangler dev --test-scheduled`, then `curl "http://localhost:8787/__scheduled?cron=*/5+*+*+*+*"`).

## Backups and restore

Daily `backups/{YYYYMMDDTHHMMSSZ}/{table}-NNN.jsonl.gz` (one JSON row per line, `_rowid`
included, ≤ 50k rows per file) plus `done.json` when complete. D1 Time Travel
(`wrangler d1 time-travel restore`) covers the last 30 days on top of that. To restore from
R2: download the files, convert each line to an `INSERT` (columns = keys minus `_rowid`) and
run them with `bun wrangler d1 execute gw2enhanced --remote --file restore.sql` on a database
created from `migrations/`.

## Limits and notes

- D1 queries per request are constant (rows go in as JSON arrays expanded with `json_each`).
- Release build reads `current` by key in pages of 5000 and gzips in memory (tens of MB for
  ~300k texts: fine within 128 MB).
- Served on the custom domain gw2enhanced.wilssola.dev (edge cache available); workers.dev responses are not CDN-cached; release files still carry `immutable` headers for
  clients. Use a custom domain + Cache Rules if download volume grows.
