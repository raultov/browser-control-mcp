//! Frames of another origin (#27). The outline and the action snippets run
//! as JavaScript in a document, and the same-origin policy keeps them out
//! of other origins' frames. Each such frame is reached as a target of its
//! own instead (Chrome: the frame's out-of-process target, or the tab's
//! session evaluating in the frame's document; Firefox: the frame's
//! browsing context), where the same snippets run and input lands in the
//! frame's own coordinates.
//!
//! `snapshot` numbers the frames it reads (per tab, never reused) and
//! prefixes their refs: `f2e5` is the element `e5` of frame 2, and
//! `f2 >>> css` a CSS selector inside it. Every selector argument takes
//! them; this table maps the number back to the frame.

use rust_mcp_sdk::schema::CallToolError;
use serde_json::Value;
use std::sync::RwLock;

/// Most frames remembered per tab (oldest forgotten first; their refs then
/// ask for a new snapshot).
pub(crate) const MAX_FRAMES: usize = 100;

/// Most tabs with remembered frames.
const MAX_TABS: usize = 64;

/// Most frames of another origin a snapshot reads (ads pages embed dozens).
pub(crate) const MAX_READ: usize = 20;

/// Deepest nesting of frames of another origin a snapshot follows.
pub(crate) const MAX_NESTING: usize = 5;

/// What the outline prints under a frame it cannot read.
pub(crate) const UNREADABLE: &str = "text: (another origin: not readable)";

/// A selector into a frame of another origin.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InFrame {
    /// The frame's number in the tab's table.
    pub(crate) frame: usize,
    /// The selector inside it: a ref (`e5`), CSS, or `None` for the frame's
    /// whole document (`f2`).
    pub(crate) inner: Option<String>,
}

/// `selector` when it names something inside a frame of another origin:
/// `f2e5`, `f2 >>> css`, or `f2` alone (the frame's document). No HTML
/// element is named `f2` (custom elements need a hyphen).
pub(crate) fn in_frame(selector: &str) -> Option<InFrame> {
    let selector = selector.trim();
    let rest = selector.strip_prefix('f')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let frame = rest[..digits].parse().ok().filter(|n| *n > 0)?;
    let tail = rest[digits..].trim_start();
    let inner = if tail.is_empty() {
        None
    } else if let Some(css) = tail.strip_prefix(crate::util::js::PIERCE) {
        let css = css.trim();
        if css.is_empty() {
            return None;
        }
        Some(css.to_string())
    } else if tail.len() == rest.len() - digits && crate::util::js::is_ref(tail) {
        Some(tail.to_string())
    } else {
        return None;
    };
    Some(InFrame { frame, inner })
}

impl InFrame {
    /// The element selector inside the frame, or why there is none (a
    /// bare `f2` names a document, not an element).
    pub(crate) fn element(&self, selector: &str) -> Result<&str, CallToolError> {
        self.inner.as_deref().ok_or_else(|| {
            CallToolError::from_message(format!(
                "'{selector}' names a frame, not an element: pass a ref inside it (f{0}e5) or 'f{0} >>> css'.",
                self.frame
            ))
        })
    }
}

/// A frame `snapshot` read: how the browser knows it, and where its
/// `<iframe>` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameEntry {
    /// Chrome: the frame id; Firefox: the browsing context id.
    pub(crate) key: String,
    /// The frame whose document holds the `<iframe>` (0: the tab's page).
    pub(crate) parent: usize,
    /// The ref of the `<iframe>` element in that document.
    pub(crate) owner: String,
}

/// The frames of one tab, by number.
#[derive(Debug, Default)]
pub(crate) struct FrameTable {
    entries: crate::util::bounded::BoundedMap<usize, FrameEntry, MAX_FRAMES>,
    last: usize,
}

