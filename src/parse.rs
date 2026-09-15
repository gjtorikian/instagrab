//! GraphQL profile / posts JSON -> ScrapeResult, window filter, and the
//! schema-drift detector (EXPECTED_GRAPHQL_PROFILE_FIELDS).

use anyhow::{Result as AnyResult, anyhow};
use chrono::{DateTime, Timelike, Utc};
use regex::Regex;
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Serialize, Clone, Debug)]
pub struct RecentPost {
    pub shortcode: String,
    pub url: String,
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_video: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken_at_unix: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_count: Option<i64>,
    #[serde(skip_serializing_if = "is_false_opt")]
    pub is_carousel: Option<bool>,
    #[serde(skip_serializing_if = "is_false_opt")]
    pub has_video: Option<bool>,
}

impl RecentPost {
    pub fn new(shortcode: String) -> Self {
        let url = format!("https://www.instagram.com/p/{shortcode}/");
        RecentPost {
            shortcode,
            url,
            caption: None,
            is_video: None,
            taken_at_unix: None,
            display_url: None,
            local_path: None,
            media_count: None,
            is_carousel: None,
            has_video: None,
        }
    }
}

//  JSON marshaling (RFC3339Nano with trailing-zero
// nanoseconds removed, and the fractional part dropped entirely when zero).
// chrono's SecondsFormat pads to groups of 3 digits
fn serialize_rfc3339<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    let base = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = t.nanosecond();
    let out = if nanos == 0 {
        format!("{base}Z")
    } else {
        let frac = format!("{nanos:09}");
        let frac = frac.trim_end_matches('0');
        format!("{base}.{frac}Z")
    };
    s.serialize_str(&out)
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !(*b)
}

fn is_false_opt(b: &Option<bool>) -> bool {
    !matches!(b, Some(true))
}

/// One JSONL line per username
#[derive(Serialize, Debug)]
pub struct ScrapeResult {
    #[serde(serialize_with = "serialize_rfc3339")]
    pub scraped_at: DateTime<Utc>,
    pub username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user_id: String,
    pub source: String,
    pub full_name: Option<String>,
    pub biography: Option<String>,
    pub is_private: Option<bool>,
    pub followers: Option<i64>,
    pub following: Option<i64>,
    pub posts: Option<i64>,
    pub recent_posts: Option<Vec<RecentPost>>,
    #[serde(skip_serializing_if = "is_zero")]
    pub window_days: i64,
    #[serde(skip_serializing_if = "is_false")]
    pub window_maybe_truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl ScrapeResult {
    pub fn new(username: &str, source: &str) -> Self {
        ScrapeResult {
            scraped_at: Utc::now(),
            username: username.to_string(),
            user_id: String::new(),
            source: source.to_string(),
            full_name: None,
            biography: None,
            is_private: None,
            followers: None,
            following: None,
            posts: None,
            recent_posts: None,
            window_days: 0,
            window_maybe_truncated: false,
            errors: Vec::new(),
        }
    }

    /// Status line for error / logged_out / not_found outcomes
    pub fn status_line(username: &str, source: &str, errors: Vec<String>) -> Self {
        let mut r = ScrapeResult::new(username, source);
        r.errors = errors;
        r
    }

    pub fn push_post(&mut self, p: RecentPost) {
        self.recent_posts.get_or_insert_with(Vec::new).push(p);
    }

    /// Trims recent_posts to entries whose taken_at_unix is within `days` of
    /// now (UTC). Returns true if the filter is "windowed-out": the oldest
    /// *kept* post is still inside the window, meaning pagination stopped
    /// before proving the window was exhausted. days <= 0 disables the filter.
    pub fn filter_by_window(&mut self, days: i64) -> bool {
        let posts = match &mut self.recent_posts {
            Some(p) if days > 0 && !p.is_empty() => p,
            _ => return false,
        };
        let cutoff = Utc::now().timestamp() - days * 24 * 60 * 60;

        let original_len = posts.len();
        posts.retain(|p| matches!(p.taken_at_unix, Some(ts) if ts >= cutoff));

        // If we kept everything, the oldest *visible* post is still inside the
        // window — so a paginated fetch could surface more in-window posts.
        posts.len() == original_len && !posts.is_empty()
    }
}

#[derive(Debug)]
pub struct ParsedProfile {
    pub result: Option<ScrapeResult>,
    pub missing: Vec<String>,
    pub requires_login: bool,
}

/// Checks for IG's logged-out shape.
///
/// Only explicit signals count. The old structural test ("`data.user` is
/// absent or empty") belonged to `web_profile_info`, whose envelope always had
/// that key; GraphQL responses are keyed by query (`xdt_user_by_username` and
/// friends), so applying it here would flag every healthy scrape as logged
/// out. On the GraphQL path a dead session shows up earlier and louder: the
/// profile navigation redirects to /accounts/login (caught by `read_dom_facts`)
/// or the query itself answers 401/403.
fn requires_login(doc: &Value) -> bool {
    for key in ["require_login", "requires_login"] {
        if doc.get(key).and_then(Value::as_bool) == Some(true) {
            return true;
        }
    }
    if let Some(msg) = doc.get("message").and_then(Value::as_str) {
        let m = msg.to_lowercase();
        if m.contains("login") || m.contains("log in") {
            return true;
        }
    }
    false
}

// --- value navigation helpers ------------------------------------------------

fn nav<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in keys {
        cur = cur.as_object()?.get(*k)?;
    }
    Some(cur)
}

