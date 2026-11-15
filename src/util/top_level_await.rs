//! Top-level `await` in the code `evaluate_js` runs: found by a lexical
//! scan of the code, and run as an async function returning the last
//! expression statement's value, as a console does (Firefox's
//! `script.evaluate` has no top-level await, nor has Chrome's
//! `Runtime.evaluate`).
//!
//! The scan reads the code once into tokens: whitespace and
//! comments are dropped (a token records whether a line break preceded it),
//! strings, template literals (with their `${...}` substitutions read as
//! code) and regular expression literals are recognised. Every step is
//! linear in the code's length, so a large pasted expression cannot stall
//! the call (#140).

/// What a [`Token`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// An identifier, a keyword or a number.
    Word,
    /// A string, a regular expression, a template literal without
    /// substitutions, or a template's text after its last substitution.
    Literal,
    /// `(`, `[`, `{`, or a template's text up to a `${` (`$`).
    Open(u8),
    /// `)`, `]`, `}`, or the `}` ending a template substitution.
    Close,
    /// Any other punctuation: one byte, or `=>`, `++`, `--`.
    Punct,
}

/// A token of the code: its bytes `start..end`.
#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
    /// Whether a line break (in whitespace or a block comment) precedes it.
    line_before: bool,
}

impl Token {
    fn text<'a>(&self, code: &'a str) -> &'a str {
        code.get(self.start..self.end).unwrap_or_default()
    }

    fn is(&self, code: &str, text: &str) -> bool {
        self.text(code) == text
    }

    /// Whether this token, first on its line, continues the statement of
    /// the line before (`.then(...)`, an operator, a closing bracket).
    fn continues(&self, code: &str) -> bool {
        self.text(code).starts_with([
            '.', '?', ':', '+', '-', '*', '/', '%', '&', '|', '^', ',', '=', '<', '>', ')', ']',
            '}',
        ])
    }
}

/// Keywords after which a `/` starts a regular expression, not a division.
const BEFORE_OPERAND: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

/// The tokens of `code`, whitespace and comments left out.
fn lex(code: &str) -> Vec<Token> {
    let mut lexer = Lexer {
        code,
        bytes: code.as_bytes(),
        braces: Vec::new(),
        tokens: Vec::new(),
        line_before: false,
    };
    let mut at = 0;
    while at < code.len() {
        at = lexer.step(at);
    }
    lexer.tokens
}

/// The state of [`lex`].
struct Lexer<'a> {
    code: &'a str,
    bytes: &'a [u8],
    /// The `{` open at this point, innermost last: `true` for a template's
    /// `${`, whose `}` resumes the template.
    braces: Vec<bool>,
    tokens: Vec<Token>,
    /// Whether a line break was skipped since the last token.
    line_before: bool,
}

