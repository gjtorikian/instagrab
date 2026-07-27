# instagrab

A Rust CLI that scrapes Instagram profile metadata (full name, biography,
follower / following / post counts, recent posts with captions) by attaching
to a real Chrome process over the Chrome DevTools Protocol.

Designed to run on a server you own under cron, alongside a long-lived
headless Chrome whose user-data-dir holds an authenticated IG session.

It can scan a fixed list of profiles, or fan out over everyone a seed profile
follows (re-fetched on demand), writing one JSONL record + the first image per
post into a directory.

> Instagram's ToS prohibits automated scraping. Use at your own account risk.
> The defaults (daily cadence, jittered inter-profile sleeps, single account)
> are conservative for personal-scale use.

## Getting started

You'll need a recent Rust toolchain and Chrome.

```sh
# 1. Launch Chrome with a dedicated dev profile and sign in to IG manually:
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9222 \
  --user-data-dir="$HOME/.instagrab/CDPProfile"

# 2. Build and write a sample config, then edit it
# (at minimum set output_path and seed_username):
cargo build
./target/debug/instagrab --write-sample-config ./config.toml

# 3. Fetch your follows list (writes <output_path>/friends.txt):
./target/debug/instagrab --config ./config.toml --fetch-follows

# 4. Scan everyone on that list (writes <output_path>/runs.jsonl + images):
./target/debug/instagrab --config ./config.toml
tail -1 <output_path>/runs.jsonl | jq .
```

instagrab does two things:

- **`--fetch-follows`** pages your seed profile's Following into a follows file (run it rarely).
- The **default run** scans everyone in that file for recent posts + first image (run it daily).

