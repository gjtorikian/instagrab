//! CDP attach, per-username tab, GraphQL query capture/replay, and the
//! profile-posts pagination loop.
//!
//! headless_chrome's `evaluate` hardcodes returnByValue=false, so every
//! in-page script here returns a *string* and we parse it out of RemoteObject.value.
//!
//! IG serves its profile data from persisted GraphQL queries whose `doc_id`
//! rotates with each web deploy. Rather than pin one, we install a `fetch`
//! recorder before navigation, let the app issue its own queries, and replay
//! them with our own variables. See `RECORDER_SCRIPT`.

use crate::parse::{ScrapeResult, parse_graphql_feed, parse_graphql_profile, user_id_from_feed};
use crate::shutdown;
use anyhow::{Result, anyhow};
use headless_chrome::Browser;
use headless_chrome::Tab;
use headless_chrome::protocol::cdp::Page::AddScriptToEvaluateOnNewDocument;
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
    /// GraphQL queries observed from IG's own web app, refreshed on every
    /// profile load. Carried across profiles so a page that happens not to
    /// re-issue a query can still be scraped with the previous capture.
    queries: Mutex<QueryCatalog>,
}

/// A persisted GraphQL query as IG's web app issued it. `variables` is kept as
/// raw JSON text so we can edit only the fields we care about and leave the
/// rest — including the `__relay_internal__pv__*` provider flags — untouched.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CapturedQuery {
    pub friendly_name: String,
    pub doc_id: String,
    pub variables: String,
    pub fb_dtsg: String,
    pub lsd: String,
    pub jazoest: String,
    /// The request headers IG's own client sent. Replayed verbatim rather than
    /// hand-rebuilt: the first attempt omitted `X-CSRFToken` and every replay
    /// came back 403 with an HTML error shell. Mirroring what the app sends
    /// means the next header IG starts requiring costs nothing.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl CapturedQuery {
    fn is_usable(&self) -> bool {
        !self.doc_id.is_empty() && !self.variables.is_empty()
    }

    /// True when the captured variables drive a paginated connection — the
    /// posts tab. Keyed on Relay's `first`/`after` rather than the friendly
    /// name, which IG renames more often than it changes pagination style.
    fn is_paginated(&self) -> bool {
        serde_json::from_str::<Value>(&self.variables)
            .ok()
            .and_then(|v| v.get("first").map(|f| !f.is_null()))
            .unwrap_or(false)
    }

    fn keys_on_username(&self) -> bool {
        serde_json::from_str::<Value>(&self.variables)
            .ok()
            .and_then(|v| v.get("username").map(|u| u.is_string()))
            .unwrap_or(false)
    }

    /// Rewrites `username` and the pagination cursor, preserving every other
    /// variable the app sent.
    fn variables_for(&self, username: &str, after: &str) -> String {
        let mut v: Value = serde_json::from_str(&self.variables)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
        if let Some(map) = v.as_object_mut() {
            map.insert("username".into(), Value::String(username.to_string()));
            if map.contains_key("after") || !after.is_empty() {
                map.insert(
                    "after".into(),
                    if after.is_empty() {
                        Value::Null
                    } else {
                        Value::String(after.to_string())
                    },
                );
            }
        }
        v.to_string()
    }
}

#[derive(Default)]
struct QueryCatalog {
    posts: Option<CapturedQuery>,
    profile: Option<CapturedQuery>,
    /// Latest CSRF tokens seen on *any* captured query. These are per-session,
    /// not per-query, so a query reused from an earlier page load must be
    /// replayed with the freshest tokens rather than the ones it arrived with.
    tokens: Tokens,
}

#[derive(Clone, Debug, Default)]
struct Tokens {
    fb_dtsg: String,
    lsd: String,
    jazoest: String,
    /// Freshest request headers seen, for the same per-session reason.
    headers: std::collections::BTreeMap<String, String>,
}

