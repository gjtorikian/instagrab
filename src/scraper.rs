//! CDP attach, per-username tab, active web_profile_info fetch, and the
//! mobile-API user-feed pagination loop.
//!
//! headless_chrome's `evaluate` hardcodes returnByValue=false, so every
//! in-page script here returns a *string* and we parse it out of RemoteObject.value.

use crate::parse::{parse_user_feed, parse_web_profile_info, ScrapeResult};
use crate::shutdown;
use anyhow::{anyhow, Result};
use headless_chrome::{Browser, Tab};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// What we ship with. The page-extracted value (if found) supersedes this on
/// each run.
const FALLBACK_APP_ID: &str = "936619743392459";

pub struct Scraper {
    nav_timeout: Duration,
    debug: bool,
    browser: Browser,
    // dynamically scraped from page; falls back to FALLBACK_APP_ID
    app_id: Mutex<String>,
}

pub struct ScrapeOpts {
    /// When > 0, asks the scraper to page the feed until the oldest captured
    /// post is older than this Unix timestamp. 0 disables pagination — only
    /// the first feed page (beyond web_profile_info) is returned.
    pub paginate_until_unix: i64,
    /// Caps how many feed pages we'll fetch. Default 8.
    pub max_scrolls: i64,
}

#[derive(Default)]
pub struct Outcome {
    pub result: Option<ScrapeResult>,
    pub missing_schema: Vec<String>,
    pub requires_login: bool,
    pub profile_not_found: bool,
    pub err: Option<anyhow::Error>,
}

