//! The console of a Firefox tab's workers, read from their own targets
//! (#213). A Firefox launched with the debugger keeps each worker's console
//! calls in the worker ([`super::launch::WORKER_CONSOLE_PREF`]), so the
//! worker's target reports them: a dedicated worker's (WebDriver BiDi then
//! has none) and the service worker's (which reach no other feed). A
//! Firefox attached with the default hands them to the page: BiDi has the
//! dedicated workers' and the targets report none. Either way each message
//! comes from one feed only ([`Channel::Sole`]).
//!
//! Every announcement of a worker's target (a first watch, a watch again, a
//! service worker started again under a new actor) first sends again what
//! the worker logged before: a message is listed once per worker `id`,
//! stamp (Firefox's, to the microsecond), level and arguments.
//!
//! The targets send `console.count`'s `counter` and a timer's `timer`
//! empty (Firefox 160): the count is kept per worker global (its target
//! actor) and the time read from the stamps, so the line reads as
//! Firefox's own text does on the page (`c: 2`, `t: 1.5ms - timer
//! ended`).

use crate::handler::cdp_domains::log::{
    Channel, ConsoleMessage, array_preview, canonical_level, object_preview, quoted,
};
use crate::util::bounded::BoundedMap;
use crate::util::json::str_field;
use serde_json::Value;

/// How many messages [`WorkerConsole`] remembers to tell a replay: as many
/// as the console keeps.
const SEEN: usize = crate::handler::MAX_CONSOLE_MESSAGES;

/// How many counters and timers it keeps, all workers together.
const LABELS: usize = 256;

/// The worker a console message came from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Worker<'a> {
    /// Its target actor: a new one is a new global (a service worker
    /// started again) or a watch again, whose counters start over.
    pub(crate) actor: &'a str,
    /// Its `id`, the same across its announcements and restarts.
    pub(crate) id: &'a str,
    /// Whether it is the tab's service worker (its lines say so).
    pub(crate) service: bool,
}

/// What a tab remembers of its workers' console: the messages listed, to
/// tell a replay, and the counters and timers the targets do not send.
/// Bounded.
#[derive(Debug, Default)]
pub(crate) struct WorkerConsole {
    seen: BoundedMap<String, (), SEEN>,
    counts: BoundedMap<(String, String), u64, LABELS>,
    timers: BoundedMap<(String, String), f64, LABELS>,
}

impl WorkerConsole {
    /// The console message of a `console-message` resource of `worker`'s
    /// target; `None` for one listed before (a replay) or one that prints
    /// nothing (`console.time`, `console.groupEnd`). A replay still counts
    /// and times, so a new actor's counters come back as they were.
    pub(crate) fn message(
        &mut self,
        worker: Worker<'_>,
        resource: &Value,
    ) -> Option<ConsoleMessage> {
        let level = str_field(resource, "level").unwrap_or("log");
        let stamp = resource
            .get("timeStamp")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let args: &[Value] = resource
            .get("arguments")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        let printed: Vec<String> = args.iter().map(|arg| grip_text(arg, true)).collect();
        let call = Call {
            level,
            printed: &printed,
            stamp,
        };
        let text = self.text(worker.actor, &call)?;
        let key = format!(
            "{}\u{0}{}\u{0}{level}\u{0}{}",
            worker.id,
            stamp.to_bits(),
            printed.join("\u{0}")
        );
        if self.seen.contains_key(&key) {
            return None;
        }
        self.seen.insert(key, ());
        let line = resource.get("lineNumber").and_then(Value::as_i64);
        Some(ConsoleMessage {
            source: "console-api".to_string(),
            level: canonical_level(level),
            text,
            timestamp: stamp,
            url: str_field(resource, "filename")
                .filter(|url| !url.is_empty())
                .map(str::to_string),
            // Firefox counts lines from 1; the console reports 0-based ones.
            line_number: line.filter(|&l| l > 0).map(|l| l - 1),
            channel: Channel::Sole,
            document: None,
            uncaught: false,
            target_type: worker
                .service
                .then(|| crate::handler::SERVICE_WORKER.to_string()),
        })
    }

