//! Plain-HTTPS download of display_url, idempotent.

use crate::parse::ScrapeResult;
use crate::shutdown;
use anyhow::{Result, bail};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

pub struct Downloader {
    dir: String,
    agent: ureq::Agent,
}

pub fn new(dir: &str) -> Downloader {
    Downloader {
        dir: dir.to_string(),
        agent: ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build(),
    }
}

impl Downloader {
    /// Downloads display_url for each post in r.recent_posts that has one,
    /// writes <dir>/<username>/<shortcode>.jpg, and sets local_path. Existing
    /// files are kept (idempotent across runs). Per-post errors are appended
    /// to r.errors and don't stop the loop.
    pub fn fill(&self, r: &mut ScrapeResult) {
        if self.dir.is_empty() {
            return;
        }
        let posts = match &mut r.recent_posts {
            Some(p) if !p.is_empty() => p,
            _ => return,
        };
        let udir = Path::new(&self.dir).join(&r.username);
        if let Err(e) = fs::create_dir_all(&udir) {
            r.errors.push(format!("images_mkdir: {e}"));
            return;
        }
        for p in posts.iter_mut() {
            if shutdown::requested() {
                return;
            }
            let display_url = match p.display_url.as_deref() {
                Some(u) if !u.is_empty() => u,
                _ => continue,
            };
            let target = udir.join(format!("{}.jpg", p.shortcode));
            if target.exists() {
                p.local_path = Some(target.to_string_lossy().into_owned());
                continue;
            }
            if let Err(e) = self.fetch_one(display_url, &target) {
                r.errors.push(format!("image {}: {}", p.shortcode, e));
                continue;
            }
            p.local_path = Some(target.to_string_lossy().into_owned());
        }
    }

    fn fetch_one(&self, url: &str, target: &PathBuf) -> Result<()> {
        let resp = match self
            .agent
            .get(url)
            .set("User-Agent", USER_AGENT)
            .set("Referer", "https://www.instagram.com/")
            .set(
                "Accept",
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            )
            .call()
        {
            Ok(resp) => resp,
            Err(ureq::Error::Status(code, resp)) => {
                bail!("http {} {}", code, resp.status_text())
            }
            Err(e) => bail!(e),
        };

        let tmp = target.with_extension("jpg.tmp");
        let mut f = fs::File::create(&tmp)?;
        let mut reader = resp.into_reader();
        if let Err(e) = io::copy(&mut reader, &mut f) {
            drop(f);
            let _ = fs::remove_file(&tmp);
            bail!(e);
        }
        if let Err(e) = f.sync_all() {
            drop(f);
            let _ = fs::remove_file(&tmp);
            bail!(e);
        }
        drop(f);
        fs::rename(&tmp, target)?;
        Ok(())
    }
}
