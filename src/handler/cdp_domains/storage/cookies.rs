use crate::handler::McpHandler;
use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};

#[macros::mcp_tool(
    name = "cookies",
    description = "Reads, sets or deletes the browser's cookies ('action'). 'get' lists the cookies the page's URL (or 'url') would be sent, or all those of 'domain', optionally only 'name': name, domain, path, expires (Unix seconds; null for a session cookie), http_only, secure, same_site and size; values are hidden ('value_length' instead) unless 'show_values' is true, as they often hold session tokens. 'set' sets 'name' = 'value' for 'url' or 'domain' (default: the page's URL), with optional 'path', 'expires', 'http_only', 'secure', 'same_site'. 'delete' deletes the cookies 'get' would list with the same filters ('name' to delete one). Side effects: 'set' and 'delete' change what the browser sends from now on (reload to see the effect). Returns: JSON {cookies: [...]} for 'get' (at most 200), or what was set or deleted. Use this to inspect or change a session, log out, or test cookie handling. Alternatives: 'web_storage' for localStorage and sessionStorage; 'navigate' with copy_cookies to import cookies from your Chrome."
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct CookiesTool {
    /// Chrome instance id from open_instance/list_instances. Omit for the default instance.
    pub instance_id: Option<String>,
    /// The Tab ID of the target tab. Omit to use the active tab.
    pub tab_id: Option<String>,
    /// What to do: 'get', 'set' or 'delete'.
    pub action: CookieAction,
    /// The cookie's name: required for 'set'; a filter for 'get' and 'delete'.
    pub name: Option<String>,
    /// The cookie's value ('set' only).
    pub value: Option<String>,
    /// The URL the cookies are for. Defaults to: the page's URL (unless 'domain' is given).
    pub url: Option<String>,
    /// A domain instead of a URL: 'get' and 'delete' take every cookie of it and its subdomains; 'set' sets a domain cookie.
    pub domain: Option<String>,
    /// The cookie's path ('set'). Defaults to: '/'.
    pub path: Option<String>,
    /// When the cookie expires, in Unix seconds ('set'). Defaults to: a session cookie.
    pub expires: Option<f64>,
    /// Hide the cookie from JavaScript ('set'). Defaults to: false.
    pub http_only: Option<bool>,
    /// Send the cookie over HTTPS only ('set'). Defaults to: false.
    pub secure: Option<bool>,
    /// The cookie's SameSite ('set'). Defaults to: the browser's.
    pub same_site: Option<SameSite>,
    /// Return the values ('get'). Defaults to: false (values hidden, they are often session tokens).
    pub show_values: Option<bool>,
}

/// The `action` of `cookies`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum CookieAction {
    /// List cookies.
    #[serde(rename = "get")]
    Get,
    /// Set one cookie.
    #[serde(rename = "set")]
    Set,
    /// Delete cookies.
    #[serde(rename = "delete")]
    Delete,
}

/// A cookie's SameSite.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema,
)]
pub enum SameSite {
    /// Same site only.
    Strict,
    /// Same site, and top-level navigations from other sites.
    Lax,
    /// Every request (needs 'secure').
    None,
}

impl SameSite {
    fn name(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

/// Most cookies `get` returns.
const MAX_COOKIES: usize = 200;

/// A cookie as both browsers report it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cookie {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) domain: String,
    pub(crate) path: String,
    /// Unix seconds; `None` for a session cookie.
    pub(crate) expires: Option<f64>,
    pub(crate) http_only: bool,
    pub(crate) secure: bool,
    pub(crate) same_site: Option<String>,
}

impl Cookie {
    /// From a CDP `Network.Cookie`.
    pub(crate) fn from_cdp(raw: &Value) -> Self {
        let text = |key: &str| raw[key].as_str().unwrap_or_default().to_string();
        let session = raw["session"].as_bool().unwrap_or(false);
        Self {
            name: text("name"),
            value: text("value"),
            domain: text("domain"),
            path: text("path"),
            expires: raw["expires"].as_f64().filter(|e| !session && *e > 0.0),
            http_only: raw["httpOnly"].as_bool().unwrap_or(false),
            secure: raw["secure"].as_bool().unwrap_or(false),
            same_site: raw["sameSite"].as_str().map(str::to_string),
        }
    }