    /// The text of `call`, made in the global of target `actor`; counts
    /// and times as Firefox's own text does.
    fn text(&mut self, actor: &str, call: &Call<'_>) -> Option<String> {
        let Call {
            level,
            printed,
            stamp,
        } = *call;
        let label = || {
            let label = printed.first().map_or("default", String::as_str);
            (actor.to_string(), label.to_string())
        };
        let rest = || printed.get(1..).unwrap_or_default().join(" ");
        match level {
            "count" | "countReset" => {
                let key = label();
                let count = match level {
                    "count" => self.counts.get(&key).copied().unwrap_or(0) + 1,
                    _ => 0,
                };
                let text = format!("{}: {count}", key.1);
                self.counts.insert(key, count);
                Some(text)
            }
            "time" => {
                self.timers.insert(label(), stamp);
                None
            }
            "timeLog" | "timeEnd" => {
                let key = label();
                let Some(&start) = self.timers.get(&key) else {
                    return Some(printed.join(" "));
                };
                let time = format!("{}: {}ms", key.1, duration(stamp - start));
                Some(match level {
                    "timeEnd" => {
                        self.timers.remove(&key);
                        format!("{time} - timer ended")
                    }
                    _ if printed.len() > 1 => format!("{time} {}", rest()),
                    _ => time,
                })
            }
            "groupEnd" if printed.is_empty() => None,
            _ => Some(printed.join(" ")),
        }
    }
}

/// One console call of a worker: its level (the console method), its
/// arguments as printed and its stamp.
struct Call<'a> {
    level: &'a str,
    printed: &'a [String],
    stamp: f64,
}

/// A timer's milliseconds as Firefox's text gives them: to the hundredth,
/// without trailing zeros (`0.01`, `0`, `12.5`).
fn duration(ms: f64) -> String {
    let rounded = (ms.max(0.0) * 100.0).round() / 100.0;
    format!("{rounded}")
}

/// An RDP grip (a console argument) as the console prints it, as the
/// page's messages read (see `pump::bidi_value`): at the top level strings
/// as is, objects and arrays previewed; inside a preview strings quoted and
/// objects by their class.
fn grip_text(grip: &Value, top: bool) -> String {
    match grip {
        Value::String(text) if top => text.clone(),
        Value::String(text) => quoted(text),
        Value::Null => "null".to_string(),
        Value::Bool(_) | Value::Number(_) => grip.to_string(),
        Value::Array(_) => "Array".to_string(),
        Value::Object(_) => match str_field(grip, "type").unwrap_or_default() {
            "object" => object_text(grip, top),
            "BigInt" => format!("{}n", str_field(grip, "text").unwrap_or_default()),
            "symbol" => format!("Symbol({})", str_field(grip, "name").unwrap_or_default()),
            "longString" => {
                let start = str_field(grip, "initial").unwrap_or_default();
                match top {
                    true => format!("{start}…"),
                    false => quoted(start),
                }
            }
            // null, undefined, NaN, Infinity, -Infinity, -0
            other => other.to_string(),
        },
    }
}

/// An object grip as the console prints it: a plain object or an array
/// previewed at the top level, an error as `Name: message`, a map or a set
/// by its class and size, anything else (and anything inside a preview)
/// by its class.
fn object_text(grip: &Value, top: bool) -> String {
    let class = str_field(grip, "class").unwrap_or("Object");
    let preview = grip.get("preview").filter(|p| p.is_object());
    let size = |key: &str| preview.and_then(|p| p.get(key)).and_then(Value::as_u64);
    let kind = preview.and_then(|p| str_field(p, "kind"));
    match kind {
        Some("Error") => {
            let preview = preview.unwrap_or(&Value::Null);
            let name = str_field(preview, "name").unwrap_or(class);
            match str_field(preview, "message").filter(|m| !m.is_empty()) {
                Some(message) => format!("{name}: {message}"),
                None => name.to_string(),
            }
        }
        Some("ArrayLike") if class == "Array" && top => {
            let items = preview
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array);
            let items: Vec<String> = items
                .into_iter()
                .flatten()
                .map(|item| grip_text(item, false))
                .collect();
            let overflow = size("length").is_some_and(|n| n > items.len() as u64);
            array_preview(items, overflow)
        }
        // `console.table` sends its array with its own properties instead.
        None if class == "Array" && top && grip.get("ownProperties").is_some() => {
            array_preview(own_items(grip), false)
        }
        Some("ArrayLike") => format!("{class}({})", size("length").unwrap_or(0)),
        Some("MapLike") => format!("{class}({})", size("size").unwrap_or(0)),
        Some("Object") if top => {
            let properties = preview.and_then(|p| p.get("ownProperties"));
            let entries: Vec<(String, String)> = properties
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(name, property)| (name.clone(), property_text(property)))
                .collect();
            let overflow = size("ownPropertiesLength").is_some_and(|n| n > entries.len() as u64);
            let object = object_preview(entries, overflow);
            match class {
                "Object" => object,
                class => format!("{class} {object}"),
            }
        }
        _ if class == "Array" => format!("Array({})", size("length").unwrap_or(0)),
        _ => class.to_string(),
    }
}

