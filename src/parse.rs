//! web_profile_info / user-feed JSON -> Result, window filter, and the
//! schema-drift detector (EXPECTED_PROFILE_PATHS).

use anyhow::{Result as AnyResult, anyhow};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

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

/// The JSON paths instagrab expects in a `web_profile_info` payload.
/// Used for schema-drift detection.
pub const EXPECTED_PROFILE_PATHS: &[&str] = &[
    "data.user.username",
    "data.user.full_name",
    "data.user.biography",
    "data.user.edge_followed_by.count",
    "data.user.edge_follow.count",
    "data.user.edge_owner_to_timeline_media.count",
];

#[derive(Debug)]
pub struct ParsedProfile {
    pub result: Option<ScrapeResult>,
    pub missing: Vec<String>,
    pub requires_login: bool,
}

/// Parses a raw `users/web_profile_info` JSON body. Returns the result, the
/// list of missing-but-expected paths (for drift detection), and whether the
/// response indicates a logged-out state.
pub fn parse_web_profile_info(username: &str, raw: &[u8]) -> AnyResult<ParsedProfile> {
    let doc: Value = serde_json::from_slice(raw)?;

    if requires_login(&doc) {
        return Ok(ParsedProfile {
            result: None,
            missing: Vec::new(),
            requires_login: true,
        });
    }

    let missing = validate_schema(&doc, EXPECTED_PROFILE_PATHS);

    let user = match nav(&doc, &["data", "user"]).filter(|v| v.is_object()) {
        Some(u) => u,
        None => return Err(anyhow!("data.user missing")),
    };

    let mut r = ScrapeResult::new(username, "graphql");

    if let Some(v) = user.get("id").and_then(Value::as_str) {
        r.user_id = v.to_string();
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
    r.followers = nav_int(user, &["edge_followed_by", "count"]);
    r.following = nav_int(user, &["edge_follow", "count"]);
    r.posts = nav_int(user, &["edge_owner_to_timeline_media", "count"]);

    if let Some(edges) =
        nav(user, &["edge_owner_to_timeline_media", "edges"]).and_then(Value::as_array)
    {
        for e in edges {
            let node = match e.get("node").filter(|v| v.is_object()) {
                Some(n) => n,
                None => continue,
            };
            let shortcode = node.get("shortcode").and_then(Value::as_str).unwrap_or("");
            if shortcode.is_empty() {
                continue;
            }
            let mut rp = RecentPost::new(shortcode.to_string());
            rp.caption = caption_from_node(node);
            rp.is_video = node.get("is_video").and_then(Value::as_bool);
            if let Some(ts) = node.get("taken_at_timestamp").and_then(Value::as_f64) {
                rp.taken_at_unix = Some(ts as i64);
            }
            rp.display_url = node
                .get("display_url")
                .and_then(Value::as_str)
                .map(String::from);
            let (mc, carousel, hv) = media_facts_from_graphql_node(node);
            rp.media_count = mc;
            rp.is_carousel = carousel;
            rp.has_video = hv;
            r.push_post(rp);
        }
    }

    Ok(ParsedProfile {
        result: Some(r),
        missing,
        requires_login: false,
    })
}

/// Checks for IG's logged-out shape. Empirically the API returns
/// `{"data": {}, "status": "ok"}` or includes a `require_login` indicator;
/// we accept several signals.
fn requires_login(doc: &Value) -> bool {
    if doc.get("require_login").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if doc.get("requires_login").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    if let Some(msg) = doc.get("message").and_then(Value::as_str) {
        if msg.to_lowercase().contains("login") {
            return true;
        }
    }
    if let Some(data) = doc.get("data").and_then(Value::as_object) {
        match data.get("user") {
            None => return true,
            Some(u) => {
                if let Some(m) = u.as_object() {
                    if m.is_empty() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Derives (media_count, is_carousel, has_video) from a web_profile_info
/// timeline node. Carousels expose `edge_sidecar_to_children`; a node's own
/// `is_video` covers single videos. Shape-tolerant — a missing sidecar
/// degrades to a single image (count 1).
fn media_facts_from_graphql_node(node: &Value) -> (Option<i64>, Option<bool>, Option<bool>) {
    let top_is_video = node
        .get("is_video")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(children) =
        nav(node, &["edge_sidecar_to_children", "edges"]).and_then(Value::as_array)
    {
        let count = children.len() as i64;
        let child_video = children.iter().any(|c| {
            c.get("node")
                .and_then(|n| n.get("is_video"))
                .and_then(Value::as_bool)
                == Some(true)
        });
        let hv = if top_is_video || child_video {
            Some(true)
        } else {
            None
        };
        return (Some(count), Some(true), hv);
    }
    let hv = if top_is_video { Some(true) } else { None };
    (Some(1), None, hv)
}

fn caption_from_node(node: &Value) -> Option<String> {
    let edges = nav(node, &["edge_media_to_caption", "edges"]).and_then(Value::as_array)?;
    let first = edges.first()?;
    nav(first, &["node", "text"])
        .and_then(Value::as_str)
        .map(String::from)
}

/// Returns the subset of expected dotted paths that are missing or null in doc.
fn validate_schema(doc: &Value, expected: &[&str]) -> Vec<String> {
    let mut missing = Vec::new();
    for p in expected {
        let parts: Vec<&str> = p.split('.').collect();
        match nav(doc, &parts) {
            Some(v) if !v.is_null() => {}
            _ => missing.push((*p).to_string()),
        }
    }
    missing
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

#[derive(Deserialize, Default)]
#[serde(default)]
struct FeedDoc {
    items: Vec<Value>,
    more_available: bool,
    next_max_id: String,
}

pub struct FeedPage {
    pub posts: Vec<RecentPost>,
    /// Raw count of items in the response, so callers can distinguish "IG
    /// returned nothing" from "IG returned items but our extractor couldn't
    /// find shortcodes."
    pub raw_count: usize,
    pub next_max_id: String,
    pub more_available: bool,
}

/// Parses a /api/v1/feed/user/<id>/ response. The mobile-API shape uses
/// `code` (not `shortcode`), `taken_at` (not `taken_at_timestamp`), and
/// `image_versions2.candidates[].url` (not `display_url`). For carousels the
/// top-level item may lack `code`/image; we then fall back to the first child
/// in `carousel_media`.
pub fn parse_user_feed(raw: &[u8]) -> AnyResult<FeedPage> {
    let doc: FeedDoc = serde_json::from_slice(raw)?;

    let mut posts = Vec::new();
    for it in &doc.items {
        if let Some(rp) = post_from_feed_item(it) {
            posts.push(rp);
        }
    }
    Ok(FeedPage {
        posts,
        raw_count: doc.items.len(),
        next_max_id: doc.next_max_id,
        more_available: doc.more_available,
    })
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
    fn parse_web_profile_info_happy_path() {
        let raw = br#"{
          "data": {"user": {
            "id": "42",
            "username": "zuck",
            "full_name": "Mark",
            "biography": "bio",
            "is_private": false,
            "edge_followed_by": {"count": 100},
            "edge_follow": {"count": 5},
            "edge_owner_to_timeline_media": {
              "count": 2,
              "edges": [
                {"node": {
                  "shortcode": "ABC",
                  "is_video": false,
                  "taken_at_timestamp": 1715000000,
                  "display_url": "https://x/1.jpg",
                  "edge_media_to_caption": {"edges": [{"node": {"text": "hi"}}]}
                }},
                {"node": {"shortcode": ""}},
                {"node": {"shortcode": "DEF"}}
              ]
            }
          }}
        }"#;
        let p = parse_web_profile_info("zuck", raw).unwrap();
        assert!(!p.requires_login);
        assert!(p.missing.is_empty());
        let r = p.result.unwrap();
        assert_eq!(r.user_id, "42");
        assert_eq!(r.full_name.as_deref(), Some("Mark"));
        assert_eq!(r.followers, Some(100));
        assert_eq!(r.following, Some(5));
        assert_eq!(r.posts, Some(2));
        let posts = r.recent_posts.as_ref().unwrap();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].shortcode, "ABC");
        assert_eq!(posts[0].url, "https://www.instagram.com/p/ABC/");
        assert_eq!(posts[0].caption.as_deref(), Some("hi"));
        assert_eq!(posts[0].taken_at_unix, Some(1715000000));
        assert_eq!(posts[1].shortcode, "DEF");
        assert_eq!(posts[1].caption, None);
    }

    #[test]
    fn requires_login_shapes() {
        for raw in [
            r#"{"require_login": true}"#,
            r#"{"requires_login": true}"#,
            r#"{"message": "Please Login to continue"}"#,
            r#"{"data": {}, "status": "ok"}"#,
            r#"{"data": {"user": {}}}"#,
        ] {
            let p = parse_web_profile_info("x", raw.as_bytes()).unwrap();
            assert!(p.requires_login, "expected requires_login for {raw}");
        }
        // data.user = null: not a login signal, but data.user is unusable.
        let err = parse_web_profile_info("x", br#"{"data": {"user": null}}"#).unwrap_err();
        assert_eq!(err.to_string(), "data.user missing");
    }

    #[test]
    fn validate_schema_reports_missing_paths() {
        let raw = br#"{"data": {"user": {"username": "zuck", "full_name": null,
            "edge_followed_by": {"count": "12345"}}}}"#;
        let p = parse_web_profile_info("zuck", raw).unwrap();
        assert_eq!(
            p.missing,
            vec![
                "data.user.full_name",
                "data.user.biography",
                "data.user.edge_follow.count",
                "data.user.edge_owner_to_timeline_media.count",
            ]
        );
        // string count still parses
        assert_eq!(p.result.unwrap().followers, Some(12345));
    }

    #[test]
    fn parse_user_feed_shapes() {
        let raw = br#"{
          "items": [
            {"code": "AAA", "taken_at": 1715000000, "media_type": 2,
             "caption": {"text": "vid"},
             "image_versions2": {"candidates": [
               {"url": "https://x/small.jpg", "width": 100},
               {"url": "https://x/big.jpg", "width": 1080}
             ]}},
            {"carousel_media": [
              {"code": "BBB", "image_versions2": {"candidates": [{"url": "https://x/c.jpg", "width": 50}]}}
            ]},
            {"caption": {"text": "no code at all"}}
          ],
          "more_available": true,
          "next_max_id": "cursor123"
        }"#;
        let page = parse_user_feed(raw).unwrap();
        assert_eq!(page.raw_count, 3);
        assert_eq!(page.posts.len(), 2);
        assert!(page.more_available);
        assert_eq!(page.next_max_id, "cursor123");

        let a = &page.posts[0];
        assert_eq!(a.shortcode, "AAA");
        assert_eq!(a.is_video, Some(true));
        assert_eq!(a.caption.as_deref(), Some("vid"));
        assert_eq!(a.display_url.as_deref(), Some("https://x/big.jpg"));

        let b = &page.posts[1];
        assert_eq!(b.shortcode, "BBB");
        assert_eq!(b.is_video, None);
        assert_eq!(b.display_url.as_deref(), Some("https://x/c.jpg"));
    }

    #[test]
    fn media_flags_from_feed() {
        let raw = br#"{
          "items": [
            {"code": "IMG", "media_type": 1,
             "image_versions2": {"candidates": [{"url": "https://x/i.jpg", "width": 640}]}},
            {"code": "VID", "media_type": 2,
             "image_versions2": {"candidates": [{"url": "https://x/v.jpg", "width": 640}]}},
            {"code": "CAR", "media_type": 8, "carousel_media": [
              {"code": "CAR", "media_type": 1, "image_versions2": {"candidates": [{"url": "https://x/1.jpg", "width": 640}]}},
              {"code": "CAR", "media_type": 2, "image_versions2": {"candidates": [{"url": "https://x/2.jpg", "width": 640}]}},
              {"code": "CAR", "media_type": 1, "image_versions2": {"candidates": [{"url": "https://x/3.jpg", "width": 640}]}}
            ]}
          ],
          "more_available": false,
          "next_max_id": ""
        }"#;
        let page = parse_user_feed(raw).unwrap();
        assert_eq!(page.posts.len(), 3);

        let img = &page.posts[0];
        assert_eq!(img.media_count, Some(1));
        assert_eq!(img.is_carousel, None);
        assert_eq!(img.has_video, None);

        let vid = &page.posts[1];
        assert_eq!(vid.media_count, Some(1));
        assert_eq!(vid.is_carousel, None);
        assert_eq!(vid.has_video, Some(true));
        assert_eq!(vid.is_video, Some(true));

        let car = &page.posts[2];
        assert_eq!(car.media_count, Some(3));
        assert_eq!(car.is_carousel, Some(true));
        assert_eq!(car.has_video, Some(true));
    }

    #[test]
    fn media_flags_from_graphql() {
        let raw = br#"{
          "data": {"user": {
            "id": "1", "username": "u",
            "edge_followed_by": {"count": 0}, "edge_follow": {"count": 0},
            "edge_owner_to_timeline_media": {"count": 3, "edges": [
              {"node": {"shortcode": "IMG", "is_video": false}},
              {"node": {"shortcode": "VID", "is_video": true}},
              {"node": {"shortcode": "CAR", "is_video": false,
                "edge_sidecar_to_children": {"edges": [
                  {"node": {"is_video": false}},
                  {"node": {"is_video": true}}
                ]}}}
            ]}
          }}
        }"#;
        let p = parse_web_profile_info("u", raw).unwrap();
        let posts = p.result.unwrap().recent_posts.unwrap();
        assert_eq!(posts.len(), 3);

        assert_eq!(posts[0].media_count, Some(1));
        assert_eq!(posts[0].is_carousel, None);
        assert_eq!(posts[0].has_video, None);

        assert_eq!(posts[1].media_count, Some(1));
        assert_eq!(posts[1].has_video, Some(true));

        assert_eq!(posts[2].media_count, Some(2));
        assert_eq!(posts[2].is_carousel, Some(true));
        assert_eq!(posts[2].has_video, Some(true));
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
