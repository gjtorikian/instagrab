# Deploying instagrab to a Linux host

A long-lived headless Chrome (systemd unit) holds the
authenticated Instagram session in a persistent profile dir; cron invokes the
`instagrab` binary daily to attach over CDP and append JSONL.

## 1. Provision the host

Any small Ubuntu (22.04+) host. ~1 GB RAM is enough since only one Chrome
process runs.

```sh
# As root on the host:
adduser --system --group --home /var/lib/instagrab instagrab
mkdir -p /var/lib/instagrab/CDPProfile /var/log
chown -R instagrab:instagrab /var/lib/instagrab

# Chrome
apt-get update
apt-get install -y wget gnupg
wget -qO- https://dl.google.com/linux/linux_signing_key.pub | gpg --dearmor -o /usr/share/keyrings/google-chrome.gpg
echo "deb [arch=amd64 signed-by=/usr/share/keyrings/google-chrome.gpg] http://dl.google.com/linux/chrome/deb/ stable main" \
  > /etc/apt/sources.list.d/google-chrome.list
apt-get update && apt-get install -y google-chrome-stable
```

## 2. Install the binary and config

### From a local cross-compile (default)

Build locally. `scripts/release` produces a stripped, static (musl) binary so
there's no libc dependency on the host. It uses
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) — no Docker
required — so install it once (`cargo install cargo-zigbuild` plus zig, e.g.
`brew install zig`):

```sh
# from the repo root, on eg macOS:
./scripts/release                             # -> dist/instagrab-x86_64-unknown-linux-musl
scp dist/instagrab-x86_64-unknown-linux-musl <user>@<host>:/tmp/instagrab
scp config.example.toml <user>@<host>:/tmp/config.toml
```

On the host:

```sh
install -m 0755 /tmp/instagrab /usr/local/bin/instagrab
install -d -o instagrab -g instagrab /etc/instagrab
install -m 0644 -o instagrab -g instagrab /tmp/config.toml /etc/instagrab/config.toml
vi /etc/instagrab/config.toml   # add real config
```

### Alternative: install from crates.io

`instagrab` is published, so the host can build it itself and skip the
cross-compile and `scp` above. The trade-off: this needs a Rust toolchain on
the host and compiles there, where the static musl binary needs neither.

```sh
# On the host:
sudo apt-get install -y build-essential pkg-config
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

# --root /usr/local lands it at /usr/local/bin/instagrab, as above
cargo install instagrab --locked --root /usr/local
```

`cargo install` installs only the binary, so there's no `config.example.toml`
to copy over — have the binary write its own:

```sh
install -d -o instagrab -g instagrab /etc/instagrab
/usr/local/bin/instagrab --write-sample-config /etc/instagrab/config.toml
chown instagrab:instagrab /etc/instagrab/config.toml
chmod 0644 /etc/instagrab/config.toml
vi /etc/instagrab/config.toml   # add real config
```

Two caveats:

- **RAM.** The release profile sets `lto = true`; linking can OOM on the ~1 GB
  host sized in step 1. Add swap first, or stay on the cross-compiled binary.
- **`deploy/` files.** `cargo install` puts only the binary on the host, not
  `chrome.service` or `instagrab.cron`. Steps 3 and 5 give the fetch commands.

Pin a version with `--version 0.2.0`; upgrade later by re-running the
`cargo install` with `--force`.

> **Note:** crates.io currently serves 0.2.0, which predates the move off
> `/api/v1/users/web_profile_info/` (dead since 2026-09-14 — it answers 429
> with an HTML error page). Until a newer version is published, build from a
> checkout with `./scripts/release` and copy the binary across; a `cargo
> install` of 0.2.0 will fail its canary on the first run.

## 3. Install the systemd unit

From a repo checkout on the host:

```sh
install -m 0644 deploy/chrome.service /etc/systemd/system/chrome.service
```

If there's no checkout (i.e. the crates.io path) — fetch it instead:

```sh
curl -fsSL -o /etc/systemd/system/chrome.service \
  https://raw.githubusercontent.com/gjtorikian/instagrab/main/deploy/chrome.service
chmod 0644 /etc/systemd/system/chrome.service
```

Then, either way:

```sh
systemctl daemon-reload
systemctl enable --now chrome
ss -tlnp | grep 9222   # confirms loopback listener
```

Before starting it, confirm the unit's `User=`/`Group=` and `--user-data-dir`
still match step 1 (`instagrab`, `/var/lib/instagrab/CDPProfile`), and that
`ExecStart`'s binary path exists — it's `/usr/bin/google-chrome`, which the
`google-chrome-stable` package in step 1 provides.

## 4. One-time login bootstrap

The host's IP is new to Instagram. Doing the login _on the host_ means
cookies are minted from that IP, and IG's first-login flag fires now (during
bootstrap) instead of during a cron run.

1. From your laptop, open an SSH tunnel:

   ```sh
   ssh -L 9222:127.0.0.1:9222 <user>@<host>   # e.g. dar@192.168.1.5
   ```

2. On your laptop, in Chrome, open `chrome://inspect`. Click "Configure…" and
   add `localhost:9222`. The host's headless Chrome `about:blank` target
   appears under "Remote Target".

3. Click **inspect** on that target. DevTools opens with a screencast pane —
   you can click and type in the headless page through this pane.

4. Navigate to `https://www.instagram.com/`, sign in, complete any 2FA / new-
   device verification email. Browse a profile or two so the session warms.

5. Close the DevTools tab and the SSH tunnel. Cookies persist in
   `/var/lib/instagrab/CDPProfile/` and survive Chrome restarts.

Re-run this bootstrap only when the session expires (rare). The binary will
emit a `kind: "logged_out"` alert line and exit code 2 to tell you.

## 5. Verify and schedule

Manual smoke test:

```sh
sudo -u instagrab /usr/local/bin/instagrab \
  --config /etc/instagrab/config.toml \
  --once <your-handle>

tail -1 /var/lib/instagrab/runs.jsonl   # one JSON line
```

Wire cron — from a checkout, or fetched if you installed from crates.io:

```sh
install -m 0644 deploy/instagrab.cron /etc/cron.d/instagrab

# ...or, with no checkout on the host:
curl -fsSL -o /etc/cron.d/instagrab \
  https://raw.githubusercontent.com/gjtorikian/instagrab/main/deploy/instagrab.cron
chmod 0644 /etc/cron.d/instagrab
```

`cron` silently ignores anything in `/etc/cron.d` that is group- or
world-writable, or whose last line lacks a trailing newline — so keep the mode
at `0644` and check with `tail -c1 /etc/cron.d/instagrab | xxd`.

Two entries: the daily scan (04:17) and a monthly `--fetch-follows` refresh
(03:23 on the 1st), staggered so the two never share the one Chrome session.

## 6. Operational notes

- Output: `/var/lib/instagrab/runs.jsonl` (append-only). One line per
  username per run; plus `event: "alert"` lines for `logged_out` (exit 2)
  and `schema_drift` (exit 3).
- Logs: `/var/log/instagrab.log` (cron stdout/stderr).
- Rotating: drop a `logrotate(8)` snippet pointing at both files; nothing
  in instagrab opens long-lived handles to them across runs.
- Pause: `systemctl disable --now chrome` halts everything; cron will then
  exit code 4 (browser unreachable) until re-enabled.
