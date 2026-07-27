//! Seed "Following" list fetch.
//!
//! THIS IS THE ONLY FILE that references Instagram's private friendship
//! endpoint. Keeping that URL literal confined here makes "normal runs never
//! touch the endpoint" a single-file `grep` guarantee (see the contract's
//! isolation criterion). Do not write that path literal anywhere else in
//! `src/` — not even in a comment.
//!
//! Like the feed fetch in `scraper.rs`, the in-page fetch returns a JSON
//! string envelope and pages via a `max_id` cursor. The response shape parsed
//! by `parse_following_page` is provisional (see the spec's Open Item) and
//! must be verified against a real capture during the manual fetch-follows test.

use crate::scraper::{eval_string, js_str, FetchEnvelope};
use crate::shutdown;
use anyhow::{anyhow, Result};
use headless_chrome::Tab;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// One page of the seed's Following list.
pub struct FollowingPage {
    pub usernames: Vec<String>,
    pub next_max_id: String,
    pub more_available: bool,
    pub requires_login: bool,
}

/// Result of a full (possibly multi-page) follows fetch.
#[derive(Default)]
pub struct FollowsOutcome {
    pub usernames: Vec<String>,
    pub requires_login: bool,
    pub err: Option<anyhow::Error>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FollowingDoc {
    users: Vec<Value>,
    // IG sends this as a string, occasionally as a number — accept both.
    next_max_id: Value,
    big_list: bool,
    status: String,
    message: String,
    require_login: bool,
    requires_login: bool,
}

/// Parses one Following-endpoint response body. Provisional shape:
/// `{ "users": [ {"username": ".."} ], "next_max_id": "..", "big_list": true,
/// "status": "ok" }`. Missing/renamed keys degrade to an empty page rather than
/// erroring, matching the parser discipline in `parse.rs`.
pub fn parse_following_page(raw: &[u8]) -> Result<FollowingPage> {
    let doc: FollowingDoc = serde_json::from_slice(raw)?;

    let login = doc.require_login
        || doc.requires_login
        || doc.message.to_lowercase().contains("login")
        || (doc.status.eq_ignore_ascii_case("fail") && doc.users.is_empty());
    if login {
        return Ok(FollowingPage {
            usernames: Vec::new(),
            next_max_id: String::new(),
            more_available: false,
            requires_login: true,
        });
    }

    let mut usernames = Vec::new();
    for u in &doc.users {
        if let Some(name) = u.get("username").and_then(Value::as_str) {
            if !name.is_empty() {
                usernames.push(name.to_string());
            }
        }
    }
    let next_max_id = match &doc.next_max_id {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let more_available = !next_max_id.is_empty();
    Ok(FollowingPage {
        usernames,
        next_max_id,
        more_available,
        requires_login: false,
    })
}

/// Pages the seed's Following via the logged-in `tab` until the cursor is
/// exhausted, `max_pages` is reached, or a shutdown is requested. Sleeps a
/// jittered human pace between pages. `page_pause` is a `[min, max)` second
/// range. On HTTP 429 it stops immediately with whatever it has — never
/// retry-hammers (the endpoint is the project's top ban trigger).
pub fn fetch_follows(
    tab: &Tab,
    user_id: &str,
    app_id: &str,
    max_pages: i64,
    page_pause: std::ops::Range<u64>,
) -> FollowsOutcome {
    let mut out = FollowsOutcome::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = String::new();
    let max_pages = if max_pages <= 0 { 20 } else { max_pages };

    for page in 0..max_pages {
        if shutdown::requested() {
            return out;
        }
        if page > 0 {
            sleep_jittered(page_pause.clone());
            if shutdown::requested() {
                return out;
            }
        }

        let expr = format!(
            "{}({}, {}, {})",
            FOLLOWING_FN,
            js_str(user_id),
            js_str(&cursor),
            js_str(app_id)
        );
        let env_json = match eval_string(tab, &expr, true) {
            Ok(s) => s,
            Err(e) => {
                out.err = Some(anyhow!("following_eval_p{page}: {e}"));
                return out;
            }
        };
        let env: FetchEnvelope = match serde_json::from_str(&env_json) {
            Ok(e) => e,
            Err(e) => {
                out.err = Some(anyhow!("following_envelope_p{page}: {e}"));
                return out;
            }
        };
        if env.status == 0 && !env.error.is_empty() {
            out.err = Some(anyhow!("following in-page fetch failed: {}", env.error));
            return out;
        }
        if env.status == 401 || env.status == 403 {
            out.requires_login = true;
            return out;
        }
        if env.status == 429 {
            out.err = Some(anyhow!(
                "following_http_429_p{page} (rate limited; stopping)"
            ));
            return out;
        }
        if env.status >= 400 {
            out.err = Some(anyhow!("following_http_{}_p{}", env.status, page));
            return out;
        }

        let fp = match parse_following_page(env.body.as_bytes()) {
            Ok(f) => f,
            Err(e) => {
                out.err = Some(anyhow!("following_parse_p{page}: {e}"));
                return out;
            }
        };
        if fp.requires_login {
            out.requires_login = true;
            return out;
        }
        for name in fp.usernames {
            if seen.insert(name.clone()) {
                out.usernames.push(name);
            }
        }
        eprintln!("fetching follows... {} so far", out.usernames.len());
        if !fp.more_available || fp.next_max_id.is_empty() {
            return out;
        }
        cursor = fp.next_max_id;
    }
    out
}

/// Interruptible jittered sleep between pages, mirroring `main.rs::sleep_jittered`.
fn sleep_jittered(range: std::ops::Range<u64>) {
    let lo = range.start;
    let hi = range.end.max(lo + 1);
    let secs = lo + fastrand::u64(0..(hi - lo));
    let total = Duration::from_secs(secs);
    let step = Duration::from_millis(200);
    let mut elapsed = Duration::ZERO;
    while elapsed < total {
        if shutdown::requested() {
            return;
        }
        let nap = (total - elapsed).min(step);
        std::thread::sleep(nap);
        elapsed += nap;
    }
}

// The in-page fetch. The friendship-endpoint path literal appears ONLY here.
const FOLLOWING_FN: &str = r#"(async function(uid, maxId, appId){
  try {
    let url = '/api/v1/friendships/' + uid + '/following/?count=50';
    if (maxId) url += '&max_id=' + encodeURIComponent(maxId);
    const r = await fetch(url, {
      headers: {
        'X-IG-App-ID': appId,
        'X-Requested-With': 'XMLHttpRequest',
        'X-ASBD-ID': '129477',
        'Sec-Fetch-Site': 'same-origin'
      },
      credentials: 'include',
      referrerPolicy: 'no-referrer-when-downgrade'
    });
    const text = await r.text();
    return JSON.stringify({status: r.status, url: r.url, body: text});
  } catch (e) {
    return JSON.stringify({status: 0, error: String(e)});
  }
})"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_following_page() {
        let raw = br#"{
          "users": [
            {"username": "alice", "pk": 1, "is_private": false},
            {"username": "bob", "pk": 2, "is_private": true}
          ],
          "big_list": true,
          "next_max_id": "cursor123",
          "status": "ok"
        }"#;
        let fp = parse_following_page(raw).unwrap();
        assert_eq!(fp.usernames, vec!["alice", "bob"]);
        assert_eq!(fp.next_max_id, "cursor123");
        assert!(fp.more_available);
        assert!(!fp.requires_login);
    }

    #[test]
    fn terminal_page_has_no_more() {
        let raw =
            br#"{"users": [{"username": "carol"}], "big_list": false, "next_max_id": "", "status": "ok"}"#;
        let fp = parse_following_page(raw).unwrap();
        assert_eq!(fp.usernames, vec!["carol"]);
        assert!(!fp.more_available);
        assert!(fp.next_max_id.is_empty());
    }

    #[test]
    fn numeric_next_max_id_is_stringified() {
        let raw = br#"{"users": [{"username": "dave"}], "big_list": true, "next_max_id": 42}"#;
        let fp = parse_following_page(raw).unwrap();
        assert_eq!(fp.next_max_id, "42");
        assert!(fp.more_available);
    }

    #[test]
    fn login_shapes_flag_requires_login() {
        for raw in [
            &br#"{"require_login": true}"#[..],
            &br#"{"message": "Please Login to continue", "users": []}"#[..],
            &br#"{"status": "fail", "users": []}"#[..],
        ] {
            let fp = parse_following_page(raw).unwrap();
            assert!(
                fp.requires_login,
                "expected requires_login for {:?}",
                std::str::from_utf8(raw)
            );
            assert!(fp.usernames.is_empty());
        }
    }

    #[test]
    fn empty_users_with_ok_status_is_not_login() {
        let raw = br#"{"users": [], "big_list": false, "next_max_id": "", "status": "ok"}"#;
        let fp = parse_following_page(raw).unwrap();
        assert!(!fp.requires_login);
        assert!(fp.usernames.is_empty());
    }
}
