//! `get_css_styles` (ISSUE-59): why an element looks the way it does. The
//! CSS rules that match it in cascade order (selector, stylesheet, 0-based
//! line and column in it, the at-rules around it, each declaration marked
//! `!important`, invalid or overridden), its pseudo-elements, optionally
//! what it inherits, and the computed values of what is declared. Chrome
//! reads them with its `CSS` domain, Firefox with its inspector (RDP); both
//! are turned into the same [`CssStyles`], whose overridden declarations
//! are worked out here (neither browser marks them).

use crate::handler::cdp_domains::cdp_target::CdpTarget;
use rust_mcp_sdk::{
    macros,
    schema::{CallToolError, CallToolRequestParams, CallToolResult},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

/// Rules listed by default (matched, pseudo-element and inherited ones).
pub(crate) const DEFAULT_MAX_RULES: usize = 30;

/// Most rules a call may ask for.
const MAX_RULES_LIMIT: usize = 200;

/// Longest value of a declaration or computed property (characters).
const VALUE_MAX_CHARS: usize = 300;

/// One declaration of a rule.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Declaration {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) important: bool,
    /// The browser could not parse it (an unknown property or value).
    pub(crate) invalid: bool,
    /// A declaration of higher priority sets the same property.
    pub(crate) overridden: bool,
}

/// One rule that applies to the element (or its `style` attribute).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CssRule {
    /// The selector, or `element.style` for the `style` attribute.
    pub(crate) selector: String,
    /// `author`, `user-agent`, `inline-style` (the `style` attribute)...
    pub(crate) origin: String,
    /// The stylesheet's URL (the page's for an inline `<style>`).
    pub(crate) stylesheet: Option<String>,
    /// 0-based, in its stylesheet (for an inline `<style>`, from its text).
    pub(crate) line: Option<u32>,
    pub(crate) column: Option<u32>,
    /// The at-rules around it (`@media (min-width: 100px)`...).
    pub(crate) at_rules: Vec<String>,
    pub(crate) declarations: Vec<Declaration>,
}

/// What `get_css_styles` answers, the same on both browsers.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CssStyles {
    /// The element (`p#inner.box`).
    pub(crate) element: String,
    /// Highest priority first: the `style` attribute, then the rules.
    pub(crate) matched: Vec<CssRule>,
    /// By pseudo-element (`::before`...), highest priority first.
    pub(crate) pseudo_elements: Vec<(String, Vec<CssRule>)>,
    /// By ancestor, the nearest first (the parent is 1).
    pub(crate) inherited: Vec<Vec<CssRule>>,
    /// Computed values, by property.
    pub(crate) computed: BTreeMap<String, String>,
}

impl CssStyles {
    /// Marks each declaration a higher-priority one of the same property
    /// beats (rules are in priority order; `!important` beats the rest).
    pub(crate) fn mark_overridden(&mut self) {
        mark_overridden(&mut self.matched);
        for (_, rules) in &mut self.pseudo_elements {
            mark_overridden(rules);
        }
    }

    /// Keeps the declarations of `properties` (all when empty) and the
    /// computed values of what is declared (or of `properties`); drops
    /// user-agent rules unless asked; at most `max_rules` rules. How many
    /// rules were left out.
    pub(crate) fn select(&mut self, args: &GetCssStylesTool) -> usize {
        if args.include_inherited != Some(true) {
            self.inherited.clear();
        }
        let wanted: Vec<String> = args
            .properties
            .iter()
            .flatten()
            .map(|p| p.trim().to_ascii_lowercase())
            .collect();
        let keep_ua = args.include_user_agent == Some(true);
        let keep = |rule: &mut CssRule| {
            if !wanted.is_empty() {
                rule.declarations
                    .retain(|d| wanted.iter().any(|w| related(&d.name, w)));
            }
            (keep_ua || rule.origin != "user-agent") && !rule.declarations.is_empty()
        };
        self.matched.retain_mut(keep);
        for rules in self
            .pseudo_elements
            .iter_mut()
            .map(|(_, r)| r)
            .chain(&mut self.inherited)
        {
            rules.retain_mut(keep);
        }
        self.keep_computed(&wanted);
        self.keep_at_most(args.max_rules())
    }

    /// Keeps the computed values of `wanted`, else of what is declared.
    fn keep_computed(&mut self, wanted: &[String]) {
        let declared: Vec<String> = match wanted.is_empty() {
            false => wanted.to_vec(),
            true => self
                .matched
                .iter()
                .chain(self.inherited.iter().flatten())
                .flat_map(|rule| {
                    rule.declarations
                        .iter()
                        .map(|d| d.name.to_ascii_lowercase())
                })
                .collect(),
        };
        // A property computed itself is kept alone; a shorthand (never
        // computed: `margin`) brings its longhands (`margin-top`...).
        let names: Vec<&String> = self.computed.keys().collect();
        let kept: std::collections::HashSet<String> = declared
            .iter()
            .flat_map(|d| match self.computed.contains_key(d) {
                true => vec![d.clone()],
                false => names
                    .iter()
                    .filter(|n| related(n, d))
                    .map(|n| (*n).clone())
                    .collect(),
            })
            .collect();
        self.computed.retain(|name, _| kept.contains(name));
    }

    /// Keeps the first `max` rules (matched, then pseudo-elements', then
    /// ancestors'); how many were left out.
    fn keep_at_most(&mut self, max: usize) -> usize {
        let mut room = max;
        let mut left_out = 0;
        let mut take = |rules: &mut Vec<CssRule>| {
            let kept = rules.len().min(room);
            left_out += rules.len() - kept;
            rules.truncate(kept);
            room -= kept;
        };
        take(&mut self.matched);
        for (_, rules) in &mut self.pseudo_elements {
            take(rules);
        }
        for rules in &mut self.inherited {
            take(rules);
        }
        self.pseudo_elements.retain(|(_, rules)| !rules.is_empty());
        self.inherited.retain(|rules| !rules.is_empty());
        left_out
    }

    /// The JSON of the reply.
    pub(crate) fn to_json(&self) -> Value {
        let rules = |rules: &[CssRule]| Value::Array(rules.iter().map(CssRule::to_json).collect());
        let mut out = json!({"element": self.element, "matched": rules(&self.matched)});
        if !self.pseudo_elements.is_empty() {
            let pseudo: Vec<Value> = self
                .pseudo_elements
                .iter()
                .map(|(pseudo, list)| json!({"pseudo": pseudo, "rules": rules(list)}))
                .collect();
            out["pseudo_elements"] = json!(pseudo);
        }
        if !self.inherited.is_empty() {
            let inherited: Vec<Value> = self
                .inherited
                .iter()
                .enumerate()
                .map(|(i, list)| json!({"distance": i + 1, "rules": rules(list)}))
                .collect();
            out["inherited"] = json!(inherited);
        }
        if !self.computed.is_empty() {
            out["computed"] = json!(self.computed);
        }
        out
    }