fn nav_int(v: &Value, keys: &[&str]) -> Option<i64> {
    match nav(v, keys)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

// --- user feed ---------------------------------------------------------------

#[derive(Debug)]
pub struct FeedPage {
    pub posts: Vec<RecentPost>,
    /// Raw count of items in the response, so callers can distinguish "IG
    /// returned nothing" from "IG returned items but our extractor couldn't
    /// find shortcodes."
    pub raw_count: usize,
    /// Opaque continuation token. The mobile API calls this `next_max_id`; the
    /// GraphQL connection calls it `page_info.end_cursor`. Same role, and
    /// empirically the same `<pk>_<uid>` shape.
    pub next_cursor: String,
    pub has_more: bool,
}

/// Builds a RecentPost from a /api/v1/feed/user/ item.
/// Returns None if no shortcode can be derived.
fn post_from_feed_item(it: &Value) -> Option<RecentPost> {
    let mut code = it.get("code").and_then(Value::as_str).unwrap_or("");
    if code.is_empty() {
        // Carousels sometimes only have the code on children.
        if let Some(cm) = it.get("carousel_media").and_then(Value::as_array) {
            for c in cm {
                let cc = c.get("code").and_then(Value::as_str).unwrap_or("");
                if !cc.is_empty() {
                    code = cc;
                    break;
                }
            }
        }
    }
    if code.is_empty() {
        return None;
    }
    let mut rp = RecentPost::new(code.to_string());
    if let Some(ts) = it.get("taken_at").and_then(Value::as_f64) {
        if ts > 0.0 {
            rp.taken_at_unix = Some(ts as i64);
        }
    }
    if let Some(text) = nav(it, &["caption", "text"]).and_then(Value::as_str) {
        if !text.is_empty() {
            rp.caption = Some(text.to_string());
        }
    }
    if let Some(mt) = it.get("media_type").and_then(Value::as_f64) {
        if mt as i64 == 2 {
            rp.is_video = Some(true);
        }
    }
    let (mc, carousel, hv) = media_facts_from_feed_item(it);
    rp.media_count = mc;
    rp.is_carousel = carousel;
    rp.has_video = hv;
    let u = best_image_url(it);
    if !u.is_empty() {
        rp.display_url = Some(u);
    }
    Some(rp)
}

/// Derives (media_count, is_carousel, has_video) from a mobile-feed item.
/// media_type: 1=image, 2=video, 8=carousel. Shape-tolerant — a missing
/// media_type or carousel_media degrades to a single image (count 1).
fn media_facts_from_feed_item(it: &Value) -> (Option<i64>, Option<bool>, Option<bool>) {
    if let Some(cm) = it.get("carousel_media").and_then(Value::as_array) {
        let count = cm.len() as i64;
        let has_video = cm.iter().any(|c| {
            c.get("media_type")
                .and_then(Value::as_f64)
                .map(|f| f as i64)
                == Some(2)
        });
        let hv = if has_video { Some(true) } else { None };
        return (Some(count), Some(true), hv);
    }
    let top_is_video = it
        .get("media_type")
        .and_then(Value::as_f64)
        .map(|f| f as i64)
        == Some(2);
    let hv = if top_is_video { Some(true) } else { None };
    (Some(1), None, hv)
}

/// Pulls the highest-width candidate from image_versions2. Falls back into
/// the first carousel_media child if the top-level item has none.
fn best_image_url(it: &Value) -> String {
    let u = candidates_best_url(it);
    if !u.is_empty() {
        return u;
    }
    if let Some(cm) = it.get("carousel_media").and_then(Value::as_array) {
        if let Some(first) = cm.first() {
            return candidates_best_url(first);
        }
    }
    String::new()
}

fn candidates_best_url(it: &Value) -> String {
    let cands = match nav(it, &["image_versions2", "candidates"]).and_then(Value::as_array) {
        Some(c) if !c.is_empty() => c,
        _ => return String::new(),
    };
    let mut best_url = String::new();
    let mut best_w: i64 = -1;
    for c in cands {
        let u = c.get("url").and_then(Value::as_str).unwrap_or("");
        if u.is_empty() {
            continue;
        }
        let w = c
            .get("width")
            .and_then(Value::as_f64)
            .map(|f| f as i64)
            .unwrap_or(0);
        if w > best_w {
            best_w = w;
            best_url = u.to_string();
        }
    }
    best_url
}

// --- GraphQL: profile posts tab ----------------------------------------------

/// The connection field `PolarisProfilePostsTabContentQuery_connection`
/// returns. Looked up by name first; if IG renames it we fall back to any
/// object under `data` that looks like a Relay connection, since the rename is
/// exactly the kind of drift this project expects.
const FEED_CONNECTION_KEY: &str = "xdt_api__v1__feed__user_timeline_graphql_connection";

/// Parses a `/graphql/query` profile-posts response. The per-node shape is the
/// mobile-API shape — `code`, `taken_at`, `image_versions2.candidates[].url` —
/// so `post_from_feed_item` is reused verbatim. Only the envelope differs:
/// items arrive as `edges[].node` and the cursor as `page_info.end_cursor`.
///
/// Note the carousel inversion versus the mobile feed: here the *parent* node
/// carries `code` and children carry `"code": null`, so the child fallback in
/// `post_from_feed_item` never fires on this path.
pub fn parse_graphql_feed(raw: &[u8]) -> AnyResult<FeedPage> {
    let doc: Value = serde_json::from_slice(raw)?;

    if let Some(errs) = doc.get("errors").and_then(Value::as_array) {
        if !errs.is_empty() {
            let msg = errs
                .first()
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(anyhow!("graphql error: {msg}"));
        }
    }

    let conn = find_connection(&doc).ok_or_else(|| anyhow!("no posts connection in response"))?;

    let edges = conn
        .get("edges")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("connection has no edges"))?;

    let mut posts = Vec::new();
    for e in edges {
        let node = match e.get("node").filter(|v| v.is_object()) {
            Some(n) => n,
            None => continue,
        };
        if let Some(rp) = post_from_feed_item(node) {
            posts.push(rp);
        }
    }

    let page_info = conn.get("page_info");
    let next_cursor = page_info
        .and_then(|p| p.get("end_cursor"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let has_more = page_info
        .and_then(|p| p.get("has_next_page"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(FeedPage {
        posts,
        raw_count: edges.len(),
        next_cursor,
        has_more,
    })
}

/// Locates the posts connection: the documented key first, then any object
/// under `data` carrying both `edges` and `page_info`.
fn find_connection(doc: &Value) -> Option<&Value> {
    let data = doc.get("data")?.as_object()?;
    if let Some(v) = data.get(FEED_CONNECTION_KEY).filter(|v| v.is_object()) {
        return Some(v);
    }
    data.values().find(|v| {
        v.get("edges").map(Value::is_array).unwrap_or(false) && v.get("page_info").is_some()
    })
}

/// Pulls the owning account's numeric id out of a posts-connection response.
///
/// Guarded by username for the same reason `find_user_object` is: a node's
/// `user` is usually the profile owner, but reposts and coauthored items carry
/// someone else's. Only a node that names the account we asked for counts.
pub fn user_id_from_feed(raw: &[u8], username: &str) -> Option<String> {
    let doc: Value = serde_json::from_slice(raw).ok()?;
    let conn = find_connection(&doc)?;
    for e in conn.get("edges")?.as_array()? {
        let node = e.get("node")?;
        let owner = match node.get("user").filter(|u| u.is_object()) {
            Some(u) => u,
            None => continue,
        };
        let names_it = owner
            .get("username")
            .and_then(Value::as_str)
            .map(|u| u.eq_ignore_ascii_case(username))
            .unwrap_or(false);
        if !names_it {
            continue;
        }
        for key in ["pk", "id"] {
            match owner.get(key) {
                Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
                Some(Value::Number(n)) => return Some(n.to_string()),
                _ => {}
            }
        }
    }
    None
}

// --- profile metadata from the server-rendered page --------------------------

/// Profile fields recovered from the page's meta tags.
///
/// Neither GraphQL query the profile page fires carries the header — both
/// return the posts connection — so the counts come from the `og:description`
/// Instagram server-renders on a direct navigation. No extra request: the HTML
/// is already in hand for app-id extraction.
///
/// Caveat worth knowing: large accounts get abbreviated counts ("695M"), so
/// followers for mega-accounts are rounded. Ordinary accounts render exact
/// comma-separated numbers.
#[derive(Debug, Default, PartialEq)]
pub struct HtmlProfile {
    pub full_name: Option<String>,
    pub followers: Option<i64>,
    pub following: Option<i64>,
    pub posts: Option<i64>,
    pub is_private: Option<bool>,
}

pub fn profile_fields_from_html(username: &str, html: &str) -> HtmlProfile {
    let mut out = HtmlProfile::default();

    if let Some(desc) = meta_content(html, "og:description")
        .or_else(|| meta_content(html, "description"))
        .filter(|d| mentions_handle(d, username))
    {
        if let Some(caps) = counts_re().captures(&desc) {
            out.followers = parse_abbreviated_count(caps.get(1).map_or("", |m| m.as_str()));
            out.following = parse_abbreviated_count(caps.get(2).map_or("", |m| m.as_str()));
            out.posts = parse_abbreviated_count(caps.get(3).map_or("", |m| m.as_str()));
        }
    }

    // "Instagram (@instagram) • Instagram photos and videos"
    if let Some(title) = meta_content(html, "og:title").filter(|t| mentions_handle(t, username)) {
        if let Some((name, _)) = title.split_once(" (@") {
            let name = name.trim();
            if !name.is_empty() {
                out.full_name = Some(name.to_string());
            }
        }
    }

    if html.contains("This Account is Private") || html.contains("This account is private") {
        out.is_private = Some(true);
    }

    out
}

/// Only trust a meta tag that names the profile we asked for — the same guard
/// `find_user_object` needs, for the same reason.
fn mentions_handle(text: &str, username: &str) -> bool {
    text.to_lowercase()
        .contains(&format!("(@{})", username.to_lowercase()))
}

fn meta_content(html: &str, key: &str) -> Option<String> {
    static PATTERNS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Regex>>> =
        OnceLock::new();
    let cache = PATTERNS.get_or_init(Default::default);
    let re = {
        let mut map = cache.lock().ok()?;
        map.entry(key.to_string())
            .or_insert_with(|| {
                // Attribute order varies; match either spelling around content=.
                Regex::new(&format!(
                    r#"(?is)<meta[^>]*(?:property|name)\s*=\s*["']{}["'][^>]*content\s*=\s*["']([^"']*)["']|<meta[^>]*content\s*=\s*["']([^"']*)["'][^>]*(?:property|name)\s*=\s*["']{}["']"#,
                    regex::escape(key),
                    regex::escape(key)
                ))
                .expect("meta regex compiles")
            })
            .clone()
    };
    let caps = re.captures(html)?;
    let raw = caps
        .get(1)
        .or_else(|| caps.get(2))
        .map(|m| m.as_str())
        .unwrap_or("");
    if raw.is_empty() {
        return None;
    }
    Some(decode_entities(raw))
}

fn decode_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn counts_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)([0-9][0-9,.]*\s*[KMB]?)\s+Followers,\s*([0-9][0-9,.]*\s*[KMB]?)\s+Following,\s*([0-9][0-9,.]*\s*[KMB]?)\s+Posts",
        )
        .expect("counts regex compiles")
    })
}

