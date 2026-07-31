//! Read/merge the follows list as a plain, hand-editable text file.
//!
//! One username per line. Blank lines and `#` comments are ignored by the scan
//! reader; a leading `@` and surrounding whitespace are stripped (the same
//! cleaning `config.rs` applies to `usernames`). `--fetch-follows` merges into
//! it; the daily scan reads it. Plain text (not JSON) so it stays trivially
//! hand-editable.
//!
//! A commented-out username is an **exclusion**: still followed on Instagram,
//! deliberately not scanned. `merge_friends` carries those comments across a
//! harvest, which is why `--fetch-follows` merges instead of overwriting — the
//! file is the durable record of what to scrape, not a disposable cache. A
//! curating UI can own this file and have its edits survive the next harvest.
//!
//! Heuristic worth knowing: a comment is read as an exclusion when its body is
//! a lone username-shaped token. `# nononancy` is an exclusion; `# porthole
//! manages this file` is prose and is preserved verbatim. A *one-word* prose
//! comment (`# archived`) is indistinguishable from an exclusion and will be
//! dropped once no such account is followed — keep header comments to more than
//! one word.

use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Instagram caps usernames at 30 characters of `[A-Za-z0-9._]`.
const MAX_USERNAME_LEN: usize = 30;

/// One parsed line of the follows file.
#[derive(Debug)]
enum Line {
    /// A lone username, commented out or not. `raw` is kept verbatim so a
    /// surviving entry is written back byte-for-byte — indentation, `@` prefix
    /// and comment spacing included.
    Entry {
        name: String,
        excluded: bool,
        raw: String,
    },
    /// A blank line, or a comment that isn't a username. Always preserved.
    Other(String),
}

/// What a merge did. Feeds the operator-facing summary line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// New follows appended.
    pub added: usize,
    /// Entries dropped because the seed no longer follows them.
    pub removed: usize,
    /// Commented-out entries carried through untouched.
    pub excluded: usize,
    /// Usernames the scan will pick up afterwards.
    pub active: usize,
    /// Deletions were withheld: either the caller flagged the harvest
    /// incomplete, or it tripped the shrink guard.
    pub deletions_skipped: bool,
}