/// The items of an array grip that lists its own properties (`0`, `1`...)
/// rather than a preview, in index order, printed inside a preview.
fn own_items(grip: &Value) -> Vec<String> {
    let mut items: Vec<(u64, String)> = grip
        .get("ownProperties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(name, property)| Some((name.parse().ok()?, property_text(property))))
        .collect();
    items.sort_by_key(|(index, _)| *index);
    items.into_iter().map(|(_, text)| text).collect()
}

/// A property of an object preview: its value inside a preview, or
/// `(...)` for an accessor (a getter is not run).
fn property_text(property: &Value) -> String {
    match property.get("value") {
        Some(value) => grip_text(value, false),
        None => "(...)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bidi_browser_lite::test_support::fixtures::{
        RDP_SERVICE_WORKER_RESOURCES_CONSOLE_MESSAGE, RDP_WORKER_RESOURCES_CONSOLE_MESSAGE,
    };
    use serde_json::json;

    fn resources(fixture: &str) -> Vec<Value> {
        let packet: Value = serde_json::from_str(fixture).unwrap();
        packet["array"][0][1].as_array().unwrap().clone()
    }

    const DEDICATED: Worker<'static> = Worker {
        actor: "server1.conn0.watcher58.process8//workerTarget19/workerTarget5",
        id: "114418db-0bea-4620-8753-7213d0faf32a",
        service: false,
    };

    /// #213: a worker's console messages, from its own target (Firefox
    /// 160), read as the page's do: arguments previewed, the level in the
    /// console's words, the script and its 1-based line, and a service
    /// worker's marked as its.
    #[test]
    fn worker_messages_read_as_the_pages() {
        // Given a dedicated worker's console.warn with an object and its
        // console.error, and a service worker's
        let dedicated = resources(RDP_WORKER_RESOURCES_CONSOLE_MESSAGE);
        let service = resources(RDP_SERVICE_WORKER_RESOURCES_CONSOLE_MESSAGE);
        let sw = Worker {
            actor: "server1.conn3.watcher184.process26//workerTarget1/workerTarget5",
            id: "d3aa6c32-5908-4563-a36a-d9c7040684f7",
            service: true,
        };
        let mut console = WorkerConsole::default();
        // When they are read
        let warn = console.message(DEDICATED, &dedicated[0]).unwrap();
        let error = console.message(DEDICATED, &dedicated[1]).unwrap();
        let installed = console.message(sw, &service[0]).unwrap();
        // Then they read as the page's
        assert_eq!(
            (warn.level.as_str(), warn.text.as_str()),
            ("warning", "dw warn {a: 1}")
        );
        assert_eq!(
            warn.url.as_deref(),
            Some("http://127.0.0.1:38241/dw/worker.js")
        );
        assert_eq!(warn.line_number, Some(3));
        assert_eq!(warn.timestamp, 1_795_726_142_687.598);
        assert_eq!(
            (warn.source.as_str(), warn.target_type),
            ("console-api", None)
        );
        assert_eq!(
            (error.level.as_str(), error.text.as_str()),
            ("error", "dw error")
        );
        assert!(!error.uncaught);
        assert_eq!(installed.text, "sw warn 42");
        assert_eq!(installed.target_type.as_deref(), Some("service_worker"));
        assert_eq!(installed.channel, Channel::Sole);
    }

    /// #213: each announcement of a worker's target sends again what it
    /// logged before: a message already listed, by the worker's id, stamp,
    /// level and arguments, is not listed again, under a new actor too;
    /// another worker's, or the next one, is.
    #[test]
    fn a_replayed_message_is_listed_once() {
        // Given a message listed
        let dedicated = resources(RDP_WORKER_RESOURCES_CONSOLE_MESSAGE);
        let mut console = WorkerConsole::default();
        assert!(console.message(DEDICATED, &dedicated[0]).is_some());
        // When it comes again, from a new actor of the same worker
        let again = Worker {
            actor: "server1.conn0.watcher58.process8//workerTarget19/workerTarget9",
            ..DEDICATED
        };
        // Then it is not listed again; the next one, and another
        // worker's same message, are
        assert!(console.message(again, &dedicated[0]).is_none());
        assert!(console.message(again, &dedicated[1]).is_some());
        let other = Worker {
            id: "another",
            ..DEDICATED
        };
        assert!(console.message(other, &dedicated[0]).is_some());
    }

    /// A `console.*` call of `level` with `args`, from a worker's target,
    /// at `at`.
    fn call(level: &str, args: Value, at: f64) -> Value {
        json!({"level": level, "arguments": args, "counter": {}, "timer": {},
            "filename": "http://a/w.js", "lineNumber": 1, "timeStamp": at})
    }

    /// The level and text `console` lists for a call of `worker`.
    fn listed(
        console: &mut WorkerConsole,
        worker: Worker<'_>,
        (level, args, at): (&str, Value, f64),
    ) -> Option<(String, String)> {
        console
            .message(worker, &call(level, args, at))
            .map(|m| (m.level, m.text))
    }

    fn line(level: &str, text: &str) -> Option<(String, String)> {
        Some((level.to_string(), text.to_string()))
    }

    /// #213: the targets send `console.count`'s counter empty: the count
    /// is kept per worker global, as Firefox's text on the page gives it;
    /// a replay under a new actor counts again without listing.
    #[test]
    fn counts_read_as_firefox_prints_them() {
        // When a worker counts twice and resets
        let mut console = WorkerConsole::default();
        let first = listed(&mut console, DEDICATED, ("count", json!(["c"]), 1.0));
        let second = listed(&mut console, DEDICATED, ("count", json!(["c"]), 2.0));
        let reset = listed(&mut console, DEDICATED, ("countReset", json!(["c"]), 3.0));
        // Then each reads as Firefox prints it
        assert_eq!(first, line("count", "c: 1"));
        assert_eq!(second, line("count", "c: 2"));
        assert_eq!(reset, line("countreset", "c: 0"));
        // And a replay under a new actor counts again without listing
        let again = Worker {
            actor: "new-actor",
            ..DEDICATED
        };
        assert_eq!(
            listed(&mut console, again, ("count", json!(["c"]), 1.0)),
            None
        );
        assert_eq!(
            listed(&mut console, again, ("count", json!(["c"]), 2.0)),
            None
        );
        let next = listed(&mut console, again, ("count", json!(["c"]), 40.0));
        assert_eq!(next, line("count", "c: 3"));
    }

    /// #213: the targets send a timer's time empty: it is read from the
    /// stamps, as Firefox's text on the page gives it; `console.time` and
    /// an empty `console.groupEnd` print nothing.
    #[test]
    fn timers_read_as_firefox_prints_them() {
        // When a worker times, ends an unknown timer and groups
        let mut console = WorkerConsole::default();
        let calls = [
            ("time", json!(["t"]), 10.0),
            ("timeLog", json!(["t", "more"]), 10.003),
            ("timeEnd", json!(["t"]), 22.5075),
            ("timeEnd", json!(["t"]), 30.0),
            ("group", json!(["g"]), 31.0),
            ("groupEnd", json!([]), 32.0),
        ];
        let lines: Vec<_> = calls
            .into_iter()
            .map(|c| listed(&mut console, DEDICATED, c))
            .collect();
        // Then each reads as Firefox prints it
        assert_eq!(
            lines,
            [
                None,
                line("timelog", "t: 0ms more"),
                line("timeend", "t: 12.51ms - timer ended"),
                line("timeend", "t"),
                line("group", "g"),
                None,
            ]
        );
    }

    /// #213: console arguments read as the page's: primitives, Firefox's
    /// special numbers, big integers, symbols, objects and arrays
    /// previewed with their values quoted inside, errors as `Name:
    /// message`, maps and sets by their class and size, long strings by
    /// their start.
    #[test]
    fn arguments_read_as_the_console_prints_them() {
        let object = |class: &str, preview: Value| json!({"type": "object", "class": class, "preview": preview});
        let props = json!({"kind": "Object", "ownPropertiesLength": 3, "ownProperties": {
            "a": {"value": 1}, "s": {"value": "x"},
            "o": {"value": {"type": "object", "class": "Object", "preview": {"kind": "Object"}}}}});
        let args = json!([
            "top", 3, 1.5, true, {"type": "null"}, {"type": "undefined"}, {"type": "NaN"},
            {"type": "-0"}, {"type": "BigInt", "text": "10"}, {"type": "symbol", "name": "sy"},
            object("Object", props),
            object("Array", json!({"kind": "ArrayLike", "length": 3, "items": [1, "two", {"type": "null"}]})),
            object("Error", json!({"kind": "Error", "name": "TypeError", "message": "bad"})),
            object("Map", json!({"kind": "MapLike", "size": 1, "entries": [[1, 2]]})),
            object("Set", json!({"kind": "ArrayLike", "length": 2, "items": [3, 4]})),
            object("Function", Value::Null),
            {"type": "longString", "initial": "abc", "length": 70000},
            {"type": "object", "class": "Array", "preview": null, "ownProperties": {
                "1": {"value": 2}, "0": {"value": {"type": "object", "class": "Object"}}, "length": {"value": 2}}}
        ]);
        let printed: Vec<String> = args
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| grip_text(arg, true))
            .collect();
        assert_eq!(
            printed,
            [
                "top",
                "3",
                "1.5",
                "true",
                "null",
                "undefined",
                "NaN",
                "-0",
                "10n",
                "Symbol(sy)",
                "{a: 1, o: Object, s: 'x'}",
                "[1, 'two', null]",
                "TypeError: bad",
                "Map(1)",
                "Set(2)",
                "Function",
                "abc…",
                "[Object, 2]",
            ]
        );
    }
}