impl Lexer<'_> {
    /// Reads what starts at `at`; answers where the next thing starts.
    fn step(&mut self, at: usize) -> usize {
        let byte = self.bytes[at];
        if byte.is_ascii_whitespace() {
            return self.skip(at, |b| b.is_ascii_whitespace());
        }
        match (byte, self.bytes.get(at + 1)) {
            (b'/', Some(b'/')) => return self.skip(at, |b| b != b'\n' && b != b'\r'),
            (b'/', Some(b'*')) => return self.block_comment(at),
            (b'\'' | b'"', _) => {
                let end = string_end(self.bytes, at, byte);
                return self.push(Kind::Literal, at, end);
            }
            (b'`', _) => return self.template(at, at + 1),
            _ => {}
        }
        if byte == b'/'
            && self.operand_expected()
            && let Some(end) = regex_end(self.bytes, at)
        {
            return self.push(Kind::Literal, at, end);
        }
        self.other(byte, at)
    }

    /// Words, brackets and punctuation.
    fn other(&mut self, byte: u8, at: usize) -> usize {
        match byte {
            _ if is_word_byte(byte) => {
                let end = run_end(self.bytes, at, is_word_byte);
                self.push(Kind::Word, at, end)
            }
            b'{' => {
                self.braces.push(false);
                self.push(Kind::Open(byte), at, at + 1)
            }
            b'}' => {
                let end = self.push(Kind::Close, at, at + 1);
                match self.braces.pop() {
                    Some(true) => self.template(end, end),
                    _ => end,
                }
            }
            b'(' | b'[' => self.push(Kind::Open(byte), at, at + 1),
            b')' | b']' => self.push(Kind::Close, at, at + 1),
            _ => {
                let pair = self.bytes.get(at + 1).copied();
                let long = matches!(
                    (byte, pair),
                    (b'=', Some(b'>')) | (b'+', Some(b'+')) | (b'-', Some(b'-'))
                );
                self.push(Kind::Punct, at, at + 1 + usize::from(long))
            }
        }
    }

    /// Skips the run of bytes from `at` that `keep` accepts, noting a line
    /// break in it.
    fn skip(&mut self, at: usize, keep: fn(u8) -> bool) -> usize {
        let end = run_end(self.bytes, at, keep);
        self.line_before |= self.bytes[at..end].contains(&b'\n');
        end
    }

    fn block_comment(&mut self, at: usize) -> usize {
        let end = self.code[at + 2..]
            .find("*/")
            .map_or(self.bytes.len(), |n| at + n + 4);
        self.line_before |= self.bytes[at..end].contains(&b'\n');
        end
    }

    /// A template's text from `text` (just after its backtick or the `}`
    /// of a substitution), the token starting at `start`: up to its closing
    /// backtick, or an open bracket up to its next `${`.
    fn template(&mut self, start: usize, text: usize) -> usize {
        let mut at = text;
        while at < self.bytes.len() {
            match self.bytes[at] {
                b'\\' => at += 2,
                b'`' => return self.push(Kind::Literal, start, at + 1),
                b'$' if self.bytes.get(at + 1) == Some(&b'{') => {
                    self.braces.push(true);
                    return self.push(Kind::Open(b'$'), start, at + 2);
                }
                _ => at += 1,
            }
        }
        self.push(Kind::Literal, start, self.bytes.len())
    }

    /// Whether a `/` here starts an operand (a regular expression), from
    /// the token before it.
    fn operand_expected(&self) -> bool {
        let Some(last) = self.tokens.last() else {
            return true;
        };
        match last.kind {
            Kind::Open(_) => true,
            Kind::Punct => !matches!(last.text(self.code), "++" | "--"),
            Kind::Word => BEFORE_OPERAND.contains(&last.text(self.code)),
            Kind::Literal | Kind::Close => false,
        }
    }

    /// Records the token `start..end` (capped at the code's end); answers
    /// its end.
    fn push(&mut self, kind: Kind, start: usize, end: usize) -> usize {
        let end = end.min(self.bytes.len());
        self.tokens.push(Token {
            kind,
            start,
            end,
            line_before: std::mem::take(&mut self.line_before),
        });
        end
    }
}

/// Where the run of bytes from `at` that `keep` accepts ends.
fn run_end(bytes: &[u8], at: usize, keep: fn(u8) -> bool) -> usize {
    bytes[at..]
        .iter()
        .position(|b| !keep(*b))
        .map_or(bytes.len(), |n| at + n)
}

/// Where the string opening at `open` (with `quote`) ends, past its
/// closing quote.
fn string_end(bytes: &[u8], open: usize, quote: u8) -> usize {
    let mut at = open + 1;
    while at < bytes.len() && bytes[at] != quote {
        at += if bytes[at] == b'\\' { 2 } else { 1 };
    }
    at + 1
}

/// Where the regular expression literal opening at `open` ends, past its
/// flags; `None` when no unescaped `/` outside a character class closes it
/// on its line (then the `/` is a division).
fn regex_end(bytes: &[u8], open: usize) -> Option<usize> {
    let (mut at, mut class) = (open + 1, false);
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 1,
            b'\n' | b'\r' => return None,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => return Some(run_end(bytes, at + 1, is_word_byte)),
            _ => {}
        }
        at += 1;
    }
    None
}

/// Whether `byte` can be part of an identifier (any byte of a character
/// outside ASCII counts).
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || !byte.is_ascii()
}

