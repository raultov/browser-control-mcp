//! Mocked routes (`mock_route`, `list_routes`, `remove_route`): the table
//! each tab keeps, shared by both browsers. Chrome answers the requests it
//! pauses (`Fetch`) from it, Firefox the ones its intercept blocks
//! (`network.addIntercept`). Every request the browser holds for a route is
//! answered: with the route's response, let through, or failed when the
//! browser refuses the response (#194), whose error the route then keeps.

use crate::handler::cdp_domains::event_pump::ListenerHandles;
use serde_json::{Value, json};

/// Most routes one tab keeps.
pub(crate) const MAX_ROUTES: usize = 50;

/// Most characters of a browser error a route keeps (see `RouteState::note_error`).
const MAX_ERROR_CHARS: usize = 300;

/// Most former Firefox intercepts remembered (see `RouteState::retired`).
pub(crate) const MAX_RETIRED: usize = 8;

/// Largest mocked body, in bytes: a route is held in memory for the tab's life.
pub(crate) const MAX_BODY: usize = 1_000_000;

/// The response a route answers with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MockResponse {
    /// HTTP status, 100 to 599.
    pub(crate) status: u16,
    /// Response headers, in order.
    pub(crate) headers: Vec<(String, String)>,
    /// Response body (text).
    pub(crate) body: String,
}

/// One mocked route.
struct Route {
    id: String,
    url_pattern: String,
    response: MockResponse,
    hits: u64,
    /// Requests failed because the browser refused the route's response.
    errors: u64,
    /// The browser's error for the last of them, cut.
    last_error: Option<String>,
}

/// The routes of one tab, and what answers its intercepted requests while
/// there are any.
#[derive(Default)]
pub(crate) struct RouteState {
    routes: Vec<Route>,
    counter: usize,
    /// The task answering the tab's intercepted requests (aborted on drop).
    pub(crate) listener: ListenerHandles,
    /// Firefox: the `network.addIntercept` id blocking the tab's requests.
    pub(crate) intercept: Option<String>,
    /// Firefox: the tab's former intercepts, newest last (at most
    /// [`MAX_RETIRED`]). Firefox keeps a request blocked when its intercept
    /// is removed, so what they blocked is still let through (#195).
    pub(crate) retired: std::collections::VecDeque<String>,
    /// Firefox: the tab is offline (`set_offline`), which fails the mocked
    /// requests too (#108).
    pub(crate) offline: bool,
    /// Chrome: the tab's watched frames of another origin, by target id
    /// (at most `frame_watch::MAX_WATCHED`): each pauses its own requests
    /// in its own session, which the tab's `Fetch` does not see (#199).
    pub(crate) frames: std::collections::HashMap<
        String,
        crate::handler::cdp_domains::fetch::mock_route::FrameRoutes,
    >,
}

impl RouteState {
    /// Adds a route; its id (`route-N`, never reused).
    pub(crate) fn add(
        &mut self,
        url_pattern: &str,
        response: MockResponse,
    ) -> Result<String, String> {
        if self.routes.len() >= MAX_ROUTES {
            return Err(format!(
                "the tab already has {MAX_ROUTES} routes: remove some with remove_route first"
            ));
        }
        self.counter += 1;
        let id = format!("route-{}", self.counter);
        self.routes.push(Route {
            id: id.clone(),
            url_pattern: url_pattern.to_string(),
            response,
            hits: 0,
            errors: 0,
            last_error: None,
        });
        Ok(id)
    }

    /// Removes route `id`, or every route when `None`; the removed ids. An
    /// unknown id fails listing the tab's routes.
    pub(crate) fn remove(&mut self, id: Option<&str>) -> Result<Vec<String>, String> {
        let Some(id) = id else {
            return Ok(self.routes.drain(..).map(|r| r.id).collect());
        };
        match self.routes.iter().position(|r| r.id == id) {
            Some(at) => Ok(vec![self.routes.remove(at).id]),
            None => {
                let ids: Vec<&str> = self.routes.iter().map(|r| r.id.as_str()).collect();
                Err(format!(
                    "no route '{id}' on this tab; its routes: [{}]",
                    ids.join(", ")
                ))
            }
        }
    }

