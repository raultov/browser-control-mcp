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
//! the call (#140, #157).

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
    /// Whether it is a word after `.` (or `?.`): a property's name, never
    /// a keyword (`o.do`, `o.new`, `o.in`; #169).
    property: bool,
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

    /// Whether this token is one of the [`CONTINUING`] keywords, which
    /// never start a statement: the `catch`, `finally` or `else` of a
    /// `try` or `if` whose block closed before it, on its line or the one
    /// before (#151).
    fn is_continuing(&self, code: &str) -> bool {
        CONTINUING.contains(&self.text(code))
    }

    /// Whether this token, last on its line, leaves the statement waiting
    /// for an operand on the next line: an operator (`+`, `=`, `?`, `:`,
    /// `=>`, not `++`, `--` or `;`) or a keyword taking an operand
    /// (`typeof`, `in`, `await`), not `return`, `throw` or `yield`, which
    /// a line break ends (#155), nor `of`, more often a variable's name
    /// than the end of a line in a `for (... of` head, which is inside
    /// brackets anyway (#162). A property named like one (`o.in`) is an
    /// operand (#169).
    fn awaits_operand(&self, code: &str) -> bool {
        let text = self.text(code);
        match self.kind {
            Kind::Punct => !matches!(text, "++" | "--" | ";"),
            Kind::Word => {
                self.is_keyword(code, BEFORE_OPERAND)
                    && !matches!(text, "return" | "throw" | "yield" | "of")
            }
            _ => false,
        }
    }

    /// Whether this token is one of `keywords`, not a property named like
    /// it.
    fn is_keyword(&self, code: &str, keywords: &[&str]) -> bool {
        !self.property && keywords.contains(&self.text(code))
    }
}

/// Whether a line break before `token` (after `previous`) ends the
/// statement: the next line does not go on with it (`.then(...)`, an
/// operator) and the line before does not wait for an operand (#155).
fn line_ends_statement(code: &str, previous: Option<&Token>, token: &Token) -> bool {
    token.line_before && !token.continues(code) && !previous.is_some_and(|p| p.awaits_operand(code))
}