/// Keywords starting a last statement that has no value to return: a
/// declaration, a control statement or a block.
const DECLARING: &[&str] = &[
    "const", "let", "var", "function", "class", "if", "for", "while", "do", "switch", "try",
    "return", "throw",
];

/// `expression`, whose statements use `await` at their top level, as an
/// async function called at once that returns its last expression
/// statement's value, as a console does. A last statement that is a
/// declaration or a block returns nothing. Trailing `;` and comments are
/// left out (#142). No line is added before the code, so an error's line
/// is the expression's.
pub fn with_top_level_await(expression: &str) -> String {
    let tokens = lex(expression);
    let kept = tokens
        .iter()
        .rposition(|t| !t.is(expression, ";"))
        .map_or(0, |n| n + 1);
    let tokens = &tokens[..kept];
    let Some(end) = tokens.last().map(|t| t.end) else {
        return format!("(async () => {{ {expression}\n}})()");
    };
    let code = &expression[..end];
    let first = tokens[last_statement(expression, tokens)];
    let declares = match first.kind {
        Kind::Open(b'{') => true,
        Kind::Word => DECLARING.contains(&first.text(expression)),
        _ => false,
    };
    if declares {
        return format!("(async () => {{ {code}\n}})()");
    }
    let (head, last) = code.split_at(first.start);
    // The line break ends a comment inside the last line.
    format!("(async () => {{ {head}return ({last}\n); }})()")
}

/// The index of the token starting the last statement of `tokens`: after
/// their last `;` outside brackets, or line break when the next line does
/// not continue the statement.
fn last_statement(code: &str, tokens: &[Token]) -> usize {
    let (mut depth, mut start) = (0_i32, 0);
    for (i, token) in tokens.iter().enumerate() {
        let after_semicolon = i > 0 && tokens[i - 1].is(code, ";");
        let new_line = token.line_before && !token.continues(code);
        if depth == 0 && i > 0 && (after_semicolon || new_line) {
            start = i;
        }
        match token.kind {
            Kind::Open(_) => depth += 1,
            Kind::Close => depth -= 1,
            _ => {}
        }
    }
    start
}

/// What a bracket of the code [`has_top_level_await`] scans opened.
#[derive(Clone, Copy)]
enum Scope {
    /// `(`: after `if`, `for`, `while`, `switch`, `catch` or `with`
    /// (`control`), a `{` after its `)` is a block, else a function body.
    Paren { control: bool },
    /// `[`, a template's `${`, or a `{` that is a block, an object or a
    /// class body.
    Block,
    /// A function's `{ ... }`.
    Body,
    /// An arrow function's body without braces, up to its `,`, `;`, line
    /// end or enclosing bracket.
    ArrowBody,
}

/// The token before the current one, as [`has_top_level_await`] needs it.
#[derive(Clone, Copy, PartialEq)]
enum Last {
    Other,
    Dot,
    Arrow,
    /// A keyword whose `(` holds a condition, not parameters.
    Control,
    /// A `)`; `control` as in [`Scope::Paren`].
    Closed {
        control: bool,
    },
}

/// Keywords followed by a condition in parentheses and a block.
const CONTROL: &[&str] = &["if", "for", "while", "switch", "catch", "with"];

/// Whether `code` uses `await` outside every function, which only runs as
/// an async function: a script refuses it, or reads `await (x)` as a call
/// to a function named await (#135). Strings, comments, regular
/// expressions, properties (`o.await`), object keys (`{await: 1}`) and
/// function bodies (with braces or an arrow's without) are skipped; a
/// template's substitutions are read (#139). A regular expression the
/// lexer takes for a division (after `)`) may still hide one: the fallback
/// on the browser's error stays.
pub fn has_top_level_await(code: &str) -> bool {
    let tokens = lex(code);
    let mut scan = AwaitScan {
        scopes: Vec::new(),
        last: Last::Other,
    };
    tokens
        .iter()
        .enumerate()
        .any(|(i, token)| scan.is_top_level_await(code, token, tokens.get(i + 1)))
}

