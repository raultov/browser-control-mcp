//! `get_css_styles` on Firefox (ISSUE-59): its inspector (RDP) reads the
//! rules that apply to an element (`pageStyle.getApplied`) and its
//! computed values (`getComputed`). The tab's descriptor gives a target of
//! its own (`getTarget`), detached afterwards, so the debugger's watcher is
//! left alone. The element is found by the same snippet as on Chrome, run
//! in that target's console, and handed to the inspector by its content DOM
//! reference: refs and open shadow roots work as in the other tools.

use super::tools::{ids, target};
use crate::handler::McpHandler;
use crate::handler::css_styles::{self, CssStyles, GetCssStylesTool};
use bidi_browser_lite::RdpClient;
use bidi_browser_lite::bidi_lite::rdp::thread::TargetThread;
use rust_mcp_sdk::schema::{CallToolError, CallToolRequestParams, CallToolResult};
use serde_json::{Value, json};

/// Why `get_css_styles` cannot run on a Firefox without its debugger port.
pub(crate) const NO_DEBUGGER: &str = "get_css_styles on Firefox reads styles with its inspector, which needs its debugger port (the server runs with --no-firefox-debugger, or the attached Firefox has no --firefox-rdp-port)";

/// `get_css_styles` on a Firefox instance.
pub(crate) async fn css_styles(
    params: CallToolRequestParams,
    handler: &McpHandler,
) -> Result<CallToolResult, CallToolError> {
    let args: GetCssStylesTool = crate::handler::parse_args(params)?;
    let (session, _, tab) = target(handler, &ids(&args.instance_id, &args.tab_id)).await?;
    let browser = session.browser().await?;
    let rdp = browser
        .rdp_client()
        .await
        .map_err(|_| CallToolError::from_message(NO_DEBUGGER))?;
    let styles = read(&rdp, tab.tab.context_id(), &tab.url, &args).await;
    Ok(args.answer(styles.map_err(CallToolError::from_message)?))
}

/// The styles through a target of the tab of `context`, detached after.
async fn read(
    rdp: &RdpClient,
    context: &str,
    page: &str,
    args: &GetCssStylesTool,
) -> Result<CssStyles, String> {
    let descriptor = rdp
        .tab_for_context(context)
        .await
        .map_err(|e| format!("Firefox's debugger could not find the tab: {e}"))?
        .ok_or("Firefox's debugger does not know this tab (closed, or still opening): try again, or call list_tabs")?;
    let form = request(rdp, &descriptor.actor, "getTarget", json!({})).await?["frame"].clone();
    let read = read_in(rdp, &form, page, args).await;
    if let Some(actor) = form["actor"].as_str() {
        let _ = rdp.request(actor, "detach", json!({})).await;
    }
    read
}