/// Merges a freshly harvested Following list into the follows file, preserving
/// comments, prose and line order.
///
/// Entries already on file are kept verbatim when the harvest still contains
/// them — an exclusion stays an exclusion. Entries absent from the harvest are
/// dropped (the seed unfollowed them). Harvested names not on file are appended.
///
/// `allow_deletes` must be false whenever the harvest may be truncated. Absence
/// from a partial list is not evidence of an unfollow, and this deletes on
/// absence; one rate-limited page would otherwise prune hundreds of people and
/// every exclusion recorded against them.
///
/// A missing file is treated as empty, so a first run simply writes the harvest.
pub fn merge_friends(
    path: &str,
    harvested: &[String],
    allow_deletes: bool,
) -> Result<MergeOutcome> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow!("follows file read {path}: {e}")),
    };

    let lines: Vec<Line> = text.lines().map(parse_line).collect();
    let harvest = clean(harvested);
    let harvest_set: HashSet<&str> = harvest.iter().map(String::as_str).collect();

    // `read_friends` treats a name as scannable when *any* line for it is
    // uncommented, so the de-duplication below has to resolve duplicates the
    // same way — otherwise a merge would silently change who gets scanned.
    let active_names: HashSet<&str> = lines
        .iter()
        .filter_map(|l| match l {
            Line::Entry {
                name,
                excluded: false,
                ..
            } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    let entry_count = lines
        .iter()
        .filter_map(|l| match l {
            Line::Entry { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>()
        .len();

    let deletes = allow_deletes && !shrank_suspiciously(harvest.len(), entry_count);

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + harvest.len());
    let mut emitted: HashSet<String> = HashSet::new();
    let mut outcome = MergeOutcome {
        deletions_skipped: !deletes,
        ..Default::default()
    };

    for line in &lines {
        match line {
            Line::Other(raw) => out.push(raw.clone()),
            Line::Entry {
                name,
                excluded,
                raw,
            } => {
                // Already resolved this name, or this comment is shadowed by an
                // uncommented line elsewhere in the file: drop the duplicate.
                if emitted.contains(name) || (*excluded && active_names.contains(name.as_str())) {
                    continue;
                }
                emitted.insert(name.clone());
                if deletes && !harvest_set.contains(name.as_str()) {
                    outcome.removed += 1;
                    continue;
                }
                out.push(raw.clone());
                if *excluded {
                    outcome.excluded += 1;
                } else {
                    outcome.active += 1;
                }
            }
        }
    }

    for name in &harvest {
        if emitted.insert(name.clone()) {
            out.push(name.clone());
            outcome.added += 1;
            outcome.active += 1;
        }
    }

    write_lines(path, &out)?;
    Ok(outcome)
}

/// Reads usernames, skipping blank lines and `#` comments, stripping a leading
/// `@` and surrounding whitespace, and de-duplicating while preserving order.
/// A missing file yields an empty list — a fresh install has no follows yet.
pub fn read_friends(path: &str) -> Result<Vec<String>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow!("follows file read {path}: {e}")),
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

/// A harvest returning fewer than half the entries already on file is suspect
/// even when the fetch reported no error — pagination can end early without
/// erroring. With delete-on-absence that would silently prune most of the list
/// *and* the exclusions recorded against it. A stale extra name costs one wasted
/// scrape; a wrong delete costs data this file is the only record of.
fn shrank_suspiciously(harvest_len: usize, existing_entries: usize) -> bool {
    harvest_len * 2 < existing_entries
}

/// Classifies a line. Anything that isn't a lone username — blank, prose,
/// a multi-word comment — is `Other` and survives untouched.
fn parse_line(raw: &str) -> Line {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Line::Other(raw.to_string());
    }
    let (body, excluded) = match trimmed.strip_prefix('#') {
        Some(rest) => (rest.trim(), true),
        None => (trimmed, false),
    };
    let name = body.strip_prefix('@').unwrap_or(body).trim();
    if is_username(name) {
        Line::Entry {
            name: name.to_string(),
            excluded,
            raw: raw.to_string(),
        }
    } else {
        Line::Other(raw.to_string())
    }
}

fn is_username(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_USERNAME_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

/// Applies the same cleaning as `read_friends` to a harvested list, dropping
/// anything username-shaped checks reject and de-duplicating in order.
fn clean(usernames: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(usernames.len());
    for u in usernames {
        let name = u.trim();
        let name = name.strip_prefix('@').unwrap_or(name).trim();
        if is_username(name) && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
    out
}

fn write_lines(path: &str, lines: &[String]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| anyhow!("follows file mkdir: {e}"))?;
        }
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    fs::write(path, body).map_err(|e| anyhow!("follows file write {path}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> String {
        let dir = std::env::temp_dir().join("instagrab-cache-test");
        fs::create_dir_all(&dir).unwrap();
        dir.join(name).to_string_lossy().into_owned()
    }

    fn seed(name: &str, body: &str) -> String {
        let p = tmp(name);
        fs::write(&p, body).unwrap();
        p
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn skips_comments_blanks_and_dupes() {
        let p = seed("read.txt", "# header\n\n@zuck\nzuck\n  plain  \n@\n");
        assert_eq!(read_friends(&p).unwrap(), vec!["zuck", "plain"]);
    }

    #[test]
    fn missing_file_is_empty() {
        let p = tmp("does-not-exist.txt");
        let _ = fs::remove_file(&p);
        assert_eq!(read_friends(&p).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn missing_file_writes_the_harvest() {
        let p = tmp("merge-fresh.txt");
        let _ = fs::remove_file(&p);
        let outcome = merge_friends(&p, &names(&["@a", "b"]), true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\n");
        assert_eq!(outcome.added, 2);
        assert_eq!(outcome.removed, 0);
        assert!(!outcome.deletions_skipped);
    }

    /// The whole point of merging: an exclusion set in the UI must survive a
    /// harvest that still sees the account in the seed's Following.
    #[test]
    fn exclusion_survives_a_harvest() {
        let p = seed(
            "merge-exclusion.txt",
            "# porthole manages this file\nanniebannie\n# nononancy\nberryp\n",
        );
        let outcome =
            merge_friends(&p, &names(&["anniebannie", "nononancy", "berryp"]), true).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "# porthole manages this file\nanniebannie\n# nononancy\nberryp\n"
        );
        assert_eq!(outcome.excluded, 1);
        assert_eq!(outcome.active, 2);
        assert_eq!(outcome.added, 0);
        assert_eq!(outcome.removed, 0);
        // And the scan still refuses to pick the excluded name up.
        assert_eq!(read_friends(&p).unwrap(), vec!["anniebannie", "berryp"]);
    }

    #[test]
    fn unfollowed_entries_are_dropped_active_or_excluded() {
        let p = seed(
            "merge-drop.txt",
            "keeper\ngone\n# excluded_gone\n# excluded_kept\n",
        );
        let outcome = merge_friends(&p, &names(&["keeper", "excluded_kept"]), true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "keeper\n# excluded_kept\n");
        assert_eq!(outcome.removed, 2);
        assert_eq!(outcome.excluded, 1);
        assert_eq!(outcome.active, 1);
    }

    #[test]
    fn new_follows_are_appended_in_harvest_order() {
        let p = seed("merge-append.txt", "existing\n");
        let outcome = merge_friends(&p, &names(&["existing", "newer", "newest"]), true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "existing\nnewer\nnewest\n");
        assert_eq!(outcome.added, 2);
    }

    #[test]
    fn prose_blanks_and_formatting_are_preserved_verbatim() {
        let p = seed(
            "merge-prose.txt",
            "# instagrab follows list\n#\n\n  @zuck  \n\n# a note about the next one\nplain\n",
        );
        merge_friends(&p, &names(&["zuck", "plain"]), true).unwrap();
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "# instagrab follows list\n#\n\n  @zuck  \n\n# a note about the next one\nplain\n"
        );
    }

    /// A 429 mid-pagination hands back a truncated list. Absence from it is not
    /// evidence of an unfollow, so nothing may be deleted.
    #[test]
    fn partial_harvest_never_deletes() {
        let p = seed("merge-partial.txt", "a\nb\nc\n# d\n");
        let outcome = merge_friends(&p, &names(&["a", "e"]), false).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\nc\n# d\ne\n");
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.added, 1);
        assert!(outcome.deletions_skipped);
    }

    /// Pagination can end early without erroring, so a "clean" harvest that
    /// shrank by more than half is treated as partial too.
    #[test]
    fn suspicious_shrink_withholds_deletes() {
        let p = seed("merge-shrink.txt", "a\nb\nc\nd\ne\nf\n");
        let outcome = merge_friends(&p, &names(&["a", "b"]), true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\nc\nd\ne\nf\n");
        assert_eq!(outcome.removed, 0);
        assert!(outcome.deletions_skipped);

        // Exactly half is not suspicious; it deletes.
        let p = seed("merge-shrink-half.txt", "a\nb\nc\nd\n");
        let outcome = merge_friends(&p, &names(&["a", "b"]), true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\n");
        assert_eq!(outcome.removed, 2);
        assert!(!outcome.deletions_skipped);
    }

    /// An empty harvest can never be the reason the file empties out.
    #[test]
    fn empty_harvest_keeps_everything() {
        let p = seed("merge-empty.txt", "a\nb\n");
        let outcome = merge_friends(&p, &[], true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\n");
        assert!(outcome.deletions_skipped);
    }

    /// Duplicates have to collapse the way `read_friends` resolves them, or the
    /// merge would quietly change the scan list. An uncommented line anywhere
    /// wins over a comment for the same name.
    #[test]
    fn duplicates_collapse_the_way_read_friends_resolves_them() {
        for body in ["# zuck\nzuck\n", "zuck\n# zuck\n", "zuck\n@zuck\n"] {
            let p = seed("merge-dupe.txt", body);
            let before = read_friends(&p).unwrap();
            let outcome = merge_friends(&p, &names(&["zuck"]), true).unwrap();
            assert_eq!(
                read_friends(&p).unwrap(),
                before,
                "changed scan list: {body:?}"
            );
            assert_eq!(outcome.active, 1, "{body:?}");
            assert_eq!(outcome.excluded, 0, "{body:?}");
        }
    }

    #[test]
    fn merge_is_idempotent() {
        let p = seed("merge-idempotent.txt", "# header note\na\n# b\n");
        merge_friends(&p, &names(&["a", "b", "c"]), true).unwrap();
        let once = fs::read_to_string(&p).unwrap();
        let outcome = merge_friends(&p, &names(&["a", "b", "c"]), true).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), once);
        assert_eq!(outcome.added, 0);
        assert_eq!(outcome.removed, 0);
    }
}
