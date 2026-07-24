# instagrab

A Rust CLI that scrapes Instagram profile metadata (full name, biography,
follower / following / post counts, recent posts with captions) by attaching
to a real Chrome process over the Chrome DevTools Protocol.

Designed to run on a server you own under cron, alongside a long-lived
headless Chrome whose user-data-dir holds an authenticated IG session.

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

# 2. Build and write a sample config:
cargo build
./target/debug/instagrab -write-sample-config /etc/instagrab/config.toml

# Edit /tmp/instagrab.toml: set output_path to /tmp/runs.jsonl

# 3. Scrape one profile:
./target/debug/instagrab -config /etc/instagrab/config.toml -once gjtorikian
tail -1 /tmp/runs.jsonl | jq .
```

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
      "shortcode": "DZ2vvy5CUBr",
      "url": "https://www.instagram.com/p/DZ2vvy5CUBr/",
      "caption": "Once every few years, The Jersey comes out.",
      "taken_at_unix": 1782060967,
      "display_url": "https://scontent-lga3-1.cdninstagram.com/...",
      "local_path": "./images/gjtorikian/DZ2vvy5CUBr.jpg"
    },
    {
      "shortcode": "DZeGQP5i8Wi",
      "url": "https://www.instagram.com/p/DZeGQP5i8Wi/",
      "caption": "Always a great time @chrisgeth show!",
      "taken_at_unix": 1781234021,
      "display_url": "https://scontent-lga3-2.cdninstagram.com/...",
      "local_path": "./images/gjtorikian/DZeGQP5i8Wi.jpg"
    },
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

## v2 backlog

- **Stories** (`feed/user/<id>/story/`) — same fetch pattern, needs a
  click step to trigger the fetch.
- **Followers / following lists** (`friendships/<id>/followers/` paginated) —
  highest IG-detection risk; will be gated behind an explicit config flag
  with its own throttle.
- **Carousel images / videos** — currently only `display_url` (cover image)
  is downloaded; sidecar children and video files require per-post fetches.

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
4. If `images_dir` is set, each kept post's `display_url` is downloaded
   over plain HTTPS to `<images_dir>/<username>/<shortcode>.jpg`
   (idempotent — existing files are kept).
5. One JSONL line per username is appended to the configured output file.

## License

[MIT](LICENSE.txt)
