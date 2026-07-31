//! Flag parsing, orchestration, alert + exit codes, canary.

mod cache;
mod config;
mod follows;
mod images;
mod output;
mod parse;
mod scraper;
mod shutdown;

use chrono::Local;
use output::new_alert;
use parse::ScrapeResult;
use scraper::{ScrapeOpts, Scraper};
use std::process;
use std::time::Duration;

const EXIT_OK: i32 = 0;
const EXIT_CONFIG_ERROR: i32 = 1;
const EXIT_LOGGED_OUT: i32 = 2;
const EXIT_SCHEMA_DRIFT: i32 = 3;
const EXIT_BROWSER_ERROR: i32 = 4;
const EXIT_CANARY_FAILED: i32 = 5;

/// The public account scraped first each run as a control. Zero posts without a
/// logged_out signal means our extraction broke.
const CANARY_USERNAME: &str = "instagram";

struct Flags {
    config_path: String,
    one_shot: String,
    write_sample: String,
    dry_run: bool,
    debug: bool,
    fetch_follows: bool,
    seed: String,
    limit: i64,
}

/// Resolves the scan list: an explicit `--once` username wins; otherwise the
/// follows file (already read into `follows`); if that's empty, the config
/// `usernames` fallback. `limit > 0` caps the list to that many profiles — the
/// caller orders the follows file stalest-first, so a limit scans the oldest N.
/// Pure so it can be unit-tested.
fn scan_list(
    one_shot: &str,
    follows: Vec<String>,
    usernames: &[String],
    limit: i64,
) -> Vec<String> {
    let mut list = if !one_shot.is_empty() {
        vec![one_shot.to_string()]
    } else if !follows.is_empty() {
        follows
    } else {
        usernames.to_vec()
    };
    if limit > 0 && (limit as usize) < list.len() {
        list.truncate(limit as usize);
    }
    list
}

/// The post-loop exit code. Preserves the documented precedence: a logged_out
/// signal (2) outranks schema drift (3), and drift only fires when it hit every
/// successful result this run. Canary/config/browser codes (5/1/4) are decided
/// earlier via direct process::exit and are not routed through here.
fn exit_for(logged_out_seen: bool, drift_count: i64, results_this_run: i64) -> i32 {
    if logged_out_seen {
        return EXIT_LOGGED_OUT;
    }
    if drift_count > 0 && drift_count == results_this_run {
        return EXIT_SCHEMA_DRIFT;
    }
    EXIT_OK
}