pub struct ScrapeOpts {
    /// When > 0, asks the scraper to page the feed until the oldest captured
    /// post is older than this Unix timestamp. 0 disables pagination — only
    /// the first page of the posts connection is returned.
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
            queries: Mutex::new(QueryCatalog::default()),
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
    fn extract_app_id(&self, html: &str) {
        if html.is_empty() {
            return;
        }
        for re in app_id_patterns() {
            if let Some(caps) = re.captures(html) {
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

    /// Installs the fetch/XHR recorder so it runs before any page script on
    /// the *next* navigation. Must be called before `navigate_to`.
    fn install_recorder(&self, tab: &Tab) -> Result<()> {
        tab.call_method(AddScriptToEvaluateOnNewDocument {
            source: RECORDER_SCRIPT.to_string(),
            world_name: None,
            include_command_line_api: None,
            run_immediately: Some(true),
        })
        .map(|_| ())
        .map_err(|e| anyhow!("install recorder: {e}"))
    }

    /// Reads whatever GraphQL queries the page issued and folds them into the
    /// catalog. Best-effort: a page that issued nothing leaves the previous
    /// capture in place.
    fn harvest_queries(&self, tab: &Tab) {
        let json = match eval_string(tab, READ_RECORDER_SCRIPT, false) {
            Ok(j) if !j.is_empty() => j,
            _ => return,
        };
        let seen: Vec<CapturedQuery> = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(e) => {
                if self.debug {
                    eprintln!("[debug] recorder decode failed: {e}");
                }
                return;
            }
        };

        let mut cat = self.queries.lock().unwrap();
        let mut found_posts = false;
        let mut found_profile = false;
        for q in seen.into_iter().filter(|q| q.is_usable()) {
            // Tokens are worth taking from any query, including ones we don't
            // otherwise classify.
            if !q.fb_dtsg.is_empty() {
                cat.tokens.fb_dtsg = q.fb_dtsg.clone();
            }
            if !q.lsd.is_empty() {
                cat.tokens.lsd = q.lsd.clone();
            }
            if !q.jazoest.is_empty() {
                cat.tokens.jazoest = q.jazoest.clone();
            }
            if !q.headers.is_empty() {
                cat.tokens.headers = q.headers.clone();
            }
            if !q.keys_on_username() {
                continue;
            }
            if q.is_paginated() {
                found_posts = true;
                cat.posts = Some(q);
            } else {
                found_profile = true;
                cat.profile = Some(q);
            }
        }
        if self.debug {
            eprintln!(
                "[debug] harvest posts_query={} profile_query={} (refreshed posts={} profile={})",
                cat.posts.as_ref().map(|q| q.doc_id.as_str()).unwrap_or("-"),
                cat.profile
                    .as_ref()
                    .map(|q| q.doc_id.as_str())
                    .unwrap_or("-"),
                found_posts,
                found_profile
            );
        }
    }

    fn posts_query(&self) -> Option<CapturedQuery> {
        self.queries.lock().unwrap().posts.clone()
    }

    fn profile_query(&self) -> Option<CapturedQuery> {
        self.queries.lock().unwrap().profile.clone()
    }

    /// Replays a captured query with our own variables and the freshest CSRF
    /// tokens this session has seen.
    fn issue_query(&self, tab: &Tab, q: &CapturedQuery, variables: &str) -> Result<FetchEnvelope> {
        let t = self.queries.lock().unwrap().tokens.clone();
        let headers = serde_json::to_string(&t.headers).unwrap_or_else(|_| "{}".to_string());
        let expr = format!(
            "{}({}, {}, {}, {}, {}, {}, {}, {})",
            GRAPHQL_FN,
            js_str(&q.doc_id),
            js_str(variables),
            js_str(&t.fb_dtsg),
            js_str(&t.lsd),
            js_str(&t.jazoest),
            js_str(&q.friendly_name),
            js_str(&self.current_app_id()),
            js_str(&headers),
        );
        let json = eval_string(tab, &expr, true).map_err(|e| anyhow!("graphql eval: {e}"))?;
        serde_json::from_str(&json).map_err(|e| anyhow!("graphql envelope: {e}"))
    }

    /// Loads a profile and reports the GraphQL queries its page fired, without
    /// scraping anything. Backs `--capture-queries`.
    pub fn capture_queries(&self, username: &str) -> Result<Vec<CapturedQuery>> {
        let tab = self
            .browser
            .new_tab()
            .map_err(|e| anyhow!("new tab: {e}"))?;
        tab.set_default_timeout(self.nav_timeout);
        let out = (|| -> Result<Vec<CapturedQuery>> {
            self.install_recorder(&tab)?;
            tab.navigate_to(&format!("https://www.instagram.com/{username}/"))?;
            tab.wait_until_navigated()?;
            tab.wait_for_element("body")?;
            if let Ok(dom) = read_dom_facts(&tab) {
                if dom.is_login_page {
                    return Err(anyhow!("logged out: re-run the SSH-tunnel login bootstrap"));
                }
            }
            settle(&tab);
            let json = eval_string(&tab, READ_RECORDER_SCRIPT, false)?;
            Ok(serde_json::from_str(&json).unwrap_or_default())
        })();
        let _ = tab.close(false);
        out
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
                };
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
        let fail = |e: anyhow::Error| FollowsOutcome {
            err: Some(e),
            ..Default::default()
        };
        tab.set_default_timeout(self.nav_timeout);

        if let Err(e) = self.install_recorder(tab) {
            return fail(e);
        }

        let profile_url = format!("https://www.instagram.com/{seed}/");
        let navigate = || -> Result<()> {
            tab.navigate_to(&profile_url)?;
            tab.wait_until_navigated()?;
            tab.wait_for_element("body")?;
            Ok(())
        };
        if let Err(e) = navigate() {
            return fail(anyhow!("navigate: {e}"));
        }
        if let Ok(dom) = read_dom_facts(tab) {
            if dom.is_login_page {
                return FollowsOutcome {
                    requires_login: true,
                    ..Default::default()
                };
            }
            if dom.not_found {
                return fail(anyhow!("seed profile not found: {seed}"));
            }
        }

        let html = page_html(tab);
        self.extract_app_id(&html);
        settle(tab);
        self.harvest_queries(tab);

        // The friendships endpoint is keyed on the numeric id, so we still
        // need one — but it now comes from the GraphQL profile rather than
        // web_profile_info.
        let user_id = match self.resolve_user_id(tab, seed) {
            Ok(id) => id,
            Err(e) => return fail(e),
        };

        // Human-paced paging: 4-8s between pages, capped by max_pages.
        crate::follows::fetch_follows(tab, &user_id, &self.current_app_id(), max_pages, 4..9)
    }

    /// Resolves a username to its numeric id via the captured profile query,
    /// falling back to the `user.pk` the posts feed embeds on every node.
    fn resolve_user_id(&self, tab: &Tab, username: &str) -> Result<String> {
        if let Some(q) = self.profile_query() {
            let env = self.issue_query(tab, &q, &q.variables_for(username, ""))?;
            if env.status < 400 {
                if let Ok(p) = parse_graphql_profile(username, env.body.as_bytes()) {
                    if let Some(id) = p.result.map(|r| r.user_id).filter(|s| !s.is_empty()) {
                        return Ok(id);
                    }
                }
            }
        }
        if let Some(q) = self.posts_query() {
            let env = self.issue_query(tab, &q, &q.variables_for(username, ""))?;
            if env.status < 400 {
                if let Some(id) = user_id_from_feed(env.body.as_bytes(), username) {
                    return Ok(id);
                }
            }
        }
        Err(anyhow!("no user_id for seed {username}"))
    }

    fn scrape_in_tab(&self, tab: &Tab, username: &str, opts: &ScrapeOpts) -> Outcome {
        tab.set_default_timeout(self.nav_timeout);

        // Recorder must be installed before navigation so it wraps fetch ahead
        // of the app's own bootstrap queries.
        if let Err(e) = self.install_recorder(tab) {
            return Outcome::err(e);
        }

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

        let html = page_html(tab);
        self.extract_app_id(&html);
        // Relay fires its queries after first paint; give them a moment.
        settle(tab);
        self.harvest_queries(tab);

        let posts_q = self.posts_query();
        let profile_q = self.profile_query();
        if posts_q.is_none() && profile_q.is_none() {
            return Outcome::err(anyhow!(
                "no GraphQL queries captured from profile page — IG's client changed; \
                 run --capture-queries to inspect"
            ));
        }

        let mut missing_schema = Vec::new();

        // Profile metadata comes from the page's server-rendered meta tags.
        // The captured "profile" query turns out to return the posts
        // connection with no header in it, so issuing it buys nothing — and an
        // extra request per profile is exactly the cost this project is
        // careful about. It stays as a fallback for when the tags come up
        // empty, and would start contributing again if IG ever puts a header
        // behind it.
        let meta = crate::parse::profile_fields_from_html(username, &html);
        if self.debug {
            eprintln!("[debug] {username} meta {meta:?}");
        }

        let mut result = ScrapeResult::new(username, "graphql");
        result.full_name = meta.full_name;
        result.followers = meta.followers;
        result.following = meta.following;
        result.posts = meta.posts;
        result.is_private = meta.is_private;
        result.profile_pic_url = meta.profile_pic_url;

        if result.followers.is_none() && result.posts.is_none() {
            match &profile_q {
                Some(q) => match self.issue_query(tab, q, &q.variables_for(username, "")) {
                    Ok(env) => {
                        if self.debug {
                            eprintln!(
                                "[debug] {username} profile doc_id={} status={} body_len={}",
                                q.doc_id,
                                env.status,
                                env.body.len()
                            );
                        }
                        if env.status == 404 {
                            return Outcome::not_found();
                        }
                        // Deliberately NOT mapped to logged_out. The navigation
                        // above already cleared the login redirect, so a 401/403
                        // here means IG rejected *our replay* — a missing header
                        // or a rotated token — not that the cookie jar died.
                        // Calling it logged_out sends whoever is on call to redo
                        // the SSH login bootstrap for nothing, and exit 2 is that
                        // contract.
                        if env.status >= 400 {
                            return Outcome::err(anyhow!(
                                "profile query rejected: http {} (replay refused, session is live)",
                                env.status
                            ));
                        }
                        match parse_graphql_profile(username, env.body.as_bytes()) {
                            Ok(p) => {
                                if p.requires_login {
                                    return Outcome {
                                        requires_login: true,
                                        missing_schema: p.missing,
                                        ..Default::default()
                                    };
                                }
                                missing_schema = p.missing;
                                if let Some(r) = p.result {
                                    result.user_id = r.user_id;
                                    result.full_name = r.full_name;
                                    result.biography = r.biography;
                                    result.is_private = r.is_private.or(result.is_private);
                                    result.followers = r.followers;
                                    result.following = r.following;
                                    result.posts = r.posts;
                                    if result.profile_pic_url.is_none() {
                                        result.profile_pic_url = r.profile_pic_url;
                                    }
                                }
                            }
                            Err(e) => result.errors.push(format!("profile_parse: {e}")),
                        }
                    }
                    Err(e) => result.errors.push(format!("profile_query: {e}")),
                },
                None => result.errors.push("profile_query_not_captured".to_string()),
            }
        }

        if result.followers.is_none() && result.posts.is_none() {
            result.errors.push("profile_fields_unavailable".to_string());
        }

        match posts_q {
            Some(q) => self.fill_feed(tab, &q, &mut result, opts),
            None => result.errors.push("posts_query_not_captured".to_string()),
        }

        result.scraped_at = chrono::Utc::now();
        Outcome {
            result: Some(result),
            missing_schema,
            ..Default::default()
        }
    }

    /// Pages the profile-posts connection until either the time window is
    /// satisfied, IG says there's no next page, or the safety cap is hit.
    fn fill_feed(&self, tab: &Tab, q: &CapturedQuery, r: &mut ScrapeResult, opts: &ScrapeOpts) {
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
            let env = match self.issue_query(tab, q, &q.variables_for(&r.username, &cursor)) {
                Ok(e) => e,
                Err(e) => {
                    r.errors.push(format!("feed_query_p{page}: {e}"));
                    return;
                }
            };
            if self.debug {
                eprintln!(
                    "[debug] feed page={} doc_id={} status={} body_len={} cursor_in={:?}",
                    page,
                    q.doc_id,
                    env.status,
                    env.body.len(),
                    cursor
                );
            }
            if env.status >= 400 {
                r.errors.push(format!("feed_http_{}_p{}", env.status, page));
                return;
            }
            if r.user_id.is_empty() {
                if let Some(id) = user_id_from_feed(env.body.as_bytes(), &r.username) {
                    r.user_id = id;
                }
            }
            let feed = match parse_graphql_feed(env.body.as_bytes()) {
                Ok(f) => f,
                Err(e) => {
                    r.errors.push(format!("feed_parse_p{page}: {e}"));
                    return;
                }
            };
            if self.debug {
                eprintln!(
                    "[debug] feed page={} edges={} posts_extracted={} more={} next={:?}",
                    page,
                    feed.raw_count,
                    feed.posts.len(),
                    feed.has_more,
                    feed.next_cursor
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
            if !feed.has_more || feed.next_cursor.is_empty() {
                return;
            }
            cursor = feed.next_cursor;
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

/// Grabs the rendered document once; callers mine it for both the app id and
/// the profile meta tags.
fn page_html(tab: &Tab) -> String {
    eval_string(tab, APP_ID_DUMP_SCRIPT, false).unwrap_or_default()
}

/// Gives Relay a beat to fire its queries after first paint. Interruptible so
/// SIGTERM during a scan doesn't wait it out.
fn settle(tab: &Tab) {
    for _ in 0..12 {
        if shutdown::requested() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
        if eval_string(tab, RECORDER_COUNT_SCRIPT, false)
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            >= 2
        {
            return;
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

/// Installed via Page.addScriptToEvaluateOnNewDocument so it wraps fetch/XHR
/// before IG's bundle runs. Records the raw form bodies of the app's own
/// /graphql/query POSTs into window.__igq.
///
/// Our own replays (GRAPHQL_FN) tag themselves with `__igq_replay=1` and are
/// skipped, so the catalog only ever holds queries the app authored.
const RECORDER_SCRIPT: &str = r#"(function(){
  if (window.__igq) { return; }
  window.__igq = [];
  var CAP = 40;
  var flatten = function(h){
    var out = {};
    try {
      if (!h) { return out; }
      if (typeof h.forEach === 'function' && !Array.isArray(h)) {
        h.forEach(function(v, k){ out[k] = v; });
      } else if (Array.isArray(h)) {
        h.forEach(function(pair){ if (pair && pair.length === 2) { out[pair[0]] = pair[1]; } });
      } else {
        Object.keys(h).forEach(function(k){ out[k] = h[k]; });
      }
    } catch (e) {}
    return out;
  };
  var note = function(url, body, headers){
    try {
      if (typeof url !== 'string' || url.indexOf('/graphql/query') === -1) { return; }
      if (typeof body !== 'string' || body.indexOf('__igq_replay=1') !== -1) { return; }
      if (body.indexOf('doc_id=') === -1) { return; }
      if (window.__igq.length < CAP) {
        window.__igq.push({body: body, headers: headers || {}});
      }
    } catch (e) {}
  };
  var of = window.fetch;
  if (of) {
    window.fetch = function(input, init){
      try {
        var url = (typeof input === 'string') ? input : (input && input.url) || '';
        note(url, init && init.body, flatten(init && init.headers));
      } catch (e) {}
      return of.apply(this, arguments);
    };
  }
  var XHR = window.XMLHttpRequest;
  if (XHR && XHR.prototype) {
    var oopen = XHR.prototype.open;
    var osend = XHR.prototype.send;
    var oset = XHR.prototype.setRequestHeader;
    XHR.prototype.open = function(method, url){
      try { this.__igq_url = url; this.__igq_h = {}; } catch (e) {}
      return oopen.apply(this, arguments);
    };
    XHR.prototype.setRequestHeader = function(k, v){
      try { (this.__igq_h = this.__igq_h || {})[k] = v; } catch (e) {}
      return oset.apply(this, arguments);
    };
    XHR.prototype.send = function(body){
      try { note(this.__igq_url, body, this.__igq_h); } catch (e) {}
      return osend.apply(this, arguments);
    };
  }
})()"#;

/// Decodes the recorded form bodies in-page (URLSearchParams beats hand-rolling
/// a percent-decoder in Rust) and returns them as a JSON array of objects
/// matching CapturedQuery.
const READ_RECORDER_SCRIPT: &str = r#"(function(){
  var out = [];
  try {
    (window.__igq || []).forEach(function(rec){
      try {
        var body = (rec && typeof rec === 'object') ? rec.body : rec;
        var headers = (rec && typeof rec === 'object' && rec.headers) ? rec.headers : {};
        var p = new URLSearchParams(body);
        var docId = p.get('doc_id') || '';
        if (!docId) { return; }
        out.push({
          friendly_name: p.get('fb_api_req_friendly_name') || '',
          doc_id: docId,
          variables: p.get('variables') || '',
          fb_dtsg: p.get('fb_dtsg') || '',
          lsd: p.get('lsd') || '',
          jazoest: p.get('jazoest') || '',
          headers: headers
        });
      } catch (e) {}
    });
  } catch (e) {}
  return JSON.stringify(out);
})()"#;

/// How many queries the recorder has seen so far, as a string. Used to stop
/// waiting early once the page has fired its bootstrap queries.
const RECORDER_COUNT_SCRIPT: &str = r#"(function(){
  return String((window.__igq || []).length);
})()"#;

/// Replays a captured persisted query with our own variables. Same JSON string
/// envelope as the other in-page fetches. The `__igq_replay` marker keeps the
/// recorder from capturing our own request back into the catalog.
const GRAPHQL_FN: &str = r#"(async function(docId, variables, dtsg, lsd, jazoest, friendly, appId, headersJson){
  try {
    var body = new URLSearchParams();
    body.set('doc_id', docId);
    body.set('variables', variables);
    body.set('server_timestamps', 'true');
    body.set('fb_api_caller_class', 'RelayModern');
    if (friendly) { body.set('fb_api_req_friendly_name', friendly); }
    if (dtsg) { body.set('fb_dtsg', dtsg); }
    if (lsd) { body.set('lsd', lsd); }
    if (jazoest) { body.set('jazoest', jazoest); }
    body.set('__igq_replay', '1');

    // Start from the headers IG's own client sent, minus the ones the browser
    // computes itself (setting them throws or is silently dropped).
    var skip = {'content-length': 1, 'host': 1, 'connection': 1, 'cookie': 1};
    var headers = {};
    try {
      var captured = JSON.parse(headersJson || '{}');
      Object.keys(captured).forEach(function(k){
        if (!skip[String(k).toLowerCase()]) { headers[k] = captured[k]; }
      });
    } catch (e) {}

    headers['content-type'] = 'application/x-www-form-urlencoded';
    if (appId) { headers['X-IG-App-ID'] = appId; }
    if (friendly) { headers['X-FB-Friendly-Name'] = friendly; }
    if (lsd) { headers['X-FB-LSD'] = lsd; }

    // Required on GraphQL POSTs and not always present on the captured
    // request: mirror the csrftoken cookie.
    var hasCsrf = Object.keys(headers).some(function(k){
      return String(k).toLowerCase() === 'x-csrftoken';
    });
    if (!hasCsrf) {
      var m = document.cookie.match(/(?:^|;\s*)csrftoken=([^;]+)/);
      if (m) { headers['X-CSRFToken'] = decodeURIComponent(m[1]); }
    }

    var r = await fetch('/graphql/query', {
      method: 'POST',
      headers: headers,
      body: body.toString(),
      credentials: 'include',
      referrerPolicy: 'no-referrer-when-downgrade'
    });
    var text = await r.text();
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

    /// The real variables blob IG's posts query sends, cursor and all.
    fn live_posts_query() -> CapturedQuery {
        CapturedQuery {
            friendly_name: "PolarisProfilePostsTabContentQuery_connection".into(),
            doc_id: "28648141034875162".into(),
            variables: r#"{"after":"3973159376496644044_25025320","before":null,"data":{"count":12},"first":12,"include_multi_captions":true,"last":null,"username":"instagram","__relay_internal__pv__PolarisShortDramaEnabledrelayprovider":false}"#.into(),
            fb_dtsg: "NAfw...".into(),
            lsd: "ydEHmY2DIXFzHcLmz0cTRV".into(),
            jazoest: "26330".into(),
            headers: [
                ("x-csrftoken".to_string(), "tok".to_string()),
                ("X-IG-App-ID".to_string(), "936619743392459".to_string()),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn posts_query_is_classified_as_paginated_and_username_keyed() {
        let q = live_posts_query();
        assert!(q.is_usable());
        assert!(q.is_paginated());
        assert!(q.keys_on_username());
    }

    #[test]
    fn profile_query_is_not_classified_as_paginated() {
        let q = CapturedQuery {
            doc_id: "123".into(),
            variables: r#"{"username":"instagram","render_surface":"PROFILE"}"#.into(),
            ..Default::default()
        };
        assert!(q.is_usable());
        assert!(!q.is_paginated());
        assert!(q.keys_on_username());
    }

    #[test]
    fn variables_rewrite_swaps_username_and_cursor_keeping_the_rest() {
        let q = live_posts_query();
        let out: Value = serde_json::from_str(&q.variables_for("gjtorikian", "CURSOR_2")).unwrap();
        assert_eq!(out["username"], "gjtorikian");
        assert_eq!(out["after"], "CURSOR_2");
        // Untouched: provider flags and page size must survive verbatim.
        assert_eq!(out["first"], 12);
        assert_eq!(out["include_multi_captions"], true);
        assert_eq!(
            out["__relay_internal__pv__PolarisShortDramaEnabledrelayprovider"],
            false
        );
        assert_eq!(out["data"]["count"], 12);
    }

    #[test]
    fn first_page_sends_null_cursor() {
        let out: Value =
            serde_json::from_str(&live_posts_query().variables_for("instagram", "")).unwrap();
        assert!(out["after"].is_null(), "got {}", out["after"]);
    }

    #[test]
    fn unpaginated_query_gains_no_after_key() {
        let q = CapturedQuery {
            doc_id: "1".into(),
            variables: r#"{"username":"a"}"#.into(),
            ..Default::default()
        };
        let out: Value = serde_json::from_str(&q.variables_for("b", "")).unwrap();
        assert_eq!(out["username"], "b");
        assert!(out.get("after").is_none());
    }

    #[test]
    fn captured_headers_survive_into_the_catalog_shape() {
        // The 403 that broke the first live run came from rebuilding headers by
        // hand instead of replaying what IG's client sent.
        let q = live_posts_query();
        assert_eq!(
            q.headers.get("x-csrftoken").map(String::as_str),
            Some("tok")
        );
        let json = serde_json::to_string(&q.headers).unwrap();
        let back: std::collections::BTreeMap<String, String> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, q.headers);
    }

    #[test]
    fn capture_without_headers_still_deserializes() {
        // Older recordings, and the XHR path when nothing set a header.
        let q: CapturedQuery =
            serde_json::from_str(r#"{"doc_id":"1","variables":"{\"username\":\"a\"}"}"#).unwrap();
        assert!(q.is_usable());
        assert!(q.headers.is_empty());
    }

    #[test]
    fn unusable_captures_are_rejected() {
        assert!(!CapturedQuery::default().is_usable());
        assert!(
            !CapturedQuery {
                doc_id: "1".into(),
                ..Default::default()
            }
            .is_usable()
        );
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