impl FrameTable {
    /// The number of the frame `entry.key`: the one it already has, else
    /// the next one (numbers are never reused, so an old ref cannot reach
    /// another frame).
    pub(crate) fn register(&mut self, entry: FrameEntry) -> usize {
        let known = self
            .entries
            .iter()
            .find(|(_, e)| e.key == entry.key)
            .map(|(n, _)| *n);
        let number = known.unwrap_or_else(|| {
            self.last += 1;
            self.last
        });
        self.entries.insert(number, entry);
        number
    }

    /// Frame `number` and its ancestors up to the page, innermost first.
    pub(crate) fn chain(&self, number: usize) -> Result<Vec<FrameEntry>, CallToolError> {
        let mut chain = Vec::new();
        let mut current = number;
        while current != 0 {
            let Some(entry) = self.entries.get(&current) else {
                return Err(unknown_frame(number));
            };
            if chain.len() > MAX_NESTING * 4 {
                return Err(unknown_frame(number));
            }
            chain.push(entry.clone());
            current = entry.parent;
        }
        Ok(chain)
    }
}

/// The error for a frame number the tab does not know (any more).
pub(crate) fn unknown_frame(number: usize) -> CallToolError {
    CallToolError::from_message(format!(
        "Frame f{number} is not known in this tab (frame refs last until the page navigates): take a new snapshot."
    ))
}

/// The frame tables of every tab, by tab key (browser and tab).
#[derive(Debug, Default)]
pub(crate) struct FrameTables(
    RwLock<crate::util::bounded::BoundedMap<String, FrameTable, MAX_TABS>>,
);

impl FrameTables {
    /// Runs `f` on the table of tab `tab` (created empty).
    pub(crate) fn with<T>(&self, tab: &str, f: impl FnOnce(&mut FrameTable) -> T) -> T {
        let mut tables = crate::util::sync::write(&self.0);
        if tables.get_mut(tab).is_none() {
            tables.insert(tab.to_string(), FrameTable::default());
        }
        match tables.get_mut(tab) {
            Some(table) => f(table),
            None => f(&mut FrameTable::default()),
        }
    }
}

/// `lines` of frame `number`'s outline with their refs prefixed (`e5` →
/// `f2e5`), so action tools route them to the frame.
pub(crate) fn prefix_refs(lines: &str, number: usize) -> String {
    lines.replace("[ref=e", &format!("[ref=f{number}e"))
}

/// `outline` with `frame_lines` (none for an empty frame) in place of the
/// line saying that the frame whose `<iframe>` has ref `owner` cannot be
/// read (indented like it), and the `<iframe>` marked with the frame's
/// number (`[frame=f2]`), so `f2 >>> css` can reach a frame whose elements
/// have no refs. Unchanged when the `<iframe>` or that line is not there.
pub(crate) fn splice(outline: &str, owner: &str, number: usize, frame_lines: &str) -> String {
    let marker = format!("[ref={owner}]");
    let marked = outline
        .lines()
        .find(|line| line.contains(&marker))
        .map(|line| line.replacen(&marker, &format!("{marker} [frame=f{number}]"), 1));
    let mut lines: Vec<&str> = outline.lines().collect();
    let Some(at) = lines.iter().position(|line| line.contains(&marker)) else {
        return outline.to_string();
    };
    let Some(note) = lines
        .get(at + 1)
        .filter(|l| l.trim_start().ends_with(UNREADABLE))
    else {
        return outline.to_string();
    };
    let indent = &note[..note.len() - note.trim_start().len()];
    let indented: Vec<String> = frame_lines
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect();
    let indented: Vec<&str> = indented.iter().map(String::as_str).collect();
    lines.splice(at + 1..at + 2, indented);
    if let Some(marked) = &marked {
        lines[at] = marked;
    }
    lines.join("\n")
}

/// The nesting depth of the line holding ref `owner` (0 at the top level).
pub(crate) fn depth_of(outline: &str, owner: &str) -> Option<usize> {
    let marker = format!("[ref={owner}]");
    let line = outline.lines().find(|line| line.contains(&marker))?;
    Some((line.len() - line.trim_start().len()) / 2)
}

