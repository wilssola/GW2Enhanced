# Backend API (v1)

Server: `https://gw2enhanced.wilssola.dev` (Cloudflare Worker in Rust, see the [README](../README.md)).
Storage: D1 (all records) + R2 (published dictionary releases and backups).

Nothing sent by the mod or by the site is trusted: there is no authentication,
anyone can call these endpoints with forged data.

## Mod → server

### `POST /v1/contributions`

Sent by the mod only when the player opted in (`contribute=true`). Gzip
allowed (`Content-Encoding: gzip`). At most 500 items and 256 KiB per request.

```json
{
  "client": "gw2translated/0.1.0",
  "game_build": 209026,
  "lang": "pt-BR",
  "items": [
    { "id": 621576, "en": "Ingredient" },
    { "en": "Sand Lioness", "tr": "Leoa de Areia", "src": "argos" },
    { "id": 1601, "en": "The Cost of Victory", "tr": "O Custo da Vitória", "src": "argos" }
  ]
}
```

- `id` (optional): the game's text ID. `en` (required): the English text.
- `tr` + `src` (optional): a translation made on the player's PC (`src` must be
  `argos`; any other source, e.g. a dictionary text the server's AI translated, is
  refused and only the English/ID part is kept).
- A new translation is not stored when the text already has a translation by the
  server's AI (source `ai`, not hidden) in that language, or already has 10 different
  translations (`MAX_TRANSLATIONS_PER_TEXT`); sending an existing one still counts its
  reporter.
- Response `202 {"accepted": n, "rejected": m}`; `400` on a malformed body;
  `413` when too large; `429` when the IP is over its rate limit (the mod
  waits and retries later).

Server rules:

- Items go to a review queue; nothing is public before it passes the checks.
- English texts are verified too (they can be forged): an ID/English pair is
  only trusted when it matches what other players sent for the same ID, or
  the AI review finds it plausible game text. A pair that conflicts with an
  already trusted one is rejected.
- Translations are reviewed by AI (NVIDIA NIM): must translate the English
  text, keep the markup (`%str1%`, `<c=…>`, `<br>`, `[s]`…), and must not be
  a joke, an insult, spam or a link.
- The first translation that passes review becomes the current one for that
  text, until votes replace it (see Voting).

## Dictionary downloads

### `GET /v1/releases/{lang}/latest`

```json
{
  "lang": "pt-BR",
  "version": 42,
  "created_at": "<RFC 3339 UTC time>",
  "files": [
    { "name": "dictionary-000.jsonl.gz", "sha256": "…", "size": 123456 }
  ],
  "signature": "<base64 Ed25519 signature of the JSON above without this field, keys sorted>"
}
```

- Files: `GET /v1/releases/{lang}/{version}/{name}` (immutable, cached by the
  CDN). Record format is the mod's own `dictionary.jsonl` (`records.rs`): one
  line per English text, `{"en","tr","src","ids":[…]}` (`ids` = every game
  text ID known for that English text, omitted when empty).
- Every release is kept in R2 (versioning); an admin can roll `latest` back to
  any earlier version. D1 is also backed up to R2 on a schedule.
- The mod verifies the signature with the public key it ships with, and
  ignores the release if it fails.

## Site (no login)

All writes need a Cloudflare Turnstile token (`turnstile` field), and are
limited to one vote per IP per translation.

- `GET /` — the site: texts with original and translation side by side,
  sorted lists with numbered pages, search, metrics.
- `GET /v1/texts?lang=pt-BR&sort=recent|oldest|best|worst&page=1&limit=20&q=…` —
  one page of texts with their current translation and alternatives (with vote
  counts). See [Text listing](#text-listing).
- `POST /v1/votes` `{ "translation_id": "…", "value": 1 | -1, "turnstile": "…" }`
- `POST /v1/corrections` `{ "text_key": "…", "lang": "pt-BR", "tr": "…", "turnstile": "…" }`
  — reviewed by the same AI check before it can be voted on.
- `GET /v1/stats?lang=pt-BR` — % of known texts translated, % "perfect"
  (enough votes and a high score), mod download count.
- `GET /v1/download` — counts a download, then redirects to the latest GitHub
  release asset.

### Text listing

`GET /v1/texts` lists the texts that have a current translation in `lang`
(without `q`), one numbered page at a time:

| Parameter | Values | Default |
|---|---|---|
| `sort` | `recent` (newest current translation first), `oldest`, `best` (highest score), `worst` (most down votes minus up votes) | `recent` |
| `page` | 1, 2, … | 1 |
| `limit` | 1 to 100 rows per page | 20 |
| `q` | search text (English or translation), up to 100 characters | none |
| `cursor` | old form: rows to skip; ignored when `page` is given | none |

Every sort ends with the text key as a tie-break, so pages never overlap or
skip a row while the data stands still. Each sort reads one index of `current`
in order (`current_updated`, `current_score`, `current_net`, migration 0002),
so a page is `LIMIT`/`OFFSET` over an index walk with no sort; deep pages still
read the rows they skip, which the edge cache below absorbs.

Answer:

```json
{ "lang": "pt-BR", "sort": "best", "page": 3, "limit": 20, "offset": 40,
  "total": 229000, "more": true, "next_cursor": "60",
  "items": [{ "text_key": "…", "en": "…", "ids": [1601], "current": { … }, "alternatives": [ … ] }] }
```

- `total` is the number of texts with a translation (the same for every sort),
  taken from the cached `/v1/stats` value, so it can lag the list by a few
  minutes. It is `null` for searches.
- Searches scan for matches, so they only serve their first 3000 rows
  (`page` beyond that → 400 `bad_page`) and have no total; `more` says whether a
  next page exists.
- Pages without `q` are cached at the edge for 5 minutes, keyed by a canonical
  URL (`lang`, `sort`, offset, `limit`; any other parameter is ignored), so a
  vote can take up to 5 minutes to show in the lists. The Cache API only works
  on a custom domain, not on `*.workers.dev`. The read rate limit applies before the cache.
- Errors (400): `bad_sort`, `bad_page` (not a whole number ≥ 1, or past the
  last row served), `bad_limit` (not 1–100), `bad_cursor`, `unsupported_lang`.

## Voting

- The current translation of a text is the reviewed one with the best score
  (Wilson lower bound of up/down votes); ties keep the oldest.
- Translations flagged by the AI review or by many down votes are never
  current.
- Admin endpoints (behind a secret) can ban an IP range, hide a translation and
  roll back a release.
