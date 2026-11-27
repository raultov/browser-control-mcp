//! Top-level `await` in the code `evaluate_js` runs: found by parsing the
//! code, and run as an async function returning the last expression
//! statement's value, as a console does (Firefox's `script.evaluate` has
//! no top-level await, nor has Chrome's `Runtime.evaluate`).
//!
//! The code is parsed by a JavaScript parser (oxc) as the body of an async
//! function, so statements, regular expressions, templates, labels and
//! automatic semicolons are read as the browser reads them: the scanner it
//! replaces missed one construct after another (#135-#179).
//!
//! This is a convenience with a stated limit, not a repair of the agent's
//! code: code the parser refuses, or longer than [`MAX_PARSED_LEN`], is
//! taken to have no top-level `await` and `evaluate_js` runs it once, as
//! written; the browser's error is the answer ([`over_limit_note`] says why
//! for long code). It is never run a second time on what an error says
//! (#188: a runtime error means part of the code already ran).
//!
//! The parser is recursive, and a stack overflow aborts the whole server:
//! it runs in a thread of its own whose stack grows with the code's length
//! (`STACK_PER_BYTE`), and a panic there only means "not parsed".

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrowFunctionExpression, AwaitExpression, ForOfStatement, Function, FunctionBody, Statement,
    VariableDeclaration, VariableDeclarationKind,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType, Span};
use oxc_syntax::scope::ScopeFlags;

/// Longest code parsed (bytes). Longer code is run as written, without
/// top-level `await`: the stack the parser may need grows with the length.
pub const MAX_PARSED_LEN: usize = 64 * 1024;

/// What an error of code longer than [`MAX_PARSED_LEN`] is told with: the
/// code ran as a plain script, and how to await in it. `None` for code
/// within the limit. It goes with any error of such code, whatever the
/// error says.
pub fn over_limit_note(code: &str) -> Option<&'static str> {
    (code.len() > MAX_PARSED_LEN).then_some(
        "Note: the expression is over 64 KB, so it ran as a plain script. If it uses await at its top level, wrap it in an async function: (async () => { ...; return value })()",
    )
}

/// Stack of the parse thread for empty code.
const STACK_BASE: usize = 8 << 20;

/// Stack of the parse thread per byte of code. Every level of the
/// parser's recursion takes at least one byte of code; the deepest cost
/// measured (oxc 0.159, x86_64) is about 1.25 KB a byte in a release build
/// (`let [[[[...`) and 4.2 KB in a debug build (`[[[[...`): about three
/// times that. The stack is only reserved: pages are used as the
/// recursion reaches them.
const STACK_PER_BYTE: usize = if cfg!(debug_assertions) {
    12 << 10
} else {
    4 << 10
};

/// What the code opens with when parsed: the body of an async function,
/// where `await` is an expression.
const OPEN: &str = "async function w() { ";

/// An expression as `evaluate_js` runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncForm {
    /// Whether the code uses `await` outside every function (or
    /// `for await`, `await using`), which only runs as an async function:
    /// a script refuses it, or reads `await (x)` as a call to a function
    /// named await (#135).
    pub awaits: bool,
    /// The code as an async function called at once, returning its last
    /// expression statement's value (none when the last statement is a
    /// declaration or a control statement). What runs when [`Self::awaits`]
    /// (but on a page paused in Chrome's debugger, where nothing can be
    /// awaited).
    pub wrapped: String,
    /// Where the code sits in [`Self::wrapped`].
    layout: Layout,
}

/// What [`AsyncForm::wrapped`] opens with.
const WRAP_OPEN: &str = "(async () => { ";

/// What [`AsyncForm::wrapped`] puts before the last expression.
const WRAP_RETURN: &str = "return (";

/// Where the code sits in [`AsyncForm::wrapped`], as browsers count
/// positions: 0-based lines and columns in UTF-16 units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    /// Where [`WRAP_RETURN`] went in: the last expression's start.
    returned: Option<(usize, usize)>,
    /// Where the code kept ends (the last expression's, or the code's);
    /// the wrapper's closing follows.
    end: (usize, usize),
}

impl AsyncForm {
    /// The position in the code of `line` and `column` in
    /// [`Self::wrapped`] (0-based, columns in UTF-16 units, as browsers
    /// report an exception's), `None` in the wrapper (#221). Both browsers
    /// place an error of the returned expression at `return`: that is the
    /// expression's start.
    pub fn position_in_code(&self, line: usize, column: usize) -> Option<(usize, usize)> {
        let Layout { returned, end } = self.layout;
        let mut column = match line {
            0 => column.checked_sub(WRAP_OPEN.len())?,
            _ => column,
        };
        if let Some((at_line, at)) = returned
            && line == at_line
            && column >= at
        {
            column = column.saturating_sub(WRAP_RETURN.len()).max(at);
        }
        ((line, column) < end).then_some((line, column))
    }
}