    /// The response for a request to `url`, with the id of the route that
    /// gives it: the latest route whose pattern matches it (counted as a
    /// hit), or `None` to let it through. The URL's `#fragment` is left
    /// out, as Chrome's paused URL leaves it out (#201: Firefox's keeps it).
    pub(crate) fn answer(&mut self, url: &str) -> Option<(String, MockResponse)> {
        let url = url.split_once('#').map_or(url, |(sent, _)| sent);
        let route = self
            .routes
            .iter_mut()
            .rev()
            .find(|r| glob_matches(&r.url_pattern, url))?;
        route.hits += 1;
        Some((route.id.clone(), route.response.clone()))
    }

    /// Notes that the browser refused route `id`'s response with `error`
    /// (its own words, cut): `list_routes` shows it, since the request then
    /// fails. A route removed meanwhile is left alone.
    pub(crate) fn note_error(&mut self, id: &str, error: &str) {
        if let Some(route) = self.routes.iter_mut().find(|r| r.id == id) {
            route.errors += 1;
            route.last_error = Some(crate::util::text::capped(error, MAX_ERROR_CHARS));
        }
    }

    /// The routes' URL patterns, oldest first.
    pub(crate) fn patterns(&self) -> Vec<String> {
        self.routes.iter().map(|r| r.url_pattern.clone()).collect()
    }

    /// Whether the tab has no route.
    pub(crate) fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// `{routes: [{route_id, url_pattern, status, hits, errors?,
    /// last_error?}]}`, oldest first; `errors` and `last_error` once the
    /// browser refused a route's response.
    pub(crate) fn to_json(&self) -> Value {
        let routes: Vec<Value> = self
            .routes
            .iter()
            .map(|r| {
                let mut route = json!({"route_id": r.id, "url_pattern": r.url_pattern,
                       "status": r.response.status, "hits": r.hits});
                if r.errors > 0 {
                    route["errors"] = json!(r.errors);
                    route["last_error"] = json!(r.last_error);
                }
                route
            })
            .collect();
        json!({ "routes": routes })
    }
}

/// The standard reason phrase of HTTP status `status` (RFC 9110 and the
/// IANA registry), or `Unknown`: Chrome refuses a mocked response without
/// one (#194: a mocked 299 left the request paused), and gives none itself
/// for a status it does not know.
pub(crate) fn reason_phrase(status: u16) -> &'static str {
    REASONS
        .iter()
        .find(|(code, _)| *code == status)
        .map_or("Unknown", |(_, phrase)| phrase)
}

/// The registered HTTP statuses and their reason phrases.
const REASONS: &[(u16, &str)] = &[
    (100, "Continue"),
    (101, "Switching Protocols"),
    (102, "Processing"),
    (103, "Early Hints"),
    (200, "OK"),
    (201, "Created"),
    (202, "Accepted"),
    (203, "Non-Authoritative Information"),
    (204, "No Content"),
    (205, "Reset Content"),
    (206, "Partial Content"),
    (207, "Multi-Status"),
    (208, "Already Reported"),
    (226, "IM Used"),
    (300, "Multiple Choices"),
    (301, "Moved Permanently"),
    (302, "Found"),
    (303, "See Other"),
    (304, "Not Modified"),
    (305, "Use Proxy"),
    (307, "Temporary Redirect"),
    (308, "Permanent Redirect"),
    (400, "Bad Request"),
    (401, "Unauthorized"),
    (402, "Payment Required"),
    (403, "Forbidden"),
    (404, "Not Found"),
    (405, "Method Not Allowed"),
    (406, "Not Acceptable"),
    (407, "Proxy Authentication Required"),
    (408, "Request Timeout"),
    (409, "Conflict"),
    (410, "Gone"),
    (411, "Length Required"),
    (412, "Precondition Failed"),
    (413, "Content Too Large"),
    (414, "URI Too Long"),
    (415, "Unsupported Media Type"),
    (416, "Range Not Satisfiable"),
    (417, "Expectation Failed"),
    (421, "Misdirected Request"),
    (422, "Unprocessable Content"),
    (423, "Locked"),
    (424, "Failed Dependency"),
    (425, "Too Early"),
    (426, "Upgrade Required"),
    (428, "Precondition Required"),
    (429, "Too Many Requests"),
    (431, "Request Header Fields Too Large"),
    (451, "Unavailable For Legal Reasons"),
    (500, "Internal Server Error"),
    (501, "Not Implemented"),
    (502, "Bad Gateway"),
    (503, "Service Unavailable"),
    (504, "Gateway Timeout"),
    (505, "HTTP Version Not Supported"),
    (506, "Variant Also Negotiates"),
    (507, "Insufficient Storage"),
    (508, "Loop Detected"),
    (511, "Network Authentication Required"),
];