/// "8,021" -> 8021; "695M" -> 695000000; "1.2K" -> 1200.
fn parse_abbreviated_count(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1_000f64),
        'M' => (&s[..s.len() - 1], 1_000_000f64),
        'B' => (&s[..s.len() - 1], 1_000_000_000f64),
        _ => (s, 1f64),
    };
    let cleaned: String = num
        .chars()
        .filter(|c| *c != ',' && !c.is_whitespace())
        .collect();
    let v: f64 = cleaned.parse().ok()?;
    Some((v * mult).round() as i64)
}

// --- GraphQL: profile header --------------------------------------------------

/// Profile metadata field names, as IG's GraphQL responses spell them. Used
/// both to locate the user object and to report drift.
pub const EXPECTED_GRAPHQL_PROFILE_FIELDS: &[&str] = &[
    "username",
    "full_name",
    "biography",
    "follower_count",
    "following_count",
    "media_count",
];

/// Extracts profile metadata from a `/graphql/query` profile-header response.
///
/// The exact query and envelope key are discovered at runtime rather than
/// pinned, so this walks the document for the object that carries the most
/// expected fields instead of a fixed path. That keeps it working across the
/// `xdt_user_by_username` / `user` / `xdt_api__v1__users__*` spellings IG has
/// used, at the cost of being heuristic.
///
/// Returns the result plus the expected-but-absent field names, so a uniform
/// miss can raise the existing schema_drift alert.
pub fn parse_graphql_profile(username: &str, raw: &[u8]) -> AnyResult<ParsedProfile> {
    let doc: Value = serde_json::from_slice(raw)?;

    if requires_login(&doc) {
        return Ok(ParsedProfile {
            result: None,
            missing: Vec::new(),
            requires_login: true,
        });
    }

    let user = match find_user_object(&doc, username) {
        Some(u) => u,
        None => return Err(anyhow!("no profile object in response")),
    };

    let missing: Vec<String> = EXPECTED_GRAPHQL_PROFILE_FIELDS
        .iter()
        .filter(|f| user.get(**f).map(Value::is_null).unwrap_or(true))
        .map(|f| (*f).to_string())
        .collect();

    let mut r = ScrapeResult::new(username, "graphql");

    // `pk` and `id` both appear; either may be string or number.
    for key in ["pk", "id"] {
        if r.user_id.is_empty() {
            if let Some(v) = user.get(key) {
                r.user_id = match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => String::new(),
                };
            }
        }
    }
    r.full_name = user
        .get("full_name")
        .and_then(Value::as_str)
        .map(String::from);
    r.biography = user
        .get("biography")
        .and_then(Value::as_str)
        .map(String::from);
    r.is_private = user.get("is_private").and_then(Value::as_bool);
    r.followers = profile_count(user, &["follower_count", "edge_followed_by"]);
    r.following = profile_count(user, &["following_count", "edge_follow"]);
    r.posts = profile_count(user, &["media_count", "edge_owner_to_timeline_media"]);

    Ok(ParsedProfile {
        result: Some(r),
        missing,
        requires_login: false,
    })
}