    /// `get_css_styles`' reply, with a note when rules were left out.
    pub(crate) fn reply(&self, left_out: usize) -> CallToolResult {
        let first = format!(
            "Styles of {}: {} matched rule(s), highest priority first.",
            self.element,
            self.matched.len()
        );
        let body = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
        let mut text = format!("{first}\n{body}");
        if left_out > 0 {
            text.push_str(&format!(
                "\nNote: {left_out} more rule(s) left out: raise max_rules, or ask for some properties."
            ));
        }
        CallToolResult::text_content(vec![text.into()])
    }
}

impl CssRule {
    fn to_json(&self) -> Value {
        let mut out = json!({"selector": self.selector, "origin": self.origin});
        if let Some(sheet) = &self.stylesheet {
            out["stylesheet"] = json!(sheet);
        }
        if let Some(line) = self.line {
            out["line_number"] = json!(line);
        }
        if let Some(column) = self.column {
            out["column_number"] = json!(column);
        }
        if !self.at_rules.is_empty() {
            out["at_rules"] = json!(self.at_rules);
        }
        let declarations: Vec<Value> = self
            .declarations
            .iter()
            .map(|d| {
                let mut out = json!({"name": d.name, "value": d.value});
                for (flag, on) in [
                    ("important", d.important),
                    ("invalid", d.invalid),
                    ("overridden", d.overridden),
                ] {
                    if on {
                        out[flag] = json!(true);
                    }
                }
                out
            })
            .collect();
        out["declarations"] = json!(declarations);
        out
    }
}

/// Whether properties `a` and `b` are the same, or a shorthand and one of
/// its longhands (`margin`, `margin-top`).
fn related(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    a == b || a.starts_with(&format!("{b}-")) || b.starts_with(&format!("{a}-"))
}

/// Marks, in `rules` (highest priority first), each valid declaration
/// another one of the same property beats: the first `!important` one,
/// else the first one.
fn mark_overridden(rules: &mut [CssRule]) {
    let mut winners: HashMap<String, (bool, usize, usize)> = HashMap::new();
    for (r, rule) in rules.iter().enumerate() {
        for (d, declaration) in rule
            .declarations
            .iter()
            .enumerate()
            .filter(|(_, d)| !d.invalid)
        {
            let name = declaration.name.to_ascii_lowercase();
            let beats = |won: &(bool, usize, usize)| declaration.important && !won.0;
            if winners.get(&name).is_none_or(beats) {
                winners.insert(name, (declaration.important, r, d));
            }
        }
    }
    for (r, rule) in rules.iter_mut().enumerate() {
        for (d, declaration) in rule.declarations.iter_mut().enumerate() {
            let won = winners.get(&declaration.name.to_ascii_lowercase());
            declaration.overridden =
                !declaration.invalid && won.is_some_and(|w| (w.1, w.2) != (r, d));
        }
    }
}

/// `value` within [`VALUE_MAX_CHARS`].
fn bounded(value: &str) -> String {
    crate::util::text::capped(value, VALUE_MAX_CHARS)
}

/// The styles of a Chrome `CSS.getMatchedStylesForNode` reply (`matched`)
/// and `CSS.getComputedStyleForNode` reply (`computed`), the stylesheets'
/// URLs by id from `CSS.styleSheetAdded` (`sheets`).
pub(crate) fn from_chrome(
    element: &str,
    matched: &Value,
    computed: &Value,
    sheets: &HashMap<String, String>,
) -> CssStyles {
    let inherited = matched["inherited"].as_array().into_iter().flatten();
    CssStyles {
        element: element.to_string(),
        matched: chrome_rules(matched, sheets),
        pseudo_elements: chrome_pseudo_elements(matched, sheets),
        inherited: inherited
            .map(|holder| chrome_rules(holder, sheets))
            .collect(),
        computed: chrome_computed(computed),
    }
}

/// The rules of `holder` (the element, or an ancestor), highest priority
/// first: its `style` attribute, then its matched rules from the last
/// (Chrome lists them by ascending priority).
fn chrome_rules(holder: &Value, sheets: &HashMap<String, String>) -> Vec<CssRule> {
    let inline = holder.get("inlineStyle").and_then(chrome_inline);
    let matches = holder["matchedCSSRules"].as_array().into_iter().flatten();
    let matched = matches
        .rev()
        .filter_map(|m| chrome_rule(&m["rule"], sheets));
    inline.into_iter().chain(matched).collect()
}

/// The rules of each pseudo-element (`::before`...), highest priority first.
fn chrome_pseudo_elements(
    matched: &Value,
    sheets: &HashMap<String, String>,
) -> Vec<(String, Vec<CssRule>)> {
    let pseudo = matched["pseudoElements"].as_array().into_iter().flatten();
    pseudo
        .map(|p| {
            let name = format!("::{}", p["pseudoType"].as_str().unwrap_or_default());
            let matches = p["matches"].as_array().into_iter().flatten().rev();
            let rules = matches.filter_map(|m| chrome_rule(&m["rule"], sheets));
            (name, rules.collect())
        })
        .collect()
}

/// The values of a `CSS.getComputedStyleForNode` reply, by property.
fn chrome_computed(computed: &Value) -> BTreeMap<String, String> {
    let properties = computed["computedStyle"].as_array().into_iter().flatten();
    properties
        .filter_map(|p| {
            Some((
                p["name"].as_str()?.to_string(),
                bounded(p["value"].as_str()?),
            ))
        })
        .collect()
}

/// The `style` attribute's declarations (a Chrome `inlineStyle`), if any.
fn chrome_inline(style: &Value) -> Option<CssRule> {
    let declarations = chrome_declarations(style);
    (!declarations.is_empty()).then(|| CssRule {
        selector: "element.style".into(),
        origin: "inline-style".into(),
        declarations,
        ..Default::default()
    })
}

/// A Chrome `CSSRule`.
fn chrome_rule(rule: &Value, sheets: &HashMap<String, String>) -> Option<CssRule> {
    let selector = rule["selectorList"]["text"].as_str()?.to_string();
    let range = &rule["selectorList"]["selectors"][0]["range"];
    let number = |key: &str| range[key].as_u64().and_then(|n| u32::try_from(n).ok());
    let origin = match rule["origin"].as_str().unwrap_or_default() {
        "regular" => "author",
        other => other,
    };
    let mut at_rules = Vec::new();
    for (key, name) in [
        ("layers", "@layer"),
        ("supports", "@supports"),
        ("media", "@media"),
        ("containerQueries", "@container"),
    ] {
        let texts = rule[key].as_array().into_iter().flatten();
        // A container query keeps its container's name (ISSUE-86).
        at_rules.extend(texts.filter_map(|at| {
            let text = at["text"].as_str()?;
            Some(match at["name"].as_str().filter(|n| !n.is_empty()) {
                Some(container) => format!("{name} {container} {text}"),
                None => format!("{name} {text}"),
            })
        }));
    }
    let sheet = rule["styleSheetId"]
        .as_str()
        .and_then(|id| sheets.get(id))
        .cloned();
    Some(CssRule {
        selector,
        origin: origin.to_string(),
        stylesheet: sheet,
        line: number("startLine"),
        column: number("startColumn"),
        at_rules,
        declarations: chrome_declarations(&rule["style"]),
    })
}