/// The position of byte `at` of `code`: 0-based line (after each line
/// terminator, `\r\n` counting once) and column in UTF-16 units.
fn position_of(code: &str, at: usize) -> (usize, usize) {
    let mut chars = code.get(..at).unwrap_or(code).chars().peekable();
    let (mut line, mut column) = (0, 0);
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' | '\u{2028}' | '\u{2029}' => (line, column) = (line + 1, 0),
            _ => column += c.len_utf16(),
        }
    }
    (line, column)
}

/// [`AsyncForm`] of `code`, from one parse.
pub fn async_form(code: &str) -> AsyncForm {
    let code = &*closing_comment_at_start(code);
    let parsed = parsed(code);
    let last = parsed.as_ref().and_then(|p| p.last).and_then(|span| {
        let start = usize::try_from(span.start).ok()?.checked_sub(OPEN.len())?;
        let end = usize::try_from(span.end).ok()?.checked_sub(OPEN.len())?;
        Some((code.get(..start)?, code.get(start..end)?))
    });
    let (wrapped, layout) = match last {
        // The line break ends a comment inside the last line.
        Some((head, last)) => (
            format!("{WRAP_OPEN}{head}{WRAP_RETURN}{last}\n); }})()"),
            Layout {
                returned: Some(position_of(code, head.len())),
                end: position_of(code, head.len() + last.len()),
            },
        ),
        None => (
            format!("{WRAP_OPEN}{code}\n}})()"),
            Layout {
                returned: None,
                end: position_of(code, code.len()),
            },
        ),
    };
    AsyncForm {
        awaits: parsed.is_some_and(|p| p.awaits),
        wrapped,
        layout,
    }
}

/// `code` with the `-->` it starts with (after spaces) written `// `: an
/// HTML-like closing comment (Annex B), which only counts at the start of
/// a line, where the wrapper's opening would no longer leave it. As long,
/// so lines and columns stay.
fn closing_comment_at_start(code: &str) -> std::borrow::Cow<'_, str> {
    let indent = code.len() - code.trim_start_matches([' ', '\t']).len();
    match code
        .get(indent..)
        .is_some_and(|rest| rest.starts_with("-->"))
    {
        true => std::borrow::Cow::Owned(format!("{}// {}", &code[..indent], &code[indent + 3..])),
        false => std::borrow::Cow::Borrowed(code),
    }
}

/// Whether `code` uses `await` outside every function ([`AsyncForm::awaits`]).
pub fn has_top_level_await(code: &str) -> bool {
    async_form(code).awaits
}

/// `code` as an async function called at once ([`AsyncForm::wrapped`]).
pub fn with_top_level_await(code: &str) -> String {
    async_form(code).wrapped
}

/// What the parse of the code found.
struct Parsed {
    awaits: bool,
    /// The span of the last expression statement's expression, in the
    /// parsed source (after [`OPEN`]), when the last statement is one.
    last: Option<Span>,
}

/// The parse of `code`, in a thread whose stack fits its length; `None`
/// when it is too long, does not parse, or the thread could not run.
fn parsed(code: &str) -> Option<Parsed> {
    if code.len() > MAX_PARSED_LEN {
        return None;
    }
    let stack = STACK_BASE.saturating_add(code.len().saturating_mul(STACK_PER_BYTE));
    std::thread::scope(|scope| {
        let thread = std::thread::Builder::new()
            .name("js-parse".into())
            .stack_size(stack);
        thread
            .spawn_scoped(scope, || parse(code))
            .ok()?
            .join()
            .ok()?
    })
}

/// Parses `code` as the body of an async function.
fn parse(code: &str) -> Option<Parsed> {
    let source = format!("{OPEN}{code}\n}}");
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, &source, SourceType::script()).parse();
    if parsed.fatal_error || parsed.diagnostics.has_errors() {
        return None;
    }
    // Code that closes the function and opens another is not one body.
    let [Statement::FunctionDeclaration(function)] = parsed.program.body.as_slice() else {
        return None;
    };
    let body = function.body.as_ref()?;
    let mut finder = AwaitFinder::default();
    finder.visit_statements(&body.statements);
    Some(Parsed {
        awaits: finder.found,
        last: last_expression(body),
    })
}

/// The expression of the body's last statement (empty ones left out),
/// when it is an expression statement, labelled or not; else the last
/// directive's, when there is no other statement (`'v'`).
fn last_expression(body: &FunctionBody<'_>) -> Option<Span> {
    let mut statements = body.statements.iter().rev();
    match statements.find(|s| !matches!(s, Statement::EmptyStatement(_))) {
        Some(statement) => statement_expression(statement),
        None => body.directives.last().map(|d| d.expression.span),
    }
}

/// The expression of `statement`, an expression statement or a label on
/// one (`l: 'v'`).
fn statement_expression(mut statement: &Statement<'_>) -> Option<Span> {
    loop {
        match statement {
            Statement::ExpressionStatement(it) => return Some(it.expression.span()),
            Statement::LabeledStatement(it) => statement = &it.body,
            _ => return None,
        }
    }
}

/// Finds `await` outside every function: in an expression, a
/// `for await` loop or an `await using` declaration. Function bodies
/// (with braces or an arrow's without) are not read; a class's computed
/// keys are, as they run where the class is.
#[derive(Default)]
struct AwaitFinder {
    found: bool,
}