For more information, see [Friends fan-out](#friends-fan-out) and [Configuration](#configuration).

## Output shape

A successful run emits one JSON object per username:

```json
{
  "scraped_at": "2026-07-24T02:17:10.765462Z",
  "username": "gjtorikian",
  "user_id": "1018933991",
  "source": "graphql",
  "full_name": "Garen J Torikian",
  "biography": "I inhale and exhale.",
  "is_private": false,
  "followers": 654,
  "following": 577,
  "posts": 497,
  "recent_posts": [
    {
      "shortcode": "DXztvjcCTy8",
      "url": "https://www.instagram.com/p/DXztvjcCTy8/",
      "caption": "Fausto is SEVENTEEN today! This is the best photo he would let me take!!!",
      "taken_at_unix": 1777664290,
      "display_url": "https://scontent-lga3-3.cdninstagram.com/...",
      "local_path": "./images/gjtorikian/DXztvjcCTy8.jpg"
    }
  ],
  "window_days": 90
}
```

`source` is `"graphql"` (the only successful kind), `"not_found"`,
`"logged_out"`, or `"error"`. Per-field problems land in the `errors` array.

`window_maybe_truncated: true` means paging stopped before reaching the
oldest post in the window; bump `max_scrolls` if you need to go further.

A post may also carry `media_count`, `is_carousel`, and `has_video` (present
only when it's a carousel or a video). Due to rate limiting, only the first image is downloaded;
one can link out to Instagram for the rest.

## Friends fan-out

instagrab's main mode scans everyone a seed profile follows, re-fetching that
list on demand.

`instagrab --fetch-follows` pages `seed_username`'s _Following_ once, at a human
pace, and writes the handles to the follows file (`<output_path>/friends.txt`).
This is the only path that touches Instagram's friendship endpoint (isolated in
`src/follows.rs`); it is off by default and detection-sensitive, so run it
rarely. It prints progress (`fetching follows... N so far`) and a final count.
`--seed <name>` overrides `seed_username` ad hoc.

## Commands

| Command                                     | What it does                                                                                       |
| ------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `instagrab`                                 | Scan everyone in the follows file (the daily job).                                                 |
| `instagrab --fetch-follows [--seed <name>]` | Rebuild the follows file from the seed's Following (run rarely).                                   |
| `instagrab --once <name>`                   | Scan a single profile, ignoring the follows list.                                                  |
| `instagrab --limit <n>`                     | Scan at most N profiles from the follows list (order stalest-first upstream to scan the oldest N). |
| `instagrab --dry-run`                       | Connect to Chrome and exit — a connection test.                                                    |
| `instagrab --write-sample-config <path>`    | Write a starter config and exit.                                                                   |
| `instagrab --debug`                         | Add per-profile fetch/DOM debug output on stderr.                                                  |
| `instagrab --config <path>`                 | Use a specific config (default `/etc/instagrab/config.toml`).                                      |

## Configuration

All settings live in the TOML file passed with `--config` (generate a starter
with `--write-sample-config`). **`output_path` (an output directory) is
required**; everything else has a default.

| Key                | Default                 | Meaning                                                                     |
| ------------------ | ----------------------- | --------------------------------------------------------------------------- |
| `browser_url`      | `http://127.0.0.1:9222` | CDP endpoint of the long-lived Chrome instagrab attaches to.                |
| `output_path`      | — (required)            | The one output directory. Holds `runs.jsonl`, `images/`, and `friends.txt`. |
| `seed_username`    | —                       | Profile whose Following `--fetch-follows` pages (or pass `--seed`).         |
| `time_window_days` | `0` (no filter)         | Keep only posts within the last N days; `> 0` also drives feed pagination.  |
| `jitter_min_secs`  | `30`                    | Minimum inter-profile sleep, seconds (anti-detection).                      |
| `jitter_max_secs`  | `75`                    | Maximum inter-profile sleep, seconds.                                       |
| `nav_timeout_sec`  | `20`                    | Per-profile navigation/wait budget, seconds.                                |
| `max_scrolls`      | `8`                     | Safety cap on feed pages fetched per profile.                               |
| `usernames`        | `[]`                    | Fixed scrape list, used only as a fallback when the follows file is empty.  |

The canary scrape (against `instagram.com/instagram`) has no knob — it always
runs first and exits 5 if extraction looks broken.

## Alerts

Out-of-band conditions surface as `event: "alert"` lines and a non-zero exit:

| Kind           | Meaning                                                                                 | Exit |
| -------------- | --------------------------------------------------------------------------------------- | ---- |
| `logged_out`   | IG demanded login or redirected to `/accounts/login`                                    | 2    |
| `schema_drift` | `web_profile_info` parsed but expected fields are missing on **every** profile this run | 3    |

`logged_out` means: re-run the SSH-tunnel login bootstrap (see `deploy/README.md`).
`schema_drift` means: IG renamed/removed a JSON path; update
`EXPECTED_PROFILE_PATHS` and the parser in `src/parse.rs`.

## Deploying

See [`deploy/README.md`](deploy/README.md) for a full deployment walkthrough,
including the SSH-tunnel login bootstrap that mints cookies
(so IG's first-login flag fires once, during setup, not during cron).

## How it works

1. A persistent `--headless=new` Chrome runs on the host with
   `--remote-debugging-port=9222 --user-data-dir=…/CDPProfile`. You log in
   to Instagram once into that profile.
2. `instagrab` connects to that Chrome via CDP, navigates each profile in
   its own tab, then _actively_ fetches `/api/v1/users/web_profile_info/`
   from inside the page (with the `X-IG-App-ID` header IG's web app sends).
   Passively listening for that endpoint isn't reliable — the logged-in
   React shell doesn't always trigger it.
3. Posts come from the mobile-API user-feed endpoint
   (`/api/v1/feed/user/<id>/`), fetched from inside the page and paged via
   `max_id`. If `time_window_days > 0`, instagrab keeps paging until the
   oldest captured post is older than the window.
4. Each kept post's `display_url` is downloaded over plain HTTPS to
   `<output_path>/images/<username>/<shortcode>.jpg` (idempotent — existing
   files are kept).
5. One JSONL line per username is appended to `<output_path>/runs.jsonl`.

## License

[MIT](LICENSE.txt)