/// How a browser reaches the frames of a tab: it evaluates in one, and
/// finds the frame an `<iframe>` shows.
pub(crate) trait FrameReader {
    /// A frame of another origin, ready to evaluate in.
    type Frame: Clone;

    /// The value of `expression` in `frame` (`None`: the tab's page).
    async fn evaluate(
        &self,
        frame: Option<&Self::Frame>,
        expression: &str,
    ) -> Result<Value, CallToolError>;

    /// The frame shown by the `<iframe>` whose ref is `owner` in `parent`'s
    /// document, and its key; with `scroll`, the `<iframe>` is first
    /// scrolled into view there.
    async fn child(
        &self,
        parent: Option<&Self::Frame>,
        owner: &str,
        scroll: bool,
    ) -> Result<(Self::Frame, String), CallToolError>;
}

/// The way to frame `number` of a tab.
#[derive(Debug)]
pub(crate) struct Route<F> {
    /// The frame, marked as `number` ([`crate::util::js::frame_mark`]).
    pub(crate) frame: F,
    /// Each `<iframe>` from the page down to the frame, outermost first:
    /// the frame holding it (`None`: the page) and its ref there.
    pub(crate) levels: Vec<(Option<F>, String)>,
}

/// The selector that decides where a call acting on all of `selectors`
/// runs: one into a frame of another origin when they all are in that
/// frame, else none (they all are in the page). A call cannot act in two
/// documents, so selectors in different frames (or a frame and the page)
/// are refused.
pub(crate) fn one_frame<'a>(
    selectors: impl IntoIterator<Item = &'a str>,
) -> Result<Option<&'a str>, CallToolError> {
    let mut first: Option<(&str, Option<usize>)> = None;
    for selector in selectors {
        let frame = in_frame(selector).map(|inside| inside.frame);
        match first {
            None => first = Some((selector, frame)),
            Some((_, known)) if known == frame => {}
            Some((other, _)) => {
                return Err(CallToolError::from_message(format!(
                    "'{other}' and '{selector}' are in different documents (frames of another origin have refs and selectors starting with fN): act on each in its own call."
                )));
            }
        }
    }
    Ok(first
        .filter(|(_, frame)| frame.is_some())
        .map(|(selector, _)| selector))
}

/// Room an outline needs to read one more frame (a few lines).
const MIN_ROOM: usize = 200;

/// A frame of another origin an outline could not read.
struct Pending<F> {
    /// The frame whose document holds its `<iframe>` (0: the page).
    parent: usize,
    /// That frame, to evaluate in (`None`: the page).
    frame: Option<F>,
    /// The ref of the `<iframe>` there.
    owner: String,
    /// How many frames of another origin it is inside, itself included.
    nesting: usize,
}

