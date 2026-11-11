//! Swift parser plugin, full-parse mode.
//!
//! This compact parser avoids a host tree-sitter dependency. It lexes the whole
//! source into tokens (dropping whitespace and `//` / nested `/* */` comments,
//! keeping string literals and extended `#/.../#` regex literals intact) and groups the tokens into declarations and
//! statements by braces, semicolons and Swift's newline statement boundaries.
//! Every non-trivia token of the source ends up in exactly one node label, so a
//! changed token is never silently dropped and comments/layout never surface.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

struct SwiftParser;

/// Maximum nesting of braced blocks (and string interpolations) projected as
/// nested nodes. Deeper content is still kept, flattened into one label, so the
/// recursion stays bounded inside the Wasm sandbox and the emitted JSON stays
/// well under serde_json's 128-level parse limit (each block level adds a
/// statement and a block node, i.e. four JSON levels).
const MAX_DEPTH: usize = 20;

#[derive(Debug, Clone)]
struct Token {
    text: String,
    start: usize,
    end: usize,
    /// Whitespace or a comment separates this token from the previous one.
    gap_before: bool,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_ident_continue(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

fn is_operator_char(b: u8) -> bool {
    matches!(
        b,
        b'/' | b'='
            | b'-'
            | b'+'
            | b'!'
            | b'*'
            | b'%'
            | b'<'
            | b'>'
            | b'&'
            | b'|'
            | b'^'
            | b'~'
            | b'?'
            | b'.'
    )
}

/// `=` and compound assignments (`+=`, `<<=`, ...), not comparisons.
fn is_assignment_operator(text: &str) -> bool {
    is_operator(text)
        && text.ends_with('=')
        && !matches!(text, "==" | "!=" | "<=" | ">=" | "===" | "!==")
}

fn is_operator(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(is_operator_char)
}

/// Scan a string literal starting at `p` (the opening quote) with `hashes`
/// raw-string delimiters already consumed before it. Returns the end offset.
fn scan_string(bytes: &[u8], p: usize, hashes: usize, depth: usize) -> usize {
    let n = bytes.len();
    let multiline = bytes[p..].starts_with(b"\"\"\"");
    let mut q = p + if multiline { 3 } else { 1 };
    let closes_at = |q: usize| -> bool {
        let quote_len = if multiline { 3 } else { 1 };
        bytes[q..].starts_with(if multiline { b"\"\"\"" } else { b"\"" })
            && bytes.len() >= q + quote_len + hashes
            && bytes[q + quote_len..q + quote_len + hashes]
                .iter()
                .all(|&b| b == b'#')
    };
    while q < n {
        let c = bytes[q];
        if c == b'\\' {
            let r = q + 1 + hashes;
            if r <= n && bytes[q + 1..r].iter().all(|&b| b == b'#') {
                if r < n && bytes[r] == b'(' && depth < MAX_DEPTH {
                    q = scan_interpolation(bytes, r + 1, depth + 1);
                } else {
                    q = (r + 1).min(n);
                }
                continue;
            }
            q += 1;
            continue;
        }
        if c == b'\n' && !multiline {
            return q;
        }
        if c == b'"' && closes_at(q) {
            return q + if multiline { 3 } else { 1 } + hashes;
        }
        q += 1;
    }
    n
}

/// Scan an extended regex literal `#/.../#` (any number of `#`) whose opening
/// `/` is at `p`, with `hashes` delimiters already consumed before it. Returns
/// the end offset. Swift only allows `/` straight after `#` to open a regex
/// literal, so this is unambiguous. When only whitespace follows the opening
/// delimiter on its row, the literal is multi-line and may span rows; otherwise
/// a newline ends an unterminated literal. A backslash escapes the next byte.
/// The whole literal is one token, so `//` and `/* */` inside it are payload.
fn scan_regex(bytes: &[u8], p: usize, hashes: usize) -> usize {
    let n = bytes.len();
    let mut q = p + 1;
    let rest_blank = bytes[q..]
        .iter()
        .take_while(|&&b| b != b'\n')
        .all(|&b| b == b' ' || b == b'\t' || b == b'\r');
    let multiline = q < n && rest_blank && bytes[q..].contains(&b'\n');
    while q < n {
        match bytes[q] {
            b'\\' if multiline || bytes.get(q + 1) != Some(&b'\n') => q += 2,
            b'\n' if !multiline => return q,
            b'/' if bytes.len() >= q + 1 + hashes
                && bytes[q + 1..q + 1 + hashes].iter().all(|&b| b == b'#') =>
            {
                return q + 1 + hashes;
            }
            _ => q += 1,
        }
    }
    n
}

fn scan_interpolation(bytes: &[u8], mut q: usize, depth: usize) -> usize {
    let n = bytes.len();
    let mut parens = 1usize;
    while q < n {
        match bytes[q] {
            b'(' => parens += 1,
            b')' => {
                parens -= 1;
                if parens == 0 {
                    return q + 1;
                }
            }
            b'"' => {
                q = scan_string(bytes, q, 0, depth);
                continue;
            }
            b'#' => {
                let h = bytes[q..].iter().take_while(|&&b| b == b'#').count();
                if q + h < n && bytes[q + h] == b'"' {
                    q = scan_string(bytes, q + h, h, depth);
                    continue;
                }
                if q + h < n && bytes[q + h] == b'/' {
                    q = scan_regex(bytes, q + h, h);
                    continue;
                }
            }
            b'\n' => return q,
            _ => {}
        }
        q += 1;
    }
    n
}

fn lex(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let n = bytes.len();
    let mut tokens = Vec::new();
    let mut p = 0usize;
    let mut last_end = 0usize;
    while p < n {
        let b = bytes[p];
        if b.is_ascii_whitespace() {
            p += 1;
            continue;
        }
        if b == b'/' && p + 1 < n && bytes[p + 1] == b'/' {
            while p < n && bytes[p] != b'\n' {
                p += 1;
            }
            continue;
        }
        if b == b'/' && p + 1 < n && bytes[p + 1] == b'*' {
            let mut depth = 1usize;
            p += 2;
            while p < n && depth > 0 {
                if bytes[p..].starts_with(b"/*") {
                    depth += 1;
                    p += 2;
                } else if bytes[p..].starts_with(b"*/") {
                    depth -= 1;
                    p += 2;
                } else {
                    p += 1;
                }
            }
            continue;
        }
        let start = p;
        let end = if b == b'"' {
            scan_string(bytes, p, 0, 0)
        } else if b == b'#' {
            let h = bytes[p..].iter().take_while(|&&c| c == b'#').count();
            if p + h < n && bytes[p + h] == b'"' {
                scan_string(bytes, p + h, h, 0)
            } else if p + h < n && bytes[p + h] == b'/' {
                scan_regex(bytes, p + h, h)
            } else if h == 1 && p + 1 < n && is_ident_start(bytes[p + 1]) {
                let mut q = p + 1;
                while q < n && is_ident_continue(bytes[q]) {
                    q += 1;
                }
                q
            } else {
                p + 1
            }
        } else if b == b'@' && p + 1 < n && is_ident_start(bytes[p + 1]) {
            let mut q = p + 1;
            while q < n && is_ident_continue(bytes[q]) {
                q += 1;
            }
            q
        } else if b == b'`' {
            let mut q = p + 1;
            while q < n && bytes[q] != b'`' && bytes[q] != b'\n' {
                q += 1;
            }
            if q < n && bytes[q] == b'`' {
                q + 1
            } else {
                q
            }
        } else if is_ident_start(b) {
            let mut q = p;
            while q < n && is_ident_continue(bytes[q]) {
                q += 1;
            }
            q
        } else if b.is_ascii_digit() {
            let hex = bytes[p..].starts_with(b"0x") || bytes[p..].starts_with(b"0X");
            let mut q = p;
            while q < n {
                let c = bytes[q];
                let word = c.is_ascii_alphanumeric() || c == b'_';
                let fraction = c == b'.' && q + 1 < n && bytes[q + 1].is_ascii_digit();
                let exponent_sign = (c == b'+' || c == b'-')
                    && q > p
                    && if hex {
                        matches!(bytes[q - 1], b'p' | b'P')
                    } else {
                        matches!(bytes[q - 1], b'e' | b'E')
                    };
                if !(word || fraction || exponent_sign) {
                    break;
                }
                q += 1;
            }
            q
        } else if is_operator_char(b) {
            let dotted = b == b'.';
            let mut q = p;
            while q < n && is_operator_char(bytes[q]) && (dotted || bytes[q] != b'.' || q == p) {
                if q > p && bytes[q] == b'/' && q + 1 < n && matches!(bytes[q + 1], b'/' | b'*') {
                    break;
                }
                q += 1;
            }
            q
        } else {
            // Single punctuation character; keep multi-byte characters whole.
            let len = source[p..].chars().next().map(char::len_utf8).unwrap_or(1);
            p + len
        };
        let end = end.max(start + 1).min(n);
        tokens.push(Token {
            text: source.get(start..end).unwrap_or("").to_string(),
            start,
            end,
            gap_before: tokens.is_empty() || start > last_end,
        });
        last_end = end;
        p = end;
    }
    tokens
}

/// Byte offset -> (0-based row, UTF-8 byte column).
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        LineIndex { starts }
    }

