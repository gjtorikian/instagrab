//! TOML loader + write_example for --write-sample-config.

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct Config {
    pub browser_url: String,
    /// The one output directory. instagrab writes everything under it:
    /// <output_path>/runs.jsonl, <output_path>/images/, <output_path>/friends.txt.
    pub output_path: String,
    pub usernames: Vec<String>,
    pub jitter_min_secs: i64,
    pub jitter_max_secs: i64,
    pub nav_timeout_sec: i64,
    /// 0 = no filter
    pub time_window_days: i64,
    /// safety cap on pagination
    pub max_scrolls: i64,
    /// Seed profile whose Following `--fetch-follows` pages (or pass `--seed`).
    pub seed_username: String,
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

    /// The JSONL scan output file, `<output_path>/runs.jsonl`.
    pub fn jsonl_path(&self) -> String {
        Path::new(&self.output_path)
            .join("runs.jsonl")
            .to_string_lossy()
            .into_owned()
    }

    /// The image download root, `<output_path>/images`.
    pub fn images_path(&self) -> String {
        Path::new(&self.output_path)
            .join("images")
            .to_string_lossy()
            .into_owned()
    }

    /// The follows list, `<output_path>/friends.txt`.
    pub fn friends_path(&self) -> String {
        Path::new(&self.output_path)
            .join("friends.txt")
            .to_string_lossy()
            .into_owned()
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
#
# Two commands share this file:
#   instagrab --fetch-follows  -> page seed_username's Following, write friends.txt (run rarely, e.g. monthly)
#   instagrab                  -> scan everyone in friends.txt, write runs.jsonl + images (run daily)

# CDP endpoint of the long-lived Chrome service.
browser_url = "http://127.0.0.1:9222"

# The one output directory. instagrab writes everything under it — point your UI here:
#   <output_path>/runs.jsonl                     scan output (one JSON line per profile)
#   <output_path>/images/<user>/<shortcode>.jpg  first image per in-window post
#   <output_path>/friends.txt                    the follows list (--fetch-follows writes, scan reads)
output_path = "/var/lib/instagrab"

# Seed profile whose Following `instagrab --fetch-follows` pages (or pass --seed <name>).
# Fetching the list touches Instagram's friendship endpoint, so run it rarely.
seed_username = "your_handle"

# Only keep posts whose timestamp falls within the last N days (0 = no filter).
# With time_window_days > 0, the scan also pages the feed until the oldest
# captured post is older than the window.
time_window_days = 90

# --- Anti-detection / tuning (defaults shown; usually leave these) ---
# Per-run sleep between profiles, in seconds (uniform random in [min, max]).
jitter_min_secs = 30
jitter_max_secs = 75
# Per-profile navigation/wait budget.
nav_timeout_sec = 20
# Safety cap on feed pages per profile.
max_scrolls = 8

# A canary scrape against instagram.com/instagram runs before each scan. Zero
# posts without a logged_out signal emits a "canary_failed" alert and exits 5.
# Always on; no knob.

# Fixed scrape list, used only as a fallback when the follows file is empty.
# Leading @ is allowed and stripped. For the follows-list workflow, leave empty.
# usernames = ["gjtorikian"]
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
    fn output_path_derives_subpaths() {
        let dir = std::env::temp_dir().join("instagrab-config-test");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("od.toml");
        fs::write(&p, "output_path = \"/var/lib/ig\"\n").unwrap();
        let c = load(p.to_str().unwrap()).unwrap();
        assert_eq!(c.jsonl_path(), "/var/lib/ig/runs.jsonl");
        assert_eq!(c.images_path(), "/var/lib/ig/images");
        assert_eq!(c.friends_path(), "/var/lib/ig/friends.txt");
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