impl Outcome {
    fn err(e: anyhow::Error) -> Self {
        Outcome {
            err: Some(e),
            ..Default::default()
        }
    }
    fn login() -> Self {
        Outcome {
            requires_login: true,
            ..Default::default()
        }
    }
    fn not_found() -> Self {
        Outcome {
            profile_not_found: true,
            ..Default::default()
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct FetchEnvelope {
    pub(crate) status: i64,
    pub(crate) url: String,
    pub(crate) body: String,
    pub(crate) error: String,
}

impl Scraper {
    /// Resolves the CDP HTTP endpoint to its websocket URL, connects, and
    /// probes the connection. `idle_timeout` must exceed the longest gap
    /// between CDP calls (inter-profile jitter sleeps + per-tab budget) or the
    /// connection is dropped mid-run.
    pub fn new(
        browser_url: &str,
        nav_timeout: Duration,
        debug: bool,
        idle_timeout: Duration,
    ) -> Result<Scraper> {
        let ws_url =
            resolve_ws_url(browser_url).map_err(|e| anyhow!("connect {browser_url}: {e}"))?;
        let browser = Browser::connect_with_timeout(ws_url, idle_timeout)
            .map_err(|e| anyhow!("connect {browser_url}: {e}"))?;
        // Probe: fetching the version implicitly happened during resolve; a
        // successful connect_with_timeout means the CDP handshake worked.
        Ok(Scraper {
            nav_timeout,
            debug,
            browser,
            app_id: Mutex::new(FALLBACK_APP_ID.to_string()),
        })
    }

    fn current_app_id(&self) -> String {
        self.app_id.lock().unwrap().clone()
    }

    fn set_app_id(&self, id: String) {
        *self.app_id.lock().unwrap() = id;
    }

    /// Scans the loaded page for an X-IG-App-ID candidate and updates the
    /// cached value if found. Best-effort; failures silently fall back to the
    /// cached/hardcoded value.
    fn extract_app_id(&self, tab: &Tab) {
        let html = match eval_string(tab, APP_ID_DUMP_SCRIPT, false) {
            Ok(h) if !h.is_empty() => h,
            _ => return,
        };
        for re in app_id_patterns() {
            if let Some(caps) = re.captures(&html) {
                let id = caps.get(1).unwrap().as_str().to_string();
                if id != self.current_app_id() {
                    if self.debug {
                        eprintln!(
                            "[debug] app_id_extracted old={} new={}",
                            self.current_app_id(),
                            id
                        );
                    }
                    self.set_app_id(id);
                }
                return;
            }
        }
        if self.debug {
            eprintln!(
                "[debug] app_id_extract no_match using={}",
                self.current_app_id()
            );
        }
    }

    pub fn scrape(&self, username: &str, opts: &ScrapeOpts) -> Outcome {
        let tab = match self.browser.new_tab() {
            Ok(t) => t,
            Err(e) => return Outcome::err(anyhow!("navigate: {e}")),
        };
        let outcome = self.scrape_in_tab(&tab, username, opts);
        let _ = tab.close(false);
        outcome
    }

    /// Fetches the seed's Following usernames. Resolves the seed's numeric
    /// user_id and the current app_id via the same profile fetch the scrape
    /// path uses, then delegates paging to `crate::follows` (the only caller of
    /// the private endpoint). Invoked only by the `-fetch-follows` command — never on
    /// the normal scrape path.
    pub fn fetch_follows(&self, seed: &str, max_pages: i64) -> crate::follows::FollowsOutcome {
        use crate::follows::FollowsOutcome;
        let tab = match self.browser.new_tab() {
            Ok(t) => t,
            Err(e) => {
                return FollowsOutcome {
                    err: Some(anyhow!("navigate: {e}")),
                    ..Default::default()
                }
            }
        };
        let outcome = self.fetch_follows_in_tab(&tab, seed, max_pages);
        let _ = tab.close(false);
        outcome
    }

    fn fetch_follows_in_tab(
        &self,
        tab: &Tab,
        seed: &str,
        max_pages: i64,
    ) -> crate::follows::FollowsOutcome {
        use crate::follows::FollowsOutcome;
        tab.set_default_timeout(self.nav_timeout);

        let profile_url = format!("https://www.instagram.com/{seed}/");
        let navigate = || -> Result<()> {
            tab.navigate_to(&profile_url)?;
            tab.wait_until_navigated()?;
            tab.wait_for_element("body")?;
            Ok(())
        };
        if let Err(e) = navigate() {
            return FollowsOutcome {
                err: Some(anyhow!("navigate: {e}")),
                ..Default::default()
            };
        }
        if let Ok(dom) = read_dom_facts(tab) {
            if dom.is_login_page {
                return FollowsOutcome {
                    requires_login: true,
                    ..Default::default()
                };
            }
            if dom.not_found {
                return FollowsOutcome {
                    err: Some(anyhow!("seed profile not found: {seed}")),
                    ..Default::default()
                };
            }
        }

        self.extract_app_id(tab);
        let expr = format!(
            "{}({}, {})",
            FETCH_FN,
            js_str(seed),
            js_str(&self.current_app_id())
        );
        let env_json = match eval_string(tab, &expr, true) {
            Ok(s) => s,
            Err(e) => {
                return FollowsOutcome {
                    err: Some(anyhow!("fetch eval: {e}")),
                    ..Default::default()
                }
            }
        };
        let env: FetchEnvelope = match serde_json::from_str(&env_json) {
            Ok(e) => e,
            Err(e) => {
                return FollowsOutcome {
                    err: Some(anyhow!("fetch envelope: {e}")),
                    ..Default::default()
                }
            }
        };
        if env.status == 401 || env.status == 403 {
            return FollowsOutcome {
                requires_login: true,
                ..Default::default()
            };
        }
        if env.status >= 400 {
            return FollowsOutcome {
                err: Some(anyhow!("web_profile_info http {}", env.status)),
                ..Default::default()
            };
        }

        let parsed = match parse_web_profile_info(seed, env.body.as_bytes()) {
            Ok(p) => p,
            Err(e) => {
                return FollowsOutcome {
                    err: Some(anyhow!("parse: {e}")),
                    ..Default::default()
                }
            }
        };
        if parsed.requires_login {
            return FollowsOutcome {
                requires_login: true,
                ..Default::default()
            };
        }
        let user_id = parsed.result.map(|r| r.user_id).unwrap_or_default();
        if user_id.is_empty() {
            return FollowsOutcome {
                err: Some(anyhow!("no user_id for seed {seed}")),
                ..Default::default()
            };
        }

        // Human-paced paging: 4-8s between pages, capped by max_pages.
        crate::follows::fetch_follows(tab, &user_id, &self.current_app_id(), max_pages, 4..9)
    }

    fn scrape_in_tab(&self, tab: &Tab, username: &str, opts: &ScrapeOpts) -> Outcome {
        tab.set_default_timeout(self.nav_timeout);

        let profile_url = format!("https://www.instagram.com/{username}/");
        let navigate = || -> Result<()> {
            tab.navigate_to(&profile_url)?;
            tab.wait_until_navigated()?;
            tab.wait_for_element("body")?;
            Ok(())
        };
        if let Err(e) = navigate() {
            return Outcome::err(anyhow!("navigate: {e}"));
        }

        // Detect login redirect / not-found before doing the fetch.
        if let Ok(dom) = read_dom_facts(tab) {
            if dom.is_login_page {
                return Outcome::login();
            }
            if dom.not_found {
                return Outcome::not_found();
            }
        }

        // Pull the latest X-IG-App-ID off the rendered page, then fetch.
        self.extract_app_id(tab);
        let expr = format!(
            "{}({}, {})",
            FETCH_FN,
            js_str(username),
            js_str(&self.current_app_id())
        );
        let env_json = match eval_string(tab, &expr, true) {
            Ok(s) => s,
            Err(e) => return Outcome::err(anyhow!("fetch eval: {e}")),
        };
        let env: FetchEnvelope = match serde_json::from_str(&env_json) {
            Ok(e) => e,
            Err(e) => return Outcome::err(anyhow!("fetch envelope: {e}")),
        };

        if self.debug {
            eprintln!(
                "[debug] {} status={} url={} body_len={}",
                username,
                env.status,
                env.url,
                env.body.len()
            );
            if !env.error.is_empty() {
                eprintln!("[debug] {} fetch_error={}", username, env.error);
            }
            if env.status == 200 && !env.body.is_empty() {
                let snippet: String = env.body.chars().take(800).collect();
                eprintln!("[debug] {username} body_head={snippet}");
            }
        }

        if env.status == 0 && !env.error.is_empty() {
            return Outcome::err(anyhow!("in-page fetch failed: {}", env.error));
        }
        if env.status == 404 {
            return Outcome::not_found();
        }
        if env.status == 401 || env.status == 403 {
            return Outcome::login();
        }
        if env.status >= 400 {
            return Outcome::err(anyhow!("web_profile_info http {}", env.status));
        }

        let parsed = match parse_web_profile_info(username, env.body.as_bytes()) {
            Ok(p) => p,
            Err(e) => return Outcome::err(anyhow!("parse: {e}")),
        };
        if parsed.requires_login {
            return Outcome {
                requires_login: true,
                missing_schema: parsed.missing,
                ..Default::default()
            };
        }

        let mut result = parsed.result.expect("non-login parse yields a result");

        // Posts come from the mobile-API feed endpoint — IG no longer renders
        // them inline, and the React shell doesn't reliably trigger grid XHRs.
        if !result.user_id.is_empty() {
            self.fill_feed(tab, &mut result, opts);
        } else if self.debug {
            eprintln!("[debug] {username} no user_id, skipping feed fetch");
        }

        result.scraped_at = chrono::Utc::now();
        Outcome {
            result: Some(result),
            missing_schema: parsed.missing,
            ..Default::default()
        }
    }

    /// Actively pages the user-feed endpoint until either the time window is
    /// satisfied, IG says no more, or the safety cap is hit.
    fn fill_feed(&self, tab: &Tab, r: &mut ScrapeResult, opts: &ScrapeOpts) {
        let max_pages = if opts.max_scrolls <= 0 {
            8
        } else {
            opts.max_scrolls
        };

        let mut cursor = String::new();
        let mut seen: std::collections::HashSet<String> = r
            .recent_posts
            .iter()
            .flatten()
            .map(|p| p.shortcode.clone())
            .collect();

        for page in 0..max_pages {
            if shutdown::requested() {
                return;
            }
            let expr = format!(
                "{}({}, {}, {})",
                FEED_FN,
                js_str(&r.user_id),
                js_str(&cursor),
                js_str(&self.current_app_id())
            );
            let env_json = match eval_string(tab, &expr, true) {
                Ok(s) => s,
                Err(e) => {
                    r.errors.push(format!("feed_eval_p{page}: {e}"));
                    return;
                }
            };
            let env: FetchEnvelope = match serde_json::from_str(&env_json) {
                Ok(e) => e,
                Err(e) => {
                    r.errors.push(format!("feed_envelope_p{page}: {e}"));
                    return;
                }
            };
            if self.debug {
                eprintln!(
                    "[debug] feed page={} uid={} status={} body_len={} cursor_in={:?}",
                    page,
                    r.user_id,
                    env.status,
                    env.body.len(),
                    cursor
                );
                if env.status == 200 && !env.body.is_empty() {
                    let code_present = env.body.contains("\"code\":");
                    eprintln!("[debug] feed page={page} body_contains_code_field={code_present}");
                }
            }
            if env.status >= 400 {
                r.errors.push(format!("feed_http_{}_p{}", env.status, page));
                return;
            }
            let feed = match parse_user_feed(env.body.as_bytes()) {
                Ok(f) => f,
                Err(e) => {
                    r.errors.push(format!("feed_parse_p{page}: {e}"));
                    return;
                }
            };
            if self.debug {
                eprintln!(
                    "[debug] feed page={} items_in_response={} posts_extracted={} more={} next={:?}",
                    page,
                    feed.raw_count,
                    feed.posts.len(),
                    feed.more_available,
                    feed.next_max_id
                );
            }
            let mut oldest_this_page: i64 = -1;
            for p in feed.posts {
                if seen.contains(&p.shortcode) {
                    continue;
                }
                seen.insert(p.shortcode.clone());
                if let Some(ts) = p.taken_at_unix {
                    if oldest_this_page < 0 || ts < oldest_this_page {
                        oldest_this_page = ts;
                    }
                }
                r.push_post(p);
            }
            if !feed.more_available || feed.next_max_id.is_empty() {
                return;
            }
            cursor = feed.next_max_id;
            if opts.paginate_until_unix > 0
                && oldest_this_page > 0
                && oldest_this_page <= opts.paginate_until_unix
            {
                return;
            }
            // No window asked for — first page is enough.
            if opts.paginate_until_unix == 0 {
                return;
            }
        }
    }
}

struct DomRead {
    is_login_page: bool,
    not_found: bool,
}

fn read_dom_facts(tab: &Tab) -> Result<DomRead> {
    let json = eval_string(tab, DOM_FACTS_SCRIPT, false)?;
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Out {
        url: String,
        body: String,
    }
    let out: Out = serde_json::from_str(&json)?;
    Ok(DomRead {
        is_login_page: out.url.contains("/accounts/login/"),
        not_found: out.body.contains("Sorry, this page isn")
            || out.body.contains("page isn't available"),
    })
}

// --- CDP eval plumbing -------------------------------------------------------

/// Evaluates an expression and returns its string result. Non-string / absent
/// values come back as ""
pub(crate) fn eval_string(tab: &Tab, expr: &str, await_promise: bool) -> Result<String> {
    let obj = tab.evaluate(expr, await_promise)?;
    Ok(obj
        .value
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        })
        .unwrap_or_default())
}

/// Quotes a string as a JavaScript/JSON string literal.
pub(crate) fn js_str(s: &str) -> String {
    serde_json::to_string(s).expect("string always serializes")
}

/// Resolves an `http://host:port` CDP endpoint to its browser websocket URL
/// via `/json/version`, mirroring chromedp's RemoteAllocator.
fn resolve_ws_url(browser_url: &str) -> Result<String> {
    let base = browser_url.trim_end_matches('/');
    let version_url = format!("{base}/json/version");
    let text = ureq::get(&version_url)
        .timeout(Duration::from_secs(10))
        .call()?
        .into_string()?;
    let body: Value = serde_json::from_str(&text)?;
    body.get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("no webSocketDebuggerUrl in {version_url}"))
}

