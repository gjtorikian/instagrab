//! TOML loader + write_example for -write-sample-config.

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::fs;

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct Config {
    pub browser_url: String,
    pub output_path: String,
    pub usernames: Vec<String>,
    pub jitter_min_secs: i64,
    pub jitter_max_secs: i64,
    pub nav_timeout_sec: i64,
    /// 0 = no filter
    pub time_window_days: i64,
    /// safety cap on pagination
    pub max_scrolls: i64,
    /// empty = don't download
    pub images_dir: String,
}

pub fn load(path: &str) -> Result<Config> {
    let text = fs::read_to_string(path).map_err(|e| anyhow!("decode {path}: {e}"))?;
    let mut c: Config = toml::from_str(&text).map_err(|e| anyhow!("decode {path}: {e}"))?;
    c.apply_defaults();
    c.validate()?;
    Ok(c)
}

impl Config {
    fn apply_defaults(&mut self) {
        if self.browser_url.is_empty() {
            self.browser_url = "http://127.0.0.1:9222".to_string();
        }
        if self.jitter_min_secs == 0 {
            self.jitter_min_secs = 30;
        }
        if self.jitter_max_secs == 0 {
            self.jitter_max_secs = 75;
        }
        if self.nav_timeout_sec == 0 {
            self.nav_timeout_sec = 20;
        }
        if self.max_scrolls == 0 {
            self.max_scrolls = 8;
        }
    }

    fn validate(&mut self) -> Result<()> {
        if self.output_path.is_empty() {
            bail!("output_path is required");
        }
        if self.jitter_min_secs < 0 || self.jitter_max_secs < self.jitter_min_secs {
            bail!(
                "invalid jitter range: {}..{}",
                self.jitter_min_secs,
                self.jitter_max_secs
            );
        }
        // Strip a leading @ first, then whitespace
        let mut clean = Vec::with_capacity(self.usernames.len());
        for u in &self.usernames {
            let u = u.strip_prefix('@').unwrap_or(u).trim();
            if u.is_empty() {
                continue;
            }
            clean.push(u.to_string());
        }
        self.usernames = clean;
        Ok(())
    }
}

pub fn write_example(path: &str) -> Result<()> {
    const SAMPLE: &str = r#"# instagrab config

# CDP endpoint of the long-lived Chrome service.
browser_url = "http://127.0.0.1:9222"

# JSONL file. Each run appends one line per username plus any alert lines.
output_path = "/var/lib/instagrab/runs.jsonl"

# Per-run sleep between profiles, in seconds (uniform random in [min, max]).
jitter_min_secs = 30
jitter_max_secs = 75

# Per-username navigation/wait budget.
nav_timeout_sec = 20

# Only keep recent_posts whose taken_at_timestamp falls within the last N days.
# 0 disables the filter. With time_window_days > 0, instagrab also paginates
# (scrolls the profile grid) past the initial 12 posts until the oldest
# captured post is older than the window.
time_window_days = 7

# Safety cap on pagination scrolls per profile. Each iteration is ~2.5s.
max_scrolls = 8

# If set, download each kept post's display_url to <images_dir>/<username>/<shortcode>.jpg.
# Idempotent: existing files are kept. Empty string disables image download.
images_dir = ""

# A canary scrape against instagram.com/instagram runs before each batch.
# If it comes back with zero posts and no logged_out signal, we emit a
# "canary_failed" alert and exit 5 — distinguishes "IG broke" from "this
# user is private". No config knob; the canary is always on.

# Profiles to scrape. Leading @ is allowed and stripped.
usernames = [
  "gjtorikian",
]
"#;
    fs::write(path, SAMPLE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_username_cleaning() {
        let dir = std::env::temp_dir().join("instagrab-config-test");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.toml");
        fs::write(
            &p,
            r#"
output_path = "/tmp/runs.jsonl"
usernames = ["@zuck", "  ", "plain", "@"]
"#,
        )
        .unwrap();
        let c = load(p.to_str().unwrap()).unwrap();
        assert_eq!(c.browser_url, "http://127.0.0.1:9222");
        assert_eq!(c.jitter_min_secs, 30);
        assert_eq!(c.jitter_max_secs, 75);
        assert_eq!(c.nav_timeout_sec, 20);
        assert_eq!(c.max_scrolls, 8);
        assert_eq!(c.time_window_days, 0);
        assert_eq!(c.usernames, vec!["zuck", "plain"]);
    }

    #[test]
    fn missing_output_path_rejected() {
        let dir = std::env::temp_dir().join("instagrab-config-test");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        fs::write(&p, "usernames = []\n").unwrap();
        let err = load(p.to_str().unwrap()).unwrap_err();
        assert_eq!(err.to_string(), "output_path is required");
    }

    #[test]
    fn invalid_jitter_rejected() {
        let dir = std::env::temp_dir().join("instagrab-config-test");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("jitter.toml");
        fs::write(
            &p,
            "output_path = \"/tmp/r.jsonl\"\njitter_min_secs = 100\n",
        )
        .unwrap();
        let err = load(p.to_str().unwrap()).unwrap_err();
        assert_eq!(err.to_string(), "invalid jitter range: 100..75");
    }
}