/// Keywords that go on with the statement before them.
const CONTINUING: &[&str] = &["catch", "finally", "else"];

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
        unclosed: 0,
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
    /// Where the scan of the last regular expression that did not close
    /// stopped (see [`Self::regex`]).
    unclosed: usize,
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
            && let Some(end) = self.regex(at)
        {
            return self.push(Kind::Literal, at, end);
        }
        self.other(byte, at)
    }

    /// Where the regular expression opening at `at` ends ([`regex_end`]).
    /// One that does not close on its line is invalid code, or a division
    /// the lexer took for one: every `/` before where its scan stopped is
    /// then a division, not scanned again, so a line of `(/[` is read once
    /// rather than once per `/` (#157).
    fn regex(&mut self, at: usize) -> Option<usize> {
        if at < self.unclosed {
            return None;
        }
        match regex_end(self.bytes, at) {
            Ok(end) => Some(end),
            Err(stopped) => {
                self.unclosed = stopped;
                None
            }
        }
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
            Kind::Word => last.is_keyword(self.code, BEFORE_OPERAND),
            Kind::Literal | Kind::Close => false,
        }
    }

    /// Records the token `start..end` (capped at the code's end); answers
    /// its end.
    fn push(&mut self, kind: Kind, start: usize, end: usize) -> usize {
        let end = end.min(self.bytes.len());
        let after_dot = self.tokens.last().is_some_and(|t| t.is(self.code, "."));
        self.tokens.push(Token {
            kind,
            start,
            end,
            line_before: std::mem::take(&mut self.line_before),
            property: kind == Kind::Word && after_dot,
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
/// flags; `Err` with where the scan stopped when no unescaped `/` outside
/// a character class closes it on its line (then the `/` is a division).
fn regex_end(bytes: &[u8], open: usize) -> Result<usize, usize> {
    let (mut at, mut class) = (open + 1, false);
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 1,
            b'\n' | b'\r' => return Err(at),
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => return Ok(run_end(bytes, at + 1, is_word_byte)),
            _ => {}
        }
        at += 1;
    }
    Err(bytes.len())
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
    let index = last_statement(expression, tokens);
    let first = tokens[index];
    let declares = match first.kind {
        Kind::Open(b'{') => true,
        Kind::Word => {
            DECLARING.contains(&first.text(expression))
                || is_async_function(expression, tokens, index)
        }
        _ => false,
    };
    if declares {
        return format!("(async () => {{ {code}\n}})()");
    }
    let (head, last) = code.split_at(first.start);
    // The line break ends a comment inside the last line.
    format!("(async () => {{ {head}return ({last}\n); }})()")
}

/// Whether the tokens at `i` are `async function`, on one line (a line
/// break after `async` makes it a variable's name).
fn is_async_function(code: &str, tokens: &[Token], i: usize) -> bool {
    let next = tokens.get(i + 1);
    tokens[i].is(code, "async") && next.is_some_and(|n| n.is(code, "function") && !n.line_before)
}

/// Whether the statement starting at the token `i` declares a function
/// or a class, whose body's `}` ends it (#162).
fn starts_declaration(code: &str, tokens: &[Token], i: usize) -> bool {
    matches!(tokens[i].text(code), "function" | "class") || is_async_function(code, tokens, i)
}

/// The index of the token starting the last statement of `tokens`: after
/// their last `;` outside brackets, after the `}` of a block statement
/// (#156) or of a function or class declaration (#162), or after a line
/// break neither line goes on over (#155).
fn last_statement(code: &str, tokens: &[Token]) -> usize {
    let mut scan = StatementScan::default();
    for i in 0..tokens.len() {
        scan.read(code, tokens, i);
    }
    scan.start
}

/// What the token before closed at depth 0, as [`StatementScan`] needs it.
#[derive(Clone, Copy, Default, PartialEq)]
enum Closed {
    #[default]
    Other,
    /// The `)` of a control statement's head (`if (...)`).
    Head,
    /// The `}` of a block statement or of a declaration's body.
    Block,
}

/// The state of [`last_statement`]'s scan.
#[derive(Default)]
struct StatementScan {
    /// How many brackets are open.
    depth: i32,
    /// Where the current statement starts.
    start: usize,
    /// Whether the `(` open at depth 0 holds a control statement's head.
    head: bool,
    /// Whether the `{` open at depth 0 opens a block statement or the body
    /// of a function or class declaration, not an object or a function
    /// expression's body.
    block: bool,
    /// Whether the current statement declares a function or a class whose
    /// body has not opened yet.
    declaring: bool,
    /// How many `do` statements at depth 0 wait for their `while`.
    pending_do: usize,
    do_while: DoWhile,
    closed: Closed,
}

/// Where the scan is in a do-while at depth 0, whose condition's `)` ends
/// the statement (#169).
#[derive(Clone, Copy, Default, PartialEq)]
enum DoWhile {
    #[default]
    Outside,
    /// The `while` of a `do` was just read: its `(` holds the condition.
    While,
    /// The `(` open at depth 0 holds the condition.
    Condition,
}

/// Keywords after which a `{` opens a block statement.
const BEFORE_BLOCK: &[&str] = &["else", "try", "catch", "finally", "do"];

impl StatementScan {
    /// Reads the token at `i` of `tokens`.
    fn read(&mut self, code: &str, tokens: &[Token], i: usize) {
        let token = &tokens[i];
        let previous = i.checked_sub(1).and_then(|p| tokens.get(p));
        let closed = std::mem::take(&mut self.closed);
        if self.depth == 0
            && !self.read_do(code, token)
            && let Some(previous) = previous
            && starts_statement(code, (previous, token), closed)
        {
            self.start = i;
        }
        if i == self.start {
            self.declaring = starts_declaration(code, tokens, i);
        }
        match token.kind {
            Kind::Open(byte) => {
                if self.depth == 0 {
                    self.head = byte == b'(' && opens_head(code, tokens, i);
                    let do_while = std::mem::take(&mut self.do_while);
                    if byte == b'(' && do_while == DoWhile::While {
                        self.do_while = DoWhile::Condition;
                    }
                    let after = previous.is_some_and(|p| BEFORE_BLOCK.contains(&p.text(code)));
                    let body = byte == b'{' && std::mem::take(&mut self.declaring);
                    self.block = byte == b'{'
                        && (i == self.start || closed == Closed::Head || after || body);
                }
                self.depth += 1;
            }
            Kind::Close => {
                self.depth -= 1;
                if self.depth == 0 {
                    self.closed = match token.text(code) {
                        ")" if std::mem::take(&mut self.do_while) == DoWhile::Condition => {
                            Closed::Block
                        }
                        ")" if self.head => Closed::Head,
                        "}" if self.block => Closed::Block,
                        _ => Closed::Other,
                    };
                }
            }
            _ => {}
        }
    }

    /// Counts a `do` at depth 0; answers whether `token` is the `while` of
    /// one, which goes on with the `do` statement instead of starting one
    /// (`do {...} while (c)`; #169).
    fn read_do(&mut self, code: &str, token: &Token) -> bool {
        if token.is_keyword(code, &["do"]) {
            self.pending_do += 1;
        }
        let ends_do = self.pending_do > 0 && token.is_keyword(code, &["while"]);
        if ends_do {
            self.pending_do -= 1;
            self.do_while = DoWhile::While;
        }
        ends_do
    }
}

/// Whether `token`, after `previous` outside brackets, starts a statement:
/// after a `;`, after the `}` of a block statement (`if (a) {...} 'x'`,
/// #156), or after a line break neither line goes on over. `catch`,
/// `finally` and `else` never start one.
fn starts_statement(code: &str, (previous, token): (&Token, &Token), closed: Closed) -> bool {
    if token.is_continuing(code) {
        return false;
    }
    previous.is(code, ";")
        || closed == Closed::Block
        || line_ends_statement(code, Some(previous), token)
}

/// Whether the `(` at `i` opens a control statement's head: `if (`,
/// `while (`, `for await (`...
fn opens_head(code: &str, tokens: &[Token], i: usize) -> bool {
    let word = |back: usize| {
        i.checked_sub(back)
            .and_then(|j| tokens.get(j))
            .map(|t| t.text(code))
    };
    match word(1) {
        Some("await") => word(2) == Some("for"),
        Some(word) => CONTROL.contains(&word),
        None => false,
    }
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
        functions: 0,
        last: Last::Other,
    };
    (0..tokens.len()).any(|i| scan.is_top_level_await(code, &tokens, i))
}