impl<'a> Visit<'a> for AwaitFinder {
    fn visit_await_expression(&mut self, _: &AwaitExpression<'a>) {
        self.found = true;
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        self.found |= it.r#await;
        walk::walk_for_of_statement(self, it);
    }

    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        self.found |= it.kind == VariableDeclarationKind::AwaitUsing;
        walk::walk_variable_declaration(self, it);
    }

    fn visit_function(&mut self, _: &Function<'a>, _: ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _: &ArrowFunctionExpression<'a>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-43: an expression using `await` at its top level becomes an
    /// async function returning its last expression statement, as a
    /// console does; strings, brackets, comments and chained lines do not
    /// split statements; a final declaration returns nothing and runs as
    /// written.
    #[test]
    fn top_level_await_is_wrapped() {
        let wrapped = |code: &str| with_top_level_await(code);
        assert_eq!(
            wrapped("await Promise.resolve(41) + 1"),
            "(async () => { return (await Promise.resolve(41) + 1\n); })()"
        );
        assert_eq!(
            wrapped("const r = await fetch('/a;b'); r.status;"),
            "(async () => { const r = await fetch('/a;b'); return (r.status\n); })()"
        );
        assert_eq!(
            wrapped("const r = await f({a: 1, b: [2, 3], c: ';'})\nr.ok // done; really"),
            "(async () => { const r = await f({a: 1, b: [2, 3], c: ';'})\nreturn (r.ok\n); })()"
        );
        assert_eq!(
            wrapped("await fetch(x)\n  .then(r => r.status)"),
            "(async () => { return (await fetch(x)\n  .then(r => r.status)\n); })()"
        );
        assert_eq!(
            wrapped("await 1; const z = `a;${1}`;"),
            "(async () => { await 1; const z = `a;${1}`;\n})()"
        );
    }

    /// #138: a last expression starting with a declaring keyword's letters
    /// (`done`, `document`, `format`, `tryCount`) is returned, not taken
    /// for a declaration.
    #[test]
    fn identifiers_starting_with_keywords_are_returned() {
        for (code, last) in [
            ("const done = await p; done", "done"),
            ("await x; document.title", "document.title"),
            ("const format = await p; format", "format"),
            ("await x; iframes.length", "iframes.length"),
            ("await x; tryCount", "tryCount"),
            ("await x; classes", "classes"),
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then its last expression is returned
            assert!(wrapped.contains(&format!("return ({last}\n)")), "{wrapped}");
        }
    }

    /// #147: a last declaration written without a space after its keyword
    /// (destructuring, minified, a tab) returns nothing instead of being
    /// wrapped in `return (...)`.
    #[test]
    fn declarations_without_a_space_return_nothing() {
        for code in [
            "const{a}=await Promise.resolve({a:5})",
            "let[b]=await Promise.resolve([6])",
            "var\tv = await Promise.resolve(7)",
            "await x; {}",
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then it runs as is in the async function
            assert_eq!(wrapped, format!("(async () => {{ {code}\n}})()"));
        }
    }

    /// #151: a `catch`, `finally` or `else` on its own line after the `}`
    /// of its block goes on with the `try` or `if`, which runs as is.
    #[test]
    fn keywords_on_their_own_line_continue_the_statement() {
        for code in [
            "try {\n  await Promise.reject(new Error('boom'))\n}\ncatch (e) {\n  window.qaCaught = e.message\n}",
            "try {\n  await Promise.resolve(1)\n}\nfinally {\n  window.qaFin = 'fin-ok'\n}",
            "if (await Promise.resolve(false)) {\n  window.qaBr = 'then'\n}\nelse {\n  window.qaBr = 'else-ok'\n}",
            "do {\n  await x\n}\nwhile (false)",
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then it runs as is in the async function
            assert_eq!(wrapped, format!("(async () => {{ {code}\n}})()"));
        }
        // And a word starting with one of them is returned
        let wrapped = with_top_level_await("await x\nelsewhere");
        assert!(wrapped.ends_with("return (elsewhere\n); })()"), "{wrapped}");
    }

    /// #155: a line ending with an operator, `?`, `:` or a keyword waiting
    /// for its operand goes on on the next line: the statement is not cut
    /// there. A line ending with `x++` or `return` still ends it.
    #[test]
    fn trailing_operators_continue_on_the_next_line() {
        for (code, last) in [
            (
                "await Promise.resolve(40) +\n  2",
                "await Promise.resolve(40) +\n  2",
            ),
            (
                "window.qaV =\n  await Promise.resolve('v')",
                "window.qaV =\n  await Promise.resolve('v')",
            ),
            (
                "const f = await Promise.resolve(3)\nf ===\n  3 ?\n  'yes' :\n  'no'",
                "f ===\n  3 ?\n  'yes' :\n  'no'",
            ),
            ("const t = await x\ntypeof\n  t", "typeof\n  t"),
            (
                "const n = await x\nn.map(v =>\n  v + 1)",
                "n.map(v =>\n  v + 1)",
            ),
            ("let y = await x\ny++\ny", "y"),
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then the whole last statement is returned
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped}"
            );
        }
        // And an arrow function's body goes on after an operator too
        assert!(!has_top_level_await("const g = async x => x +\n  await x"));
    }

    /// #156: a value after the `}` of a block statement on the same line
    /// is the last statement, returned; the `}` of an object or a function
    /// is not the end of a statement, and `else`, `catch` and `finally`
    /// never start one.
    #[test]
    fn values_after_a_block_are_returned() {
        for (code, last) in [
            ("for(let i=0;i<2;i++){await 0}'done'", "'done'"),
            ("if(1){await 0}'x'", "'x'"),
            ("{await 0}'blk'", "'blk'"),
            ("for await (const x of xs){f(x)}'end'", "'end'"),
            ("try{await x}catch(e){}finally{}'t'", "'t'"),
            ("try{await x}catch{}`c`", "`c`"),
            ("if(a){await x}else{y}'e'", "'e'"),
            ("while(await f()){}[1, 2]", "[1, 2]"),
            ("switch(await x){case 1: y}'s'", "'s'"),
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then the value after the block is returned
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped}"
            );
        }
        for code in [
            "await x; if (a) y = function(){}(1)",
            "if (await a) b; else c",
            "await x; o = {a: 1}['a']",
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then no statement starts inside it
            assert!(
                !wrapped.contains("return (function")
                    && !wrapped.contains("return (else")
                    && !wrapped.contains("return (['a']"),
                "{wrapped}"
            );
        }
    }

    /// #162: a value after the `}` of a function or class declaration on
    /// the same line is the last statement, returned; an `async function`
    /// declaration is a declaration; a line ending with an identifier named
    /// `of` ends its statement.
    #[test]
    fn values_after_a_declaration_are_returned() {
        for (code, last) in [
            ("await 0; function f(){ return 7 } f()", "f()"),
            (
                "await 0; class A { v() { return 8 } } new A().v()",
                "new A().v()",
            ),
            ("async function g(){ return 9 } await g()", "await g()"),
            ("await 0; function* h(){ yield 1 } [...h()]", "[...h()]"),
            ("await 0; class B extends (class {}) {} new B()", "new B()"),
            ("await 0; function h(){ return 7 }\nh()", "h()"),
            (
                "const of = await Promise.resolve(3)\nconst y = of\ny * 2",
                "y * 2",
            ),
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then the value after the declaration is returned
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped}"
            );
        }
        for code in [
            "await 0; async function g(){ return 9 }",
            "await 0; async function g(){ return 9 };",
            "await 0; class C { m() {} }",
        ] {
            // When the last statement is a declaration
            let wrapped = with_top_level_await(code);
            // Then it runs as is in the async function
            assert!(!wrapped.contains("return ("), "{wrapped}");
        }
        for code in [
            "await 0; x = function(){ return 1 }()",
            "await 0; x = class { v() {} }\n.name",
            "await 0; const k = async function(){ return 1 }",
        ] {
            // When a function or class is an expression
            let wrapped = with_top_level_await(code);
            // Then no statement starts after its body
            assert!(
                !wrapped.contains("return (()") && !wrapped.contains("return (.name"),
                "{wrapped}"
            );
        }
    }

    /// #169: a property named like a keyword (`o.do`, `o.new`, `o?.in`)
    /// at a line's end ends its statement, and a value after a do-while's
    /// condition on the same line is the last statement, returned.
    #[test]
    fn values_after_keyword_properties_and_do_while_are_returned() {
        for (code, last) in [
            (
                "const o = await Promise.resolve({do: 4041})\nconst v = o.do\nv * 2",
                "v * 2",
            ),
            (
                "const o = await Promise.resolve({new: 5051})\nconst v = o.new\nv + 1",
                "v + 1",
            ),
            (
                "const o = await Promise.resolve({in: 6061})\nlet w = o?.in\nw - 1",
                "w - 1",
            ),
            ("await 0; do { } while (false) 'dw-ok'", "'dw-ok'"),
            ("await 0; do x++; while (x < 3) x", "x"),
            ("await 0; do { } while (false)\n'dw-nl'", "'dw-nl'"),
            ("await 0; o.do /2/ 1", "o.do /2/ 1"),
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then the last expression is returned
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped}"
            );
        }
        for code in [
            "await 0; do { x++ } while (x < 3)",
            "await 0; do {\n  await x\n}\nwhile (false)",
        ] {
            // When the last statement is a do-while
            let wrapped = with_top_level_await(code);
            // Then it runs as is in the async function
            assert!(!wrapped.contains("return ("), "{wrapped}");
        }
    }

    /// #179: a `while` loop that is the body of a braceless `do` (or
    /// nested deeper in it) is not the `do`'s condition, so the value
    /// after the real condition is still the last statement.
    #[test]
    fn values_after_a_do_whose_body_is_a_loop_are_returned() {
        for (code, last) in [
            (
                "await 0; let x = 0; do while (x < 3) x++; while (false) 'v'",
                "'v'",
            ),
            ("await 0; do if (a) while (b) c(); while (d) 'w'", "'w'"),
            (
                "await 0; do if (a) b(); else while (c) d(); while (e) 'e'",
                "'e'",
            ),
            ("await 0; do l: while (a) b(); while (c) 'l'", "'l'"),
            ("await 0; do while (a) { b() } while (c) 'b'", "'b'"),
            ("await 0; do do x(); while (a); while (b) 'n'", "'n'"),
            ("await 0; do\nwhile (a) x++; while (b)\n'nl'", "'nl'"),
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then the value after the do-while is returned
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped}"
            );
        }
        // And a do-while whose body is a loop, last, returns nothing
        let wrapped = with_top_level_await("await 0; do while (a) b(); while (c)");
        assert!(!wrapped.contains("return ("), "{wrapped}");
    }

    /// The differential check against the browser's own `eval` found the
    /// value after a labelled block, a labelled loop with `continue`, an
    /// arrow function whose body is a block, and a line starting with `[`
    /// (a member access of the line before) lost or turned into a syntax
    /// error.
    #[test]
    fn values_found_by_the_differential_check_are_returned() {
        for (code, last) in [
            ("await 0; l2: { } 'v'", "'v'"),
            (
                "await 0; x2: while (0) continue x2\n/v/.source",
                "/v/.source",
            ),
            ("await 0; var g = () => {}\n/v/.source", "/v/.source"),
            ("await 0; [1, 'v']\n[1]", "[1, 'v']\n[1]"),
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then the last expression statement is returned
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped}"
            );
        }
    }

    /// The last expression is cut by bytes: characters of 2 to 4 bytes
    /// before it, in it and after it (strings, identifiers, comments) keep
    /// it whole; line breaks of every kind end a statement.
    #[test]
    fn characters_outside_ascii_and_line_terminators_keep_the_cut() {
        for (code, last) in [
            ("const s = 'Žluť😀'; await 0; s + '✓'", "s + '✓'"),
            ("await 0; /* ünïcödé 😀 */ const ñ = 1\nñ + 'é'", "ñ + 'é'"),
            ("await 0; 'a😀b' // 末尾", "'a😀b'"),
            ("await 0;\r\nconst a = 1\r\na\r\n", "a"),
            ("await 0;\u{2028}const b = 2\u{2029}b", "b"),
            ("\u{feff}await 0; 'bom'", "'bom'"),
            ("await 0;\t\u{b}\u{c}\u{a0}'ws'", "'ws'"),
        ] {
            let wrapped = with_top_level_await(code);
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped:?}"
            );
        }
    }

    /// Annex B comments: `<!--` anywhere and `-->` at a line's start are
    /// comments; a `-->` that starts the code still is, once wrapped (the
    /// wrapper writes it `// `, as long).
    #[test]
    fn html_like_comments_stay_comments() {
        for (code, wrapped) in [
            (
                "--> closer\nawait 0; 'v'",
                "(async () => { //  closer\nawait 0; return ('v'\n); })()",
            ),
            (
                "  --> closer\nawait 0; 'v'",
                "(async () => {   //  closer\nawait 0; return ('v'\n); })()",
            ),
            (
                "await 0; x = 1 <!-- opener\n--> closer\n'v'",
                "(async () => { await 0; x = 1 <!-- opener\n--> closer\nreturn ('v'\n); })()",
            ),
        ] {
            assert_eq!(with_top_level_await(code), wrapped);
        }
        assert!(!has_top_level_await("--> await x\n1"));
        assert_eq!(
            with_top_level_await("x-->0"),
            "(async () => { return (x-->0\n); })()"
        );
    }

    /// Code with nothing to return runs as it is: empty, blank, only
    /// semicolons or comments, or a last statement that is not an
    /// expression (a declaration, a loop, a block, `debugger`).
    #[test]
    fn code_without_a_last_expression_runs_as_it_is() {
        for code in [
            "",
            "   \n\t",
            ";;;",
            "// only\n/* comments */",
            "await 0; debugger",
            "await 0; for (;;) break",
            "await 0; { 'v' }",
            "await 0; if (1) 'v'",
            "await 0; class K {}",
            "await 0; let x = 'v'",
        ] {
            assert_eq!(
                with_top_level_await(code),
                format!("(async () => {{ {code}\n}})()"),
                "{code:?}"
            );
        }
        assert!(!has_top_level_await(""));
    }

    /// The last expression statement is whatever the parser makes of the
    /// last line: a line starting with `(`, `[` or a template goes on with
    /// the line before, as in the browser; one starting with `++` does not;
    /// a sequence, a function and an assignment are returned whole.
    #[test]
    fn automatic_semicolons_are_inserted_as_the_browser_inserts_them() {
        for (code, last) in [
            ("await 0; f\n(1)", "f\n(1)"),
            ("await 0; a\n[0]", "a\n[0]"),
            ("await 0; tag\n`t`", "tag\n`t`"),
            ("await 0; x\n++y", "++y"),
            ("await 0; 'u', 'v'", "'u', 'v'"),
            ("await 0; () => 'v'", "() => 'v'"),
            ("await 0; async x => await x", "async x => await x"),
            ("await 0; o.v ??= await p", "o.v ??= await p"),
            ("await 0; a: b: 'v'", "'v'"),
            (
                "await 0; (class { static v = 'v' }).v",
                "(class { static v = 'v' }).v",
            ),
        ] {
            let wrapped = with_top_level_await(code);
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped:?}"
            );
        }
    }

    /// `await` is found wherever it runs at the top level (control
    /// statements, switch cases, labels, destructuring defaults, optional
    /// chains, computed keys, template substitutions), never inside a
    /// function of any kind, nor where it is a name or a property.
    #[test]
    fn await_is_found_only_at_the_top_level() {
        for code in [
            "switch (x) { case await y: break }",
            "l: { await x }",
            "const { a = await d } = o",
            "o?.[await k]",
            "({ [await k]: 1 })",
            "try {} finally { await x }",
            "for (const x of await xs) {}",
            "if (a) b; else await c",
            "`${`${await x}`}`",
            "x = class { static [await k] = 1 }",
        ] {
            assert!(has_top_level_await(code), "{code}");
        }
        for code in [
            "async function f() { await x }",
            "const f = async () => await x",
            "const f = async x => { for await (const y of x) {} }",
            "({ async m() { await x }, get g() { return 1 } })",
            "class C { async m() { await x } static async s() { await y } }",
            "async function* g() { yield await x }",
            "o.await; o?.await; ({ await: 1 }).await",
            "'await x'; `await ${'x'}`; /await x/",
            "var async = 1; async",
        ] {
            assert!(!has_top_level_await(code), "{code}");
        }
    }

    /// A code exactly [`MAX_PARSED_LEN`] bytes long is parsed, one byte
    /// more is not; a character of several bytes counts for its bytes.
    #[test]
    fn the_length_limit_counts_bytes() {
        let at = |len: usize, pad: char| {
            let fill = len - "await 0;'v'".len();
            let pad = pad.to_string().repeat(fill / pad.len_utf8());
            format!("await 0;{pad}'v'")
        };
        assert!(has_top_level_await(&at(MAX_PARSED_LEN, ' ')));
        assert!(!has_top_level_await(&at(MAX_PARSED_LEN + 1, ' ')));
        let wide = at(MAX_PARSED_LEN - 1, '\u{2003}');
        assert!(wide.len() <= MAX_PARSED_LEN && has_top_level_await(&wide));
        let over = format!("{wide}\u{2003}\u{2003}");
        assert!(over.len() > MAX_PARSED_LEN && !has_top_level_await(&over));
    }

    /// Syntax only an async function body accepts (`return` at the top
    /// level) runs in the async function; syntax neither accepts (a
    /// hashbang after the start, `import.meta`) and an unclosed string,
    /// template, comment or bracket are not parsed: no top-level await is
    /// found and nothing is cut. Errors the parser leaves to semantic
    /// analysis (`super` outside a method, a name declared twice) are the
    /// browser's to report, as for the code as written.
    #[test]
    fn code_the_parser_refuses_is_not_cut() {
        let wrapped = with_top_level_await("await 0; return 5");
        assert_eq!(wrapped, "(async () => { await 0; return 5\n})()");
        for code in [
            "await 0; super.x",
            "await 0; let a = 1; let a = 2; a",
            "await 0; let let = 1",
        ] {
            assert!(has_top_level_await(code), "{code}");
        }
        for code in [
            "await 0;\n#!/bin/x\n1",
            "await 0; import.meta.url",
            "await 0; 'open",
            "await 0; `open ${x}",
            "await 0; /* open",
            "await 0; f(",
            "await 0; var await = 1",
        ] {
            assert!(!has_top_level_await(code), "{code}");
            assert_eq!(
                with_top_level_await(code),
                format!("(async () => {{ {code}\n}})()")
            );
        }
    }

    /// #157: a long line of regular expressions that never close (`(/[`
    /// over and over) and deep brackets are dealt with quickly: over
    /// [`MAX_PARSED_LEN`] they are not parsed; under it, they are parsed
    /// in a thread whose stack fits them, never overflowing it.
    #[test]
    fn unclosed_regexes_and_deep_brackets_are_quick() {
        // Given 120 KB of `(/[`, 100 000 words in 100 000 brackets, and
        // both under the limit
        let regexes = format!("await 0;\n{}", "(/[".repeat(40_000));
        let deep = format!("{}{}", "[".repeat(100_000), "a ".repeat(100_000));
        let short_regexes = format!("await 0;\n{}", "(/[".repeat(21_000));
        let nested = format!("await 0; {}{}", "[".repeat(30_000), "]".repeat(30_000));
        let started = std::time::Instant::now();
        // When they are read
        for code in [&regexes, &deep, &short_regexes] {
            // Then those that do not parse have no top-level await found
            assert!(!has_top_level_await(code));
            assert_eq!(
                with_top_level_await(code),
                format!("(async () => {{ {code}\n}})()")
            );
        }
        // And deep brackets that parse are read whole
        assert!(has_top_level_await(&nested));
        assert!(with_top_level_await(&nested).contains("return ([[["));
        // And it is quick
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// The parser is recursive: code at [`MAX_PARSED_LEN`] made of the
    /// deepest nesting found (brackets, unary operators, arrow functions,
    /// conditionals, templates, calls, statements) is parsed, or refused,
    /// without overflowing the parse thread's stack, which would abort the
    /// server.
    #[test]
    fn the_deepest_code_parsed_does_not_overflow_the_stack() {
        for unit in [
            "[",
            "(",
            "{",
            "!",
            "-",
            "a=>",
            "`${",
            "x?y:",
            "new ",
            "await ",
            "if(1)",
            "x[",
            "function(){",
            "a.b(",
            "class{[",
            "async()=>",
            "({a:",
            "f(...",
            "{a:[",
            "try{",
        ] {
            // Given the unit repeated up to the limit
            let fill = (MAX_PARSED_LEN - 9) / unit.len();
            let code = format!("await 0;{}", unit.repeat(fill));
            assert!(code.len() <= MAX_PARSED_LEN);
            // When it is read, Then the server is still there
            let form = async_form(&code);
            assert!(!form.awaits, "{unit}");
        }
        // And a destructuring pattern, the deepest per byte in a release build
        let pattern = format!("let {}", "[".repeat(MAX_PARSED_LEN - 4));
        assert!(!has_top_level_await(&pattern));
    }

    /// Code longer than [`MAX_PARSED_LEN`] is not parsed: no top-level
    /// await is found, and it is wrapped without returning a value; code
    /// that leaves the function it is parsed in is not one body.
    #[test]
    fn code_too_long_or_leaving_its_function_is_not_parsed() {
        let long = format!("await 0;{}'v'", " ".repeat(MAX_PARSED_LEN));
        assert!(!has_top_level_await(&long));
        assert_eq!(
            with_top_level_await(&long),
            format!("(async () => {{ {long}\n}})()")
        );
        let under = format!("await 0;{}'v'", " ".repeat(MAX_PARSED_LEN - 11));
        assert!(with_top_level_await(&under).ends_with("return ('v'\n); })()"));
        for code in ["await 0 } function z() { 'v'", "} await 0; {"] {
            assert!(!has_top_level_await(code), "{code}");
            assert_eq!(
                with_top_level_await(code),
                format!("(async () => {{ {code}\n}})()")
            );
        }
    }

    /// `for await` and `await using` need an async function; a class's
    /// computed key runs where the class is, its methods do not; a lone
    /// string, a labelled value and a value after empty statements are
    /// returned.
    #[test]
    fn other_awaits_and_last_values_are_found() {
        for code in [
            "for await (const x of xs) {}",
            "class A { [await k]() {} }",
            "{ await using r = f() }",
        ] {
            assert!(has_top_level_await(code), "{code}");
        }
        assert!(!has_top_level_await(
            "class A { async m() { await x } static s = 1 }"
        ));
        for (code, last) in [
            ("'v'", "'v'"),
            ("await 0; l: 'v'", "'v'"),
            ("await 0; 'v';;; // e", "'v'"),
            ("'use strict'; await 0; 'v'", "'v'"),
        ] {
            let wrapped = with_top_level_await(code);
            assert!(
                wrapped.ends_with(&format!("return ({last}\n); }})()")),
                "{wrapped}"
            );
        }
    }

    /// #142: comments after the last `;` are not the last statement: the
    /// expression before them is returned.
    #[test]
    fn trailing_comments_are_left_out() {
        for code in [
            "const r = await Promise.resolve(6161); r; // done",
            "const r = await Promise.resolve(6262);\nr;\n// done",
            "const r = await Promise.resolve(6363); r; /* end */",
            "const r = await Promise.resolve(6464); r /* a */ ; ; // b\n/* c */",
        ] {
            // When the code is wrapped
            let wrapped = with_top_level_await(code);
            // Then `r` is returned
            assert!(wrapped.ends_with("return (r\n); })()"), "{wrapped}");
        }
        // And a code of comments only runs as is
        assert_eq!(
            with_top_level_await("// only"),
            "(async () => { // only\n})()"
        );
    }

    /// #135: `await` used outside every function is found, however it is
    /// written (`await (x)` parses as a call to a function named await, so
    /// the browser's error does not say "await is only valid"); one inside
    /// a function, a string, a comment, a property or an object key is not,
    /// so an expression defining helpers for later calls runs unwrapped.
    #[test]
    fn top_level_await_is_detected() {
        for code in [
            "await (await Promise.resolve('a'))",
            "(await Promise.resolve('abc')).length",
            "await (1+2)",
            "const r = await p; r",
            "x = 1;\nawait (y)",
            "if (ok) { await f() }",
            "for await (const x of xs) {}",
            "[await a, await b]",
            "ok ? await a : b",
            "const f = async () => 1; await f()",
            "const f = x => x\nawait f(1)",
            "f(a => a, await b)",
        ] {
            assert!(has_top_level_await(code), "{code}");
        }
        for code in [
            "document.title",
            "(async () => { await f() })()",
            "async function load(u) { return (await fetch(u)).json() }",
            "const get = async u => await fetch(u);\nget",
            "[1].map(async x => await g(x))",
            "class A { async m() { await this.x } }",
            "({ async m() { await x } })",
            "'await (1)'; `await ${x}`",
            "// await (1)\n/* await (2) */ 3",
            "o.await(1); ({await: 1})",
            "awaited(1); x.awaitAll",
            "function f() { var await = 1; return await }",
            "const g = async x =>\n  await x",
        ] {
            assert!(!has_top_level_await(code), "{code}");
        }
    }

    /// #139: an `await` inside a template literal's `${...}` is at the top
    /// level; one in a function inside the substitution, or in the
    /// template's text, is not.
    #[test]
    fn await_in_template_substitutions_is_detected() {
        for code in [
            "`sum=${await (1+2)}`",
            "`len=${(await Promise.resolve('abc')).length}`",
            "`a${1}b${`c${await x}`}d`",
        ] {
            assert!(has_top_level_await(code), "{code}");
        }
        for code in ["`await ${x} await`", "`${(async () => await x)()}`"] {
            assert!(!has_top_level_await(code), "{code}");
        }
        // And the last statement after a template with braces is returned
        let wrapped = with_top_level_await("const t = `{${await x}}`; t");
        assert!(wrapped.ends_with("return (t\n); })()"), "{wrapped}");
    }

    /// #146: a quote inside a regular expression literal does not start a
    /// string, so a later `await` is found; a `/` after an operand is a
    /// division.
    #[test]
    fn regex_literals_are_skipped() {
        for code in [
            r#"const n = 'a"b'.replace(/"/g, '').length; await (n + 4000)"#,
            r#"/'/.test("it's"); await Promise.resolve(4141)"#,
            "x = /[/'](\\/)/g; await y",
            "a = b / 2; c = d / 3; await e",
        ] {
            assert!(has_top_level_await(code), "{code}");
        }
        assert!(!has_top_level_await("/await (1)/.test(s)"));
        let wrapped = with_top_level_await(r"await x; /;/.test(s)");
        assert!(
            wrapped.ends_with("return (/;/.test(s)\n); })()"),
            "{wrapped}"
        );
    }

    /// #140: long runs of line breaks are dealt with quickly: 200 000 of
    /// them are over [`MAX_PARSED_LEN`], not parsed; 10 000 comment lines
    /// are parsed, and the `await` after them found.
    #[test]
    fn long_blank_runs_are_quick() {
        // Given 200 000 line breaks between two operands, and as many
        // comment lines, then fewer
        let blank = format!("1{}+1", "\n".repeat(200_000));
        let comments = format!("1{}\n+await x", "\n// c".repeat(100_000));
        let fewer = format!("1{}\n+await x", "\n// c".repeat(10_000));
        let started = std::time::Instant::now();
        // When they are read
        assert!(!has_top_level_await(&blank));
        assert!(!has_top_level_await(&comments));
        assert!(has_top_level_await(&fewer));
        with_top_level_await(&blank);
        with_top_level_await(&fewer);
        // Then it is quick
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// #221: a position the browser gives in the wrapped code (0-based,
    /// columns in UTF-16 units) is read in the agent's code: past the
    /// wrapper's opening on the first line and its `return (` on the last
    /// expression's line; one at `return` (where both browsers put an
    /// error of the returned expression) is the expression's start; one in
    /// the wrapper is none.
    #[test]
    fn positions_in_the_wrapped_code_are_read_in_the_code() {
        // Given code wrapped as an async function, and positions the
        // browser gave in the wrapped code
        let cases = [
            // Then the first line loses the wrapper's opening
            ("await 0;\n  nope_c;\n1", (1, 2), Some((1, 2))),
            ("const v = await nope; v", (0, 31), Some((0, 16))),
            // And the returned expression loses `return (`, measured at
            // `return` or inside the expression
            ("await 0; nope_a", (0, 24), Some((0, 9))),
            ("await 0; nope_a", (0, 32), Some((0, 9))),
            ("await 0; libThrow()", (0, 32), Some((0, 9))),
            ("const v = await 1;\nnope_b + v", (1, 0), Some((1, 0))),
            ("const v = await 1;\nnope_b + v", (1, 17), Some((1, 9))),
            ("  await 0; nope_d", (0, 26), Some((0, 11))),
            // And columns count UTF-16 units, lines every line terminator
            ("await 0; 'ñ😀'; nope_u;\n1", (0, 31), Some((0, 16))),
            ("await 0;\r\n'é'\u{2028}nope", (2, 8), Some((2, 0))),
            // And a position in the wrapper is none: its opening, past the
            // code kept (its closing), and code without a last value too
            ("await 0; nope_a", (0, 3), None),
            ("await 0; nope_a", (1, 5), None),
            ("await 0; nope_a // c", (0, 40), None),
            ("await 0; let x = nope;", (0, 32), Some((0, 17))),
            ("await 0; let x = nope;", (1, 2), None),
        ];
        for (code, (line, column), expected) in cases {
            let found = async_form(code).position_in_code(line, column);
            assert_eq!(found, expected, "{code:?} at {line}:{column}");
        }
    }
}
