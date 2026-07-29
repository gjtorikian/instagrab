//! Read/write the friends list as a plain, hand-editable text file.
//!
//! One username per line. Blank lines and `#` comments are ignored; a leading
//! `@` and surrounding whitespace are stripped (the same cleaning `config.rs`
//! applies to `usernames`). The `-fetch-follows` command writes it; the daily scan
//! reads it. Plain text (not JSON) so it stays trivially hand-editable.

use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Writes usernames one per line, overwriting any existing file. Creates the
/// parent directory if missing.
pub fn write_friends(path: &str, usernames: &[String]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| anyhow!("friends cache mkdir: {e}"))?;
        }
    }
    let mut body = usernames.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    fs::write(path, body).map_err(|e| anyhow!("friends cache write {path}: {e}"))?;
    Ok(())
}

/// Reads usernames, skipping blank lines and `#` comments, stripping a leading
/// `@` and surrounding whitespace, and de-duplicating while preserving order.
/// A missing file yields an empty list — a fresh install has no cache yet.
pub fn read_friends(path: &str) -> Result<Vec<String>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow!("friends cache read {path}: {e}")),
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = line.strip_prefix('@').unwrap_or(line).trim();
        if name.is_empty() {
            continue;
        }
        if seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> String {
        let dir = std::env::temp_dir().join("instagrab-cache-test");
        fs::create_dir_all(&dir).unwrap();
        dir.join(name).to_string_lossy().into_owned()
    }

    #[test]
    fn round_trip_and_at_stripping() {
        let p = tmp("friends.txt");
        write_friends(&p, &["@a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(read_friends(&p).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn skips_comments_blanks_and_dupes() {
        let p = tmp("friends2.txt");
        fs::write(&p, "# header\n\n@zuck\nzuck\n  plain  \n@\n").unwrap();
        assert_eq!(read_friends(&p).unwrap(), vec!["zuck", "plain"]);
    }

    #[test]
    fn missing_file_is_empty() {
        let p = tmp("does-not-exist.txt");
        let _ = fs::remove_file(&p);
        assert_eq!(read_friends(&p).unwrap(), Vec::<String>::new());
    }
}