/// Whether `url` matches the glob `pattern`, as Chrome's `Fetch` patterns
/// do: `*` is any run of characters, `?` exactly one, `\` escapes the next.
pub(crate) fn glob_matches(pattern: &str, url: &str) -> bool {
    let pattern = glob_tokens(pattern);
    let url: Vec<char> = url.chars().collect();
    // Greedy match with one backtrack point: the last `*` seen.
    let (mut p, mut u) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while u < url.len() {
        match pattern.get(p) {
            Some(Glob::Any) => {
                star = Some((p, u));
                p += 1;
            }
            Some(Glob::One) => (p, u) = (p + 1, u + 1),
            Some(Glob::Char(c)) if *c == url[u] => (p, u) = (p + 1, u + 1),
            _ => match star {
                Some((star_p, star_u)) => {
                    star = Some((star_p, star_u + 1));
                    (p, u) = (star_p + 1, star_u + 1);
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|t| *t == Glob::Any)
}

/// One element of a glob pattern.
#[derive(Debug, PartialEq)]
enum Glob {
    /// `*`.
    Any,
    /// `?`.
    One,
    /// A literal character (escaped or not).
    Char(char),
}

fn glob_tokens(pattern: &str) -> Vec<Glob> {
    let mut chars = pattern.chars();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '*' => Glob::Any,
            '?' => Glob::One,
            '\\' => Glob::Char(chars.next().unwrap_or('\\')),
            other => Glob::Char(other),
        });
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(body: &str) -> MockResponse {
        MockResponse {
            status: 200,
            headers: vec![],
            body: body.into(),
        }
    }

    /// NET-01: route patterns are globs, as Chrome's Fetch patterns.
    #[test]
    fn patterns_are_globs() {
        let cases = [
            ("*", "http://a/x", true),
            ("*/api/*", "http://a/api/users?id=1", true),
            ("*/api/*", "http://a/static/app.js", false),
            ("http://a/v?/x", "http://a/v1/x", true),
            ("http://a/v?/x", "http://a/v10/x", false),
            ("http://a/x", "http://a/x", true),
            ("http://a/x", "http://a/x?q", false),
            (r"*\?q=1", "http://a/x?q=1", true),
            (r"*\?q=1", "http://a/xq=1", false),
            (r"*\*", "http://a/*", true),
            ("**.json", "http://a/b/c.json", true),
        ];
        for (pattern, url, expected) in cases {
            assert_eq!(glob_matches(pattern, url), expected, "{pattern} vs {url}");
        }
    }

    /// NET-02: the latest matching route answers and counts the hit; a
    /// request no route matches goes through.
    #[test]
    fn latest_matching_route_answers() {
        // Given a broad route, then a narrower one added later
        let mut routes = RouteState::default();
        let broad = routes.add("*/api/*", ok("broad")).unwrap();
        let narrow = routes.add("*/api/users*", ok("narrow")).unwrap();
        assert_eq!((broad.as_str(), narrow.as_str()), ("route-1", "route-2"));
        // When requests come
        let (users_route, users) = routes.answer("http://a/api/users?id=1").unwrap();
        let (_, other) = routes.answer("http://a/api/items").unwrap();
        assert_eq!(users_route, "route-2");
        // Then the latest matching route answers each, and others go through
        assert_eq!(
            (users.body.as_str(), other.body.as_str()),
            ("narrow", "broad")
        );
        assert_eq!(routes.answer("http://a/index.html"), None);
        assert_eq!(routes.patterns(), vec!["*/api/*", "*/api/users*"]);
        let listed = routes.to_json();
        assert_eq!(
            listed,
            json!({"routes": [
                {"route_id": "route-1", "url_pattern": "*/api/*", "status": 200, "hits": 1},
                {"route_id": "route-2", "url_pattern": "*/api/users*", "status": 200, "hits": 1},
            ]})
        );
    }

    /// #201: a request's `#fragment` is not part of what a route matches:
    /// the fragment is never sent, Chrome's paused URL leaves it out and
    /// Firefox's keeps it, so the same glob matches the same requests on
    /// both browsers.
    #[test]
    fn fragments_are_not_matched() {
        let mut routes = RouteState::default();
        routes.add("*/frag.txt", ok("G")).unwrap();
        assert!(routes.answer("http://a/frag.txt#part").is_some());
        assert!(routes.answer("http://a/frag.txt?q=1#x").is_none());
        routes.add("*?q=1", ok("Q")).unwrap();
        let (_, response) = routes.answer("http://a/frag.txt?q=1#x").unwrap();
        assert_eq!(response.body, "Q");
    }

    /// #194: a route keeps how many of its responses the browser refused,
    /// and the browser's last error, cut; list_routes shows them only
    /// then. An error for a route removed meanwhile is dropped.
    #[test]
    fn refused_responses_are_listed_on_their_route() {
        let mut routes = RouteState::default();
        let id = routes.add("*/api/*", ok("")).unwrap();
        routes.note_error(&id, "Invalid http status code or phrase");
        routes.note_error(&id, &"e".repeat(MAX_ERROR_CHARS + 10));
        routes.note_error("route-9", "gone");
        let listed = &routes.to_json()["routes"][0];
        assert_eq!(listed["errors"], 2);
        let last = listed["last_error"].as_str().unwrap();
        assert!(last.starts_with(&"e".repeat(MAX_ERROR_CHARS)), "{last}");
        assert!(last.contains("[cut: the first 300"), "{last}");
        routes.add("*/b", ok("")).unwrap();
        let quiet = &routes.to_json()["routes"][1];
        assert!(quiet.get("errors").is_none() && quiet.get("last_error").is_none());
    }

    /// #194: every status gets a reason phrase: its registered one, or
    /// `Unknown` (which Chrome needs for a mocked 299).
    #[test]
    fn every_status_has_a_reason_phrase() {
        let phrases: Vec<&str> = [200, 201, 418, 503, 299, 250, 599]
            .into_iter()
            .map(reason_phrase)
            .collect();
        assert_eq!(
            phrases,
            [
                "OK",
                "Created",
                "Unknown",
                "Service Unavailable",
                "Unknown",
                "Unknown",
                "Unknown"
            ]
        );
    }

    /// NET-03: routes are removed by id or all at once; ids are not
    /// reused; an unknown id lists the tab's routes; the table is bounded.
    #[test]
    fn routes_are_removed_and_bounded() {
        let mut routes = RouteState::default();
        routes.add("*/a", ok("")).unwrap();
        routes.add("*/b", ok("")).unwrap();
        assert_eq!(
            routes.remove(Some("route-1")),
            Ok(vec!["route-1".to_string()])
        );
        let unknown = routes.remove(Some("route-1")).unwrap_err();
        assert!(unknown.contains("route-2"), "{unknown}");
        assert_eq!(routes.add("*/c", ok("")).unwrap(), "route-3");
        assert_eq!(
            routes.remove(None),
            Ok(vec!["route-2".to_string(), "route-3".to_string()])
        );
        assert!(routes.is_empty());
        for n in 0..MAX_ROUTES {
            routes.add(&format!("*/{n}"), ok("")).unwrap();
        }
        let full = routes.add("*/more", ok("")).unwrap_err();
        assert!(full.contains("50"), "{full}");
    }
}