/// A Chrome `CSSStyle`'s declared properties: those with a place in the
/// source (the others are longhands Chrome expanded), or all of a style
/// without source (a user-agent rule).
fn chrome_declarations(style: &Value) -> Vec<Declaration> {
    let properties: Vec<&Value> = style["cssProperties"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["disabled"].as_bool() != Some(true))
        .collect();
    let sourced = properties.iter().any(|p| p.get("range").is_some());
    properties
        .into_iter()
        .filter(|p| !sourced || p.get("range").is_some())
        .filter_map(|p| {
            let important = p["important"].as_bool() == Some(true);
            let value = p["value"].as_str()?;
            let value = value
                .strip_suffix("!important")
                .map_or(value, str::trim_end);
            Some(Declaration {
                name: p["name"].as_str()?.to_string(),
                value: bounded(value),
                important,
                invalid: p["parsedOk"].as_bool() == Some(false),
                overridden: false,
            })
        })
        .collect()
}

/// Where the rules of a Firefox page come from: the page's URL (an inline
/// `<style>`'s sheet), and the URLs of the other sheets by their
/// `parentStyleSheet` id (see [`firefox_sheet_samples`], ISSUE-86).
#[derive(Debug, Default)]
pub(crate) struct Sheets {
    pub(crate) page: String,
    pub(crate) by_id: HashMap<String, String>,
}

/// The styles of a Firefox page style `getApplied` reply (`applied`) and
/// `getComputed` reply (`computed`), its rules' sheets named from `sheets`.
pub(crate) fn from_firefox(
    element: &str,
    applied: &Value,
    computed: &Value,
    sheets: &Sheets,
) -> CssStyles {
    let mut styles = CssStyles {
        element: element.to_string(),
        ..Default::default()
    };
    let mut ancestors: Vec<String> = Vec::new();
    for entry in applied["entries"].as_array().into_iter().flatten() {
        let Some(rule) = firefox_rule(&entry["rule"], sheets) else {
            continue;
        };
        let pseudo = entry["pseudoElement"].as_str().unwrap_or_default();
        if let Some(ancestor) = entry["inherited"].as_str() {
            match ancestors.iter().position(|a| a == ancestor) {
                Some(at) => styles.inherited[at].push(rule),
                None => {
                    ancestors.push(ancestor.to_string());
                    styles.inherited.push(vec![rule]);
                }
            }
        } else if !pseudo.is_empty() {
            match styles
                .pseudo_elements
                .iter_mut()
                .find(|(name, _)| name == pseudo)
            {
                Some((_, rules)) => rules.push(rule),
                None => styles
                    .pseudo_elements
                    .push((pseudo.to_string(), vec![rule])),
            }
        } else {
            styles.matched.push(rule);
        }
    }
    styles.computed = computed["computed"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, p)| Some((name.clone(), bounded(p["value"].as_str()?))))
        .collect();
    styles
}

/// The rule of a Firefox `getApplied` entry (type 100: the `style`
/// attribute); its 1-based line and column made 0-based, its sheet's URL
/// from `sheets`.
fn firefox_rule(rule: &Value, sheets: &Sheets) -> Option<CssRule> {
    let declarations = firefox_declarations(rule)?;
    let number = |key: &str| {
        rule[key]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(|n| n.saturating_sub(1))
    };
    let element_style = rule["type"].as_u64() == Some(100);
    let origin = match (element_style, rule["origin"].as_str()) {
        (true, _) => "inline-style",
        (false, Some("user-agent")) => "user-agent",
        (false, _) => "author",
    };
    let selectors: Vec<&str> = rule["selectors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    Some(CssRule {
        selector: match element_style {
            true => "element.style".into(),
            false => selectors.join(", "),
        },
        origin: origin.to_string(),
        stylesheet: match origin {
            "inline-style" => None,
            "user-agent" => rule["href"].as_str().map(str::to_string),
            _ => Some(firefox_sheet_url(rule, sheets)),
        },
        line: number("line"),
        column: number("column"),
        at_rules: firefox_at_rules(rule),
        declarations,
    })
}

/// A Firefox rule's declarations (`!important` from its `priority`).
fn firefox_declarations(rule: &Value) -> Option<Vec<Declaration>> {
    let declarations = rule["declarations"].as_array()?.iter();
    let read = declarations.filter_map(|d| {
        Some(Declaration {
            name: d["name"].as_str()?.to_string(),
            value: bounded(d["value"].as_str()?),
            important: d["priority"].as_str() == Some("important"),
            invalid: d["isValid"].as_bool() == Some(false),
            overridden: false,
        })
    });
    Some(read.collect())
}

/// The URL of the sheet of an author rule: Firefox 159 names it only by
/// `parentStyleSheet`, found in `sheets` (ISSUE-86); an older `href`; else
/// the page's (an inline `<style>`, or a sheet the page did not find).
fn firefox_sheet_url(rule: &Value, sheets: &Sheets) -> String {
    let found = rule["parentStyleSheet"].as_str();
    let found = found.and_then(|id| sheets.by_id.get(id));
    let named = found.map(String::as_str).or(rule["href"].as_str());
    named.unwrap_or(&sheets.page).to_string()
}

/// A Firefox rule's at-rules (its `ancestorData`: `@media (min-width: 1px)`;
/// a container query's name and query from its `conditions`, ISSUE-86).
fn firefox_at_rules(rule: &Value) -> Vec<String> {
    let ancestors = rule["ancestorData"].as_array().into_iter().flatten();
    ancestors
        .filter_map(|at| {
            let kind = at["type"].as_str()?;
            let value = match at["conditions"].as_array() {
                Some(conditions) => container_conditions(conditions),
                None => at["value"].as_str().unwrap_or_default().to_string(),
            };
            Some(format!("@{kind} {value}").trim_end().to_string())
        })
        .collect()
}

/// `box (min-width: 100px)` from a Firefox container rule's `conditions`.
fn container_conditions(conditions: &[Value]) -> String {
    let each = conditions.iter().map(|c| {
        let name = c["containerName"].as_str().unwrap_or_default();
        let query = c["containerQuery"].as_str().unwrap_or_default();
        format!("{name} {query}").trim().to_string()
    });
    each.collect::<Vec<_>>().join(", ")
}