    /// From a BiDi `network.Cookie` (a binary value is shown as base64).
    pub(crate) fn from_bidi(raw: &Value) -> Self {
        let text = |key: &str| raw[key].as_str().unwrap_or_default().to_string();
        let same_site = raw["sameSite"].as_str().map(|s| {
            let mut name: Vec<char> = s.chars().collect();
            if let Some(first) = name.first_mut() {
                *first = first.to_ascii_uppercase();
            }
            name.into_iter().collect()
        });
        Self {
            name: text("name"),
            value: raw["value"]["value"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            domain: text("domain"),
            path: text("path"),
            expires: raw["expiry"].as_f64(),
            http_only: raw["httpOnly"].as_bool().unwrap_or(false),
            secure: raw["secure"].as_bool().unwrap_or(false),
            same_site,
        }
    }

    /// Whether it belongs to `domain` or one of its subdomains.
    fn of_domain(&self, domain: &str) -> bool {
        let wanted = domain.trim_start_matches('.').to_ascii_lowercase();
        let own = self.domain.trim_start_matches('.').to_ascii_lowercase();
        own == wanted || own.ends_with(&format!(".{wanted}"))
    }

    /// Whether a request to `url` would carry it (domain, path, secure).
    pub(crate) fn sent_to(&self, url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
        let own = self.domain.to_ascii_lowercase();
        let host_ok = if let Some(parent) = own.strip_prefix('.') {
            host == parent || host.ends_with(&own)
        } else {
            host == own
        };
        let path_ok = parsed.path().starts_with(&self.path);
        host_ok && path_ok && (!self.secure || parsed.scheme() == "https")
    }

    fn to_json(&self, show_value: bool) -> Value {
        let mut out = json!({
            "name": self.name, "domain": self.domain, "path": self.path,
            "expires": self.expires, "http_only": self.http_only, "secure": self.secure,
            "same_site": self.same_site, "size": self.name.len() + self.value.len(),
        });
        if show_value {
            out["value"] = json!(self.value);
        } else {
            out["value_length"] = json!(self.value.len());
        }
        out
    }
}

impl CookiesTool {
    /// Checks the arguments (before any browser work).
    pub(crate) fn check_arguments(&self) -> Result<(), CallToolError> {
        let fail = |message: &str| Err(CallToolError::from_message(message));
        if self.action == CookieAction::Set && (self.name.is_none() || self.value.is_none()) {
            return fail("'set' needs 'name' and 'value'");
        }
        if self.url.is_some() && self.domain.is_some() {
            return fail("give 'url' or 'domain', not both");
        }
        Ok(())
    }

    /// The URL the call is about: 'url', else the page's.
    pub(crate) fn url_or(&self, page: &str) -> String {
        self.url.clone().unwrap_or_else(|| page.to_string())
    }

    /// The cookies the filters select from `all` (those sent to `url`
    /// unless 'domain' is given; 'name' when given).
    pub(crate) fn select(&self, all: Vec<Cookie>, url: &str) -> Vec<Cookie> {
        all.into_iter()
            .filter(|c| match &self.domain {
                Some(domain) => c.of_domain(domain),
                None => c.sent_to(url),
            })
            .filter(|c| self.name.as_ref().is_none_or(|n| *n == c.name))
            .collect()
    }

    /// `get`'s reply (shared by every browser).
    pub(crate) fn listed(&self, cookies: &[Cookie], url: &str) -> CallToolResult {
        let show = self.show_values.unwrap_or(false);
        let shown: Vec<Value> = cookies
            .iter()
            .take(MAX_COOKIES)
            .map(|c| c.to_json(show))
            .collect();
        let scope = self
            .domain
            .as_ref()
            .map_or_else(|| format!("sent to {url}"), |d| format!("of {d}"));
        let mut first = format!("{} cookie(s) {scope}", cookies.len());
        if cookies.len() > MAX_COOKIES {
            first.push_str(&format!(" (showing {MAX_COOKIES})"));
        }
        if !show && !cookies.is_empty() {
            first.push_str("; values hidden (show_values: true returns them)");
        }
        let body = serde_json::to_string_pretty(&json!({ "cookies": shown })).unwrap_or_default();
        CallToolResult::text_content(vec![format!("{first}.\n{body}").into()])
    }

    /// `set`'s reply.
    pub(crate) fn set_reply(&self, target: &str) -> CallToolResult {
        let name = self.name.as_deref().unwrap_or_default();
        CallToolResult::text_content(vec![format!("Set cookie '{name}' for {target}.").into()])
    }

    /// Where 'set' put `cookie`: its domain, or `url`.
    pub(crate) fn set_place(&self, cookie: &Cookie, url: &str) -> String {
        if self.domain.is_some() {
            format!("domain {}", cookie.domain)
        } else {
            url.to_string()
        }
    }

