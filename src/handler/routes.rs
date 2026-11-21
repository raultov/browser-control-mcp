//! Mocked routes (`mock_route`, `list_routes`, `remove_route`): the table
//! each tab keeps, shared by both browsers. Chrome answers the requests it
//! pauses (`Fetch`) from it, Firefox the ones its intercept blocks
//! (`network.addIntercept`). A route either answers a request itself
//! ([`RouteAction::Mock`]) or lets it reach the server and changes the
//! status and headers of the server's response ([`RouteAction::Upstream`],
//! #193): the browsers pause such a response before the page gets it.

use crate::handler::cdp_domains::event_pump::ListenerHandles;
use serde_json::{Value, json};

/// Most routes one tab keeps.
pub(crate) const MAX_ROUTES: usize = 50;

/// Most requests answered by a mock remembered (see `RouteState::provided`).
pub(crate) const MAX_PROVIDED: usize = 256;

/// Largest mocked body, in bytes: a route is held in memory for the tab's life.
pub(crate) const MAX_BODY: usize = 1_000_000;

/// HTTP headers, `(name, value)`, in order.
pub(crate) type Headers = Vec<(String, String)>;

/// The response a route answers with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MockResponse {
    /// HTTP status, 100 to 599.
    pub(crate) status: u16,
    /// Response headers, in order.
    pub(crate) headers: Headers,
    /// Response body (text).
    pub(crate) body: String,
}

/// What a route does with the requests it matches.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RouteAction {
    /// Answers them with this response, without reaching the network.
    Mock(MockResponse),
    /// Lets them reach the server, and changes its response so.
    Upstream(ResponseOverride),
}

/// The changes an upstream route makes to the server's response: its body,
/// and every header it does not name, are kept.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ResponseOverride {
    /// The status the page gets instead of the server's.
    pub(crate) status: Option<u16>,
    /// Headers set: each replaces every header of its name (any case).
    pub(crate) headers: Headers,
    /// Names of the headers removed (any case).
    pub(crate) remove: Vec<String>,
}

impl ResponseOverride {
    /// Whether it changes the headers (else the server's go as they are).
    pub(crate) fn changes_headers(&self) -> bool {
        !(self.headers.is_empty() && self.remove.is_empty())
    }

    /// The server's response `headers` with the changes made: the headers
    /// removed or set are dropped, the ones set appended, the others kept
    /// in order, repeated ones (`Set-Cookie`) too.
    pub(crate) fn merged(&self, headers: Headers) -> Headers {
        let kept = headers.into_iter().filter(|(name, _)| !self.names(name));
        kept.chain(self.headers.iter().cloned()).collect()
    }

    /// Whether it sets or removes the header `name` (any case): the
    /// server's headers of that name go.
    pub(crate) fn names(&self, name: &str) -> bool {
        let set = self.headers.iter().map(|(n, _)| n);
        self.remove
            .iter()
            .chain(set)
            .any(|n| n.eq_ignore_ascii_case(name))
    }
}

/// One mocked route.
struct Route {
    id: String,
    url_pattern: String,
    action: RouteAction,
    hits: u64,
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
    /// Firefox: the one blocking its responses, while a route is upstream.
    pub(crate) response_intercept: Option<String>,
    /// Firefox: the requests a mock answered, newest last (at most
    /// [`MAX_PROVIDED`]): Firefox reports their response blocked too, but
    /// it is not, and continuing it fails.
    pub(crate) provided: std::collections::VecDeque<String>,
    /// Firefox: the tab is offline (`set_offline`), which fails the mocked
    /// requests too (#108).
    pub(crate) offline: bool,
}