/// The styles through the target `form` describes.
async fn read_in(
    rdp: &RdpClient,
    form: &Value,
    page: &str,
    args: &GetCssStylesTool,
) -> Result<CssStyles, String> {
    let actor = |key: &str| {
        let found = form[key].as_str().map(str::to_string);
        found.ok_or_else(|| format!("Firefox's target has no {key}"))
    };
    let inspector = actor("inspectorActor")?;
    let node = node_of(rdp, form, &inspector, &args.selector).await?;
    let node_actor = node["actor"].clone();
    let page_style = request(rdp, &inspector, "getPageStyle", json!({})).await?;
    let page_style = page_style["pageStyle"]["actor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let inherited = args.include_inherited == Some(true);
    let applied = |filter: &str| json!({"node": node_actor, "inherited": inherited, "matchedSelectors": true, "filter": filter});
    let mut rules = request(rdp, &page_style, "getApplied", applied("user")).await?;
    if args.include_user_agent == Some(true) {
        let user = css_styles::rule_actors(&rules);
        rules = request(rdp, &page_style, "getApplied", applied("ua")).await?;
        css_styles::mark_user_agent(&mut rules, &user);
    }
    let computed = json!({"node": node_actor, "filter": "user"});
    let computed = request(rdp, &page_style, "getComputed", computed).await?;
    let label = node_label(&node);
    let sheets = css_styles::Sheets {
        page: page.to_string(),
        by_id: sheet_urls(&thread_of(rdp, form), &args.selector, &rules).await,
    };
    Ok(css_styles::from_firefox(&label, &rules, &computed, &sheets))
}

/// The URLs of the sheets the rules of `applied` come from, by their
/// `parentStyleSheet` id, found in the page from one rule of each (see
/// `css_styles::sheet_lookup`, ISSUE-86). Best effort: a sheet not found
/// is left out (its rules then name the page).
async fn sheet_urls(
    thread: &TargetThread,
    selector: &str,
    applied: &Value,
) -> std::collections::HashMap<String, String> {
    let samples = css_styles::firefox_sheet_samples(applied);
    if samples.is_empty() {
        return Default::default();
    }
    let lookup = css_styles::sheet_lookup(&crate::util::js::element(selector), &samples);
    let found = thread.evaluate(&lookup).await.ok();
    let text = found.filter(|f| f.exception.is_none());
    let text = text.and_then(|f| f.result.as_str().map(str::to_string));
    let urls = text.and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let urls = urls
        .as_ref()
        .and_then(Value::as_object)
        .into_iter()
        .flatten();
    urls.filter_map(|(id, url)| Some((id.clone(), url.as_str()?.to_string())))
        .collect()
}

/// The thread (and console) of the target `form` describes.
fn thread_of(rdp: &RdpClient, form: &Value) -> TargetThread {
    TargetThread::new(
        rdp,
        form["threadActor"].as_str().unwrap_or_default(),
        form["consoleActor"].as_str().unwrap_or_default(),
    )
}

/// The inspector's node form of the element `selector` names.
async fn node_of(
    rdp: &RdpClient,
    form: &Value,
    inspector: &str,
    selector: &str,
) -> Result<Value, String> {
    let thread = thread_of(rdp, form);
    let expression = crate::util::js::element(selector);
    let evaluation = thread
        .evaluate(&expression)
        .await
        .map_err(|e| format!("Firefox could not look for the element: {e}"))?;
    if let Some(exception) = &evaluation.exception {
        return Err(super::grip::thrown(&evaluation.raw, exception));
    }
    let reference = evaluation.result.get("contentDomReference").cloned();
    let reference = reference.ok_or_else(|| css_styles::not_found(selector))?;
    let walker = request(rdp, inspector, "getWalker", json!({"options": {}})).await?;
    let walker = walker["walker"]["actor"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let found = json!({"contentDomReference": reference});
    let reply = request(rdp, &walker, "getNodeActorFromContentDomReference", found).await?;
    if let Some(node) = node_in(&reply, "nodeFront") {
        return Ok(node);
    }
    // Some Firefox instances' walkers do not resolve a reference to an
    // element that is there (ISSUE-93): the evaluation's object actor
    // holds the element itself, which the walker takes too.
    let held = held_element(&evaluation.result).ok_or_else(|| css_styles::not_found(selector))?;
    let reply = request(rdp, &walker, "getNodeFromActor", held).await?;
    node_in(&reply, "node").ok_or_else(|| css_styles::not_found(selector))
}

/// The node form a walker reply holds under `key` (a `disconnectedNode`:
/// `{node, newParents}`), when the walker found one.
fn node_in(reply: &Value, key: &str) -> Option<Value> {
    let node = &reply[key]["node"];
    node["actor"].is_string().then(|| node.clone())
}

/// The `getNodeFromActor` request for the element an evaluation answered
/// (`result`, its grip): its object actor, whose `rawObj` is the element.
fn held_element(result: &Value) -> Option<Value> {
    let actor = result["actor"].as_str()?;
    Some(json!({"actorID": actor, "path": ["rawObj"]}))
}

/// `p#inner.box` from an inspector node form.
fn node_label(node: &Value) -> String {
    let attribute = |name: &str| {
        let attributes = node["attrs"].as_array().into_iter().flatten();
        let mut found = attributes.filter(|a| a["name"] == name);
        found.next().and_then(|a| a["value"].as_str())
    };
    let name = node["nodeName"].as_str().unwrap_or("element");
    css_styles::element_label(name, attribute("id"), attribute("class"))
}

/// `kind` to `actor`: its reply's body, or why Firefox refused it.
async fn request(rdp: &RdpClient, actor: &str, kind: &str, params: Value) -> Result<Value, String> {
    let reply = rdp.request(actor, kind, params).await;
    let reply = reply.map_err(|e| format!("Firefox's inspector refused {kind}: {e}"))?;
    Ok(reply.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::bidi::test_support as ts;
    use bidi_browser_lite::test_support::MockFirefox;

    /// ISSUE-59 (Firefox): the label of an inspector node form.
    #[test]
    fn nodes_are_labelled() {
        let node = json!({"nodeName": "P", "attrs": [{"name": "id", "value": "inner"}, {"name": "class", "value": "box big"}]});
        assert_eq!(node_label(&node), "p#inner.box.big");
        assert_eq!(node_label(&json!({"nodeName": "DIV"})), "div");
    }

    /// ISSUE-93 (Firefox 159, as captured): when the walker answers no node
    /// for the element's content DOM reference (`nodeFront: null`, though
    /// the element is there), the element is asked for by the evaluation's
    /// object actor (`getNodeFromActor`, path `rawObj`), whose node is used.
    #[test]
    fn an_unresolved_reference_falls_back_to_the_object_actor() {
        // Given the walker's answer for a reference it could not resolve
        let unresolved = json!({"from": "server1.conn1.child27/domwalker21", "nodeFront": null});
        assert_eq!(node_in(&unresolved, "nodeFront"), None);
        // Then the evaluation's grip names the request to send instead
        let grip = json!({"type": "object", "actor": "server1.conn1.child27/obj19", "class": "HTMLElement",
            "contentDomReference": {"browsingContextId": 17179869185_u64, "id": 0.96}});
        assert_eq!(
            held_element(&grip),
            Some(json!({"actorID": "server1.conn1.child27/obj19", "path": ["rawObj"]}))
        );
        assert_eq!(held_element(&json!({"type": "undefined"})), None);
        // And its answer gives the node
        let found = json!({"from": "w", "node": {"node": {"actor": "server1.conn1.child27/domnode22", "nodeName": "B"}, "newParents": []}});
        assert_eq!(
            node_in(&found, "node").unwrap()["actor"],
            "server1.conn1.child27/domnode22"
        );
        let resolved = json!({"nodeFront": {"node": {"actor": "a1"}}});
        assert_eq!(node_in(&resolved, "nodeFront").unwrap()["actor"], "a1");
    }

    /// ISSUE-59 (Firefox): without the debugger port, the call fails saying
    /// it needs it.
    #[tokio::test]
    async fn styles_need_the_debugger_port() {
        let mock = MockFirefox::start(false).await.unwrap();
        let handler = ts::handler(&mock);
        let args = json!({"selector": "#inner"});
        let error = css_styles(ts::call("get_css_styles", args), &handler)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, NO_DEBUGGER);
        ts::stop(&handler).await;
    }
}