/// A rule of one Firefox stylesheet, to find that sheet in the page.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SheetSample {
    /// The sheet's `parentStyleSheet` id.
    pub(crate) id: String,
    /// The rule's selector, as the CSSOM's `selectorText`.
    pub(crate) selector: String,
    /// Its declarations, as the CSSOM's `style.cssText`.
    pub(crate) css_text: String,
}

/// One rule of each sheet the author rules of a `getApplied` reply come
/// from, in order: Firefox 159 names a rule's sheet only by an id of its
/// stylesheet resources, which `get_css_styles`' own target cannot list,
/// so the page finds the sheet holding that rule (ISSUE-86).
pub(crate) fn firefox_sheet_samples(applied: &Value) -> Vec<SheetSample> {
    let mut samples: Vec<SheetSample> = Vec::new();
    for entry in applied["entries"].as_array().into_iter().flatten() {
        let rule = &entry["rule"];
        let Some(id) = rule["parentStyleSheet"].as_str() else {
            continue;
        };
        let selectors = rule["selectors"].as_array().into_iter().flatten();
        let selectors: Vec<&str> = selectors.filter_map(Value::as_str).collect();
        let css_text = rule["cssText"].as_str();
        if selectors.is_empty() || samples.iter().any(|s| s.id == id) {
            continue;
        }
        if let Some(css_text) = css_text {
            samples.push(SheetSample {
                id: id.to_string(),
                selector: selectors.join(", "),
                css_text: css_text.to_string(),
            });
        }
    }
    samples
}

/// Finds, in the page, the sheet of each of `samples`: the first sheet of
/// the root of the element `element` (a JavaScript expression) or of its
/// document, adopted ones and `@import`ed ones included, with a rule (at
/// any depth: grouping and nested rules) of the same selector and
/// declarations. Answers a JSON object of URLs by id: the sheet's, or
/// for one without a URL (an inline `<style>`, a constructed sheet) the
/// element's document's.
pub(crate) fn sheet_lookup(element: &str, samples: &[SheetSample]) -> String {
    let wanted: Vec<[&str; 3]> = samples
        .iter()
        .map(|s| [s.id.as_str(), s.selector.as_str(), s.css_text.as_str()])
        .collect();
    let wanted = serde_json::to_string(&wanted).unwrap_or_else(|_| "[]".into());
    format!(
        "(() => {{ const el = {element}; if (!el) return null; const doc = el.ownerDocument, root = el.getRootNode(); \
         const sheets = [root, doc].flatMap(r => [...(r.styleSheets || []), ...(r.adoptedStyleSheets || [])]); \
         const holds = (rules, sel, text, depth) => {{ for (const r of rules) {{ \
           if (r.selectorText === sel && r.style && r.style.cssText === text) return true; \
           if (depth < 8 && r.cssRules && holds(r.cssRules, sel, text, depth + 1)) return true; }} return false; }}; \
         const find = (sheet, sel, text, depth) => {{ let rules; try {{ rules = sheet.cssRules; }} catch (e) {{ return null; }} \
           if (holds(rules, sel, text, 0)) return sheet; \
           for (const r of rules) {{ if (depth < 4 && r.styleSheet) {{ const s = find(r.styleSheet, sel, text, depth + 1); if (s) return s; }} }} return null; }}; \
         const out = {{}}; for (const [id, sel, text] of {wanted}) {{ \
           for (const sheet of sheets) {{ const s = find(sheet, sel, text, 0); if (s) {{ out[id] = s.href || doc.URL; break; }} }} }} \
         return JSON.stringify(out); }})()"
    )
}

#[macros::mcp_tool(
    name = "get_css_styles",
    description = "Explains why an element looks as it does: the CSS rules matching it, highest priority first (style attribute, then rules), each with selector, stylesheet URL (the page's for an inline <style>), 0-based line and column, enclosing at-rules (@media, @container, @layer, @supports) and declarations marked important, invalid or overridden; its pseudo-elements; and the computed values of the declared properties. Options add the ancestors' rules (nearest first) and the browser's, or narrow to some properties (declarations included). Firefox reads them with its inspector (needs its debugger port).",
    read_only_hint = true
)]
#[derive(Debug, ::serde::Deserialize, ::serde::Serialize, macros::JsonSchema)]
pub struct GetCssStylesTool {
    /// Default: 'default'.
    pub instance_id: Option<String>,
    /// Default: the active tab.
    pub tab_id: Option<String>,
    /// The element (CSS, 'host >>> inner' or a snapshot ref).
    pub selector: String,
    /// Only these properties (e.g. ['color', 'margin']); a shorthand also matches its longhands.
    pub properties: Option<Vec<String>>,
    /// Also its ancestors' rules (what it inherits). Default: false.
    pub include_inherited: Option<bool>,
    /// The browser's own rules too. Default: false.
    pub include_user_agent: Option<bool>,
    /// Most rules listed, 1 to 200. Default: 30.
    pub max_rules: Option<u32>,
}

impl GetCssStylesTool {
    /// The rules to list at most.
    pub(crate) fn max_rules(&self) -> usize {
        self.max_rules
            .map_or(DEFAULT_MAX_RULES, |n| {
                usize::try_from(n).unwrap_or(MAX_RULES_LIMIT)
            })
            .clamp(1, MAX_RULES_LIMIT)
    }

    /// `get_css_styles` on a Chrome instance: the `CSS` domain, in the
    /// frame the selector names.
    pub async fn handle(
        params: CallToolRequestParams,
        handler: &crate::handler::McpHandler,
    ) -> Result<CallToolResult, CallToolError> {
        let args: Self = crate::handler::parse_args(params)?;
        let (instance, tab) = (args.instance_id.clone(), args.tab_id.clone());
        let at = Some(args.selector.as_str());
        let target = crate::handler::resolve_target_at(handler, instance, tab, at).await?;
        let styles = chrome_styles(&target, &args.selector).await;
        Ok(args.answer(styles.map_err(CallToolError::from_message)?))
    }

    /// The reply from what the browser read (shared by every browser).
    pub(crate) fn answer(&self, mut styles: CssStyles) -> CallToolResult {
        styles.mark_overridden();
        let left_out = styles.select(self);
        styles.reply(left_out)
    }
}

/// The error of a selector that names no element.
pub(crate) fn not_found(selector: &str) -> String {
    format!(
        "Element not found: {selector}{}",
        crate::util::js::ref_hint(selector)
    )
}

/// `p#inner.box`: the element's tag, id and first classes.
pub(crate) fn element_label(name: &str, id: Option<&str>, class: Option<&str>) -> String {
    let mut label = name.to_ascii_lowercase();
    if let Some(id) = id.filter(|id| !id.is_empty()) {
        label.push('#');
        label.push_str(id);
    }
    for class in class.unwrap_or_default().split_whitespace().take(3) {
        label.push('.');
        label.push_str(class);
    }
    label
}

