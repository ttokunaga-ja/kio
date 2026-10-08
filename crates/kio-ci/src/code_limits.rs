//! Fixed MVP code-size and OS-separation measurement (docs/09-mvp-scope.md §3).
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

type Result<T> = std::result::Result<T, String>;
const CRATES: [&str; 10] = [
    "kio-core",
    "kio-pipeline",
    "kio-adapter",
    "kio-index",
    "kio-search",
    "kio-app",
    "kio-cli",
    "kio-process",
    "kio-eval",
    "kio-ci",
];
const LIMITS: [(&str, usize); 6] = [
    ("common", 100_000),
    ("windows", 14_000),
    ("unix", 9_000),
    ("linux", 4_000),
    ("macos", 3_000),
    ("tooling", 65_000),
];
const FILE_LIMIT: usize = 3_000;
const OS_IDENTIFIERS: [&str; 6] = [
    "unix",
    "windows",
    "target_os",
    "target_family",
    "target_vendor",
    "target_env",
];
struct Allowance {
    path: &'static str,
    rule_id: &'static str,
    reason: &'static str,
}
// File/rule exceptions only; size limits cannot be excepted.
const ALLOWLIST: &[Allowance] = &[];

pub fn entry(args: Vec<OsString>) -> Result<(Value, i32)> {
    let root = match args.as_slice() {
        [] => std::env::current_dir().map_err(|e| format!("current directory: {e}"))?,
        [flag, root] if flag == "--root" => PathBuf::from(root),
        _ => return Err("usage: kio-code-limits [--root <workspace-root>]".into()),
    };
    let report = measure(&root, ALLOWLIST)?;
    let exit = i32::from(report["has_violations"] == true);
    Ok((report, exit))
}

#[derive(Clone, Debug)]
struct Token {
    start: usize,
    end: usize,
    line: usize,
    end_line: usize,
    identifier: bool,
    close: Option<usize>,
}
impl Token {
    fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }
    fn identifier_text<'a>(&self, source: &'a str) -> &'a str {
        self.text(source)
            .strip_prefix("r#")
            .unwrap_or(self.text(source))
    }
}