/// Reads a count that IG spells either flat (`follower_count`) or nested
/// (`edge_followed_by.count`).
fn profile_count(user: &Value, keys: &[&str]) -> Option<i64> {
    for k in keys {
        match user.get(k) {
            Some(Value::Number(n)) => return n.as_i64(),
            Some(v) if v.is_object() => {
                if let Some(c) = nav_int(v, &["count"]) {
                    return Some(c);
                }
            }
            _ => {}
        }
    }
    None
}

/// Walks the document for the profile-header object.
///
/// Two guards, both learned the hard way: the object must carry a `username`
/// equal to the one we asked for, and it must carry at least one count field.
/// Without them a per-post `user` stub (username + full_name) scores 2 and
/// wins by default when no header is present — which is exactly what happened
/// on the first live run, writing a tagged creator's `user_id` and `full_name`
/// into a line labelled `username: instagram`. Missing data is recoverable;
/// confidently wrong identity data is not.
fn find_user_object<'a>(doc: &'a Value, username: &str) -> Option<&'a Value> {
    const COUNT_FIELDS: &[&str] = &[
        "follower_count",
        "following_count",
        "media_count",
        "edge_followed_by",
        "edge_follow",
        "edge_owner_to_timeline_media",
    ];

    let mut best: Option<(usize, &Value)> = None;
    let mut stack = vec![doc];
    while let Some(v) = stack.pop() {
        match v {
            Value::Object(map) => {
                let identifies = map
                    .get("username")
                    .and_then(Value::as_str)
                    .map(|u| u.eq_ignore_ascii_case(username))
                    .unwrap_or(false);
                let has_counts = COUNT_FIELDS
                    .iter()
                    .any(|f| map.get(*f).map(|x| !x.is_null()).unwrap_or(false));
                if identifies && has_counts {
                    let score = EXPECTED_GRAPHQL_PROFILE_FIELDS
                        .iter()
                        .filter(|f| map.get(**f).map(|x| !x.is_null()).unwrap_or(false))
                        .count();
                    if best.map(|(s, _)| score > s).unwrap_or(true) {
                        best = Some((score, v));
                    }
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items.iter()),
            _ => {}
        }
    }
    best.map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_serializes_like_go_zero_value_result() {
        let r = ScrapeResult::status_line("zuck", "error", vec!["boom".to_string()]);
        let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        // Nil pointers -> null; nil slice -> null; omitempty fields absent.
        assert_eq!(v["full_name"], Value::Null);
        assert_eq!(v["recent_posts"], Value::Null);
        assert!(v.get("user_id").is_none());
        assert!(v.get("window_days").is_none());
        assert!(v.get("window_maybe_truncated").is_none());
        assert_eq!(v["errors"][0], "boom");
        assert_eq!(v["source"], "error");
    }

    #[test]
    fn graphql_envelope_without_data_user_is_not_logged_out() {
        // Regression: the web_profile_info-era structural check treated a
        // missing `data.user` as logged-out, which every GraphQL response is.
        let raw = br#"{"data": {"xdt_user_by_username": {
            "username": "x", "full_name": "X", "follower_count": 1
        }}}"#;
        let p = parse_graphql_profile("x", raw).unwrap();
        assert!(!p.requires_login);
        assert!(p.result.is_some());
    }

    #[test]
    fn explicit_login_signals_still_detected() {
        for raw in [
            &br#"{"require_login": true}"#[..],
            &br#"{"requires_login": true}"#[..],
            &br#"{"message": "Please log in to continue"}"#[..],
        ] {
            let p = parse_graphql_profile("x", raw).unwrap();
            assert!(
                p.requires_login,
                "missed signal in {:?}",
                std::str::from_utf8(raw)
            );
        }
    }

    #[test]
    fn user_id_comes_from_the_node_owned_by_the_requested_account() {
        assert_eq!(
            user_id_from_feed(GRAPHQL_FEED_WITH_USER, "instagram").as_deref(),
            Some("25025320")
        );
    }

    #[test]
    fn user_id_ignores_nodes_owned_by_someone_else() {
        // A repost's node names a different owner; taking its pk would label
        // this account with a stranger's id.
        assert_eq!(
            user_id_from_feed(FEED_WITH_FOREIGN_STUBS, "instagram"),
            None
        );
    }

    const GRAPHQL_FEED_WITH_USER: &[u8] = br#"{"data": {
      "xdt_api__v1__feed__user_timeline_graphql_connection": {
        "edges": [
          {"node": {"code": "Z", "taken_at": 0, "user": {"pk": "777", "username": "reposter"}}},
          {"node": {"code": "A", "taken_at": 1,
                    "user": {"pk": "25025320", "username": "instagram"}}}
        ],
        "page_info": {"end_cursor": "", "has_next_page": false}
      }}}"#;

    // --- profile identity guard ----------------------------------------------

    /// Shaped like the real PolarisProfilePostsQuery response: posts for
    /// `instagram`, each node embedding a *different* creator's user stub.
    const FEED_WITH_FOREIGN_STUBS: &[u8] = br#"{"data": {
      "xdt_api__v1__feed__user_timeline_graphql_connection": {
        "edges": [
          {"node": {"code": "A", "taken_at": 1, "user": {
             "pk": "53528241800", "username": "janiesdaisies", "full_name": "janie's daisies"
          }}},
          {"node": {"code": "B", "taken_at": 2, "coauthor_producers": [
             {"pk": "999", "username": "someoneelse", "full_name": "Someone Else"}
          ]}}
        ],
        "page_info": {"end_cursor": "", "has_next_page": false}
      }}}"#;

    #[test]
    fn foreign_user_stubs_never_become_the_profile() {
        // Regression from the first live run: a tagged creator's identity was
        // written into a line labelled username=instagram.
        let err = parse_graphql_profile("instagram", FEED_WITH_FOREIGN_STUBS).unwrap_err();
        assert!(err.to_string().contains("no profile object"), "got {err}");
    }

    #[test]
    fn matching_username_without_counts_is_still_rejected() {
        // The account's own per-post stub matches on username but carries no
        // counts; trusting it would yield a header with everything null.
        let raw = br#"{"data": {"c": {"edges": [{"node": {"code": "A", "user": {
            "pk": "25025320", "username": "instagram", "full_name": "Instagram"
        }}}], "page_info": {}}}}"#;
        assert!(parse_graphql_profile("instagram", raw).is_err());
    }

    #[test]
    fn real_header_is_accepted_despite_surrounding_stubs() {
        let raw = br#"{"data": {
          "conn": {"edges": [{"node": {"user": {
              "pk": "53528241800", "username": "janiesdaisies", "full_name": "janie's daisies"
          }}}]},
          "header": {"pk": "25025320", "username": "instagram", "full_name": "Instagram",
                     "biography": "bio", "follower_count": 5, "following_count": 6,
                     "media_count": 7}
        }}"#;
        let r = parse_graphql_profile("instagram", raw)
            .unwrap()
            .result
            .unwrap();
        assert_eq!(r.user_id, "25025320");
        assert_eq!(r.followers, Some(5));
    }

    // --- meta-tag profile ----------------------------------------------------

    #[test]
    fn meta_tags_yield_counts_and_name() {
        let html = r#"<meta property="og:title" content="Instagram (@instagram) &#039; Instagram photos and videos" />
        <meta property="og:description" content="695M Followers, 176 Following, 8,021 Posts - See Instagram photos and videos from Instagram (@instagram)" />"#;
        let p = profile_fields_from_html("instagram", html);
        assert_eq!(p.followers, Some(695_000_000));
        assert_eq!(p.following, Some(176));
        assert_eq!(p.posts, Some(8021));
        assert_eq!(p.full_name.as_deref(), Some("Instagram"));
    }

    #[test]
    fn meta_tags_for_a_different_handle_are_ignored() {
        // Same guard as find_user_object: never attribute another account's
        // numbers to this one.
        let html = r#"<meta property="og:description" content="12 Followers, 3 Following, 4 Posts - See Instagram photos and videos from Someone (@someoneelse)" />"#;
        assert_eq!(
            profile_fields_from_html("instagram", html),
            HtmlProfile::default()
        );
    }

    #[test]
    fn meta_name_description_is_accepted_too() {
        let html = r#"<meta name="description" content="1,234 Followers, 567 Following, 89 Posts - See Instagram photos and videos from Jane (@jane_doe)" />"#;
        let p = profile_fields_from_html("jane_doe", html);
        assert_eq!(p.followers, Some(1234));
        assert_eq!(p.posts, Some(89));
    }

    #[test]
    fn content_before_property_attribute_order_parses() {
        let html = r#"<meta content="5 Followers, 6 Following, 7 Posts - See Instagram photos and videos from A (@a)" property="og:description">"#;
        assert_eq!(profile_fields_from_html("a", html).followers, Some(5));
    }

    #[test]
    fn private_account_is_flagged() {
        let html = r#"<meta property="og:description" content="5 Followers, 6 Following, 7 Posts - See Instagram photos and videos from A (@a)"><h2>This Account is Private</h2>"#;
        assert_eq!(profile_fields_from_html("a", html).is_private, Some(true));
    }

    #[test]
    fn abbreviated_counts_scale() {
        assert_eq!(parse_abbreviated_count("8,021"), Some(8021));
        assert_eq!(parse_abbreviated_count("695M"), Some(695_000_000));
        assert_eq!(parse_abbreviated_count("1.2K"), Some(1200));
        assert_eq!(parse_abbreviated_count("1.5B"), Some(1_500_000_000));
        assert_eq!(parse_abbreviated_count(""), None);
        assert_eq!(parse_abbreviated_count("n/a"), None);
    }

    #[test]
    fn missing_meta_tags_yield_nothing_rather_than_guesses() {
        assert_eq!(
            profile_fields_from_html("x", "<html></html>"),
            HtmlProfile::default()
        );
    }

    // --- GraphQL envelope -----------------------------------------------------

    /// Mirrors the live PolarisProfilePostsTabContentQuery_connection payload:
    /// edges[].node with mobile-shaped fields, and a carousel whose children
    /// carry `"code": null` while the parent holds the code.
    const GRAPHQL_FEED: &[u8] = br#"{
      "data": {
        "xdt_api__v1__feed__user_timeline_graphql_connection": {
          "edges": [
            {"node": {
               "code": "DcjKbIJyVs3",
               "taken_at": 1787846425,
               "media_type": 2,
               "caption": {"text": "game, set, algo"},
               "image_versions2": {"candidates": [
                 {"url": "https://cdn/x_1152.jpg", "width": 1152, "height": 2048},
                 {"url": "https://cdn/x_640.jpg", "width": 640, "height": 1138}
               ]}
             }, "cursor": ""},
            {"node": {
               "code": "DcgYclUEV18",
               "taken_at": 1787752983,
               "media_type": 8,
               "carousel_media_count": 4,
               "carousel_media": [
                 {"code": null, "media_type": 1, "carousel_parent_id": "3972282388667522428_25025320"},
                 {"code": null, "media_type": 1, "carousel_parent_id": "3972282388667522428_25025320"}
               ],
               "image_versions2": {"candidates": [
                 {"url": "https://cdn/y_2160.jpg", "width": 2160, "height": 2700}
               ]}
             }, "cursor": ""}
          ],
          "page_info": {
            "end_cursor": "3961559603594599528_25025320",
            "has_next_page": true,
            "has_previous_page": false,
            "start_cursor": null
          }
        },
        "xdt_viewer": {"user": {"id": "1018933991"}}
      },
      "status": "ok"
    }"#;

    #[test]
    fn graphql_feed_reuses_mobile_node_extraction() {
        let page = parse_graphql_feed(GRAPHQL_FEED).unwrap();
        assert_eq!(page.raw_count, 2);
        assert_eq!(page.posts.len(), 2);

        let video = &page.posts[0];
        assert_eq!(video.shortcode, "DcjKbIJyVs3");
        assert_eq!(video.taken_at_unix, Some(1787846425));
        assert_eq!(video.is_video, Some(true));
        assert_eq!(video.caption.as_deref(), Some("game, set, algo"));
        assert_eq!(video.display_url.as_deref(), Some("https://cdn/x_1152.jpg"));
    }

    #[test]
    fn graphql_feed_reads_code_from_carousel_parent_not_children() {
        // Inverted from the mobile feed: children have "code": null here, so
        // the parent's code is the only source.
        let page = parse_graphql_feed(GRAPHQL_FEED).unwrap();
        let carousel = &page.posts[1];
        assert_eq!(carousel.shortcode, "DcgYclUEV18");
        assert_eq!(carousel.is_carousel, Some(true));
        assert_eq!(carousel.media_count, Some(2));
    }

    #[test]
    fn graphql_feed_maps_page_info_to_cursor() {
        let page = parse_graphql_feed(GRAPHQL_FEED).unwrap();
        assert_eq!(page.next_cursor, "3961559603594599528_25025320");
        assert!(page.has_more);
    }

    #[test]
    fn graphql_feed_survives_connection_rename() {
        let raw = br#"{"data": {"xdt_api__v1__feed__renamed_tomorrow": {
            "edges": [{"node": {"code": "ABC", "taken_at": 100}}],
            "page_info": {"end_cursor": "c1", "has_next_page": false}
        }}}"#;
        let page = parse_graphql_feed(raw).unwrap();
        assert_eq!(page.posts.len(), 1);
        assert_eq!(page.next_cursor, "c1");
        assert!(!page.has_more);
    }

    #[test]
    fn graphql_feed_surfaces_errors_block() {
        let raw = br#"{"errors": [{"message": "PersistedQueryNotFound"}], "data": null}"#;
        let err = parse_graphql_feed(raw).unwrap_err().to_string();
        assert!(err.contains("PersistedQueryNotFound"), "got {err}");
    }

    #[test]
    fn graphql_feed_empty_last_page_stops_pagination() {
        let raw = br#"{"data": {"xdt_api__v1__feed__user_timeline_graphql_connection": {
            "edges": [],
            "page_info": {"end_cursor": null, "has_next_page": false}
        }}}"#;
        let page = parse_graphql_feed(raw).unwrap();
        assert!(page.posts.is_empty());
        assert!(page.next_cursor.is_empty());
        assert!(!page.has_more);
    }

    // --- GraphQL profile header ----------------------------------------------

    #[test]
    fn graphql_profile_reads_flat_counts() {
        let raw = br#"{"data": {"xdt_user_by_username": {
            "pk": "25025320",
            "username": "instagram",
            "full_name": "Instagram",
            "biography": "Discovering and telling stories",
            "is_private": false,
            "follower_count": 695000000,
            "following_count": 176,
            "media_count": 8021
        }}}"#;
        let p = parse_graphql_profile("instagram", raw).unwrap();
        let r = p.result.unwrap();
        assert_eq!(r.user_id, "25025320");
        assert_eq!(r.full_name.as_deref(), Some("Instagram"));
        assert_eq!(r.followers, Some(695000000));
        assert_eq!(r.following, Some(176));
        assert_eq!(r.posts, Some(8021));
        assert_eq!(r.is_private, Some(false));
        assert!(p.missing.is_empty(), "missing {:?}", p.missing);
    }

    #[test]
    fn graphql_profile_reads_nested_edge_counts() {
        let raw = br#"{"data": {"user": {
            "id": 25025320,
            "username": "instagram",
            "full_name": "Instagram",
            "biography": "bio",
            "edge_followed_by": {"count": 12},
            "edge_follow": {"count": 3},
            "edge_owner_to_timeline_media": {"count": 7}
        }}}"#;
        let r = parse_graphql_profile("instagram", raw)
            .unwrap()
            .result
            .unwrap();
        assert_eq!(r.user_id, "25025320");
        assert_eq!(r.followers, Some(12));
        assert_eq!(r.following, Some(3));
        assert_eq!(r.posts, Some(7));
    }

    #[test]
    fn graphql_profile_reports_missing_fields_as_drift() {
        // Passes the identity guard (right username, has counts) but has lost
        // biography and following_count — the drift this alert exists for.
        let raw = br#"{"data": {"user": {
            "username": "x", "full_name": "X", "follower_count": 5, "media_count": 7
        }}}"#;
        let p = parse_graphql_profile("x", raw).unwrap();
        assert!(p.missing.contains(&"biography".to_string()));
        assert!(p.missing.contains(&"following_count".to_string()));
        assert!(!p.missing.contains(&"username".to_string()));
        assert!(!p.missing.contains(&"follower_count".to_string()));
    }

    #[test]
    fn graphql_profile_prefers_header_over_embedded_post_stubs() {
        // The posts feed embeds a per-node `user` stub. The header object has
        // strictly more expected fields and must win.
        let raw = br#"{"data": {
            "conn": {"edges": [{"node": {"user": {
                "username": "instagram", "full_name": "Instagram", "pk": "25025320"
            }}}]},
            "header": {
                "username": "instagram", "full_name": "Instagram",
                "biography": "real one", "follower_count": 5,
                "following_count": 6, "media_count": 7, "pk": "25025320"
            }
        }}"#;
        let r = parse_graphql_profile("instagram", raw)
            .unwrap()
            .result
            .unwrap();
        assert_eq!(r.biography.as_deref(), Some("real one"));
        assert_eq!(r.followers, Some(5));
    }

    #[test]
    fn filter_by_window_semantics() {
        let now = Utc::now().timestamp();
        let mk = |ts: Option<i64>| {
            let mut p = RecentPost::new("X".to_string());
            p.taken_at_unix = ts;
            p
        };

        // All posts in-window -> everything kept -> maybe truncated.
        let mut r = ScrapeResult::new("u", "graphql");
        r.push_post(mk(Some(now - 100)));
        r.push_post(mk(Some(now - 200)));
        assert!(r.filter_by_window(7));
        assert_eq!(r.recent_posts.as_ref().unwrap().len(), 2);

        // Old + missing-timestamp posts dropped -> not truncated.
        let mut r = ScrapeResult::new("u", "graphql");
        r.push_post(mk(Some(now - 100)));
        r.push_post(mk(Some(now - 100 * 24 * 60 * 60)));
        r.push_post(mk(None));
        assert!(!r.filter_by_window(7));
        assert_eq!(r.recent_posts.as_ref().unwrap().len(), 1);

        // Everything filtered out -> empty (serializes as []), not truncated.
        let mut r = ScrapeResult::new("u", "graphql");
        r.push_post(mk(Some(now - 100 * 24 * 60 * 60)));
        assert!(!r.filter_by_window(7));
        assert_eq!(r.recent_posts.as_ref().unwrap().len(), 0);
        let v: Value = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(v["recent_posts"], serde_json::json!([]));

        // days <= 0 disables the filter.
        let mut r = ScrapeResult::new("u", "graphql");
        r.push_post(mk(None));
        assert!(!r.filter_by_window(0));
        assert_eq!(r.recent_posts.as_ref().unwrap().len(), 1);
    }
}
