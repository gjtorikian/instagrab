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
src/config.rs      TOML loader + write_example for -write-sample-config
src/scraper.rs     CDP attach, per-username tab, active web_profile_info
                   fetch, mobile-API user-feed fetch + pagination loop
src/parse.rs       web_profile_info / user-feed JSON → ScrapeResult, window
                   filter, schema-drift detector (EXPECTED_PROFILE_PATHS)
src/images.rs      plain-HTTPS download of display_url, idempotent
src/output.rs      append-only JSONL writer + Alert struct
src/shutdown.rs    SIGINT/SIGTERM flag checked at operation boundaries
deploy/            systemd unit, cron entry, Linux host bootstrap docs
scripts/           fmt, build (build+test), release (cross-compile),
                   grab (local one-shot), chrome-launch (dev Chrome)
```

## Load-bearing design choices

Things that look wrong or removable until you know why:

- **Active fetch of `/api/v1/users/web_profile_info/` from inside the page**,
  not passive CDP listening. The logged-in React shell doesn't reliably
  trigger that endpoint on navigation, so passive listening misses profiles.
  The active fetch must send the `X-IG-App-ID` header IG's web app uses.
- **Every in-page script returns a _string_ (a JSON envelope).** The
  `headless_chrome` crate's `evaluate` hardcodes `returnByValue=false`, so
  objects wouldn't come back in `RemoteObject.value`; each script therefore
  does its own `JSON.stringify` and we parse the string out.
- **Canary username (`instagram`) scraped first every run.** Zero posts
  _without_ a `logged_out` signal means our extraction broke — bail with
  exit code 5 before burning the real targets and tripping IG.
- **Schema-drift alert fires only when drift hits 100% of successful results.**
  One odd/private profile is noise; uniform drift means IG changed shape and
  `EXPECTED_PROFILE_PATHS` + the parser need updating.
- **Post extraction is shape-tolerant.** The mobile feed uses `code`,
  `taken_at`, and `image_versions2.candidates[].url`; carousels put the code
  on children. `src/parse.rs` handles the fallbacks. IG renames keys often.
- **Inter-profile jitter sleeps** (`jitter_min_secs`/`max_secs`) are
  anti-detection, not throttling for our benefit. Don't shrink them
  casually. The sleep is interruptible by SIGINT/SIGTERM.
- **The CDP connection's idle timeout must outlast a jitter sleep.** It's
  computed in `main.rs` from the per-tab budget and `jitter_max_secs`;
  a too-short idle timeout drops the connection between profiles.
- **JSONL is append-only.** Alerts are interleaved as `event: "alert"` lines
  alongside result lines; downstream readers must tolerate both shapes.
- **`--fetch-follows` merges into `friends.txt`; it must never overwrite it.**
  The file is the durable record of what to scrape, not a cache of what IG
  says — a commented-out username is an *exclusion* the user (or a UI that owns
  the file) set deliberately, and it has to survive every harvest. `cache.rs`
  keeps surviving lines byte-for-byte and preserves prose and blanks in place.
- **Deletion on absence is gated twice.** The merge drops names the harvest
  didn't return, so a truncated harvest would prune the file. `main.rs` passes
  `allow_deletes = oc.err.is_none()`, and `cache.rs` independently withholds
  deletes when a "clean" harvest returns under half the entries on file
  (pagination can end early without erroring). Either path emits a
  `follows_partial` alert. Do not relax either guard: a stale extra name costs
  one wasted scrape, a wrong delete costs data nothing else holds.
- **Exit codes are part of the contract** (cron + alerting key off them):
  0 OK, 1 config, 2 logged*out, 3 schema_drift, 4 browser, 5 canary_failed.
  See the `EXIT*\*`consts in`src/main.rs`.

## How to work on it

- Build + test: `./scripts/build` (wraps `cargo build && cargo test`).
- Format + lint before committing: `./scripts/fmt`.
- Local scrape against a dev Chrome: `./scripts/chrome-launch` in one shell,
  log in to IG manually once, then `./scripts/grab` (uses `./config.toml`).
- Dry-run the CDP attach without scraping: `instagrab -dry-run`.
- Per-username debug envelope to stderr: `-debug`.
- Cross-compile a release binary: `./scripts/release [<target-triple>]`.
  Defaults to `x86_64-unknown-linux-musl` (static).

There are no integration tests against live IG (and there shouldn't be — every run costs cookie-jar risk).
Parser changes get exercised via the canary at runtime.

## When IG breaks us

The two expected break modes and where to look:

- `logged_out` alert → cookies expired. Re-run the SSH-tunnel login bootstrap
  in `deploy/README.md`. No code change.
- `schema_drift` alert → IG renamed/removed fields. Update
  `EXPECTED_PROFILE_PATHS` and the field reads in `src/parse.rs`, then verify
  against the canary before redeploying.