// Comments disappear, while literals retain their complete physical-line span.
// This also makes identifier searches insensitive to text inside literals.
fn lex(source: &str) -> Result<Vec<Token>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = usize::from(source.starts_with('\u{feff}')) * '\u{feff}'.len_utf8();
    let mut line = 1;
    if bytes[i..].starts_with(b"#!") && !shebang_attribute(source, i + 2) {
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
    }
    while i < bytes.len() {
        let c = source[i..].chars().next().unwrap();
        if source_whitespace(c) {
            line += usize::from(c == '\n');
            i += c.len_utf8();
            continue;
        }
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            let start_line = line;
            let mut depth = 1;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    line += usize::from(bytes[i] == b'\n');
                    i += 1;
                }
            }
            if depth != 0 {
                return Err(format!("unclosed block comment at line {start_line}"));
            }
            continue;
        }
        let start = i;
        let start_line = line;
        let mut identifier = false;
        if let Some((quote, hashes)) = raw_open(bytes, i) {
            i = quote + 1;
            loop {
                if i >= bytes.len() {
                    return Err(format!("unclosed raw string at line {start_line}"));
                }
                if bytes[i] == b'"'
                    && bytes
                        .get(i + 1..i + 1 + hashes)
                        .is_some_and(|s| s.iter().all(|b| *b == b'#'))
                {
                    i += 1 + hashes;
                    break;
                }
                line += usize::from(bytes[i] == b'\n');
                i += 1;
            }
        } else if bytes[i] == b'"'
            || ((bytes[i] == b'b' || bytes[i] == b'c') && bytes.get(i + 1) == Some(&b'"'))
        {
            if bytes[i] != b'"' {
                i += 1;
            }
            i += 1;
            loop {
                if i >= bytes.len() {
                    return Err(format!("unclosed string at line {start_line}"));
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                if bytes[i] == b'\\' {
                    i += 1;
                    if i >= bytes.len() {
                        return Err(format!("unclosed string at line {start_line}"));
                    }
                }
                line += usize::from(bytes[i] == b'\n');
                i += 1;
            }
        } else if let Some(end) = char_end(source, i) {
            line += bytes[i..end].iter().filter(|b| **b == b'\n').count();
            i = end;
        } else if ident_start(source[i..].chars().next().unwrap())
            || (bytes[i..].starts_with(b"r#")
                && source
                    .get(i + 2..)
                    .and_then(|s| s.chars().next())
                    .is_some_and(ident_start))
        {
            identifier = true;
            if bytes[i..].starts_with(b"r#") {
                i += 2;
            }
            i += source[i..].chars().next().unwrap().len_utf8();
            while i < bytes.len() {
                let c = source[i..].chars().next().unwrap();
                if !ident_continue(c) {
                    break;
                }
                i += c.len_utf8();
            }
        } else {
            i += source[i..].chars().next().unwrap().len_utf8();
        }
        tokens.push(Token {
            start,
            end: i,
            line: start_line,
            end_line: line,
            identifier,
            close: None,
        });
    }
    let mut stack = Vec::new();
    for i in 0..tokens.len() {
        match tokens[i].text(source) {
            "(" | "[" | "{" => stack.push(i),
            ")" | "]" | "}" => {
                let open = stack
                    .pop()
                    .ok_or_else(|| format!("unmatched delimiter at line {}", tokens[i].line))?;
                let expected = match tokens[open].text(source) {
                    "(" => ")",
                    "[" => "]",
                    _ => "}",
                };
                if tokens[i].text(source) != expected {
                    return Err(format!("unmatched delimiter at line {}", tokens[i].line));
                }
                tokens[open].close = Some(i);
            }
            _ => {}
        }
    }
    if let Some(open) = stack.pop() {
        return Err(format!("unclosed delimiter at line {}", tokens[open].line));
    }
    Ok(tokens)
}
// Rust probes the first non-comment token after #! before stripping a shebang.
fn shebang_attribute(source: &str, mut i: usize) -> bool {
    let bytes = source.as_bytes();
    while i < bytes.len() {
        let c = source[i..].chars().next().unwrap();
        if source_whitespace(c) {
            i += c.len_utf8();
        } else if bytes[i..].starts_with(b"//") {
            if bytes.get(i + 2) == Some(&b'!')
                || (bytes.get(i + 2) == Some(&b'/') && bytes.get(i + 3) != Some(&b'/'))
            {
                return false;
            }
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i..].starts_with(b"/*") {
            if bytes.get(i + 2) == Some(&b'!')
                || (bytes.get(i + 2) == Some(&b'*')
                    && !matches!(bytes.get(i + 3), Some(b'*' | b'/')))
            {
                return false;
            }
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            return bytes[i] == b'[';
        }
    }
    false
}
// Rust punctuation is ASCII; retain complete non-ASCII identifier sequences,
// including combining marks, rather than accidentally splitting OS-like names.
fn source_whitespace(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}')
}
fn ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && !source_whitespace(c))
}
fn ident_continue(c: char) -> bool {
    ident_start(c) || c.is_ascii_digit()
}
fn raw_open(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut i = start;
    if matches!(bytes.get(i), Some(b'b' | b'c')) {
        i += 1;
    }
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    i += 1;
    let hash_start = i;
    while bytes.get(i) == Some(&b'#') {
        i += 1;
    }
    (bytes.get(i) == Some(&b'"')).then_some((i, i - hash_start))
}
// A quote followed by an identifier without a closing quote is a lifetime.
fn char_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut i = start;
    if bytes.get(i) == Some(&b'b') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'\'') {
        return None;
    }
    i += 1;
    if bytes.get(i) == Some(&b'\\') {
        i += 1;
        match bytes.get(i)? {
            b'x' => {
                i += 3;
            }
            b'u' if bytes.get(i + 1) == Some(&b'{') => {
                i += 2;
                while bytes.get(i).is_some_and(|b| *b != b'}') {
                    i += 1;
                }
                i += 1;
            }
            _ => {
                i += source.get(i..)?.chars().next()?.len_utf8();
            }
        }
    } else {
        i += source.get(i..)?.chars().next()?.len_utf8();
    }
    (bytes.get(i) == Some(&b'\'')).then_some(i + 1)
}