/// The state of [`has_top_level_await`]'s scan.
struct AwaitScan {
    /// The brackets open at this point, innermost last.
    scopes: Vec<Scope>,
    /// How many of [`Self::scopes`] are function bodies: counted, so a
    /// word deep in brackets does not look through all of them (#157).
    functions: usize,
    last: Last,
}

impl AwaitScan {
    /// Reads the token at `i` of `tokens`: whether it is an `await`
    /// outside every function.
    fn is_top_level_await(&mut self, code: &str, tokens: &[Token], i: usize) -> bool {
        let token = &tokens[i];
        let previous = i.checked_sub(1).and_then(|p| tokens.get(p));
        if line_ends_statement(code, previous, token) {
            self.end_arrow_bodies();
        }
        if self.last == Last::Arrow && token.kind != Kind::Open(b'{') {
            self.push(Scope::ArrowBody);
            self.last = Last::Other;
        }
        match token.kind {
            Kind::Word => return self.word(code, token, tokens.get(i + 1)),
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
        self.push(scope);
        self.last = Last::Other;
    }

    fn close(&mut self) {
        self.end_arrow_bodies();
        self.last = match self.pop() {
            Some(Scope::Paren { control }) => Last::Closed { control },
            _ => Last::Other,
        };
    }

    /// Opens `scope`, counting function bodies.
    fn push(&mut self, scope: Scope) {
        self.functions += usize::from(scope.is_function());
        self.scopes.push(scope);
    }

    /// Closes the innermost scope, counting function bodies.
    fn pop(&mut self) -> Option<Scope> {
        let scope = self.scopes.pop()?;
        self.functions = self
            .functions
            .saturating_sub(usize::from(scope.is_function()));
        Some(scope)
    }

    /// The word `token`, as [`Self::is_top_level_await`] reads it.
    fn word(&mut self, code: &str, token: &Token, next: Option<&Token>) -> bool {
        let word = token.text(code);
        let key = next.is_some_and(|n| n.text(code).starts_with(':'));
        let in_function = self.functions > 0;
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
            self.pop();
        }
    }
}

impl Scope {
    /// Whether this scope is a function's body.
    fn is_function(self) -> bool {
        matches!(self, Self::Body | Self::ArrowBody)
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

    /// #157: a long line of regular expressions that never close (`(/[`
    /// over and over) and deep brackets are scanned in linear time.
    #[test]
    fn unclosed_regexes_and_deep_brackets_are_scanned_in_linear_time() {
        // Given 120 KB of `(/[` and 100 000 words inside 100 000 brackets
        let regexes = format!("await 0;\n{}", "(/[".repeat(40_000));
        let deep = format!("{}{}", "[".repeat(100_000), "a ".repeat(100_000));
        let started = std::time::Instant::now();
        // When both are scanned and wrapped
        assert!(has_top_level_await(&regexes));
        assert!(!has_top_level_await(&deep));
        with_top_level_await(&regexes);
        with_top_level_await(&deep);
        // Then it is fast
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
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