fn app_id_patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            r#""appId"\s*:\s*"([0-9]{10,20})""#,
            r#""X-IG-App-ID"\s*:\s*"([0-9]{10,20})""#,
            r#"X-IG-App-ID['":\s]+([0-9]{10,20})"#,
            r#""app_id"\s*:\s*"([0-9]{10,20})""#,
        ]
        .iter()
        .map(|p| Regex::new(p).expect("static regex compiles"))
        .collect()
    })
}

const APP_ID_DUMP_SCRIPT: &str = r#"(function(){
  return document.documentElement ? document.documentElement.outerHTML : '';
})()"#;

// Returns a JSON string envelope (status/url/body or status:0/error) so we can
// distinguish HTTP errors from JSON. Invoked as FETCH_FN(username, appId).
const FETCH_FN: &str = r#"(async function(u, appId){
  try {
    const r = await fetch('/api/v1/users/web_profile_info/?username=' + encodeURIComponent(u), {
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

// Mobile-API user-feed endpoint. Cursor pagination via max_id; count=33
// matches what IG's iOS app sends. Invoked as FEED_FN(uid, maxId, appId).
const FEED_FN: &str = r#"(async function(uid, maxId, appId){
  try {
    let url = '/api/v1/feed/user/' + uid + '/?count=33';
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

const DOM_FACTS_SCRIPT: &str = r#"(function(){
  return JSON.stringify({
    url: location.href,
    body: (document.body && document.body.innerText) ? document.body.innerText.slice(0, 2000) : ''
  });
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_str_quotes_like_go_percent_q() {
        assert_eq!(js_str("zuck"), "\"zuck\"");
        assert_eq!(js_str(""), "\"\"");
        assert_eq!(js_str("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn app_id_patterns_match_expected_shapes() {
        let pats = app_id_patterns();
        let cases = [
            (r#"{"appId":"936619743392459"}"#, "936619743392459"),
            (r#"{"X-IG-App-ID":"123456789012"}"#, "123456789012"),
            (r#"X-IG-App-ID: 936619743392459"#, "936619743392459"),
            (r#"{"app_id":"1217981644879628"}"#, "1217981644879628"),
        ];
        for (input, want) in cases {
            let got = pats
                .iter()
                .find_map(|re| re.captures(input))
                .map(|c| c.get(1).unwrap().as_str().to_string());
            assert_eq!(got.as_deref(), Some(want), "input {input}");
        }
    }
}