fn is(tokens: &[Token], source: &str, i: usize, text: &str) -> bool {
    tokens.get(i).is_some_and(|t| t.text(source) == text)
}
fn identifier_is(tokens: &[Token], source: &str, i: usize, text: &str) -> bool {
    tokens
        .get(i)
        .is_some_and(|t| t.identifier && t.identifier_text(source) == text)
}
fn group_end(tokens: &[Token], _source: &str, start: usize) -> Result<usize> {
    tokens[start]
        .close
        .ok_or_else(|| format!("expected delimiter at line {}", tokens[start].line))
}
#[derive(Debug)]
struct Attribute {
    start: usize,
    end: usize,
    inner: bool,
    name: String,
    condition: Option<(usize, usize)>,
}
fn attribute(tokens: &[Token], source: &str, start: usize) -> Result<Option<Attribute>> {
    if !is(tokens, source, start, "#") {
        return Ok(None);
    }
    let inner = is(tokens, source, start + 1, "!");
    let bracket = start + 1 + usize::from(inner);
    if !is(tokens, source, bracket, "[") {
        return Ok(None);
    }
    let end = group_end(tokens, source, bracket)?;
    let name = tokens
        .get(bracket + 1)
        .map_or("", |t| t.identifier_text(source))
        .to_owned();
    let mut condition = None;
    if matches!(name.as_str(), "cfg" | "cfg_attr") && is(tokens, source, bracket + 2, "(") {
        let paren = bracket + 2;
        let close = group_end(tokens, source, paren)?;
        let mut last = close;
        if name == "cfg_attr" {
            let mut i = paren + 1;
            while i < close {
                if is(tokens, source, i, ",") {
                    last = i;
                    break;
                }
                if matches!(tokens[i].text(source), "(" | "[" | "{") {
                    i = group_end(tokens, source, i)?;
                }
                i += 1;
            }
        }
        condition = Some((paren + 1, last));
    }
    Ok(Some(Attribute {
        start,
        end,
        inner,
        name,
        condition,
    }))
}
fn test_required(tokens: &[Token], source: &str, start: usize, end: usize) -> bool {
    if start + 1 == end && identifier_is(tokens, source, start, "test") {
        return true;
    }
    if !identifier_is(tokens, source, start, "all")
        || !is(tokens, source, start + 1, "(")
        || !is(tokens, source, end.saturating_sub(1), ")")
    {
        return false;
    }
    let mut i = start + 2;
    let mut element = i;
    let mut depth = 0;
    while i < end - 1 {
        match tokens[i].text(source) {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth -= 1,
            "," if depth == 0 => {
                if element + 1 == i && identifier_is(tokens, source, element, "test") {
                    return true;
                }
                element = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    element + 1 == i && identifier_is(tokens, source, element, "test")
}
fn item_head(tokens: &[Token], source: &str, mut i: usize) -> Result<usize> {
    if is(tokens, source, i, "pub") {
        i += 1;
        if is(tokens, source, i, "(") {
            i = group_end(tokens, source, i)? + 1;
        }
    }
    Ok(i)
}
// Qualified type paths in expressions can contain generic commas too. Their
// closing angle is followed by `::`, unlike comparisons or shift operators.
fn qualified_path_angle(tokens: &[Token], source: &str, start: usize) -> Result<bool> {
    let mut depth = 1usize;
    let mut i = start + 1;
    while i < tokens.len() {
        match tokens[i].text(source) {
            "(" | "[" | "{" => i = group_end(tokens, source, i)?,
            "<" => depth += 1,
            ">" if !is(tokens, source, i.saturating_sub(1), "-") => {
                depth -= 1;
                if depth == 0 {
                    return Ok(is(tokens, source, i + 1, ":") && is(tokens, source, i + 2, ":"));
                }
            }
            ";" | "}" | "]" | ")" => return Ok(false),
            _ => {}
        }
        i += 1;
    }
    Ok(false)
}
// Declaration generic lists give attributed parameters a hard parent boundary.
// Nested grouped const expressions and function arrows cannot close that list.
fn generic_boundary(tokens: &[Token], source: &str, start: usize) -> Result<usize> {
    let mut boundary = tokens.len();
    for open in 0..start {
        if !is(tokens, source, open, "<") {
            continue;
        }
        let direct_impl = open > 0 && is(tokens, source, open - 1, "impl");
        let named = open >= 2
            && tokens[open - 1].identifier
            && matches!(
                tokens[open - 2].text(source),
                "fn" | "struct" | "enum" | "trait" | "type" | "union"
            );
        if !direct_impl && !named {
            continue;
        }
        let mut depth = 1usize;
        let mut i = open + 1;
        while i < tokens.len() {
            match tokens[i].text(source) {
                "(" | "[" | "{" => i = group_end(tokens, source, i)?,
                "<" => depth += 1,
                ">" if !is(tokens, source, i.saturating_sub(1), "-") => {
                    depth -= 1;
                    if depth == 0 {
                        if start < i {
                            boundary = boundary.min(i);
                        }
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    Ok(boundary)
}
// Named declarations/functions do not end at signature/where-clause commas.
// Fields and variants do; expression operators after `=` are not type generics.
fn item_end(tokens: &[Token], source: &str, start: usize) -> Result<usize> {
    let boundary = generic_boundary(tokens, source, start)?;
    let head = item_head(tokens, source, start)?;
    let mut keyword = head;
    while tokens
        .get(keyword)
        .is_some_and(|t| matches!(t.text(source), "const" | "async" | "unsafe" | "extern"))
    {
        keyword += 1;
        if tokens.get(keyword).is_some_and(|t| {
            t.text(source).starts_with('"') || raw_open(t.text(source).as_bytes(), 0).is_some()
        }) {
            keyword += 1;
        }
    }
    let function = is(tokens, source, keyword, "fn");
    let declaration = tokens
        .get(head)
        .is_some_and(|t| matches!(t.text(source), "use" | "static" | "type"))
        || (is(tokens, source, head, "const") && !function);
    let named_item = function
        || declaration
        || tokens.get(keyword).is_some_and(|t| {
            matches!(
                t.text(source),
                "mod" | "struct" | "enum" | "impl" | "trait" | "union"
            )
        })
        || is(tokens, source, head, "extern");
    let mut expression = false;
    let mut angles = 0usize;
    let mut i = start;
    while i < boundary {
        match tokens[i].text(source) {
            "(" | "[" => i = group_end(tokens, source, i)?,
            "{" => {
                i = group_end(tokens, source, i)?;
                if !declaration && !expression && angles == 0 {
                    let terminator = is(tokens, source, i + 1, ";")
                        || (!named_item && is(tokens, source, i + 1, ","));
                    return Ok(i + 1 + usize::from(terminator));
                }
            }
            "=" if angles == 0 => expression = true,
            "<" if !expression
                || angles > 0
                || (is(tokens, source, i.saturating_sub(1), ":")
                    && is(tokens, source, i.saturating_sub(2), ":"))
                || qualified_path_angle(tokens, source, i)? =>
            {
                angles += 1
            }
            ">" if angles > 0 && !is(tokens, source, i.saturating_sub(1), "-") => angles -= 1,
            ";" => return Ok(i + 1),
            "," if angles == 0 && !named_item => return Ok(i + 1),
            ")" | "]" | "}" => return Ok(i),
            _ => {}
        }
        i += 1;
    }
    if boundary < tokens.len() {
        return Ok(boundary);
    }
    Err(format!(
        "attributed item has no terminator at line {}",
        tokens[start].line
    ))
}

#[derive(Debug)]
struct Condition {
    start: usize,
    line: usize,
    kind: String,
    text: String,
    dispatch: bool,
}
struct Analysis {
    lines: usize,
    conditions: Vec<Condition>,
    file_test: bool,
}
fn test_module_name(name: &str) -> bool {
    name == "tests" || name.ends_with("_tests") || name == "test_support"
}
fn test_file_name(name: &str) -> bool {
    name.strip_suffix(".rs").is_some_and(test_module_name)
}
fn excluded(start: usize, ranges: &[(usize, usize)]) -> bool {
    ranges
        .iter()
        .any(|&(first, last)| first <= start && start < last)
}
// Macro token trees are data, not source module declarations. Keep them visible
// to the independent OS-condition scan.
fn macro_ranges(tokens: &[Token], source: &str) -> Result<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].identifier && is(tokens, source, i + 1, "!") {
            let mut open = i + 2;
            if identifier_is(tokens, source, i, "macro_rules") {
                open += 1;
            }
            if tokens
                .get(open)
                .is_some_and(|t| matches!(t.text(source), "(" | "[" | "{"))
            {
                let end = group_end(tokens, source, open)?;
                // Only `mod NAME {` or `mod $NAME {` keeps its contents in place;
                // any other module form names a file the macro decides.
                for j in open + 1..end {
                    let name = j + 1 + usize::from(is(tokens, source, j + 1, "$"));
                    if is(tokens, source, j, "mod")
                        && !(tokens.get(name).is_some_and(|t| t.identifier)
                            && is(tokens, source, name + 1, "{"))
                    {
                        return Err(format!(
                            "out-of-line module in macro token tree at line {}",
                            tokens[j].line
                        ));
                    }
                }
                ranges.push((tokens[open].start, tokens[end].end));
                i = end;
            }
        }
        i += 1;
    }
    Ok(ranges)
}
// Return every syntactically possible path, without evaluating cfg conditions.
fn module_paths(tokens: &[Token], source: &str, start: usize, end: usize) -> Result<Vec<String>> {
    let mut pending = vec![(start, end)];
    let mut paths = Vec::new();
    while let Some((start, end)) = pending.pop() {
        if identifier_is(tokens, source, start, "path") && is(tokens, source, start + 1, "=") {
            let literal = tokens
                .get(start + 2)
                .ok_or_else(|| "missing path literal".to_owned())?;
            let path = string_value(literal.text(source))?;
            let name = path
                .rsplit(['/', '\\'])
                .next()
                .filter(|n| !n.is_empty())
                .ok_or_else(|| "invalid module path".to_owned())?;
            paths.push(name.to_owned());
        } else if identifier_is(tokens, source, start, "cfg_attr")
            && is(tokens, source, start + 1, "(")
        {
            let close = group_end(tokens, source, start + 1)?.min(end);
            let mut i = start + 2;
            let mut condition = true;
            while i < close {
                if is(tokens, source, i, ",") {
                    condition = false;
                    i += 1;
                    continue;
                }
                let first = i;
                while i < close && !is(tokens, source, i, ",") {
                    if matches!(tokens[i].text(source), "(" | "[" | "{") {
                        i = group_end(tokens, source, i)?;
                    }
                    i += 1;
                }
                if !condition {
                    pending.push((first, i));
                }
            }
        }
    }
    Ok(paths)
}
fn analyze(source: &str, named_test_file: bool) -> Result<Analysis> {
    let tokens = lex(source)?;
    let macros = macro_ranges(&tokens, source)?;
    let mut parents = Vec::with_capacity(tokens.len());
    let mut stack = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        if matches!(token.text(source), ")" | "]" | "}") {
            stack.pop();
        }
        parents.push(stack.last().copied());
        if matches!(token.text(source), "(" | "[" | "{") {
            stack.push(i);
        }
    }
    let mut modules = BTreeMap::new();
    for i in 0..tokens.len() {
        let head = item_head(&tokens, source, i)?;
        if identifier_is(&tokens, source, head, "mod")
            && tokens.get(head + 1).is_some_and(|t| t.identifier)
            && is(&tokens, source, head + 2, "{")
            && !excluded(tokens[i].start, &macros)
        {
            modules
                .entry(head + 2)
                .or_insert((head, i, group_end(&tokens, source, head + 2)?));
        }
    }
    let mut attributes = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if let Some(a) = attribute(&tokens, source, i)? {
            i = a.end + 1;
            attributes.push(a);
        } else {
            i += 1;
        }
    }
    let mut leading_end = 0;
    let mut file_test = named_test_file;
    for a in &attributes {
        if !a.inner || a.start != leading_end {
            break;
        }
        file_test |= a.name == "cfg"
            && a.condition
                .is_some_and(|(s, e)| test_required(&tokens, source, s, e));
        leading_end = a.end + 1;
    }
    let mut ranges = if file_test {
        vec![(0, source.len())]
    } else {
        Vec::new()
    };
    let mut conditions = Vec::new();
    let mut module_attrs = BTreeMap::new();
    let mut n = 0;
    while n < attributes.len() {
        let first = n;
        let mut after = attributes[n].end + 1;
        if !attributes[n].inner {
            while n + 1 < attributes.len()
                && !attributes[n + 1].inner
                && attributes[n + 1].start == after
            {
                n += 1;
                after = attributes[n].end + 1;
            }
        }
        let group = &attributes[first..=n];
        let inner = group[0].inner;
        let test = !excluded(tokens[group[0].start].start, &macros)
            && group.iter().any(|a| {
                a.name == "cfg"
                    && a.condition
                        .is_some_and(|(s, e)| test_required(&tokens, source, s, e))
            });
        if test && !inner && after < tokens.len() {
            let end = item_end(&tokens, source, after)?;
            let last = tokens
                .get(end.saturating_sub(1))
                .map_or(source.len(), |t| t.end);
            ranges.push((tokens[group[0].start].start, last));
        }
        if test
            && inner
            && group[0].start >= leading_end
            && parents[group[0].start]
                .and_then(|p| modules.get(&p))
                .is_none()
        {
            return Err(format!(
                "unsupported test-required inner cfg at line {}",
                tokens[group[0].start].line
            ));
        }
        if test
            && inner
            && let Some((head, first, end)) = parents[group[0].start].and_then(|p| modules.get(&p))
        {
            let start = module_attrs
                .get(head)
                .map_or(*first, |attrs: &&[Attribute]| attrs[0].start);
            ranges.push((tokens[start].start, tokens[*end].end));
        }
        let head = if inner {
            after
        } else {
            item_head(&tokens, source, after)?
        };
        let dispatch = !inner
            && !excluded(tokens[group[0].start].start, &macros)
            && (is(&tokens, source, head, "mod") || is(&tokens, source, head, "use"));
        if !inner && is(&tokens, source, head, "mod") && !excluded(tokens[head].start, &macros) {
            module_attrs.insert(head, group);
        }
        for a in group {
            if let Some((s, e)) = a.condition
                && tokens[s..e]
                    .iter()
                    .any(|t| t.identifier && OS_IDENTIFIERS.contains(&t.identifier_text(source)))
            {
                conditions.push(Condition {
                    start: tokens[a.start].start,
                    line: tokens[a.start].line,
                    kind: a.name.clone(),
                    text: condition_text(&tokens, source, s, e),
                    dispatch,
                });
            }
        }
        n += 1;
    }
    // Check every out-of-line module, including modules inside a test-only item.
    for i in 0..tokens.len() {
        if !is(&tokens, source, i, "mod")
            || !tokens.get(i + 1).is_some_and(|t| t.identifier)
            || !is(&tokens, source, i + 2, ";")
        {
            continue;
        }
        let attrs = module_attrs.get(&i);
        let mut names = Vec::new();
        let mut unconditional_path = false;
        if let Some(attrs) = attrs {
            for a in *attrs {
                let bracket = a.start + 1;
                unconditional_path |= a.name == "path";
                names.extend(
                    module_paths(&tokens, source, bracket + 1, a.end)?
                        .into_iter()
                        .map(|name| (name, true)),
                );
            }
        }
        if !unconditional_path {
            names.push((tokens[i + 1].identifier_text(source).to_owned(), false));
        }
        let test = excluded(tokens[i].start, &ranges);
        for (name, path_override) in names {
            if test
                != if path_override {
                    test_file_name(&name)
                } else {
                    test_module_name(&name)
                }
            {
                return Err(format!(
                    "module filename rule at line {}: {} is {}",
                    tokens[i].line,
                    name,
                    if test { "test-only" } else { "production" }
                ));
            }
        }
    }
    for i in 0..tokens.len() {
        if identifier_is(&tokens, source, i, "cfg")
            && is(&tokens, source, i + 1, "!")
            && tokens
                .get(i + 2)
                .is_some_and(|t| matches!(t.text(source), "(" | "[" | "{"))
        {
            let end = group_end(&tokens, source, i + 2)?;
            if tokens[i + 3..end]
                .iter()
                .any(|t| t.identifier && OS_IDENTIFIERS.contains(&t.identifier_text(source)))
            {
                conditions.push(Condition {
                    start: tokens[i].start,
                    line: tokens[i].line,
                    kind: "cfg!".into(),
                    text: condition_text(&tokens, source, i + 3, end),
                    dispatch: false,
                });
            }
        }
    }
    conditions.retain(|c| !excluded(c.start, &ranges));
    conditions.sort_by_key(|c| (c.line, c.start));
    let mut lines = BTreeSet::new();
    for token in tokens {
        if !excluded(token.start, &ranges) {
            lines.extend(token.line..=token.end_line);
        }
    }
    Ok(Analysis {
        lines: lines.len(),
        conditions,
        file_test,
    })
}
fn condition_text(tokens: &[Token], source: &str, start: usize, end: usize) -> String {
    if start == end {
        String::new()
    } else {
        source[tokens[start].start..tokens[end - 1].end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}
fn string_value(literal: &str) -> Result<String> {
    if let Some((quote, hashes)) = raw_open(literal.as_bytes(), 0) {
        return Ok(literal[quote + 1..literal.len() - 1 - hashes].to_owned());
    }
    if !literal.starts_with('"') || !literal.ends_with('"') {
        return Err("module path must be a string literal".into());
    }
    let mut out = String::new();
    let mut chars = literal[1..literal.len() - 1].chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars
            .next()
            .ok_or_else(|| "incomplete module path escape".to_owned())?
        {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '\'' => out.push('\''),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '0' => out.push('\0'),
            'x' => {
                let digits: String = chars.by_ref().take(2).collect();
                let value =
                    u8::from_str_radix(&digits, 16).map_err(|_| "invalid path hex escape")?;
                if digits.len() != 2 || value > 0x7f {
                    return Err("invalid path hex escape".into());
                }
                out.push(char::from(value));
            }
            'u' => {
                if chars.next() != Some('{') {
                    return Err("invalid path Unicode escape".into());
                }
                let mut digits = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some('_') => {}
                        Some(c) => digits.push(c),
                        None => return Err("unclosed path Unicode escape".into()),
                    }
                }
                let c = u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| "invalid path Unicode escape".to_owned())?;
                out.push(c);
            }
            '\n' => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            '\r' if chars.next() == Some('\n') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            _ => return Err("invalid module path escape".into()),
        }
    }
    Ok(out)
}

fn path_text(path: &Path) -> Result<String> {
    path.components()
        .map(|p| {
            p.as_os_str()
                .to_str()
                .ok_or_else(|| "non-UTF-8 path".to_owned())
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}
fn entries(path: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .map(|e| e.map(|e| e.path()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}
fn metadata(path: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if m.file_type().is_symlink() {
        return Err(format!("symbolic link: {}", path.display()));
    }
    Ok(m)
}
fn category(relative: &str, tooling: bool, directory: bool) -> Result<&'static str> {
    if tooling {
        return Ok("tooling");
    }
    if relative == "src/platform.rs" {
        return Err("src/platform.rs is forbidden; use src/platform/mod.rs".into());
    }
    let Some(rest) = relative.strip_prefix("src/platform/") else {
        return Ok("common");
    };
    let mut parts = rest.split('/');
    let first = parts.next().unwrap();
    let nested = parts.next().is_some();
    if first == "mod.rs" && !directory && !nested {
        return Ok("common");
    }
    for os in ["windows", "unix", "linux", "macos"] {
        if first == os && (directory || nested) {
            return Ok(os);
        }
        if first == format!("{os}.rs") && !directory && !nested {
            return Ok(os);
        }
    }
    Err(format!("invalid platform placement: {relative}"))
}
struct SourceFile {
    path: String,
    category: &'static str,
    lines: usize,
}
fn collect(
    root: &Path,
    crate_root: &Path,
    dir: &Path,
    tooling: bool,
    files: &mut Vec<SourceFile>,
    violations: &mut Vec<Value>,
) -> Result<()> {
    for path in entries(dir)? {
        let m = metadata(&path)?;
        let relative = path_text(path.strip_prefix(crate_root).map_err(|e| e.to_string())?)?;
        if m.is_dir() {
            collect(root, crate_root, &path, tooling, files, violations)?;
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            category(&relative, tooling, false).map_err(|e| format!("{}: {e}", path.display()))?;
            continue;
        }
        let source = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if relative.starts_with("tests/") || relative.starts_with("benches/") {
            lex(&source).map_err(|e| format!("{}: {e}", path.display()))?;
            continue;
        }
        let named_test_file = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(test_file_name);
        let analysis =
            analyze(&source, named_test_file).map_err(|e| format!("{}: {e}", path.display()))?;
        if analysis.file_test {
            continue;
        }
        let category =
            category(&relative, tooling, false).map_err(|e| format!("{}: {e}", path.display()))?;
        let workspace_path = path_text(path.strip_prefix(root).map_err(|e| e.to_string())?)?;
        if !tooling && category == "common" {
            let dispatch = relative == "src/platform/mod.rs";
            for condition in analysis.conditions {
                if dispatch && condition.dispatch {
                    continue;
                }
                violations.push(json!({
                    "path": workspace_path, "line": condition.line, "kind": condition.kind,
                    "condition": condition.text,
                    "rule_id": if dispatch { "platform-dispatch" } else { "common-os-condition" },
                }));
            }
        }
        files.push(SourceFile {
            path: workspace_path,
            category,
            lines: analysis.lines,
        });
    }
    Ok(())
}
fn measure(root: &Path, allowances: &[Allowance]) -> Result<Value> {
    let crate_dir = root.join("crates");
    if !metadata(&crate_dir)?.is_dir() {
        return Err("workspace crates path is not a directory".into());
    }
    for path in entries(&crate_dir)? {
        let m = metadata(&path)?;
        if m.is_dir()
            && !path
                .file_name()
                .and_then(|p| p.to_str())
                .is_some_and(|p| CRATES.contains(&p))
        {
            return Err(format!("unknown crate directory: {}", path.display()));
        }
    }
    let mut files = Vec::new();
    let mut violations = Vec::new();
    let mut crate_counts = BTreeMap::new();
    for name in CRATES {
        let crate_root = crate_dir.join(name);
        if !metadata(&crate_root)?.is_dir() {
            return Err(format!("crate is not a directory: {name}"));
        }
        let first = files.len();
        collect(
            root,
            &crate_root,
            &crate_root,
            matches!(name, "kio-ci" | "kio-eval"),
            &mut files,
            &mut violations,
        )?;
        let mut counts: BTreeMap<&str, usize> =
            LIMITS.iter().map(|(category, _)| (*category, 0)).collect();
        for file in &files[first..] {
            *counts.get_mut(file.category).unwrap() += file.lines;
        }
        crate_counts.insert(name, counts);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    violations.sort_by(|a, b| {
        a["path"]
            .as_str()
            .cmp(&b["path"].as_str())
            .then_with(|| a["line"].as_u64().cmp(&b["line"].as_u64()))
    });
    let applications = apply_allowlist(root, &mut violations, allowances)?;
    let mut categories = BTreeMap::new();
    for (category, limit) in LIMITS {
        let lines: usize = files
            .iter()
            .filter(|f| f.category == category)
            .map(|f| f.lines)
            .sum();
        categories.insert(
            category,
            json!({"lines": lines, "limit": limit, "exceeded": lines > limit}),
        );
    }
    let oversized: Vec<_> = files
        .iter()
        .filter(|f| f.category != "tooling" && f.lines > FILE_LIMIT)
        .map(|f| json!({"path": f.path, "lines": f.lines, "limit": FILE_LIMIT}))
        .collect();
    let has_violations = !violations.is_empty()
        || !oversized.is_empty()
        || categories.values().any(|v| v["exceeded"] == true);
    let file_counts: Vec<_> = files
        .iter()
        .map(|f| json!({"path": f.path, "category": f.category, "lines": f.lines}))
        .collect();
    Ok(json!({
        "schema_id": "kio-code-limits/v1", "categories": categories, "crates": crate_counts,
        "files": file_counts, "oversized_product_files": oversized,
        "os_condition_violations": violations, "allowlist_applications": applications,
        "has_violations": has_violations,
    }))
}
fn apply_allowlist(
    root: &Path,
    violations: &mut Vec<Value>,
    allowances: &[Allowance],
) -> Result<Vec<Value>> {
    let mut applications = Vec::new();
    let mut seen = BTreeSet::new();
    for a in allowances {
        if a.reason.trim().is_empty() {
            return Err(format!("empty allowlist reason: {}", a.path));
        }
        if !matches!(a.rule_id, "common-os-condition" | "platform-dispatch") {
            return Err(format!("unsupported allowlist rule: {}", a.rule_id));
        }
        if !seen.insert((a.path, a.rule_id)) {
            return Err(format!(
                "duplicate allowlist registration: {} {}",
                a.path, a.rule_id
            ));
        }
        if !metadata(&root.join(a.path))?.is_file() {
            return Err(format!("allowlist path is not a file: {}", a.path));
        }
        let before = violations.len();
        violations.retain(|v| v["path"] != a.path || v["rule_id"] != a.rule_id);
        let count = before - violations.len();
        if count == 0 {
            return Err(format!(
                "allowlist registration matches no violation: {} {}",
                a.path, a.rule_id
            ));
        }
        applications.push(
            json!({"path": a.path, "rule_id": a.rule_id, "reason": a.reason, "count": count}),
        );
    }
    applications.sort_by(|a, b| {
        a["path"]
            .as_str()
            .cmp(&b["path"].as_str())
            .then_with(|| a["rule_id"].as_str().cmp(&b["rule_id"].as_str()))
    });
    Ok(applications)
}

#[cfg(test)]
mod tests;