fn main() {
    let flags = match parse_flags() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            process::exit(2);
        }
    };

    if !flags.write_sample.is_empty() {
        if let Err(e) = config::write_example(&flags.write_sample) {
            logln(&format!("write sample: {e}"));
            process::exit(EXIT_CONFIG_ERROR);
        }
        println!("wrote {}", flags.write_sample);
        return;
    }

    let cfg = match config::load(&flags.config_path) {
        Ok(c) => c,
        Err(e) => {
            logln(&format!("config: {e}"));
            process::exit(EXIT_CONFIG_ERROR);
        }
    };

    let mut out = match output::open(&cfg.jsonl_path()) {
        Ok(o) => o,
        Err(e) => {
            logln(&format!("open output: {e}"));
            process::exit(EXIT_CONFIG_ERROR);
        }
    };

    shutdown::install();

    // Idle timeout must outlast the biggest gap between CDP calls: the longest
    // per-tab budget or an inter-profile jitter sleep, whichever is larger.
    let per_tab_budget = cfg.nav_timeout_sec + 10 + cfg.max_scrolls * 3;
    let idle_secs = per_tab_budget.max(cfg.jitter_max_secs) + 60;

    let s = match Scraper::new(
        &cfg.browser_url,
        Duration::from_secs(cfg.nav_timeout_sec as u64),
        flags.debug,
        Duration::from_secs(idle_secs as u64),
    ) {
        Ok(s) => s,
        Err(e) => {
            logln(&format!("scraper: {e}"));
            process::exit(EXIT_BROWSER_ERROR);
        }
    };

    if flags.dry_run {
        println!("connected to {} ; dry run, exiting", cfg.browser_url);
        return;
    }

    if flags.fetch_follows {
        run_fetch_follows(&s, &cfg, &flags, &mut out);
        return;
    }

    let follows = if flags.one_shot.is_empty() {
        cache::read_friends(&cfg.friends_path()).unwrap_or_else(|e| {
            logln(&format!("read follows file: {e}"));
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let usernames = scan_list(&flags.one_shot, follows, &cfg.usernames, flags.limit);
    if usernames.is_empty() {
        logln(
            "no profiles to scan (run --fetch-follows to build the follows list, or set usernames)",
        );
        return;
    }

    let mut logged_out_seen = false;
    let mut drift_count = 0;
    let mut results_this_run = 0;

    let image_dl = Some(images::new(&cfg.images_path()));

    let paginate_until: i64 = if cfg.time_window_days > 0 {
        chrono::Utc::now().timestamp() - cfg.time_window_days * 24 * 60 * 60
    } else {
        0
    };

    // Canary first. We don't paginate (we just need any posts at all to
    // confirm extraction is working) and we don't download images for it.
    let mut canary_oc = s.scrape(
        CANARY_USERNAME,
        &ScrapeOpts {
            paginate_until_unix: 0,
            max_scrolls: cfg.max_scrolls,
        },
    );
    if canary_oc.requires_login {
        let mut alert = new_alert("logged_out");
        alert.note = "canary tripped logged_out; re-run SSH-tunnel login bootstrap".to_string();
        alert.username = CANARY_USERNAME.to_string();
        let _ = out.write_json(&alert);
        process::exit(EXIT_LOGGED_OUT);
    }
    let canary_healthy = canary_oc.err.is_none()
        && canary_oc
            .result
            .as_ref()
            .map(|r| r.recent_posts.as_ref().map_or(0, Vec::len) > 0)
            .unwrap_or(false);
    let canary_result = canary_oc.result.take();
    let canary_had_result = canary_result.is_some();
    if let Some(mut result) = canary_result {
        result.source = "canary".to_string();
        let _ = out.write_json(&result);
    }
    if !canary_healthy {
        let mut alert = new_alert("canary_failed");
        alert.username = CANARY_USERNAME.to_string();
        alert.note = if let Some(e) = &canary_oc.err {
            format!("canary scrape errored: {e}")
        } else if !canary_had_result {
            "canary returned no result".to_string()
        } else {
            "canary returned 0 posts — extraction likely broken".to_string()
        };
        let _ = out.write_json(&alert);
        process::exit(EXIT_CANARY_FAILED);
    }

    for (i, u) in usernames.iter().enumerate() {
        if i > 0 {
            sleep_jittered(cfg.jitter_min_secs, cfg.jitter_max_secs);
            if shutdown::requested() {
                break;
            }
        }
        let oc = s.scrape(
            u,
            &ScrapeOpts {
                paginate_until_unix: paginate_until,
                max_scrolls: cfg.max_scrolls,
            },
        );

        if let Some(e) = oc.err {
            let line = ScrapeResult::status_line(u, "error", vec![e.to_string()]);
            let _ = out.write_json(&line);
        } else if oc.requires_login {
            logged_out_seen = true;
            let line =
                ScrapeResult::status_line(u, "logged_out", vec!["requires_login".to_string()]);
            let _ = out.write_json(&line);
        } else if oc.profile_not_found {
            let line =
                ScrapeResult::status_line(u, "not_found", vec!["profile_not_found".to_string()]);
            let _ = out.write_json(&line);
        } else if let Some(mut result) = oc.result {
            if cfg.time_window_days > 0 {
                result.window_days = cfg.time_window_days;
                let before = result.recent_posts.as_ref().map_or(0, Vec::len);
                result.window_maybe_truncated = result.filter_by_window(cfg.time_window_days);
                if flags.debug {
                    let after = result.recent_posts.as_ref().map_or(0, Vec::len);
                    eprintln!(
                        "[debug] {} window_filter days={} before={} after={}",
                        u, cfg.time_window_days, before, after
                    );
                }
            }
            if let Some(dl) = &image_dl {
                dl.fill(&mut result);
            }
            if !oc.missing_schema.is_empty() {
                drift_count += 1;
                result.errors.push("schema_drift_partial".to_string());
            }
            let _ = out.write_json(&result);
            results_this_run += 1;
        }
    }

    if logged_out_seen {
        let mut alert = new_alert("logged_out");
        alert.note = "re-run SSH-tunnel login bootstrap".to_string();
        let _ = out.write_json(&alert);
    }

    // Schema-drift alert: only fire if drift hit on every successful result
    // this run. (One outlier may just be a private/odd account; uniform drift
    // means IG changed shape.)
    if drift_count > 0 && drift_count == results_this_run {
        let mut alert = new_alert("schema_drift");
        alert.note = format!("expected paths missing on {drift_count}/{results_this_run} profiles");
        let _ = out.write_json(&alert);
    }

    let exit_code = exit_for(logged_out_seen, drift_count, results_this_run);
    if exit_code != EXIT_OK {
        process::exit(exit_code);
    }
}

/// The `--fetch-follows` action: page the seed's Following and (re)write the
/// follows file the daily scan reads. Off by default; run it rarely. Exits the
/// process on error/logged_out; returns normally on success (caller then returns
/// from `main`).
fn run_fetch_follows(s: &Scraper, cfg: &config::Config, flags: &Flags, out: &mut output::Writer) {
    let seed = if !flags.seed.is_empty() {
        flags.seed.clone()
    } else {
        cfg.seed_username.clone()
    };
    if seed.is_empty() {
        logln("fetch-follows requires a seed (-seed <name> or config seed_username)");
        process::exit(EXIT_CONFIG_ERROR);
    }

    let oc = s.fetch_follows(&seed, 0);
    if oc.requires_login {
        let mut alert = new_alert("logged_out");
        alert.note =
            "fetch-follows tripped logged_out; re-run SSH-tunnel login bootstrap".to_string();
        alert.username = seed;
        let _ = out.write_json(&alert);
        process::exit(EXIT_LOGGED_OUT);
    }
    // An error with a non-empty list is a partial page (e.g. a 429 mid-
    // pagination) still worth merging — but additively only. The merge deletes
    // names the harvest didn't return, and absence from a truncated list is not
    // evidence of an unfollow.
    let complete = oc.err.is_none();
    if let Some(e) = &oc.err {
        logln(&format!("fetch-follows error: {e}"));
        // No usernames at all means the fetch truly failed.
        if oc.usernames.is_empty() {
            process::exit(EXIT_BROWSER_ERROR);
        }
    }
    let outcome = match cache::merge_friends(&cfg.friends_path(), &oc.usernames, complete) {
        Ok(o) => o,
        Err(e) => {
            logln(&format!("fetch-follows write follows file: {e}"));
            process::exit(EXIT_CONFIG_ERROR);
        }
    };
    // Withheld deletions are the one outcome a downstream reader can't infer
    // from the file itself: the list looks intact but is a merge behind.
    if outcome.deletions_skipped {
        let mut alert = new_alert("follows_partial");
        alert.note = format!(
            "harvest returned {} names; merged additively, deleted nothing",
            oc.usernames.len()
        );
        alert.username = seed;
        let _ = out.write_json(&alert);
    }
    println!(
        "fetched {} follows -> {} ({} added, {} removed, {} excluded, {} scannable)",
        oc.usernames.len(),
        cfg.friends_path(),
        outcome.added,
        outcome.removed,
        outcome.excluded,
        outcome.active
    );
}

/// Sleeps a uniform-random number of seconds in [lo, hi), interruptible by a
/// shutdown signal.
fn sleep_jittered(lo: i64, hi: i64) {
    let hi = if hi <= lo { lo + 1 } else { hi };
    let secs = lo + fastrand::i64(0..(hi - lo));
    let total = Duration::from_secs(secs.max(0) as u64);
    let step = Duration::from_millis(200);
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if shutdown::requested() {
            return;
        }
        let remaining = total - elapsed;
        let nap = remaining.min(step);
        std::thread::sleep(nap);
        elapsed += nap;
    }
}

fn logln(msg: &str) {
    eprintln!("{} {}", Local::now().format("%Y/%m/%d %H:%M:%S"), msg);
}

fn parse_flags() -> Result<Flags, String> {
    let mut flags = Flags {
        config_path: "/etc/instagrab/config.toml".to_string(),
        one_shot: String::new(),
        write_sample: String::new(),
        dry_run: false,
        debug: false,
        fetch_follows: false,
        seed: String::new(),
        limit: 0,
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].clone();
        // GNU-style flags only: --flag and --flag=value.
        let stripped = match arg.strip_prefix("--") {
            Some(s) => s,
            None if arg.starts_with('-') => {
                return Err(format!(
                    "unknown flag: {arg} (flags take two dashes, e.g. --{})\n{}",
                    arg.trim_start_matches('-'),
                    usage()
                ));
            }
            None => return Err(format!("unexpected argument: {arg}\n{}", usage())),
        };
        let (name, inline_val) = match stripped.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (stripped.to_string(), None),
        };

        // A string flag needs a value: use the inline one or consume the next arg.
        let take_value = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = &inline_val {
                return Ok(v.clone());
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("flag needs an argument: --{name}"))
        };

        match name.as_str() {
            "config" => flags.config_path = take_value(&mut i)?,
            "once" => flags.one_shot = take_value(&mut i)?,
            "write-sample-config" => flags.write_sample = take_value(&mut i)?,
            "dry-run" => flags.dry_run = parse_bool_flag(inline_val.as_deref())?,
            "debug" => flags.debug = parse_bool_flag(inline_val.as_deref())?,
            "fetch-follows" => flags.fetch_follows = parse_bool_flag(inline_val.as_deref())?,
            "seed" => flags.seed = take_value(&mut i)?,
            "limit" => {
                let v = take_value(&mut i)?;
                flags.limit = v
                    .parse::<i64>()
                    .map_err(|_| format!("invalid --limit value: {v}"))?;
            }
            "help" | "h" => return Err(usage()),
            other => {
                return Err(format!(
                    "flag provided but not defined: --{other}\n{}",
                    usage()
                ));
            }
        }
        i += 1;
    }
    Ok(flags)
}

