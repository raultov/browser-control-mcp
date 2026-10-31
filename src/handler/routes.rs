//! Mocked routes (`mock_route`, `list_routes`, `remove_route`): the table
//! each tab keeps, shared by both browsers. Chrome answers the requests it
//! pauses (`Fetch`) from it, Firefox the ones its intercept blocks
//! (`network.addIntercept`).

use crate::handler::cdp_domains::event_pump::ListenerHandles;
use serde_json::{Value, json};

/// Most routes one tab keeps.
pub(crate) const MAX_ROUTES: usize = 50;

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

    /// The response for a request to `url`: the latest route whose pattern
    /// matches it (counted as a hit), or `None` to let it through.
    pub(crate) fn answer(&mut self, url: &str) -> Option<MockResponse> {
        let route = self
            .routes
            .iter_mut()
            .rev()
            .find(|r| glob_matches(&r.url_pattern, url))?;
        route.hits += 1;
        Some(route.response.clone())
    }

    /// The routes' URL patterns, oldest first.
    pub(crate) fn patterns(&self) -> Vec<String> {
        self.routes.iter().map(|r| r.url_pattern.clone()).collect()
    }

    /// Whether the tab has no route.
    pub(crate) fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// `{routes: [{route_id, url_pattern, status, hits}]}`, oldest first.
    pub(crate) fn to_json(&self) -> Value {
        let routes: Vec<Value> = self
            .routes
            .iter()
            .map(|r| {
                json!({"route_id": r.id, "url_pattern": r.url_pattern,
                       "status": r.response.status, "hits": r.hits})
            })
            .collect();
        json!({ "routes": routes })
    }
}

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
        let users = routes.answer("http://a/api/users?id=1").unwrap();
        let other = routes.answer("http://a/api/items").unwrap();
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