/// The refs of the unreadable iframes an outline snippet listed.
fn unreadable(outcome: &Value) -> Vec<String> {
    outcome["foreign"]
        .as_array()
        .map(|refs| {
            refs.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `ref` of frame `number`'s document as the outline shows it.
fn shown_ref(number: usize, owner: &str) -> String {
    if number == 0 {
        owner.to_string()
    } else {
        format!("f{number}{owner}")
    }
}

/// The frames of one tab as a browser reaches them: its way in (`reader`),
/// the numbers `snapshot` gave (`tables`, under the tab's key `tab`).
pub(crate) struct TabFrames<'a, R> {
    pub(crate) reader: &'a R,
    pub(crate) tables: &'a FrameTables,
    pub(crate) tab: &'a str,
}

/// How much of the page an outline covers.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    /// Most nesting levels (`None`: all).
    pub(crate) depth: Option<u32>,
    /// Most characters overall.
    pub(crate) budget: usize,
}

impl<R: FrameReader> TabFrames<'_, R> {
    /// Enters frame `number` (a number `snapshot` gave): from the page,
    /// through each `<iframe>` on the way (scrolled into view), checking it
    /// still shows the frame the snapshot read.
    pub(crate) async fn enter(&self, number: usize) -> Result<Route<R::Frame>, CallToolError> {
        let chain = self.tables.with(self.tab, |table| table.chain(number))?;
        let mut parent: Option<R::Frame> = None;
        let mut levels = Vec::new();
        for entry in chain.into_iter().rev() {
            let found = self.reader.child(parent.as_ref(), &entry.owner, true).await;
            let (frame, key) = found.map_err(|_| unknown_frame(number))?;
            if key != entry.key {
                return Err(unknown_frame(number));
            }
            levels.push((parent.take(), entry.owner));
            parent = Some(frame);
        }
        let frame = parent.ok_or_else(|| unknown_frame(number))?;
        let mark = crate::util::js::frame_mark(number);
        self.reader.evaluate(Some(&frame), &mark).await?;
        Ok(Route { frame, levels })
    }

    /// `snapshot`'s outline (the snippet's value, see `util::outline`) of
    /// the page, or of the element or frame `selector` names, with the
    /// frames of another origin in it outlined in place
    /// ([`Self::with_frames`]).
    pub(crate) async fn snapshot(
        &self,
        selector: Option<&str>,
        depth: Option<u32>,
    ) -> Result<Value, CallToolError> {
        let bounds = Bounds {
            depth,
            budget: crate::util::outline::SNAPSHOT_MAX_CHARS,
        };
        let (base, scope) = match selector.and_then(in_frame) {
            None => ((0, None), selector),
            Some(inside) => {
                let route = self.enter(inside.frame).await?;
                let scope = selector.filter(|_| inside.inner.is_some());
                ((inside.frame, Some(route.frame)), scope)
            }
        };
        let expression = crate::util::outline::snapshot(scope, depth, bounds.budget);
        let outcome = self.reader.evaluate(base.1.as_ref(), &expression).await?;
        Ok(self.with_frames(base, outcome, bounds).await)
    }

    /// `find_in_snapshot`'s search (`expression`, of `text` or `regex`,
    /// see `util::outline::find`). When the page has frames of another
    /// origin the search runs again over the whole outline with them in
    /// place.
    pub(crate) async fn find(
        &self,
        expression: &str,
        (text, regex): (Option<&str>, Option<&str>),
    ) -> Result<Value, CallToolError> {
        let found = self.reader.evaluate(None, expression).await?;
        if unreadable(&found).is_empty() {
            return Ok(found);
        }
        let bounds = Bounds {
            depth: None,
            budget: crate::util::outline::FIND_MAX_CHARS,
        };
        let page = crate::util::outline::snapshot(None, None, bounds.budget);
        let page = self.reader.evaluate(None, &page).await?;
        let whole = self.with_frames((0, None), page, bounds).await;
        let lines: Vec<&str> = whole["text"].as_str().unwrap_or_default().lines().collect();
        let search = crate::util::outline::find_in_lines(&lines, text, regex);
        self.reader.evaluate(None, &search).await
    }

    /// `outcome`, an outline snippet's value (of the page, or of frame
    /// `base.0` with `base.1` to evaluate in), with every frame of another
    /// origin it could not read outlined in place ([`splice`]): in page
    /// order, nested ones included, at most [`MAX_READ`] frames
    /// [`MAX_NESTING`] deep, within `bounds` (cut on a line, with
    /// `truncated`). Each frame read gets its number in the tables and its
    /// refs the prefix ([`prefix_refs`]); one that cannot be reached keeps
    /// its "not readable" line. Nothing is scrolled.
    pub(crate) async fn with_frames(
        &self,
        (base_number, base_frame): (usize, Option<R::Frame>),
        mut outcome: Value,
        bounds: Bounds,
    ) -> Value {
        if outcome["status"] != "ok" {
            return outcome;
        }
        let mut text = outcome["text"].as_str().unwrap_or_default().to_string();
        if base_number != 0 {
            text = prefix_refs(&text, base_number);
        }
        let mut truncated = outcome["truncated"] == true;
        let mut pending = pending_of(base_number, base_frame, unreadable(&outcome), 1);
        let mut read = 0;
        while let Some(next) = pending.pop() {
            if read >= MAX_READ || next.nesting > MAX_NESTING {
                continue;
            }
            let shown = shown_ref(next.parent, &next.owner);
            let Some(left) = depth_of(&text, &shown).and_then(|at| left_below(bounds.depth, at))
            else {
                continue;
            };
            let room = bounds.budget.saturating_sub(text.len());
            if room < MIN_ROOM {
                truncated = true;
                break;
            }
            let Some((number, frame, child)) = self.read(&next, left, room).await else {
                continue;
            };
            read += 1;
            truncated |= child["truncated"] == true;
            let lines = prefix_refs(child["text"].as_str().unwrap_or_default(), number);
            text = splice(&text, &shown, number, &lines);
            pending.extend(pending_of(
                number,
                Some(frame),
                unreadable(&child),
                next.nesting + 1,
            ));
        }
        if text.len() > bounds.budget {
            let cut = text[..text.floor_char_boundary(bounds.budget)]
                .rfind('\n')
                .unwrap_or(0);
            text.truncate(cut);
            truncated = true;
        }
        outcome["text"] = Value::String(text);
        outcome["truncated"] = Value::Bool(truncated);
        outcome
    }

    /// Reads the frame `next` is about, `depth` levels deep (`None`: all)
    /// and at most `room` characters: its number, the frame, and its
    /// outline; `None` when it cannot be reached.
    async fn read(
        &self,
        next: &Pending<R::Frame>,
        depth: Option<u32>,
        room: usize,
    ) -> Option<(usize, R::Frame, Value)> {
        let found = self
            .reader
            .child(next.frame.as_ref(), &next.owner, false)
            .await;
        let (frame, key) = found.ok()?;
        let entry = FrameEntry {
            key,
            parent: next.parent,
            owner: next.owner.clone(),
        };
        let number = self.tables.with(self.tab, |table| table.register(entry));
        let expression = format!(
            "({}, {})",
            crate::util::js::frame_mark(number),
            crate::util::outline::snapshot(None, depth, room)
        );
        let child = self.reader.evaluate(Some(&frame), &expression).await.ok()?;
        Some((number, frame, child))
    }
}