    fn position(&self, offset: usize) -> (u32, u32) {
        let row = match self.starts.binary_search(&offset) {
            Ok(r) => r,
            Err(r) => r - 1,
        };
        (row as u32, (offset - self.starts[row]) as u32)
    }
}

const MODIFIERS: &[&str] = &[
    "public",
    "private",
    "fileprivate",
    "internal",
    "open",
    "package",
    "static",
    "final",
    "override",
    "mutating",
    "nonmutating",
    "lazy",
    "weak",
    "unowned",
    "convenience",
    "required",
    "dynamic",
    "indirect",
    "prefix",
    "postfix",
    "infix",
    "nonisolated",
    "isolated",
    "distributed",
    "optional",
];

const DECL_KEYWORDS: &[&str] = &[
    "func",
    "init",
    "deinit",
    "subscript",
    "var",
    "let",
    "class",
    "struct",
    "enum",
    "protocol",
    "extension",
    "actor",
    "typealias",
    "case",
    "import",
    "associatedtype",
    "operator",
];

struct Parser<'a> {
    toks: &'a [Token],
    /// For each `{` token, the index of its matching `}` (or `toks.len()`).
    matches: Vec<usize>,
    index: LineIndex,
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Token], source: &str) -> Self {
        let mut matches = vec![usize::MAX; toks.len()];
        let mut stack = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            match t.text.as_str() {
                "{" => stack.push(i),
                "}" => {
                    if let Some(open) = stack.pop() {
                        matches[open] = i;
                    }
                }
                _ => {}
            }
        }
        for open in stack {
            matches[open] = toks.len();
        }
        Parser {
            toks,
            matches,
            index: LineIndex::new(source),
        }
    }

    fn text(&self, i: usize) -> &str {
        self.toks.get(i).map(|t| t.text.as_str()).unwrap_or("")
    }

    /// Skip attributes (`@x`, `@x(...)`) and declaration modifiers
    /// (`public`, `private(set)`, `class func`, ...) starting at `k`.
    fn prefix_end(&self, mut k: usize, end: usize) -> usize {
        while k < end {
            let t = self.text(k);
            let is_attr = t.len() > 1 && t.starts_with('@');
            let is_class_modifier =
                t == "class" && DECL_KEYWORDS.contains(&self.text(k + 1)) && k + 1 < end;
            if !(is_attr || MODIFIERS.contains(&t) || is_class_modifier) {
                break;
            }
            k += 1;
            if k < end && self.text(k) == "(" && !self.toks[k].gap_before {
                let mut depth = 0usize;
                while k < end {
                    match self.text(k) {
                        "(" => depth += 1,
                        ")" => {
                            depth -= 1;
                            if depth == 0 {
                                k += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                }
            }
        }
        k
    }

    fn gap_after(&self, i: usize) -> bool {
        self.toks.get(i + 1).map(|t| t.gap_before).unwrap_or(true)
    }

    fn line_of(&self, offset: usize) -> u32 {
        self.index.position(offset).0
    }

    /// Whether token `j` continues the statement that started at `s`
    /// although it begins a new source line.
    fn continues(&self, s: usize, j: usize, has_block: bool) -> bool {
        let t = self.text(j);
        let prev = self.text(j - 1);
        if matches!(t, "else" | "catch" | "where") {
            return true;
        }
        if t == "{" && !has_block {
            return true;
        }
        // A leading binary or member operator (`.map`, `+ b`, `-> Int`).
        if is_operator(t) && (t.starts_with('.') || self.gap_after(j)) {
            return true;
        }
        // A trailing binary operator or separator (`a +`, `x =`, `a,`).
        if prev == "," || (is_operator(prev) && (self.toks[j - 1].gap_before || prev == ".")) {
            return true;
        }
        // Only attributes/modifiers so far (`@MainActor` on its own line).
        self.prefix_end(s, j) == j
    }

    /// Parse the items in token range `[start, end)`.
    fn items(
        &self,
        start: usize,
        end: usize,
        depth: usize,
        parent: Option<&str>,
        id: &str,
        first_index: usize,
    ) -> Vec<SemanticNode> {
        let mut out = Vec::new();
        let mut i = start;
        while i < end {
            if self.text(i) == ";" {
                i += 1;
                continue;
            }
            let child_id = format!("{}.{}", id, first_index + out.len());
            if self.text(i) == "}" {
                // Unbalanced closing brace: keep it rather than drop it.
                out.push(self.leaf(&child_id, "expression_statement", "}", i, i + 1));
                i += 1;
                continue;
            }
            let s = i;
            let mut parens = 0usize;
            let mut blocks: Vec<(usize, usize)> = Vec::new();
            let mut j = i;
            while j < end {
                let t = self.text(j);
                if j > s {
                    if (t == ";" && parens == 0) || t == "}" {
                        break;
                    }
                    if parens == 0
                        && self.line_of(self.toks[j].start) > self.line_of(self.toks[j - 1].end)
                        && !self.continues(s, j, !blocks.is_empty())
                    {
                        break;
                    }
                }
                match t {
                    "(" | "[" => parens += 1,
                    ")" | "]" => parens = parens.saturating_sub(1),
                    "{" => {
                        let close = self.matches[j].min(end);
                        blocks.push((j, close));
                        j = if close >= end { end } else { close + 1 };
                        continue;
                    }
                    _ => {}
                }
                j += 1;
            }
            out.push(self.statement(&child_id, s, j, &blocks, depth, parent));
            i = j;
        }
        out
    }

    /// Render tokens `[s, e)` with canonical spacing; braced blocks become `{}`.
    fn render(&self, s: usize, e: usize, blocks: &[(usize, usize)]) -> String {
        let mut pieces: Vec<(&str, bool)> = Vec::new();
        let mut j = s;
        while j < e {
            if let Some(&(_, close)) = blocks.iter().find(|(open, _)| *open == j) {
                pieces.push(("{}", true));
                j = close + 1;
                continue;
            }
            pieces.push((self.text(j), self.toks[j].gap_before));
            j += 1;
        }
        let mut out = String::new();
        for (idx, (text, gap)) in pieces.iter().enumerate() {
            if idx > 0 {
                let prev = pieces[idx - 1].0;
                let space = if matches!(prev, "(" | "[") || matches!(*text, ")" | "]" | "," | ";") {
                    false
                } else if matches!(*text, "{" | "}" | "{}") || matches!(prev, "{" | "}" | "{}") {
                    true
                } else {
                    *gap
                };
                if space {
                    out.push(' ');
                }
            }
            out.push_str(text);
        }
        out
    }

    fn span(&self, s: usize, e: usize) -> (u32, u32, u32, u32) {
        let last = e.max(s + 1).min(self.toks.len()) - 1;
        let (sl, sc) = self.index.position(self.toks[s].start);
        let (el, ec) = self.index.position(self.toks[last].end);
        (sl, sc, el, ec)
    }

    fn leaf(&self, id: &str, node_type: &str, label: &str, s: usize, e: usize) -> SemanticNode {
        let (sl, sc, el, ec) = self.span(s, e);
        SemanticNodeBuilder::new(id, node_type, label, sl, sc, el, ec, "").build()
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        &self,
        id: &str,
        node_type: &str,
        label: &str,
        s: usize,
        e: usize,
        children: Vec<SemanticNode>,
        parent: Option<&str>,
    ) -> SemanticNode {
        let (sl, sc, el, ec) = self.span(s, e);
        let mut b =
            SemanticNodeBuilder::new(id, node_type, label, sl, sc, el, ec, "").children(children);
        if let Some(p) = parent {
            b = b.parent_type(p);
        }
        b.build()
    }

    fn block_children(
        &self,
        id: &str,
        blocks: &[(usize, usize)],
        depth: usize,
        parent: Option<&str>,
        first_index: usize,
    ) -> Vec<SemanticNode> {
        blocks
            .iter()
            .enumerate()
            .map(|(k, &(open, close))| {
                let block_id = format!("{}.{}", id, first_index + k);
                let close_tok = (close + 1).min(self.toks.len());
                let items = self.block_items(open, close, depth, parent, &block_id, 0);
                self.node(&block_id, "block", "block", open, close_tok, items, None)
            })
            .collect()
    }

    /// Items inside the block whose `{` is at `open` and `}` at `close`.
    fn block_items(
        &self,
        open: usize,
        close: usize,
        depth: usize,
        parent: Option<&str>,
        id: &str,
        first_index: usize,
    ) -> Vec<SemanticNode> {
        let inner_end = close.min(self.toks.len());
        if open + 1 >= inner_end {
            return Vec::new();
        }
        if depth + 1 >= MAX_DEPTH {
            let label = self.render(open + 1, inner_end, &[]);
            return vec![self.leaf(
                &format!("{}.{}", id, first_index),
                "expression_statement",
                &label,
                open + 1,
                inner_end,
            )];
        }
        self.items(open + 1, inner_end, depth + 1, parent, id, first_index)
    }

    fn statement(
        &self,
        id: &str,
        s: usize,
        e: usize,
        blocks: &[(usize, usize)],
        depth: usize,
        parent: Option<&str>,
    ) -> SemanticNode {
        let k = self.prefix_end(s, e);
        let kw = if k < e { self.text(k) } else { "" };
        let close_end = blocks
            .last()
            .map(|&(_, c)| (c + 1).min(self.toks.len()))
            .unwrap_or(e)
            .max(e);
        let header = self.render(s, e, blocks);
        let decl_type = match kw {
            "class" => Some("class_declaration"),
            "struct" => Some("struct_declaration"),
            "enum" => Some("enum_declaration"),
            "protocol" => Some("protocol_declaration"),
            "extension" => Some("extension_declaration"),
            "actor" => Some("actor_declaration"),
            _ => None,
        };
        if kw == "import" {
            let mut label = self.render(k + 1, e, blocks);
            if k > s {
                label = format!("{} {}", self.render(s, k, &[]), label);
            }
            return self.leaf(id, "import_declaration", &label, s, close_end);
        }
        if let Some(node_type) = decl_type {
            let name = if kw == "extension" {
                let mut stop = k + 1;
                while stop < e && !matches!(self.text(stop), ":" | "where" | "{") {
                    stop += 1;
                }
                self.render(k + 1, stop, &[])
            } else {
                self.text(k + 1).to_string()
            };
            return self.declaration(id, node_type, &name, s, e, blocks, depth, None, Some(&name));
        }
        if matches!(kw, "func" | "init" | "deinit" | "subscript") {
            let name = match kw {
                "func" => self.text(k + 1).to_string(),
                "init" if matches!(self.text(k + 1), "?" | "!") && !self.toks[k + 1].gap_before => {
                    format!("init{}", self.text(k + 1))
                }
                _ => kw.to_string(),
            };
            return self.declaration(
                id,
                "function_declaration",
                &name,
                s,
                e,
                blocks,
                depth,
                parent,
                parent,
            );
        }
        let node_type = match kw {
            "let" | "var" => "variable_declaration",
            "typealias" => "typealias_declaration",
            "return" => "return_statement",
            "if" => "if_statement",
            "guard" => "guard_statement",
            "for" => "for_statement",
            "while" | "repeat" => "while_statement",
            "switch" => "switch_statement",
            "throw" => "throw_statement",
            "defer" => "defer_statement",
            "do" => "do_statement",
            _ => {
                let range = s..e;
                if range.clone().any(|j| is_assignment_operator(self.text(j))) {
                    "assignment"
                } else if range.clone().any(|j| self.text(j) == "(") {
                    "function_call_expression"
                } else {
                    "expression_statement"
                }
            }
        };
        if blocks.is_empty() {
            self.leaf(id, node_type, &header, s, e)
        } else {
            let children = self.block_children(id, blocks, depth, parent, 0);
            self.node(id, node_type, &header, s, close_end, children, None)
        }
    }

    /// A declaration node: label is the name, child 0 is the signature (all
    /// header tokens), followed by the body items of its first block.
    #[allow(clippy::too_many_arguments)]
    fn declaration(
        &self,
        id: &str,
        node_type: &str,
        name: &str,
        s: usize,
        e: usize,
        blocks: &[(usize, usize)],
        depth: usize,
        parent: Option<&str>,
        body_parent: Option<&str>,
    ) -> SemanticNode {
        let close_end = blocks
            .last()
            .map(|&(_, c)| (c + 1).min(self.toks.len()))
            .unwrap_or(e)
            .max(e);
        let sig_end = blocks.first().map(|&(open, _)| open).unwrap_or(e);
        let mut signature = self.render(s, sig_end, &[]);
        let mut children = Vec::new();
        if let Some(&(open, close)) = blocks.first() {
            let after = (close + 1).min(e);
            if after < e {
                // Tokens after the body (e.g. a trailing `where`) stay visible.
                signature = format!("{} {{}} {}", signature, self.render(after, e, &blocks[1..]));
            }
            children.push(self.leaf(&format!("{}.0", id), "signature", &signature, s, sig_end));
            children.extend(self.block_items(open, close, depth, body_parent, id, 1));
            if blocks.len() > 1 {
                let first = children.len();
                children.extend(self.block_children(id, &blocks[1..], depth, body_parent, first));
            }
        } else {
            children.push(self.leaf(&format!("{}.0", id), "signature", &signature, s, e));
        }
        self.node(id, node_type, name, s, close_end, children, parent)
    }
}

fn parse_source(source: &str) -> SemanticNode {
    let toks = lex(source);
    let parser = Parser::new(&toks, source);
    let children = parser.items(0, toks.len(), 0, None, "0", 0);
    let (end_line, end_col) = parser.index.position(source.len());
    SemanticNodeBuilder::new(
        "0",
        "source_file",
        "source_file",
        0,
        0,
        end_line,
        end_col,
        "",
    )
    .children(children)
    .build()
}

fn process_impl(source: &str) -> String {
    match serde_json::to_string(&parse_source(source)) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for SwiftParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "swift".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".swift") {
            "swift".to_string()
        } else {
            String::new()
        }
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "import Foundation\n\nfunc greet(name: String) {\n    print(\"Hello, \" + name)\n}\n\nfunc add(a: Int, b: Int) -> Int {\n    return a + b\n}\n".to_string(),
            new: "import Foundation\n\nfunc greet(name: String) {\n    print(\"Hello, \\(name)!\")\n}\n\nfunc add(_ a: Int, _ b: Int) -> Int {\n    return a + b\n}\n\nfunc multiply(_ a: Int, _ b: Int) -> Int {\n    return a * b\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        Vec::new()
    }
    fn language_ids() -> Vec<String> {
        vec!["swift".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(SwiftParser);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!SwiftParser::grammar_id().is_empty());
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            SwiftParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        assert!(SwiftParser::language_ids().contains(&SwiftParser::grammar_id()));
    }

    #[test]
    fn detect_language_known_ext() {
        assert_eq!(
            SwiftParser::detect_language("test.swift".to_string(), "".to_string()),
            "swift"
        );
    }

    #[test]
    fn detect_language_unknown_ext() {
        assert_eq!(
            SwiftParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string()),
            ""
        );
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        t::assert_valid_json(&process_impl(""), "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        t::assert_valid_json(&process_impl("   \n  "), "process(whitespace)");
    }

    #[test]
    fn playground_example_produces_functions() {
        let example = <SwiftParser as Guest>::example("swift".to_string());
        let out = process_impl(&example.new);
        t::assert_valid_json(&out, "swift example");
        t::assert_no_error(&out, "swift example");
        assert!(out.contains("function_declaration"));
        assert!(out.contains("multiply"));
    }

    fn tree(source: &str) -> serde_json::Value {
        let out = process_impl(source);
        t::assert_valid_json(&out, "swift tree");
        t::assert_no_error(&out, "swift tree");
        serde_json::from_str(&out).unwrap()
    }

    fn root_hash(source: &str) -> String {
        tree(source)["structural_hash"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Preorder (node_type, label, [start_line, start_col, end_line, end_col]).
    fn nodes(source: &str) -> Vec<(String, String, [u64; 4])> {
        fn walk(n: &serde_json::Value, out: &mut Vec<(String, String, [u64; 4])>) {
            let p = &n["position"];
            out.push((
                n["node_type"].as_str().unwrap().to_string(),
                n["label"].as_str().unwrap().to_string(),
                [
                    p["start_line"].as_u64().unwrap(),
                    p["start_col"].as_u64().unwrap(),
                    p["end_line"].as_u64().unwrap(),
                    p["end_col"].as_u64().unwrap(),
                ],
            ));
            for c in n["children"].as_array().unwrap() {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        walk(&tree(source), &mut out);
        out
    }

    fn payload(source: &str) -> Vec<(String, String)> {
        nodes(source).into_iter().map(|(t, l, _)| (t, l)).collect()
    }

    fn find(source: &str, node_type: &str, label: &str) -> [u64; 4] {
        nodes(source)
            .into_iter()
            .find(|(t, l, _)| t == node_type && l == label)
            .unwrap_or_else(|| panic!("no {node_type} {label:?} in {:?}", payload(source)))
            .2
    }

    // Issue #4: statements sharing the closing-brace row were dropped.
    #[test]
    fn issue4_closing_line_body_statement_is_retained() {
        let old = "func f() -> Int {\n  return 1 }\n";
        let new = "func f() -> Int {\n  return 2 }\n";
        assert_eq!(find(old, "return_statement", "return 1"), [1, 2, 1, 10]);
        assert_eq!(find(new, "return_statement", "return 2"), [1, 2, 1, 10]);
        assert_eq!(find(old, "function_declaration", "f"), [0, 0, 1, 12]);
        assert_ne!(root_hash(old), root_hash(new));
    }

    #[test]
    fn closing_line_content_kept_on_one_and_many_lines() {
        let pairs = [
            (
                "func f() -> Int { return 1 }\n",
                "func f() -> Int { return 2 }\n",
            ),
            (
                "func f() -> Int {\n  return 1\n}\n",
                "func f() -> Int {\n  return 2\n}\n",
            ),
            ("struct S {\n  var x = 1 }\n", "struct S {\n  var x = 2 }\n"),
            (
                "class C {\n  func g() {\n    run(1) } }\n",
                "class C {\n  func g() {\n    run(2) } }\n",
            ),
            (
                "func f() -> Int {\n  let a = 1; return a }\n",
                "func f() -> Int {\n  let a = 1; return -a }\n",
            ),
        ];
        for (old, new) in pairs {
            assert_ne!(root_hash(old), root_hash(new), "{old:?} -> {new:?}");
        }
        assert_eq!(
            payload("struct S {\n  var x = 1 }\n")[3],
            ("variable_declaration".into(), "var x = 1".into())
        );
    }

    #[test]
    fn quoted_braces_and_comment_markers_are_payload() {
        let pairs = [
            (
                "func f() -> String {\n  return \"}\" }\n",
                "func f() -> String {\n  return \"{\" }\n",
            ),
            (
                "func f() -> String { return \"// a\" }\n",
                "func f() -> String { return \"// b\" }\n",
            ),
            (
                "func f() -> String { return \"/* a */\" }\n",
                "func f() -> String { return \"/* b */\" }\n",
            ),
            (
                "func f() -> String { return \"\\(x + 1) }\" }\n",
                "func f() -> String { return \"\\(x - 1) }\" }\n",
            ),
            ("let s = #\"a\"}\"#\n", "let s = #\"b\"}\"#\n"),
        ];
        for (old, new) in pairs {
            assert_ne!(root_hash(old), root_hash(new), "{old:?} -> {new:?}");
        }
        find(
            "func f() -> String {\n  return \"}\" }\n",
            "return_statement",
            "return \"}\"",
        );
    }

    // Issue #5: an ordinary `//` comment became an expression_statement.
    #[test]
    fn issue5_ordinary_comment_is_trivia() {
        let old = "public func answer() -> Int { return 1 }\n";
        let new = "// Ordinary comment\npublic func answer() -> Int { return 1 }\n";
        assert_eq!(payload(old), payload(new));
        assert_eq!(root_hash(old), root_hash(new));
        assert!(!process_impl(new).contains("Ordinary"));
        assert_eq!(find(new, "function_declaration", "answer"), [1, 0, 1, 40]);
        assert_eq!(
            find(new, "signature", "public func answer() -> Int"),
            [1, 0, 1, 27]
        );
    }

    #[test]
    fn comment_and_layout_controls_are_style_only() {
        let base = "public func answer() -> Int { return 1 }\n";
        let variants = [
            "\npublic func answer() -> Int { return 1 }\n\n",
            "public func answer() -> Int { return 1 } // trailing\n",
            "/* block\n comment */ public func answer() -> Int { return 1 }\n",
            "public func answer() -> Int {\n    // inside\n    return 1\n}\n",
            "/// Doc comment\npublic func answer() -> Int { /* x /* y */ */ return 1 }\n",
        ];
        for v in variants {
            assert_eq!(payload(base), payload(v), "{v:?}");
            assert_eq!(root_hash(base), root_hash(v), "{v:?}");
        }
    }

    #[test]
    fn assessment_meaningful_edits_change_payload() {
        let pairs = [
            (
                "public func answer() -> Int { return 1 }\n",
                "public func answer() -> Int { return 2 }\n",
            ),
            (
                "public func answer() -> Int { return 1 }\n",
                "public func result() -> Int { return 1 }\n",
            ),
            (
                "public func combine(_ a: Int, _ b: Int) -> Int { return a + b }\n",
                "public func combine(_ a: Int, _ b: Int) -> Int { return a - b }\n",
            ),
            (
                "public func message() -> String { return \"caf\u{e9}\" }\n",
                "public func message() -> String { return \"cafe\u{96ea}\" }\n",
            ),
        ];
        for (old, new) in pairs {
            assert_ne!(root_hash(old), root_hash(new), "{old:?} -> {new:?}");
        }
        // UTF-8 byte columns: `return "cafe雪"` is 14 chars but 16 bytes.
        let src = "public func message() -> String { return \"cafe\u{96ea}\" }\n";
        assert_eq!(
            find(src, "return_statement", "return \"cafe\u{96ea}\""),
            [0, 34, 0, 50]
        );
    }

    // PR #6 review: `//` inside an extended regex literal was lexed as a line
    // comment, truncating `#/https://one.example/#` to `# / https :`.
    #[test]
    fn extended_regex_literal_is_not_a_comment() {
        let old = "let regex = #/https://one.example/#\n";
        let new = "let regex = #/https://two.example/#\n";
        assert_ne!(root_hash(old), root_hash(new));
        assert_eq!(
            find(
                old,
                "variable_declaration",
                "let regex = #/https://one.example/#"
            ),
            [0, 0, 0, 35]
        );
        let pairs = [
            ("let r = #/https?://x/#\n", "let r = #/https?://y/#\n"),
            ("let r = #/a/*b*/c/#\n", "let r = #/a/*d*/c/#\n"),
            ("let r = ##/a/#//b/##\n", "let r = ##/a/#//c/##\n"),
            ("let r = #/a\\/#//b/#\n", "let r = #/a\\/#//c/#\n"),
            ("let r = #/\"//a/#\n", "let r = #/\"//b/#\n"),
            (
                "let r = #/\n  https:// one # comment\n/#\n",
                "let r = #/\n  https:// two # comment\n/#\n",
            ),
            ("func f() { m(#/}//a/#) }\n", "func f() { m(#/}//b/#) }\n"),
            (
                "let s = \"\\(x.matches(#/)//a/#))\"\n",
                "let s = \"\\(x.matches(#/)//b/#))\"\n",
            ),
        ];
        for (old, new) in pairs {
            assert_ne!(root_hash(old), root_hash(new), "{old:?} -> {new:?}");
        }
        // A `}` inside the regex is payload, not a block close.
        assert_eq!(
            payload("func f() { m(#/}//a/#) }\n")[3],
            ("function_call_expression".into(), "m(#/}//a/#)".into())
        );
    }

    #[test]
    fn comments_around_regex_literals_are_still_trivia() {
        let base = "let r = #/https?://x/#\nlet y = 1\n";
        let variants = [
            "let r = #/https?://x/# // trailing\nlet y = 1\n",
            "// lead\nlet r = #/https?://x/#\n/* block */ let y = 1\n",
            "let r = #/https?://x/#\n\n   let y = 1 // https://z/#\n",
        ];
        for v in variants {
            assert_eq!(payload(base), payload(v), "{v:?}");
            assert_eq!(root_hash(base), root_hash(v), "{v:?}");
        }
        // Comment markers inside multi-line and raw strings stay payload.
        assert_ne!(
            root_hash("let s = \"\"\"\n// a\n\"\"\"\n"),
            root_hash("let s = \"\"\"\n// b\n\"\"\"\n")
        );
        assert_ne!(
            root_hash("let s = #\"// a\"#\n"),
            root_hash("let s = #\"// b\"#\n")
        );
    }

    #[test]
    fn identity_and_determinism() {
        let example = <SwiftParser as Guest>::example("swift".to_string());
        assert_eq!(process_impl(&example.new), process_impl(&example.new));
        assert_eq!(root_hash(&example.old), root_hash(&example.old));
        assert_ne!(root_hash(&example.old), root_hash(&example.new));
    }

    #[test]
    fn malformed_input_never_panics() {
        let deep = "{".repeat(5000);
        let inputs = [
            "public func answer( -> Int {\n",
            "}}}\n",
            "let s = \"unterminated\n",
            "let s = \"\"\"\nopen",
            "/* open",
            "let s = \"\\(",
            "let r = #/open",
            "let r = ##/a/#\n",
            "let r = #/\nopen",
            "let r = #/a\\",
            "#",
            "#/",
            deep.as_str(),
        ];
        for src in inputs {
            tree(src);
        }
        // An unterminated single-line regex stops at the newline.
        find(
            "let r = #/open\nlet y = 1\n",
            "variable_declaration",
            "let y = 1",
        );
        // Unbalanced input keeps its tokens; deep nesting is flattened, not lost.
        find("}}}\n", "expression_statement", "}");
        let deep_changed = format!("{}x", "{".repeat(40));
        assert_ne!(
            root_hash(&format!("{}y", "{".repeat(40))),
            root_hash(&deep_changed)
        );
    }
}