    /// The cookie 'set' sets, for `url` (when no 'domain' is given).
    pub(crate) fn wanted(&self, url: &str) -> Cookie {
        let host = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        Cookie {
            name: self.name.clone().unwrap_or_default(),
            value: self.value.clone().unwrap_or_default(),
            domain: self.domain.clone().unwrap_or(host),
            path: self.path.clone().unwrap_or_else(|| "/".into()),
            expires: self.expires,
            http_only: self.http_only.unwrap_or(false),
            secure: self.secure.unwrap_or(false),
            same_site: self.same_site.map(|s| s.name().to_string()),
        }
    }

    pub async fn handle(
        params: CallToolRequestParams,
        handler: &McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: CookiesTool = crate::handler::parse_args(params)?;
        args.check_arguments()?;
        let target =
            crate::handler::resolve_target(handler, args.instance_id.clone(), args.tab_id.clone())
                .await?;
        let page = crate::handler::snippet_value(&target, "location.href").await?;
        let url = args.url_or(page.as_str().unwrap_or_default());
        match args.action {
            CookieAction::Get => {
                Ok(args.listed(&chrome_cookies(&target, &args, &url).await?, &url))
            }
            CookieAction::Set => chrome_set(&target, &args, &url).await,
            CookieAction::Delete => {
                let found = chrome_cookies(&target, &args, &url).await?;
                for cookie in &found {
                    let params =
                        json!({"name": cookie.name, "domain": cookie.domain, "path": cookie.path});
                    crate::handler::send_or(&target, "Network.deleteCookies", params, "CDP Error")
                        .await?;
                }
                Ok(deleted_reply(&found))
            }
        }
    }
}

/// The cookies `args` select on Chrome: those sent to `url`
/// (`Network.getCookies`), or all the browser's for a 'domain'
/// (`Storage.getCookies`).
async fn chrome_cookies(
    target: &CdpTarget,
    args: &CookiesTool,
    url: &str,
) -> Result<Vec<Cookie>, CallToolError> {
    let (method, params) = if args.domain.is_some() {
        ("Storage.getCookies", json!({}))
    } else {
        ("Network.getCookies", json!({ "urls": [url] }))
    };
    let reply = crate::handler::send_or(target, method, params, "CDP Error").await?;
    let raw = reply.result.unwrap_or_default();
    let all = raw["cookies"]
        .as_array()
        .map(|c| c.iter().map(Cookie::from_cdp).collect())
        .unwrap_or_default();
    Ok(args.select(all, url))
}

/// `Network.setCookie` for `args`.
async fn chrome_set(
    target: &CdpTarget,
    args: &CookiesTool,
    url: &str,
) -> Result<CallToolResult, CallToolError> {
    let cookie = args.wanted(url);
    let mut params = json!({
        "name": cookie.name, "value": cookie.value, "path": cookie.path,
        "secure": cookie.secure, "httpOnly": cookie.http_only,
    });
    let place = args.set_place(&cookie, url);
    if args.domain.is_some() {
        params["domain"] = json!(cookie.domain);
    } else {
        params["url"] = json!(url);
    }
    if let Some(expires) = cookie.expires {
        params["expires"] = json!(expires);
    }
    if let Some(same_site) = &cookie.same_site {
        params["sameSite"] = json!(same_site);
    }
    let reply = crate::handler::send_or(target, "Network.setCookie", params, "CDP Error").await?;
    if reply.result.as_ref().and_then(|r| r["success"].as_bool()) == Some(false) {
        return Err(CallToolError::from_message(
            "Chrome refused the cookie (an invalid domain for the URL, or SameSite=None without 'secure')",
        ));
    }
    Ok(args.set_reply(&place))
}

/// `delete`'s reply (shared by every browser).
pub(crate) fn deleted_reply(deleted: &[Cookie]) -> CallToolResult {
    let names: Vec<String> = deleted
        .iter()
        .map(|c| format!("{} ({})", c.name, c.domain))
        .collect();
    let text = if names.is_empty() {
        "No cookie matched; nothing was deleted.".to_string()
    } else {
        format!("Deleted {} cookie(s): {}.", names.len(), names.join(", "))
    };
    CallToolResult::text_content(vec![text.into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, domain: &str, path: &str, secure: bool) -> Cookie {
        Cookie {
            name: name.into(),
            value: "s3cret-token".into(),
            domain: domain.into(),
            path: path.into(),
            expires: None,
            http_only: true,
            secure,
            same_site: Some("Lax".into()),
        }
    }

    fn tool(args: Value) -> CookiesTool {
        serde_json::from_value(args).unwrap()
    }

    /// ISSUE-8: a cookie is sent to a URL by domain (host-only or with
    /// subdomains), path and secure, as browsers decide.
    #[test]
    fn cookies_are_matched_like_browsers_do() {
        let url = "https://app.example.com/account/settings";
        assert!(cookie("a", "app.example.com", "/", false).sent_to(url));
        assert!(cookie("a", ".example.com", "/account", true).sent_to(url));
        assert!(
            !cookie("a", "example.com", "/", false).sent_to(url),
            "host-only of the parent"
        );
        assert!(!cookie("a", ".example.com", "/admin", false).sent_to(url));
        assert!(!cookie("a", ".example.com", "/", true).sent_to("http://app.example.com/"));
        assert!(cookie("a", ".example.com", "/", false).of_domain("example.com"));
        assert!(cookie("a", "app.example.com", "/", false).of_domain("example.com"));
        assert!(!cookie("a", "notexample.com", "/", false).of_domain("example.com"));
    }

    /// ISSUE-8: values stay hidden unless asked for; the filters select.
    #[test]
    fn values_are_hidden_unless_asked() {
        let all = vec![
            cookie("sid", ".example.com", "/", false),
            cookie("other", "x.org", "/", false),
        ];
        let get = tool(json!({"action": "get"}));
        let selected = get.select(all.clone(), "https://example.com/");
        assert_eq!(selected.len(), 1);
        let text = crate::handler::cdp_domains::tests::reply_text(
            &get.listed(&selected, "https://example.com/"),
        );
        assert!(
            text.starts_with("1 cookie(s) sent to https://example.com/; values hidden"),
            "{text}"
        );
        assert!(
            !text.contains("s3cret-token") && text.contains("\"value_length\": 12"),
            "{text}"
        );
        let shown = tool(json!({"action": "get", "show_values": true}));
        let text = crate::handler::cdp_domains::tests::reply_text(&shown.listed(&selected, "u"));
        assert!(text.contains("\"value\": \"s3cret-token\""), "{text}");
        let named = tool(json!({"action": "get", "domain": "x.org", "name": "nope"}));
        assert!(named.select(all, "https://example.com/").is_empty());
    }

    /// ISSUE-8: 'set' needs a name and a value; 'url' and 'domain' exclude
    /// each other.
    #[test]
    fn arguments_are_checked() {
        let error = |args: Value| tool(args).check_arguments().unwrap_err().to_string();
        assert!(error(json!({"action": "set", "name": "a"})).contains("needs 'name' and 'value'"));
        assert!(
            error(json!({"action": "get", "url": "https://a/", "domain": "a"}))
                .contains("not both")
        );
    }

    /// ISSUE-8 (Chrome): get asks Network.getCookies for the page's URL;
    /// set sends the cookie with the page's URL.
    #[tokio::test]
    async fn chrome_reads_and_sets_cookies() {
        let mock = crate::handler::cdp_domains::tests::spawn_recording_mock().await;
        mock.respond_with(
            "Runtime.evaluate",
            |_| json!({"result": {"result": {"type": "string", "value": "https://example.com/a"}}}),
        );
        mock.respond_with("Network.getCookies", |_| {
            json!({"cookies": [{"name": "sid", "value": "v", "domain": "example.com", "path": "/", "expires": -1, "session": true, "httpOnly": true, "secure": true, "sameSite": "Lax"}]})
        });
        mock.respond_with("Network.setCookie", |_| json!({"success": true}));
        let handler = crate::handler::cdp_domains::tests::mock_handler(mock.http_port);
        let call = |args| crate::handler::cdp_domains::tests::call("cookies", args);
        let listed = CookiesTool::handle(call(json!({"action": "get"})), &handler)
            .await
            .unwrap();
        let text = crate::handler::cdp_domains::tests::reply_text(&listed);
        assert!(
            text.starts_with("1 cookie(s) sent to https://example.com/a"),
            "{text}"
        );
        assert_eq!(
            mock.commands_named("Network.getCookies")[0].params,
            json!({"urls": ["https://example.com/a"]})
        );
        let set = json!({"action": "set", "name": "theme", "value": "dark"});
        let reply = CookiesTool::handle(call(set), &handler).await.unwrap();
        assert_eq!(
            crate::handler::cdp_domains::tests::reply_text(&reply),
            "Set cookie 'theme' for https://example.com/a."
        );
        let sent = &mock.commands_named("Network.setCookie")[0].params;
        assert_eq!(
            (sent["url"].clone(), sent["path"].clone()),
            (json!("https://example.com/a"), json!("/"))
        );
    }
}