fn parse_bool_flag(inline: Option<&str>) -> Result<bool, String> {
    match inline {
        None => Ok(true),
        Some("true") | Some("1") | Some("t") | Some("T") | Some("TRUE") | Some("True") => Ok(true),
        Some("false") | Some("0") | Some("f") | Some("F") | Some("FALSE") | Some("False") => {
            Ok(false)
        }
        Some(v) => Err(format!("invalid boolean value {v:?}")),
    }
}

fn usage() -> String {
    "Usage of instagrab:\n\
     \x20 --config string\n\
     \x20\x20\x20\x20TOML config path (default \"/etc/instagrab/config.toml\")\n\
     \x20 --debug\n\
     \x20\x20\x20\x20log fetch envelope + DOM facts to stderr per username\n\
     \x20 --dry-run\n\
     \x20\x20\x20\x20load config and connect to Chrome but don't scrape\n\
     \x20 --fetch-follows\n\
     \x20\x20\x20\x20page seed_username's Following, (re)write the follows file, then exit\n\
     \x20 --seed string\n\
     \x20\x20\x20\x20seed profile for --fetch-follows (overrides config seed_username)\n\
     \x20 --once string\n\
     \x20\x20\x20\x20scan just this username, ignore the follows list\n\
     \x20 --limit int\n\
     \x20\x20\x20\x20scan at most N profiles from the follows list (0 = no limit)\n\
     \x20 --write-sample-config string\n\
     \x20\x20\x20\x20write an example config to PATH and exit"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_contract_constants() {
        // Documented cron/alerting contract — these values must not drift.
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_CONFIG_ERROR, 1);
        assert_eq!(EXIT_LOGGED_OUT, 2);
        assert_eq!(EXIT_SCHEMA_DRIFT, 3);
        assert_eq!(EXIT_BROWSER_ERROR, 4);
        assert_eq!(EXIT_CANARY_FAILED, 5);
    }

    #[test]
    fn exit_for_precedence() {
        // logged_out (2) outranks schema drift (3).
        assert_eq!(exit_for(true, 3, 3), EXIT_LOGGED_OUT);
        // drift fires only when it hit every successful result this run.
        assert_eq!(exit_for(false, 3, 3), EXIT_SCHEMA_DRIFT);
        assert_eq!(exit_for(false, 2, 3), EXIT_OK);
        assert_eq!(exit_for(false, 0, 0), EXIT_OK);
    }

    #[test]
    fn scan_list_selection() {
        let usernames = vec!["a".to_string(), "b".to_string()];
        let follows = vec!["c".to_string(), "d".to_string(), "e".to_string()];

        // --once wins over everything.
        assert_eq!(
            scan_list("zuck", follows.clone(), &usernames, 0),
            vec!["zuck"]
        );
        // a non-empty follows file is used next.
        assert_eq!(
            scan_list("", follows.clone(), &usernames, 0),
            vec!["c", "d", "e"]
        );
        // an empty follows file falls back to config usernames.
        assert_eq!(scan_list("", Vec::new(), &usernames, 0), vec!["a", "b"]);
        // limit caps the list (stalest-first ordering is the caller's job).
        assert_eq!(
            scan_list("", follows.clone(), &usernames, 2),
            vec!["c", "d"]
        );
        // limit >= len is a no-op.
        assert_eq!(scan_list("", follows, &usernames, 10), vec!["c", "d", "e"]);
    }
}