impl RouteState {
    /// Adds a route; its id (`route-N`, never reused).
    pub(crate) fn add(&mut self, url_pattern: &str, action: RouteAction) -> Result<String, String> {
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
            action,
            hits: 0,
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

    /// The response for a request to `url`, when the latest route whose
    /// pattern matches it is a mock (counted as a hit); `None` lets the
    /// request through (no route matches, or the latest is upstream).
    pub(crate) fn answer(&mut self, url: &str) -> Option<MockResponse> {
        let route = self.latest(url)?;
        let RouteAction::Mock(response) = &route.action else {
            return None;
        };
        route.hits += 1;
        Some(response.clone())
    }

    /// The changes to the server's response to `url`, when the latest
    /// route whose pattern matches it is upstream (counted as a hit);
    /// `None` lets the response through as it is.
    pub(crate) fn changes(&mut self, url: &str) -> Option<ResponseOverride> {
        let route = self.latest(url)?;
        let RouteAction::Upstream(changes) = &route.action else {
            return None;
        };
        route.hits += 1;
        Some(changes.clone())
    }

    /// The latest route whose pattern matches `url`.
    fn latest(&mut self, url: &str) -> Option<&mut Route> {
        self.routes
            .iter_mut()
            .rev()
            .find(|r| glob_matches(&r.url_pattern, url))
    }

    /// The routes' URL patterns, oldest first, each with whether its route
    /// is upstream (its responses are paused, not its requests).
    pub(crate) fn patterns(&self) -> Vec<(String, bool)> {
        let upstream = |r: &Route| matches!(r.action, RouteAction::Upstream(_));
        self.routes
            .iter()
            .map(|r| (r.url_pattern.clone(), upstream(r)))
            .collect()
    }

    /// Whether a route is upstream: the tab's responses must be paused.
    pub(crate) fn has_upstream(&self) -> bool {
        self.patterns().iter().any(|(_, upstream)| *upstream)
    }

    /// Whether the tab has no route.
    pub(crate) fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// `{routes: [{route_id, url_pattern, status, hits, upstream?}]}`,
    /// oldest first; an upstream route's status is the one it sets, or
    /// null (the server's).
    pub(crate) fn to_json(&self) -> Value {
        let routes: Vec<Value> = self
            .routes
            .iter()
            .map(|r| match &r.action {
                RouteAction::Mock(response) => json!({"route_id": r.id,
                    "url_pattern": r.url_pattern, "status": response.status, "hits": r.hits}),
                RouteAction::Upstream(changes) => json!({"route_id": r.id,
                    "url_pattern": r.url_pattern, "status": changes.status, "hits": r.hits,
                    "upstream": true}),
            })
            .collect();
        json!({ "routes": routes })
    }
}

/// Whether a response with `status` and a `Location` header (`located`)
/// is a redirect the browser follows: an upstream route leaves it alone
/// and changes the response at the end of the redirects, on both browsers
/// (Chrome follows a redirect whatever status it is given; Firefox gave
/// the page the changed redirect).
pub(crate) fn is_redirect(status: u64, located: bool) -> bool {
    located && matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The standard reason phrase of HTTP status `status` (RFC 9110 and the
/// IANA registry), or an empty one: Chrome writes it for a status an
/// upstream route sets, and Firefox needs it given (it kept the server's,
/// `503 OK`).
pub(crate) fn reason_phrase(status: u16) -> &'static str {
    REASONS
        .iter()
        .find(|(code, _)| *code == status)
        .map_or("", |(_, phrase)| phrase)
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

    fn ok(body: &str) -> RouteAction {
        RouteAction::Mock(MockResponse {
            status: 200,
            headers: vec![],
            body: body.into(),
        })
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
        let users = routes.answer("http://a/api/users?id=1").unwrap();
        let other = routes.answer("http://a/api/items").unwrap();
        // Then the latest matching route answers each, and others go through
        assert_eq!(
            (users.body.as_str(), other.body.as_str()),
            ("narrow", "broad")
        );
        assert_eq!(routes.answer("http://a/index.html"), None);
        let patterns: Vec<String> = routes.patterns().into_iter().map(|(p, _)| p).collect();
        assert_eq!(patterns, vec!["*/api/*", "*/api/users*"]);
        let listed = routes.to_json();
        assert_eq!(
            listed,
            json!({"routes": [
                {"route_id": "route-1", "url_pattern": "*/api/*", "status": 200, "hits": 1},
                {"route_id": "route-2", "url_pattern": "*/api/users*", "status": 200, "hits": 1},
            ]})
        );
    }

    /// #193: an upstream route changes the server's response instead of
    /// answering: a request it is the latest match for goes through (no
    /// hit), and its response gets the changes (a hit); a mock added later
    /// for the same URLs answers them instead; the routes say which is
    /// which.
    #[test]
    fn upstream_routes_change_responses() {
        // Given an upstream route turning */api/* into a 503
        let mut routes = RouteState::default();
        let changes = ResponseOverride {
            status: Some(503),
            ..Default::default()
        };
        routes
            .add("*/api/*", RouteAction::Upstream(changes.clone()))
            .unwrap();
        assert!(routes.has_upstream());
        // When a request and its response come
        assert_eq!(routes.answer("http://a/api/x"), None);
        assert_eq!(routes.changes("http://a/api/x"), Some(changes));
        assert_eq!(routes.changes("http://a/page"), None);
        // And a mock route for some of them is added
        routes.add("*/api/mock*", ok("mocked")).unwrap();
        // Then the mock answers its requests, and their responses are left
        assert!(routes.answer("http://a/api/mock1").is_some());
        assert_eq!(routes.changes("http://a/api/mock1"), None);
        assert_eq!(
            routes.patterns(),
            vec![
                ("*/api/*".to_string(), true),
                ("*/api/mock*".to_string(), false)
            ]
        );
        assert_eq!(
            routes.to_json(),
            json!({"routes": [
                {"route_id": "route-1", "url_pattern": "*/api/*", "status": 503, "hits": 1, "upstream": true},
                {"route_id": "route-2", "url_pattern": "*/api/mock*", "status": 200, "hits": 1},
            ]})
        );
    }

    /// #193: the headers an upstream route sets replace every header of
    /// their name, in any case; the ones it removes go; the others, a
    /// repeated `Set-Cookie` included, stay in order.
    #[test]
    fn upstream_header_changes_merge_with_the_servers() {
        let changes = ResponseOverride {
            status: None,
            headers: vec![("cache-control".into(), "no-store".into())],
            remove: vec!["ACCESS-CONTROL-ALLOW-ORIGIN".into()],
        };
        let server = vec![
            ("Content-Type".into(), "text/html".into()),
            ("Cache-Control".into(), "max-age=60".into()),
            ("Set-Cookie".into(), "a=1".into()),
            ("Access-Control-Allow-Origin".into(), "*".into()),
            ("Set-Cookie".into(), "b=2".into()),
        ];
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            changes.merged(server),
            pairs(&[
                ("Content-Type", "text/html"),
                ("Set-Cookie", "a=1"),
                ("Set-Cookie", "b=2"),
                ("cache-control", "no-store"),
            ])
        );
        assert!(changes.changes_headers());
        assert!(!ResponseOverride::default().changes_headers());
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
