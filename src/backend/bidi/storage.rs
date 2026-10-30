//! The Firefox implementation of `cookies` (BiDi `storage.*`, in the tab's
//! storage partition) and `web_storage` (the shared snippet), with the same
//! filters and replies as on Chrome.

use super::page;
use super::tools::{ids, target};
use crate::handler::cdp_domains::storage::cookies::{
    Cookie, CookieAction, CookiesTool, deleted_reply,
};
use crate::handler::cdp_domains::storage::web_storage::WebStorageTool;
use crate::handler::{McpHandler, parse_args};
use bidi_browser_lite::Tab;
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

type ToolResult = Result<CallToolResult, CallToolError>;

/// `cookies`.
pub(crate) async fn cookies(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: CookiesTool = parse_args(params)?;
    args.check_arguments()?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let tab = &tab.tab;
    let page_url = page::run_snippet(tab, "location.href").await?;
    let url = args.url_or(page_url.as_str().unwrap_or_default());
    match args.action {
        CookieAction::Get => {
            let found = args.select(all_cookies(tab).await?, &url);
            Ok(args.listed(&found, &url))
        }
        CookieAction::Set => {
            let cookie = args.wanted(&url);
            set_cookie(tab, &cookie).await?;
            Ok(args.set_reply(&args.set_place(&cookie, &url)))
        }
        CookieAction::Delete => {
            let found = args.select(all_cookies(tab).await?, &url);
            for cookie in &found {
                let filter =
                    json!({"name": cookie.name, "domain": cookie.domain, "path": cookie.path});
                let params = json!({"filter": filter, "partition": partition(tab)});
                page::command(tab, "storage.deleteCookies", params).await?;
            }
            Ok(deleted_reply(&found))
        }
    }
}

/// The tab's storage partition (its user context's cookies).
fn partition(tab: &Tab) -> Value {
    json!({"type": "context", "context": tab.context_id()})
}

/// Every cookie of the tab's partition.
async fn all_cookies(tab: &Tab) -> Result<Vec<Cookie>, CallToolError> {
    let params = json!({"partition": partition(tab)});
    let reply = page::command(tab, "storage.getCookies", params).await?;
    Ok(reply["cookies"]
        .as_array()
        .map(|c| c.iter().map(Cookie::from_bidi).collect())
        .unwrap_or_default())
}

/// `storage.setCookie` for `cookie`.
async fn set_cookie(tab: &Tab, cookie: &Cookie) -> Result<(), CallToolError> {
    let mut wire = json!({
        "name": cookie.name,
        "value": {"type": "string", "value": cookie.value},
        "domain": cookie.domain,
        "path": cookie.path,
        "httpOnly": cookie.http_only,
        "secure": cookie.secure,
    });
    if let Some(same_site) = &cookie.same_site {
        wire["sameSite"] = json!(same_site.to_ascii_lowercase());
    }
    if let Some(expires) = cookie.expires {
        wire["expiry"] = json!(expires.max(0.0) as u64);
    }
    let params = json!({"cookie": wire, "partition": partition(tab)});
    page::command(tab, "storage.setCookie", params).await?;
    Ok(())
}

/// `web_storage`: the shared snippet and reply.
pub(crate) async fn web_storage(params: CallToolRequestParams, handler: &McpHandler) -> ToolResult {
    let args: WebStorageTool = parse_args(params)?;
    args.check_arguments()?;
    let (_, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let outcome = page::run_snippet(&tab.tab, &args.expression()).await?;
    args.reply(&outcome)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{self as ts, evaluate_success};
    use super::*;
    use bidi_browser_lite::test_support::MockFirefox;

    /// ISSUE-8 (Firefox): get reads the partition's cookies and keeps
    /// those sent to the page; set sends a BiDi cookie with a lowercase
    /// SameSite; delete removes each selected cookie by name, domain, path.
    #[tokio::test]
    async fn cookies_go_through_bidi_storage() {
        // Given a page at https://example.com/ with two cookies of two sites
        let mock = MockFirefox::start(false).await.unwrap();
        mock.bidi().respond_with("script.evaluate", |_| {
            evaluate_success(json!({"type": "string", "value": "https://example.com/"}))
        });
        mock.bidi().respond_with("storage.getCookies", |_| {
            let cookie = |name: &str, domain: &str| json!({"name": name, "value": {"type": "string", "value": "v"}, "domain": domain, "path": "/", "size": 2, "httpOnly": false, "secure": false, "sameSite": "lax"});
            json!({"cookies": [cookie("sid", ".example.com"), cookie("ad", "tracker.test")], "partitionKey": {}})
        });
        mock.bidi()
            .respond_with("storage.setCookie", |_| json!({"partitionKey": {}}));
        mock.bidi()
            .respond_with("storage.deleteCookies", |_| json!({"partitionKey": {}}));
        let handler = ts::handler(&mock);
        let call = |args| ts::call("cookies", args);
        // When get, set and delete run
        let listed = cookies(call(json!({"action": "get"})), &handler)
            .await
            .unwrap();
        let set = json!({"action": "set", "name": "theme", "value": "dark", "same_site": "Lax"});
        cookies(call(set), &handler).await.unwrap();
        let deleted = cookies(call(json!({"action": "delete", "name": "sid"})), &handler)
            .await
            .unwrap();
        // Then only the page's cookie is listed and deleted, and the set cookie is BiDi's
        assert!(
            ts::text(&listed).starts_with("1 cookie(s) sent to https://example.com/"),
            "{}",
            ts::text(&listed)
        );
        assert_eq!(ts::json_of(&listed)["cookies"][0]["same_site"], "Lax");
        let sent = &mock.bidi().commands_named("storage.setCookie")[0].params["cookie"];
        assert_eq!(
            (sent["domain"].clone(), sent["sameSite"].clone()),
            (json!("example.com"), json!("lax"))
        );
        assert_eq!(sent["value"], json!({"type": "string", "value": "dark"}));
        let removed = &mock.bidi().commands_named("storage.deleteCookies");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].params["filter"]["domain"], ".example.com");
        assert_eq!(
            ts::text(&deleted),
            "Deleted 1 cookie(s): sid (.example.com)."
        );
        ts::stop(&handler).await;
    }
}