/// Marks, in a Firefox `getApplied` reply asked with `filter: "ua"`, the
/// rules that the same request with `filter: "user"` did not list (their
/// actors: `user`) as the browser's own: Firefox does not mark them.
pub(crate) fn mark_user_agent(applied: &mut Value, user: &std::collections::HashSet<String>) {
    let entries = applied["entries"].as_array_mut().into_iter().flatten();
    for entry in entries {
        let rule = &mut entry["rule"];
        let actor = rule["actor"].as_str().unwrap_or_default();
        if rule["type"].as_u64() != Some(100) && !user.contains(actor) {
            rule["origin"] = json!("user-agent");
        }
    }
}

/// The rule actors of a Firefox `getApplied` reply.
pub(crate) fn rule_actors(applied: &Value) -> std::collections::HashSet<String> {
    let entries = applied["entries"].as_array().into_iter().flatten();
    entries
        .filter_map(|entry| entry["rule"]["actor"].as_str().map(str::to_string))
        .collect()
}

/// How long the stylesheets `CSS.enable` announces are waited for once
/// none came (a page adding sheets forever is cut at [`SHEETS_MAX`]).
const SHEETS_QUIET: std::time::Duration = std::time::Duration::from_millis(50);

/// Longest wait for the stylesheets `CSS.enable` announces.
const SHEETS_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// The styles of the element `selector` names on Chrome; `DOM` and `CSS`
/// are left disabled, as they were, on every path.
async fn chrome_styles(target: &CdpTarget, selector: &str) -> Result<CssStyles, String> {
    let params = json!({"expression": crate::util::js::element(selector)});
    let found = command(target, "Runtime.evaluate", params).await?;
    let thrown = found.pointer("/exceptionDetails/exception/description");
    if let Some(thrown) = thrown.and_then(Value::as_str) {
        return Err(thrown.to_string());
    }
    let object = found
        .pointer("/result/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| not_found(selector))?
        .to_string();
    let read = chrome_read(target, &object).await;
    let release = json!({"objectId": object});
    let _ = target
        .send_raw_command("Runtime.releaseObject", release)
        .await;
    for method in ["CSS.disable", "DOM.disable"] {
        let _ = target.send_raw_command(method, json!({})).await;
    }
    read
}

/// The styles of the remote object `object` (an element).
async fn chrome_read(target: &CdpTarget, object: &str) -> Result<CssStyles, String> {
    let described = command(target, "DOM.describeNode", json!({"objectId": object})).await?;
    let label = chrome_label(&described["node"]);
    command(target, "DOM.enable", json!({})).await?;
    command(target, "DOM.getDocument", json!({"depth": 0})).await?;
    let requested = command(target, "DOM.requestNode", json!({"objectId": object})).await?;
    let node = json!({"nodeId": requested["nodeId"]});
    // Subscribed first: CSS.enable announces the stylesheets loaded.
    let mut events = target.on_domain("CSS");
    command(target, "CSS.enable", json!({})).await?;
    let sheets = stylesheet_urls(&mut events).await;
    let matched = command(target, "CSS.getMatchedStylesForNode", node.clone()).await?;
    let computed = command(target, "CSS.getComputedStyleForNode", node).await?;
    Ok(from_chrome(&label, &matched, &computed, &sheets))
}

/// The label of a CDP `DOM.Node` (its attributes are a flat name, value list).
fn chrome_label(node: &Value) -> String {
    let attributes: Vec<&str> = node["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let attribute = |name: &str| {
        let mut pairs = attributes.chunks(2);
        pairs
            .find(|pair| pair[0] == name)
            .and_then(|pair| pair.get(1).copied())
    };
    let name = node["localName"].as_str().unwrap_or("element");
    element_label(name, attribute("id"), attribute("class"))
}

/// The URLs of the stylesheets `CSS.styleSheetAdded` announces, by id.
async fn stylesheet_urls(events: &mut cdp_browser_lite::EventFilter) -> HashMap<String, String> {
    use tokio_stream::StreamExt;
    let deadline = tokio::time::Instant::now() + SHEETS_MAX;
    let mut sheets = HashMap::new();
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout(SHEETS_QUIET, events.next()).await else {
            break;
        };
        let Ok(event) = event else { continue };
        if event.method.as_deref() != Some("CSS.styleSheetAdded") {
            continue;
        }
        let header = event.params.as_ref().map_or(&Value::Null, |p| &p["header"]);
        let url = header["sourceURL"].as_str().filter(|url| !url.is_empty());
        if let (Some(id), Some(url)) = (header["styleSheetId"].as_str(), url) {
            sheets.insert(id.to_string(), url.to_string());
        }
    }
    sheets
}