/// The frames `refs` name in frame `parent` (`frame` to evaluate in), as
/// a stack that pops them in page order.
fn pending_of<F: Clone>(
    parent: usize,
    frame: Option<F>,
    refs: Vec<String>,
    nesting: usize,
) -> Vec<Pending<F>> {
    refs.into_iter()
        .rev()
        .map(|owner| Pending {
            parent,
            frame: frame.clone(),
            owner,
            nesting,
        })
        .collect()
}

/// The depth left for a frame whose `<iframe>` is at level `at`: `Some(None)`
/// without a limit, `Some(Some(n))` within `depth`, `None` below it.
fn left_below(depth: Option<u32>, at: usize) -> Option<Option<u32>> {
    match depth {
        None => Some(None),
        Some(depth) => depth
            .checked_sub(u32::try_from(at).unwrap_or(u32::MAX).saturating_add(1))
            .map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-27: frame refs and frame CSS selectors are recognised; plain
    /// refs, CSS and look-alikes are not.
    #[test]
    fn frame_selectors_are_recognised() {
        let ok = |s: &str, frame, inner: Option<&str>| {
            assert_eq!(
                in_frame(s),
                Some(InFrame {
                    frame,
                    inner: inner.map(str::to_string)
                }),
                "{s}"
            );
        };
        ok("f2e5", 2, Some("e5"));
        ok(" f12e340 ", 12, Some("e340"));
        ok("f3 >>> input[name=card]", 3, Some("input[name=card]"));
        ok("f3>>>#a >>> #b", 3, Some("#a >>> #b"));
        ok("f4", 4, None);
        for other in [
            "e5", "#f2e5", "f0e1", "fe5", "f2 e5", "f2e", "form", "f2 >>> ", "f2x",
        ] {
            assert_eq!(in_frame(other), None, "{other}");
        }
    }

    /// ISSUE-27: a bare frame names no element.
    #[test]
    fn a_bare_frame_is_not_an_element() {
        let error = in_frame("f4")
            .unwrap()
            .element("f4")
            .unwrap_err()
            .to_string();
        assert!(error.contains("names a frame, not an element"), "{error}");
        assert_eq!(in_frame("f4e1").unwrap().element("f4e1").unwrap(), "e1");
    }

    fn entry(key: &str, parent: usize, owner: &str) -> FrameEntry {
        FrameEntry {
            key: key.into(),
            parent,
            owner: owner.into(),
        }
    }

    /// ISSUE-27: a frame keeps its number across snapshots; numbers are
    /// never reused; the chain goes up to the page.
    #[test]
    fn frames_keep_their_numbers() {
        let mut table = FrameTable::default();
        assert_eq!(table.register(entry("A", 0, "e3")), 1);
        assert_eq!(table.register(entry("B", 1, "e7")), 2);
        assert_eq!(table.register(entry("A", 0, "e4")), 1);
        let chain = table.chain(2).unwrap();
        assert_eq!(chain, vec![entry("B", 1, "e7"), entry("A", 0, "e4")]);
        let error = table.chain(9).unwrap_err().to_string();
        assert!(error.contains("Frame f9 is not known"), "{error}");
    }

    /// ISSUE-27: the oldest frames are forgotten past the cap, and a new
    /// frame never takes an old number.
    #[test]
    fn frame_numbers_are_bounded_and_not_reused() {
        let mut table = FrameTable::default();
        for i in 0..MAX_FRAMES + 5 {
            table.register(entry(&format!("K{i}"), 0, "e1"));
        }
        assert!(table.chain(1).is_err());
        assert!(table.chain(MAX_FRAMES + 5).is_ok());
        assert_eq!(table.register(entry("new", 0, "e1")), MAX_FRAMES + 6);
    }

    /// ISSUE-27: a frame's outline replaces the "not readable" line under
    /// its iframe, indented like it, with its refs prefixed.
    #[test]
    fn frame_outlines_are_spliced_under_their_iframe() {
        let outline = "- main [ref=e1]\n  - iframe \"Pay\" [ref=e2]\n    - text: (another origin: not readable)\n  - button \"Go\" [ref=e3]";
        let frame = prefix_refs("- textbox \"Card\" [ref=e1]\n  - text: hint", 4);
        let spliced = splice(outline, "e2", 4, &frame);
        assert_eq!(
            spliced,
            "- main [ref=e1]\n  - iframe \"Pay\" [ref=e2] [frame=f4]\n    - textbox \"Card\" [ref=f4e1]\n      - text: hint\n  - button \"Go\" [ref=e3]"
        );
        assert_eq!(splice(outline, "e9", 4, &frame), outline);
        let empty =
            "- main [ref=e1]\n  - iframe \"Pay\" [ref=e2] [frame=f4]\n  - button \"Go\" [ref=e3]";
        assert_eq!(splice(outline, "e2", 4, ""), empty);
        assert_eq!(depth_of(outline, "e2"), Some(1));
        assert_eq!(depth_of(outline, "e9"), None);
    }

    /// A page with two frames of another origin: "pay" (with a nested
    /// frame "inner") and "ad". Frames are named; each evaluation and
    /// child lookup is recorded.
    struct FakeReader {
        calls: std::sync::Mutex<Vec<String>>,
    }

    fn outcome(text: &str, foreign: &[&str]) -> Value {
        serde_json::json!({"status": "ok", "url": "http://a/", "title": "", "text": text, "truncated": false, "foreign": foreign})
    }

    impl FrameReader for FakeReader {
        type Frame = String;

        async fn evaluate(
            &self,
            frame: Option<&String>,
            expression: &str,
        ) -> Result<Value, CallToolError> {
            let frame = frame.map_or("page", String::as_str);
            self.calls.lock().unwrap().push(format!("eval {frame}"));
            if expression.starts_with("(() => { const lines = ") {
                let searched = expression.contains("[ref=f3e1]");
                return Ok(serde_json::json!({"searched": searched}));
            }
            if !expression.contains("outline(") {
                return Ok(Value::Null);
            }
            Ok(match frame {
                "pay" => outcome(
                    "- textbox \"Card\" [ref=e1]\n- iframe [ref=e2]\n  - text: (another origin: not readable)",
                    &["e2"],
                ),
                "inner" => outcome("- button \"OK\" [ref=e1]", &[]),
                "page" => outcome(PAGE, &["e2", "e4"]),
                _ => outcome("- link \"Ad\" [ref=e1]", &[]),
            })
        }

        async fn child(
            &self,
            parent: Option<&String>,
            owner: &str,
            scroll: bool,
        ) -> Result<(String, String), CallToolError> {
            let parent = parent.map_or("page", String::as_str);
            self.calls
                .lock()
                .unwrap()
                .push(format!("child {parent} {owner} {scroll}"));
            match (parent, owner) {
                ("page", "e2") => Ok(("pay".into(), "K-pay".into())),
                ("page", "e4") => Ok(("ad".into(), "K-ad".into())),
                ("pay", "e2") => Ok(("inner".into(), "K-inner".into())),
                _ => Err(CallToolError::from_message("no frame")),
            }
        }
    }

    const PAGE: &str = "- main [ref=e1]\n  - iframe \"Pay\" [ref=e2]\n    - text: (another origin: not readable)\n  - iframe [ref=e4]\n    - text: (another origin: not readable)\n  - button \"Go\" [ref=e3]";

    /// The frames of tab "tab" through `reader`.
    fn of<'a>(reader: &'a FakeReader, tables: &'a FrameTables) -> TabFrames<'a, FakeReader> {
        TabFrames {
            reader,
            tables,
            tab: "tab",
        }
    }

    fn bounds(depth: Option<u32>, budget: usize) -> Bounds {
        Bounds { depth, budget }
    }

    fn reader() -> FakeReader {
        FakeReader {
            calls: std::sync::Mutex::default(),
        }
    }

    /// ISSUE-27: the outline of each frame of another origin replaces its
    /// "not readable" line, numbered in page order, nested frames
    /// included, with frame-prefixed refs; nothing is scrolled.
    #[tokio::test]
    async fn frames_are_outlined_in_place() {
        // Given a page with a payment frame (holding another frame) and an ad
        let (reader, tables) = (reader(), FrameTables::default());
        // When its outline is completed
        let done = of(&reader, &tables)
            .with_frames(
                (0, None),
                outcome(PAGE, &["e2", "e4"]),
                bounds(None, 50_000),
            )
            .await;
        // Then every frame is in place, under its iframe
        let expected = "- main [ref=e1]\n  - iframe \"Pay\" [ref=e2] [frame=f1]\n    - textbox \"Card\" [ref=f1e1]\n    - iframe [ref=f1e2] [frame=f2]\n      - button \"OK\" [ref=f2e1]\n  - iframe [ref=e4] [frame=f3]\n    - link \"Ad\" [ref=f3e1]\n  - button \"Go\" [ref=e3]";
        assert_eq!(done["text"], expected);
        assert_eq!(done["truncated"], false);
        let calls = reader.calls.lock().unwrap().clone();
        assert!(calls.iter().all(|c| !c.ends_with("true")), "{calls:?}");
        // And the frames can be found again
        let chain = tables.with("tab", |t| t.chain(2)).unwrap();
        let keys: Vec<&str> = chain.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["K-inner", "K-pay"]);
    }

    /// ISSUE-27: a snapshot depth stops at the frames below it, and a
    /// small budget cuts the outline, saying so.
    #[tokio::test]
    async fn depth_and_size_bound_the_frames() {
        let (reader, tables) = (reader(), FrameTables::default());
        let shallow = of(&reader, &tables)
            .with_frames(
                (0, None),
                outcome(PAGE, &["e2", "e4"]),
                bounds(Some(1), 50_000),
            )
            .await;
        assert_eq!(shallow["text"], PAGE);
        let small = of(&reader, &tables)
            .with_frames(
                (0, None),
                outcome(PAGE, &["e2", "e4"]),
                bounds(None, PAGE.len() + 10),
            )
            .await;
        assert_eq!(small["truncated"], true);
        assert!(small["text"].as_str().unwrap().len() <= PAGE.len() + 10);
    }

    /// ISSUE-27: one call acts in one document: selectors of one frame
    /// route there, page selectors stay, a mix is refused.
    #[test]
    fn a_call_acts_in_one_document() {
        assert_eq!(one_frame(["#a", "e3"]).unwrap(), None);
        assert_eq!(one_frame(["f2e1", "f2 >>> #b"]).unwrap(), Some("f2e1"));
        for mixed in [["f2e1", "#a"], ["e3", "f2e1"], ["f2e1", "f3e1"]] {
            let error = one_frame(mixed).unwrap_err().to_string();
            assert!(error.contains("different documents"), "{error}");
        }
    }

    /// ISSUE-27: a snapshot scoped to a frame starts there, its refs
    /// prefixed; one of the page reads its frames in place.
    #[tokio::test]
    async fn snapshots_start_in_the_page_or_a_frame() {
        let (reader, tables) = (reader(), FrameTables::default());
        let page = of(&reader, &tables).snapshot(None, None).await.unwrap();
        assert!(
            page["text"].as_str().unwrap().contains("[ref=f3e1]"),
            "{page}"
        );
        let framed = of(&reader, &tables)
            .snapshot(Some("f1"), None)
            .await
            .unwrap();
        assert_eq!(
            framed["text"],
            "- textbox \"Card\" [ref=f1e1]\n- iframe [ref=f1e2] [frame=f2]\n  - button \"OK\" [ref=f2e1]"
        );
        let missing = of(&reader, &tables).snapshot(Some("f9"), None).await;
        assert!(missing.is_err());
    }

    /// ISSUE-27: a search of a page without frames of another origin is the
    /// page's; with them it runs again over the whole outline.
    #[tokio::test]
    async fn searches_cover_the_frames() {
        let (reader, tables) = (reader(), FrameTables::default());
        let found = of(&reader, &tables)
            .find("outline(", (Some("x"), None))
            .await
            .unwrap();
        assert_eq!(found["searched"], true, "{found}");
    }

    /// ISSUE-27: entering frame 2 walks from the page through its iframes,
    /// scrolling each into view, and marks the frame; a frame whose iframe
    /// now shows another frame is refused.
    #[tokio::test]
    async fn entering_a_frame_follows_its_iframes() {
        let (reader, tables) = (reader(), FrameTables::default());
        of(&reader, &tables)
            .with_frames(
                (0, None),
                outcome(PAGE, &["e2", "e4"]),
                bounds(None, 50_000),
            )
            .await;
        reader.calls.lock().unwrap().clear();
        let route = of(&reader, &tables).enter(2).await.unwrap();
        assert_eq!(route.frame, "inner");
        assert_eq!(
            route.levels,
            vec![
                (None, "e2".to_string()),
                (Some("pay".to_string()), "e2".to_string())
            ]
        );
        let calls = reader.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            ["child page e2 true", "child pay e2 true", "eval inner"]
        );
        let moved = tables.with("tab", |t| {
            t.register(FrameEntry {
                key: "K-moved".into(),
                parent: 0,
                owner: "e4".into(),
            })
        });
        let error = of(&reader, &tables)
            .enter(moved)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("take a new snapshot"), "{error}");
        assert!(
            TabFrames {
                reader: &reader,
                tables: &tables,
                tab: "other"
            }
            .enter(1)
            .await
            .is_err()
        );
    }
}
