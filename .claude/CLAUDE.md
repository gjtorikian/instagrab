# CLAUDE.md

`README.md` is the human-facing intro and quickstart — read it first. This file
captures the non-obvious context an agent needs on top of that.

## What this is

A Rust CLI that scrapes Instagram profile metadata by attaching to a
long-lived headless Chrome over CDP (the `headless_chrome` crate). One JSONL
line per username per run. Designed for cron on a server the user owns;
conservative defaults because IG's ToS prohibits this and the cookie jar is a
real account.

## Layout

```
src/main.rs        flag parsing, orchestration, alert + exit codes, canary
src/config.rs      TOML loader + write_example for --write-sample-config
src/scraper.rs     CDP attach, per-username tab, GraphQL query capture +
                   replay, profile-posts pagination loop
src/parse.rs       GraphQL profile / posts JSON → ScrapeResult, window
                   filter, schema-drift detector
src/images.rs      plain-HTTPS download of display_url, idempotent
src/output.rs      append-only JSONL writer + Alert struct
src/shutdown.rs    SIGINT/SIGTERM flag checked at operation boundaries
deploy/            systemd unit, cron entry, Linux host bootstrap docs
scripts/           fmt, build (build+test), release (cross-compile),
                   grab (local one-shot), chrome-launch (dev Chrome)
```

## Load-bearing design choices

Things that look wrong or removable until you know why:

- **Queries are captured from the page, then replayed — not pinned.** IG
  serves profile data from persisted GraphQL queries whose `doc_id` rotates
  with every web deploy, so hardcoding one guarantees a break. Instead
  `RECORDER_SCRIPT` is installed via `Page.addScriptToEvaluateOnNewDocument`
  (before the app's bundle runs), wraps `fetch`/`XHR`, and records the app's
  own `/graphql/query` POSTs. `harvest_queries` folds them into a catalog;
  `issue_query` replays them with our `username` and cursor.
- **Observing the query is not the same as observing the data.** The old rule
  here said "active fetch, not passive CDP listening," because the React shell
  doesn't reliably fire a request per navigation and passive listening missed
  profiles. That still holds: the recorder only learns the query *shape*
  (`doc_id`, variables, CSRF tokens), and the data still comes from an active
  fetch we issue. Replays tag themselves `__igq_replay=1` so the recorder
  doesn't ingest its own traffic.
- **Captures are told apart by Relay's own vocabulary.** A capture with `first`
  in its variables is the paginated posts tab; one without is a candidate
  profile header. Classifying on `first`/`username` rather than
  `fb_api_req_friendly_name` survives IG's renames, and the `username` check is
  what keeps the home-timeline query (`PolarisFeedTimelineRootV2Query`, which
  also has `first`) from being mistaken for the posts tab.
- **Profile counts come from meta tags, not the API.** As of 2026-09-14 both
  queries the profile page fires return the same posts connection with no
  header object in it, so followers/following/post-count are read from the
  `og:description` Instagram server-renders on a direct navigation. The HTML is
  already in hand for app-id extraction, so this costs no request — and the
  captured "profile" query is deliberately *not* issued when the tags succeed,
  because an extra request per profile is exactly the cost this project is
  careful about. Two consequences: `biography` is unavailable, and large
  accounts get rounded counts ("695M"). Ordinary accounts render exact numbers.
- **Nothing is attributed to an account that didn't name itself.** Both
  `find_user_object` and `user_id_from_feed` require a `username` matching the
  one being scraped, and the former also requires a count field. This is not
  defensive programming for its own sake: without those guards a per-post
  `user` stub wins by default when no header exists, and the first live run
  wrote a tagged creator's `user_id` and `full_name` into a line labelled
  `username: instagram`. Missing data is recoverable; confidently wrong
  identity data is not.
- **Every in-page script returns a _string_ (a JSON envelope).** The
  `headless_chrome` crate's `evaluate` hardcodes `returnByValue=false`, so
  objects wouldn't come back in `RemoteObject.value`; each script therefore
  does its own `JSON.stringify` and we parse the string out.
- **Canary username (`instagram`) scraped first every run.** Zero posts
  _without_ a `logged_out` signal means our extraction broke — bail with
  exit code 5 before burning the real targets and tripping IG.
- **Schema-drift alert fires only when drift hits 100% of successful results.**
  One odd/private profile is noise; uniform drift means IG changed shape and
  `EXPECTED_GRAPHQL_PROFILE_FIELDS` + the parser need updating.
- **Post extraction is shape-tolerant.** Nodes use `code`, `taken_at`, and
  `image_versions2.candidates[].url`. `src/parse.rs` handles the fallbacks;
  IG renames keys often. Note the carousel inversion between sources: the
  mobile feed put `code` on children, while the GraphQL connection puts it on
  the *parent* and sets children to `"code": null`.
- **Profile fields are found, not path-indexed.** `parse_graphql_profile`
  walks the response for the object carrying the most expected fields rather
  than reading a fixed path, because the envelope key is discovered at runtime
  and IG has spelled it `user`, `xdt_user_by_username`, and others. Counts are
  read flat (`follower_count`) or nested (`edge_followed_by.count`).
- **Inter-profile jitter sleeps** (`jitter_min_secs`/`max_secs`) are
  anti-detection, not throttling for our benefit. Don't shrink them
  casually. The sleep is interruptible by SIGINT/SIGTERM.
- **The CDP connection's idle timeout must outlast a jitter sleep.** It's
  computed in `main.rs` from the per-tab budget and `jitter_max_secs`;
  a too-short idle timeout drops the connection between profiles.
- **JSONL is append-only.** Alerts are interleaved as `event: "alert"` lines
  alongside result lines; downstream readers must tolerate both shapes.
- **Exit codes are part of the contract** (cron + alerting key off them):
  0 OK, 1 config, 2 logged*out, 3 schema_drift, 4 browser, 5 canary_failed.
  See the `EXIT*\*`consts in`src/main.rs`.

## How to work on it

- Build + test: `./scripts/build` (wraps `cargo build && cargo test`).
- Format + lint before committing: `./scripts/fmt`.
- Local scrape against a dev Chrome: `./scripts/chrome-launch` in one shell,
  log in to IG manually once, then `./scripts/grab` (uses `./config.toml`).
- Dry-run the CDP attach without scraping: `instagrab --dry-run`.
- Per-username debug envelope to stderr: `--debug`.
- Inspect the GraphQL queries a profile page fires, without
  scraping: `instagrab --capture-queries <username>`.
- Cross-compile a release binary: `./scripts/release [<target-triple>]`.
  Defaults to `x86_64-unknown-linux-musl` (static).

There are no integration tests against live IG (and there shouldn't be — every run costs cookie-jar risk).
Parser changes get exercised via the canary at runtime.

## When IG breaks us

The three expected break modes and where to look:

- `logged_out` alert → cookies expired. Re-run the SSH-tunnel login bootstrap
  in `deploy/README.md`. No code change. Note what does *not* raise this: a
  401/403 on a replayed query. The navigation clears the login redirect before
  any replay is issued, so a rejected replay means a missing header or rotated
  token, and it surfaces as `profile query rejected: http NNN (replay refused,
  session is live)`. Mapping that to `logged_out` would send whoever is on call
  to redo a login bootstrap they don't need.
- `schema_drift` alert → IG renamed/removed fields. Update
  `EXPECTED_GRAPHQL_PROFILE_FIELDS` and the field reads in `src/parse.rs`,
  then verify against the canary before redeploying.
- `canary_failed` with `no GraphQL queries captured` → the recorder saw
  nothing, meaning IG changed how its client issues queries (a different
  transport, or the page stopped firing them on load). Run
  `instagrab --capture-queries instagram` to see what the page actually fired.
  An empty list there confirms it; a non-empty one points at the
  classification in `CapturedQuery::is_paginated`.

A note on what *doesn't* appear here: `/api/v1/users/web_profile_info/` was the
original data source and is gone — as of 2026-09-14 it answers 429 with IG's
"Page Not Found" HTML shell even on a healthy logged-in session. The status is
misleading; it is not rate limiting. The mobile `/api/v1/feed/user/` path went
with it, since its `user_id` came from that same call. Both parsers were
removed rather than left dead; see git history if the endpoints ever return.