/// The state of [`has_top_level_await`]'s scan.
struct AwaitScan {
    /// The brackets open at this point, innermost last.
    scopes: Vec<Scope>,
    last: Last,
}

impl AwaitScan {
    /// Reads `token` (`next` follows it): whether it is an `await` outside
    /// every function.
    fn is_top_level_await(&mut self, code: &str, token: &Token, next: Option<&Token>) -> bool {
        if token.line_before && !token.continues(code) {
            self.end_arrow_bodies();
        }
        if self.last == Last::Arrow && token.kind != Kind::Open(b'{') {
            self.scopes.push(Scope::ArrowBody);
            self.last = Last::Other;
        }
        match token.kind {
            Kind::Word => return self.word(code, token, next),
            Kind::Open(byte) => self.open(byte),
            Kind::Close => self.close(),
            Kind::Literal => self.last = Last::Other,
            Kind::Punct => self.punct(token.text(code)),
        }
        false
    }

    fn punct(&mut self, text: &str) {
        self.last = match text {
            "=>" => Last::Arrow,
            "." => Last::Dot,
            ";" | "," => {
                self.end_arrow_bodies();
                Last::Other
            }
            _ => Last::Other,
        };
    }

    fn open(&mut self, byte: u8) {
        let scope = match byte {
            b'(' => Scope::Paren {
                control: self.last == Last::Control,
            },
            b'{' if matches!(self.last, Last::Arrow | Last::Closed { control: false }) => {
                Scope::Body
            }
            _ => Scope::Block,
        };
        self.scopes.push(scope);
        self.last = Last::Other;
    }

    fn close(&mut self) {
        self.end_arrow_bodies();
        self.last = match self.scopes.pop() {
            Some(Scope::Paren { control }) => Last::Closed { control },
            _ => Last::Other,
        };
    }

    /// The word `token`, as [`Self::is_top_level_await`] reads it.
    fn word(&mut self, code: &str, token: &Token, next: Option<&Token>) -> bool {
        let word = token.text(code);
        let key = next.is_some_and(|n| n.text(code).starts_with(':'));
        let in_function = self
            .scopes
            .iter()
            .any(|s| matches!(s, Scope::Body | Scope::ArrowBody));
        if word == "await" && self.last != Last::Dot && !key && !in_function {
            return true;
        }
        self.last = if CONTROL.contains(&word) {
            Last::Control
        } else {
            Last::Other
        };
        false
    }

    /// Ends the arrow function bodies without braces open at this point.
    fn end_arrow_bodies(&mut self) {
        while matches!(self.scopes.last(), Some(Scope::ArrowBody)) {
            self.scopes.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ISSUE-43: an expression using `await` at its top level becomes an
    /// async function returning its last expression statement, as a
    /// console does; strings, brackets, comments and chained lines do not
    /// split statements; a final declaration returns nothing.
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
            wrapped("const r = await f({a: 1, b: [2; 3]})\nr.ok // done; really"),
            "(async () => { const r = await f({a: 1, b: [2; 3]})\nreturn (r.ok\n); })()"
        );
        assert_eq!(
            wrapped("await fetch(x)\n  .then(r => r.status)"),
            "(async () => { return (await fetch(x)\n  .then(r => r.status)\n); })()"
        );
        assert_eq!(
            wrapped("await 1; const z = `a;${1}`;"),
            "(async () => { await 1; const z = `a;${1}`\n})()"
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

    /// #140: long runs of line breaks are scanned in linear time: 200 000
    /// of them take well under a second, not minutes.
    #[test]
    fn long_blank_runs_are_scanned_in_linear_time() {
        // Given 200 000 line breaks between two operands, and as many
        // comment lines
        let blank = format!("1{}+1", "\n".repeat(200_000));
        let comments = format!("1{}\n+await x", "\n// c".repeat(100_000));
        let started = std::time::Instant::now();
        // When both are scanned and wrapped
        assert!(!has_top_level_await(&blank));
        assert!(has_top_level_await(&comments));
        with_top_level_await(&blank);
        with_top_level_await(&comments);
        // Then it is fast
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
