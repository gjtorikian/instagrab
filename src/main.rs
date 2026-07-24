//! Flag parsing, orchestration, alert + exit codes, canary.

mod config;
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

    let mut out = match output::open(&cfg.output_path) {
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

    let usernames: Vec<String> = if !flags.one_shot.is_empty() {
        vec![flags.one_shot.clone()]
    } else {
        cfg.usernames.clone()
    };
    if usernames.is_empty() {
        logln("no usernames to scrape");
        return;
    }

    let mut logged_out_seen = false;
    let mut drift_count = 0;
    let mut results_this_run = 0;

    let image_dl = if !cfg.images_dir.is_empty() {
        Some(images::new(&cfg.images_dir))
    } else {
        None
    };

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

    let mut exit_code = EXIT_OK;

    if logged_out_seen {
        let mut alert = new_alert("logged_out");
        alert.note = "re-run SSH-tunnel login bootstrap".to_string();
        let _ = out.write_json(&alert);
        exit_code = EXIT_LOGGED_OUT;
    }

    // Schema-drift alert: only fire if drift hit on every successful result
    // this run. (One outlier may just be a private/odd account; uniform drift
    // means IG changed shape.)
    if drift_count > 0 && drift_count == results_this_run {
        let mut alert = new_alert("schema_drift");
        alert.note = format!("expected paths missing on {drift_count}/{results_this_run} profiles");
        let _ = out.write_json(&alert);
        if exit_code == EXIT_OK {
            exit_code = EXIT_SCHEMA_DRIFT;
        }
    }

    if exit_code != EXIT_OK {
        process::exit(exit_code);
    }
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
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].clone();
        // Accept both -flag and --flag, and -flag=value.
        let stripped = arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .ok_or_else(|| format!("unexpected argument: {arg}"))?;
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
                .ok_or_else(|| format!("flag needs an argument: -{name}"))
        };

        match name.as_str() {
            "config" => flags.config_path = take_value(&mut i)?,
            "once" => flags.one_shot = take_value(&mut i)?,
            "write-sample-config" => flags.write_sample = take_value(&mut i)?,
            "dry-run" => flags.dry_run = parse_bool_flag(inline_val.as_deref())?,
            "debug" => flags.debug = parse_bool_flag(inline_val.as_deref())?,
            "help" | "h" => return Err(usage()),
            other => {
                return Err(format!(
                    "flag provided but not defined: -{other}\n{}",
                    usage()
                ))
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
     \x20 -config string\n\
     \x20\x20\x20\x20TOML config path (default \"/etc/instagrab/config.toml\")\n\
     \x20 -debug\n\
     \x20\x20\x20\x20log fetch envelope + DOM facts to stderr per username\n\
     \x20 -dry-run\n\
     \x20\x20\x20\x20load config and connect to Chrome but don't scrape\n\
     \x20 -once string\n\
     \x20\x20\x20\x20scrape just this username, ignore config.usernames\n\
     \x20 -write-sample-config string\n\
     \x20\x20\x20\x20write an example config to PATH and exit"
        .to_string()
}