/// `method`'s result, or why Chrome refused it.
async fn command(target: &CdpTarget, method: &str, params: Value) -> Result<Value, String> {
    let reply = target.send_raw_command(method, params).await;
    let failure = format!("Chrome refused {method}");
    let reply = reply.map_err(|e| crate::handler::cdp_failure(&failure, &e))?;
    Ok(reply.result.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(args: Value) -> GetCssStylesTool {
        serde_json::from_value(args).unwrap()
    }

    fn rule(selector: &str, declarations: &[(&str, &str, bool)]) -> CssRule {
        CssRule {
            selector: selector.into(),
            origin: "author".into(),
            declarations: declarations
                .iter()
                .map(|(name, value, important)| Declaration {
                    name: (*name).into(),
                    value: (*value).into(),
                    important: *important,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn overridden(rule: &CssRule) -> Vec<bool> {
        rule.declarations.iter().map(|d| d.overridden).collect()
    }

    /// ISSUE-59: the declaration of highest priority wins; an `!important`
    /// one beats every normal one; an invalid one never wins.
    #[test]
    fn overridden_declarations_are_marked() {
        let mut styles = CssStyles {
            matched: vec![
                rule("element.style", &[("font-weight", "bold", false)]),
                rule(
                    "#inner",
                    &[("color", "red", false), ("padding", "2px", false)],
                ),
                rule(
                    ".box",
                    &[("color", "green", true), ("padding", "9px", false)],
                ),
            ],
            ..Default::default()
        };
        styles.matched[0].declarations.push(Declaration {
            name: "padding".into(),
            value: "x y z w q".into(),
            invalid: true,
            ..Default::default()
        });
        styles.mark_overridden();
        assert_eq!(overridden(&styles.matched[0]), [false, false]);
        assert_eq!(overridden(&styles.matched[1]), [true, false]);
        assert_eq!(overridden(&styles.matched[2]), [false, true]);
    }

    /// ISSUE-59: `properties` keeps those declarations (a shorthand its
    /// longhands too) and the rules that have one; the computed values are
    /// those of the declared properties; user-agent rules are left out by
    /// default; past `max_rules` the rest is counted.
    #[test]
    fn selection_follows_the_arguments() {
        let mut ua = rule("p", &[("display", "block", false)]);
        ua.origin = "user-agent".into();
        let styles = CssStyles {
            matched: vec![
                rule(
                    "#inner",
                    &[("color", "red", false), ("padding", "2px", false)],
                ),
                rule(".box", &[("margin", "4px", false)]),
                ua,
            ],
            computed: [
                ("color", "red"),
                ("color-scheme", "normal"),
                ("padding-top", "2px"),
                ("margin-top", "4px"),
                ("display", "block"),
                ("width", "10px"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            ..Default::default()
        };
        let mut all = styles.clone();
        assert_eq!(all.select(&tool(json!({"selector": "p"}))), 0);
        assert_eq!(all.matched.len(), 2, "no user-agent rule");
        let keys: Vec<&str> = all.computed.keys().map(String::as_str).collect();
        // `color` is computed itself: not `color-scheme` (ISSUE-59)
        assert_eq!(keys, ["color", "margin-top", "padding-top"]);
        let mut padding = styles.clone();
        padding.select(&tool(json!({"selector": "p", "properties": ["padding"]})));
        assert_eq!(padding.matched.len(), 1);
        assert_eq!(padding.matched[0].declarations.len(), 1);
        assert_eq!(padding.computed.keys().collect::<Vec<_>>(), ["padding-top"]);
        let mut one = styles.clone();
        let args = tool(json!({"selector": "p", "max_rules": 1, "include_user_agent": true}));
        assert_eq!(one.select(&args), 2);
        assert_eq!(one.matched.len(), 1);
        assert_eq!(
            styles
                .clone()
                .select(&tool(json!({"selector": "p", "max_rules": 0}))),
            1
        );
    }

    /// ISSUE-59: the reply's JSON and its first line.
    #[test]
    fn replies_show_the_cascade() {
        let mut styles = CssStyles {
            element: "p#inner".into(),
            matched: vec![CssRule {
                stylesheet: Some("http://a/".into()),
                line: Some(3),
                column: Some(0),
                at_rules: vec!["@media (min-width: 100px)".into()],
                ..rule("#inner", &[("color", "red", false)])
            }],
            pseudo_elements: vec![(
                "::before".into(),
                vec![rule("#inner::before", &[("content", "\"x\"", false)])],
            )],
            computed: [("color".to_string(), "red".to_string())].into(),
            ..Default::default()
        };
        styles.matched[0].declarations[0].invalid = true;
        assert_eq!(
            styles.to_json(),
            json!({"element": "p#inner",
                "matched": [{"selector": "#inner", "origin": "author", "stylesheet": "http://a/", "line_number": 3, "column_number": 0,
                    "at_rules": ["@media (min-width: 100px)"], "declarations": [{"name": "color", "value": "red", "invalid": true}]}],
                "pseudo_elements": [{"pseudo": "::before", "rules": [{"selector": "#inner::before", "origin": "author", "declarations": [{"name": "content", "value": "\"x\""}]}]}],
                "computed": {"color": "red"}})
        );
        let text = match &styles.reply(4).content[0] {
            rust_mcp_sdk::schema::ContentBlock::TextContent(t) => t.text.clone(),
            _ => String::new(),
        };
        assert!(
            text.starts_with("Styles of p#inner: 1 matched rule(s), highest priority first."),
            "{text}"
        );
        assert!(
            text.ends_with(
                "Note: 4 more rule(s) left out: raise max_rules, or ask for some properties."
            ),
            "{text}"
        );
    }

    /// A Chrome `CSS.getMatchedStylesForNode` reply: a style attribute, a
    /// user-agent rule, an `!important` one, an invalid one, one in
    /// `@media`, a `::before` and an ancestor's.
    fn chrome_matched() -> Value {
        let prop = |name: &str, value: &str, declared: bool| {
            let mut p = json!({"name": name, "value": value});
            if declared {
                p["range"] = json!({"startLine": 0});
            }
            p
        };
        let css = |selector: &str, line: u32, props: Vec<Value>| {
            json!({"rule": {"origin": "regular", "styleSheetId": "s1",
            "selectorList": {"text": selector, "selectors": [{"text": selector, "range": {"startLine": line, "startColumn": 1}}]},
            "style": {"cssProperties": props}}})
        };
        let mut boxed = css(
            ".box",
            2,
            vec![
                prop("color", "green !important", true),
                prop("margin-top", "4px", false),
            ],
        );
        boxed["rule"]["style"]["cssProperties"][0]["important"] = json!(true);
        let mut media = css("#inner", 4, vec![prop("border", "1px solid black", true)]);
        media["rule"]["media"] = json!([{"text": "(min-width: 100px)", "source": "mediaRule"}]);
        let mut bad = css("#inner", 3, vec![prop("colr", "x", true)]);
        bad["rule"]["style"]["cssProperties"][0]["parsedOk"] = json!(false);
        let ua = json!({"rule": {"origin": "user-agent", "selectorList": {"text": "p", "selectors": [{"text": "p"}]}, "style": {"cssProperties": [prop("display", "block", false)]}}});
        json!({
            "inlineStyle": {"cssProperties": [prop("font-weight", "bold", true), prop("font-style", "normal", false)]},
            "matchedCSSRules": [ua, boxed, bad, media],
            "pseudoElements": [{"pseudoType": "before", "matches": [css("#inner::before", 6, vec![prop("content", "\"x\"", true)])]}],
            "inherited": [{"matchedCSSRules": [css("#outer", 1, vec![prop("color", "blue", true)])]}],
        })
    }

    /// ISSUE-59 (Chrome): rules in priority order (the `style` attribute,
    /// then the matched rules from the last), only the declared
    /// properties (not Chrome's longhands), their stylesheet's URL and
    /// place, at-rules, pseudo-elements and ancestors; the computed values.
    #[test]
    fn chrome_styles_are_read() {
        let matched = chrome_matched();
        let computed = json!({"computedStyle": [{"name": "color", "value": "rgb(0, 128, 0)"}]});
        let sheets = HashMap::from([("s1".to_string(), "http://a/page.html".to_string())]);
        let styles = from_chrome("p#inner", &matched, &computed, &sheets);
        let selectors: Vec<&str> = styles.matched.iter().map(|r| r.selector.as_str()).collect();
        assert_eq!(
            selectors,
            ["element.style", "#inner", "#inner", ".box", "p"]
        );
        assert_eq!(
            styles.matched[0].declarations.len(),
            1,
            "only declared properties"
        );
        assert_eq!(styles.matched[0].origin, "inline-style");
        let media = &styles.matched[1];
        assert_eq!(
            (media.line, media.column, media.at_rules.clone()),
            (
                Some(4),
                Some(1),
                vec!["@media (min-width: 100px)".to_string()]
            )
        );
        assert_eq!(media.stylesheet.as_deref(), Some("http://a/page.html"));
    }

    /// ISSUE-59 (Chrome): invalid and `!important` declarations, the
    /// user-agent rule, pseudo-elements, ancestors and computed values.
    #[test]
    fn chrome_declarations_are_marked() {
        let computed = json!({"computedStyle": [{"name": "color", "value": "rgb(0, 128, 0)"}]});
        let sheets = HashMap::from([("s1".to_string(), "http://a/page.html".to_string())]);
        let styles = from_chrome("p#inner", &chrome_matched(), &computed, &sheets);
        assert!(styles.matched[2].declarations[0].invalid);
        let important = &styles.matched[3].declarations[0];
        assert_eq!(
            (important.value.as_str(), important.important),
            ("green", true)
        );
        assert_eq!(styles.matched[4].origin, "user-agent");
        assert_eq!(
            styles.matched[4].declarations.len(),
            1,
            "a user-agent rule's properties are all declared"
        );
        assert_eq!(styles.pseudo_elements[0].0, "::before");
        assert_eq!(styles.inherited[0][0].selector, "#outer");
        assert_eq!(styles.computed["color"], "rgb(0, 128, 0)");
    }

    /// ISSUE-59 (Firefox): the rules a `filter: "ua"` request adds to the
    /// `filter: "user"` one are the browser's: no stylesheet of the page.
    #[test]
    fn firefox_user_agent_rules_are_marked() {
        let entry = |actor: &str, kind: u64| {
            json!({"inherited": null, "pseudoElement": "",
            "rule": {"actor": actor, "type": kind, "selectors": ["p"], "declarations": [{"name": "display", "value": "block"}]}})
        };
        let user = json!({"entries": [entry("r1", 1), entry("r0", 100)]});
        let mut both = json!({"entries": [entry("r0", 100), entry("r1", 1), entry("r9", 1)]});
        mark_user_agent(&mut both, &rule_actors(&user));
        let styles = from_firefox("p", &both, &json!({}), &page_sheets("http://a/"));
        let origins: Vec<&str> = styles.matched.iter().map(|r| r.origin.as_str()).collect();
        assert_eq!(origins, ["inline-style", "author", "user-agent"]);
        assert_eq!(styles.matched[2].stylesheet, None);
        assert_eq!(styles.matched[1].stylesheet.as_deref(), Some("http://a/"));
    }

    /// A Chrome page whose `#inner` is matched by one rule of `s1`
    /// (`#nope` names nothing).
    fn css_page(mock: &cdp_browser_lite::test_support::mock_devtools::MockDevTools) {
        mock.respond_with("Runtime.evaluate", |params| {
            let missing = params["expression"].to_string().contains("#nope");
            let value = match missing {
                true => json!({"type": "object", "subtype": "null", "value": null}),
                false => json!({"type": "object", "objectId": "o1"}),
            };
            json!({"result": {"result": value}})
        });
        mock.respond_with("DOM.describeNode", |_| {
            json!({"result": {"node": {"localName": "p", "attributes": ["id", "inner", "class", "box"]}}})
        });
        mock.respond_with("DOM.requestNode", |_| json!({"result": {"nodeId": 7}}));
        mock.respond_with("CSS.getMatchedStylesForNode", |_| {
            let rule = json!({"rule": {"origin": "regular", "styleSheetId": "s1",
                "selectorList": {"text": "#inner", "selectors": [{"text": "#inner", "range": {"startLine": 3, "startColumn": 0}}]},
                "style": {"cssProperties": [{"name": "color", "value": "red", "range": {}}]}}});
            json!({"result": {"matchedCSSRules": [rule]}})
        });
        mock.respond_with("CSS.getComputedStyleForNode", |_| {
            json!({"result": {"computedStyle": [{"name": "color", "value": "rgb(255, 0, 0)"}]}})
        });
    }

    /// ISSUE-59 (Chrome): the element is found by the shared snippet, read
    /// with the CSS domain (its stylesheets' URLs from the sheets announced
    /// on CSS.enable), and DOM and CSS are disabled again; a selector that
    /// names nothing fails saying so.
    #[tokio::test]
    async fn chrome_reads_the_element_styles() {
        use crate::handler::cdp_domains::tests::{
            call, fire_events_after, mock_handler, reply_text, spawn_recording_mock,
        };
        let mock = spawn_recording_mock().await;
        css_page(mock);
        let sheet = json!({"header": {"styleSheetId": "s1", "sourceURL": "http://a/page.html"}});
        fire_events_after(mock, "CSS.enable", vec![("CSS.styleSheetAdded", sheet)]);
        let handler = mock_handler(mock.http_port);
        let reply = GetCssStylesTool::handle(
            call("get_css_styles", json!({"selector": "#inner"})),
            &handler,
        )
        .await
        .unwrap();
        let text = reply_text(&reply);
        assert!(
            text.starts_with("Styles of p#inner.box: 1 matched rule(s)"),
            "{text}"
        );
        assert!(
            text.contains("\"stylesheet\": \"http://a/page.html\""),
            "{text}"
        );
        assert!(text.contains("\"color\": \"rgb(255, 0, 0)\""), "{text}");
        assert_eq!(
            mock.commands_named("DOM.requestNode")[0].params,
            json!({"objectId": "o1"})
        );
        for method in ["CSS.disable", "DOM.disable", "Runtime.releaseObject"] {
            assert_eq!(mock.commands_named(method).len(), 1, "{method}");
        }
        let error = GetCssStylesTool::handle(
            call("get_css_styles", json!({"selector": "#nope"})),
            &handler,
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(error, not_found("#nope"));
    }

    /// Sheets of a page `page` none of which was found elsewhere.
    fn page_sheets(page: &str) -> Sheets {
        Sheets {
            page: page.into(),
            ..Default::default()
        }
    }

    /// The captured `getApplied` reply of `button#go` (r9 CSS page).
    fn captured_applied() -> Value {
        let captured = include_str!("../../tests/fixtures/rdp/css-get-applied.json");
        serde_json::from_str(captured).unwrap()
    }

    /// ISSUE-86 (Firefox 159, captured): each sheet the rules come from
    /// gives one sample rule (selector and declarations), in order.
    #[test]
    fn firefox_sheets_are_sampled() {
        let samples = firefox_sheet_samples(&captured_applied());
        let ids: Vec<&str> = samples.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "server1.conn1.child25/windowGlobalTarget2:stylesheet:0",
                "server1.conn1.child25/windowGlobalTarget2:stylesheet:1"
            ]
        );
        assert_eq!(
            (samples[1].selector.as_str(), samples[1].css_text.as_str()),
            (".card .btn", "color: blue !important;")
        );
    }

    /// ISSUE-86 (Firefox 159, captured): a rule names its sheet only by
    /// `parentStyleSheet`; each sheet gives one sample rule (selector and
    /// declarations) to find it in the page, and the URL found is the
    /// rule's stylesheet (it used to be the page's for every rule). The
    /// `@container` at-rule keeps its name and query.
    #[test]
    fn firefox_rules_name_their_stylesheet() {
        let applied = captured_applied();
        // Given the sheets of the samples found in the page
        let samples = firefox_sheet_samples(&applied);
        let page = "http://127.0.0.1:8766/r9-css.html";
        let sheet = "http://127.0.0.1:8766/r9-css.css";
        let by_id = HashMap::from([
            (samples[0].id.clone(), page.to_string()),
            (samples[1].id.clone(), sheet.to_string()),
        ]);
        let styles = from_firefox(
            "button#go.btn",
            &applied,
            &json!({}),
            &Sheets {
                page: page.into(),
                by_id,
            },
        );
        // Then each rule names its own sheet
        let at = |selector: &str| {
            let rule = styles
                .matched
                .iter()
                .find(|r| r.selector == selector)
                .unwrap();
            rule.stylesheet.clone().unwrap()
        };
        assert_eq!(at("#go"), page);
        assert_eq!(at(".card .btn"), sheet);
        // And @container keeps its name and query
        let container = styles
            .matched
            .iter()
            .find(|r| r.at_rules.iter().any(|a| a.starts_with("@container")))
            .unwrap();
        assert_eq!(container.at_rules, ["@container box (min-width: 100px)"]);
        // And a sheet the page did not find is the page's, as before
        let styles = from_firefox("button#go.btn", &applied, &json!({}), &page_sheets(page));
        assert!(
            styles
                .matched
                .iter()
                .all(|r| r.stylesheet.as_deref() != Some(sheet))
        );
    }

    /// ISSUE-86 (Chrome): a named container query keeps its name.
    #[test]
    fn chrome_container_queries_keep_their_name() {
        let rule = json!({"selectorList": {"text": ".btn", "selectors": [{"text": ".btn"}]}, "origin": "regular",
            "containerQueries": [{"text": "(min-width: 100px)", "name": "box"}, {"text": "(width > 1px)"}],
            "style": {"cssProperties": []}});
        let rule = chrome_rule(&rule, &HashMap::new()).unwrap();
        assert_eq!(
            rule.at_rules,
            [
                "@container box (min-width: 100px)",
                "@container (width > 1px)"
            ]
        );
    }

    /// ISSUE-86: the page snippet looks for each sample in the element's
    /// sheets (its root's, adopted ones, imported and nested rules) and
    /// answers their URLs by id.
    #[test]
    fn the_sheet_lookup_names_each_sample() {
        let samples = vec![SheetSample {
            id: "S:1".into(),
            selector: ".card .btn".into(),
            css_text: "color: blue !important;".into(),
        }];
        let snippet = sheet_lookup("document.querySelector('#go')", &samples);
        assert!(
            snippet.contains("document.querySelector('#go')"),
            "{snippet}"
        );
        assert!(
            snippet.contains(r#"["S:1",".card .btn","color: blue !important;"]"#),
            "{snippet}"
        );
        for part in [
            "adoptedStyleSheets",
            "styleSheet",
            "cssRules",
            "selectorText",
            "cssText",
        ] {
            assert!(snippet.contains(part), "{part}");
        }
    }

    /// A Firefox `getApplied` reply with the same rules as `chrome_matched`.
    fn firefox_applied() -> Value {
        let decl = |name: &str, value: &str| json!({"name": name, "value": value, "priority": "", "isValid": true});
        let entry = |selectors: Value, line: Value, decls: Vec<Value>| {
            json!({"inherited": null, "pseudoElement": "",
            "rule": {"type": 1, "selectors": selectors, "line": line, "column": 1, "href": null, "ancestorData": [], "declarations": decls}})
        };
        let mut element = entry(Value::Null, Value::Null, vec![decl("font-weight", "bold")]);
        element["rule"]["type"] = json!(100);
        element["rule"]["href"] = json!("http://a/page.html");
        let mut media = entry(
            json!(["#inner"]),
            json!(5),
            vec![decl("border", "1px solid black")],
        );
        media["rule"]["ancestorData"] = json!([{"type": "media", "value": "(min-width: 100px)"}]);
        let mut inner = entry(
            json!(["#inner"]),
            json!(4),
            vec![decl("color", "red"), decl("colr", "x")],
        );
        inner["rule"]["declarations"][1]["isValid"] = json!(false);
        let mut boxed = entry(json!([".box"]), json!(3), vec![decl("color", "green")]);
        boxed["rule"]["declarations"][0]["priority"] = json!("important");
        let mut before = entry(
            json!(["#inner::before"]),
            json!(6),
            vec![decl("content", "\"x\"")],
        );
        before["pseudoElement"] = json!("::before");
        let mut outer = entry(json!(["#outer"]), json!(2), vec![decl("color", "blue")]);
        outer["inherited"] = json!("domnode9");
        json!({"entries": [element, media, inner, boxed, before, outer]})
    }

    /// ISSUE-59 (Firefox): the entries of `getApplied` in their order, the
    /// `style` attribute as `element.style`, 1-based lines made 0-based,
    /// `ancestorData` as at-rules, `!important` from `priority`, pseudo-
    /// elements and ancestors apart; the computed values.
    #[test]
    fn firefox_styles_are_read() {
        let applied = firefox_applied();
        let computed = json!({"computed": {"color": {"matched": true, "value": "rgb(0, 128, 0)"}}});
        let styles = from_firefox(
            "p#inner",
            &applied,
            &computed,
            &page_sheets("http://a/page.html"),
        );
        let selectors: Vec<&str> = styles.matched.iter().map(|r| r.selector.as_str()).collect();
        assert_eq!(selectors, ["element.style", "#inner", "#inner", ".box"]);
        assert_eq!(styles.matched[0].origin, "inline-style");
        let media = &styles.matched[1];
        assert_eq!(
            (media.line, media.at_rules.clone()),
            (Some(4), vec!["@media (min-width: 100px)".to_string()])
        );
        assert_eq!(
            media.stylesheet.as_deref(),
            Some("http://a/page.html"),
            "an inline <style> is the page's"
        );
        assert!(styles.matched[2].declarations[1].invalid);
        assert!(styles.matched[3].declarations[0].important);
        assert_eq!(styles.pseudo_elements[0].0, "::before");
        assert_eq!(styles.inherited[0][0].selector, "#outer");
        assert_eq!(styles.computed["color"], "rgb(0, 128, 0)");
    }
}
