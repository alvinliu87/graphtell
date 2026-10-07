use super::*;
pub(crate) fn extract_http_args(style: &HttpStyle, args: &str) -> (Option<String>, String) {
    match style {
        HttpStyle::Obj => {
            let method = field_string(args, "method")
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            (field_expr(args, "url").and_then(|e| literal_url_expr(&e)), method)
        }
        HttpStyle::Axios(Some(verb)) => {
            let url = first_arg_expr(args)
                .and_then(|e| literal_url_expr(&e))
                .or_else(|| field_expr(args, "url").and_then(|e| literal_url_expr(&e)));
            (url, verb.clone())
        }
        HttpStyle::Axios(None) => {
            let method = field_string(args, "method")
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            (field_expr(args, "url").and_then(|e| literal_url_expr(&e)), method)
        }
        HttpStyle::Member(verb) => {
            let url = first_arg_expr(args)
                .and_then(|e| literal_url_expr(&e))
                .or_else(|| field_expr(args, "url").and_then(|e| literal_url_expr(&e)));
            (url, verb.clone())
        }
        HttpStyle::Fetch => {
            let url = first_arg_expr(args).and_then(|e| literal_url_expr(&e));
            let method = field_string(args, "method")
                .map(|m| m.to_ascii_uppercase())
                .unwrap_or_else(|| "GET".to_string());
            (url, method)
        }
        HttpStyle::Other => (None, "GET".to_string()),
    }
}

pub(crate) fn is_http_verb(s: &str) -> bool {
    matches!(s, "get" | "post" | "put" | "delete" | "patch" | "head" | "options")
}

// ---------------------------------------------------------------- URL expression normalisation

/// Normalise a URL **expression** into a literal string with `:param` placeholders.
///
/// * `'v2/invoice/detail/' + id`           -> `v2/invoice/detail/:param`
/// * `` `v2/order/invoice_detail/${id}` `` -> `v2/order/invoice_detail/:param`
/// * `'v2/index'`                          -> `v2/index` (a pure literal, unchanged)
/// * `BASE + '/api' + url`                 -> `None` (the first segment is a variable, cannot be anchored)
pub(crate) fn literal_url_expr(expr: &str) -> Option<String> {
    let mut out = String::new();
    let mut first = true;
    for part in split_top_plus(expr) {
        let t = part.trim();
        if t.is_empty() {
            continue;
        }
        let bytes = t.as_bytes();
        if bytes[0] == b'"' || bytes[0] == b'\'' {
            if let Some(s) = decode_string(t) {
                out.push_str(&s);
            }
        } else if bytes[0] == b'`' {
            // Template string: every `${expr}` folds into `:param` (the parameter name does not matter, the shape does).
            let inner = strip_ends(t, '`', '`');
            let mut s = String::with_capacity(inner.len());
            let mut rest: &str = inner;
            while let Some(pos) = rest.find("${") {
                s.push_str(&rest[..pos]);
                s.push_str(":param");
                match rest[pos + 2..].find('}') {
                    Some(end) => rest = &rest[pos + 2 + end + 1..],
                    None => {
                        rest = "";
                        break;
                    }
                }
            }
            s.push_str(rest);
            out.push_str(&s);
        } else if first {
            // The first segment is not a literal: even the URL prefix cannot be anchored, so abandon the whole thing.
            return None;
        } else {
            out.push_str(":param");
        }
        first = false;
    }
    let out = out.trim_end_matches('/').to_string();
    if out.is_empty() || out.starts_with(":param") {
        return None;
    }
    Some(out)
}

/// Collect the arguments of an ordinary call: `(a, b, c)` -> `[value, value, value]`, keeping only what is
/// statically determinable.
///
/// Why it is worth doing: the front-end FKB wants to derive identity from `{ arg: 0 }` just like the back end,
/// which requires arguments. But `$var` / `fn()` must never be treated as identity — those all fall through to
/// [`js_value`]'s `Unknown` and are rejected by FKB's `require_literal` (same handling as the back end's
/// `Cache::get($name)`).
pub(crate) fn literal_args(raw: &str) -> Vec<FactValue> {
    let inner = raw.trim();
    let inner = inner
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(inner);
    if inner.trim().is_empty() {
        return Vec::new();
    }
    split_top_commas(inner)
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .map(|s| js_value(s.trim()))
        .collect()
}

/// Fold an argument expression's text into a [`FactValue`]: string / string array / object literal / number /
/// boolean; anything else becomes `Unknown(raw text)`.
pub(crate) fn js_value(raw: &str) -> FactValue {
    let b = raw.as_bytes();
    let Some(&first) = b.first() else {
        return FactValue::Unknown(None);
    };
    match first {
        b'"' | b'\'' | b'`' => decode_string(raw).map(FactValue::String).unwrap_or_else(|| {
            FactValue::Unknown(Some(raw.to_string()))
        }),
        b'[' => {
            let inner = strip_ends(raw, '[', ']');
            let items = split_top_commas(inner)
                .into_iter()
                .enumerate()
                .map(|(i, s)| (i.to_string(), js_value(s.trim())))
                .collect();
            FactValue::Array(items)
        }
        b'{' => {
            let inner = strip_ends(raw, '{', '}');
            let items = split_top_commas(inner)
                .into_iter()
                .filter_map(|s| s.split_once(':'))
                .map(|(k, v)| {
                    (
                        k.trim().trim_matches(|c| c == '\'' || c == '"').to_string(),
                        js_value(v.trim()),
                    )
                })
                .collect();
            FactValue::Array(items)
        }
        _ if raw == "true" || raw == "false" => FactValue::Bool(raw == "true"),
        _ if raw.parse::<i64>().is_ok() => FactValue::Int(raw.parse().unwrap_or(0)),
        _ => FactValue::Unknown(Some(raw.to_string())),
    }
}

/// Strip one wrapping pair of characters (e.g. `[...]` / `{...}` / `` `...` ``).
///
/// Equivalent to `&raw[1..raw.len()-1]`, but it never **cuts in the middle of a multi-byte character**: argument
/// text may lack the closing symbol or end with a Chinese character, and a raw slice would panic. When the closing
/// symbol is missing it degrades to "strip only the left half" and the caller keeps parsing as best it can.
pub(crate) fn strip_ends(raw: &str, open: char, close: char) -> &str {
    let s = raw.strip_prefix(open).unwrap_or(raw);
    s.strip_suffix(close).unwrap_or(s)
}

/// Split on top-level `,` (skipping string literals and bracket interiors), returning whitespace-trimmed fragments.
pub(crate) fn split_top_commas(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' | b'`' => i = skip_string(b, i),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(s[start..].trim());
    parts
}

/// Split an expression on top-level `+` (skipping string literals and bracket / brace interiors).
pub(crate) fn split_top_plus(expr: &str) -> Vec<String> {
    let b = expr.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] as char {
            '"' | '\'' | '`' => i = skip_string(b, i),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '+' if depth == 0 => {
                parts.push(expr[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(expr[start..].to_string());
    parts
}

/// Take the expression text of the **first top-level argument** from `args` (`'a/' + id, data, {...}` -> `'a/' + id`).
fn first_arg_expr(args: &str) -> Option<String> {
    let open = args.find('(')?;
    top_level_segment(&args[open + 1..])
}

/// Take the **value expression** text after `key:` in an object literal (up to a top-level `,` or end of string).
///
/// Same prefix rule as [`field_string`]: `url: url` (a variable) must not overrun its key, and `base_url:` must
/// not be mistaken for it.
pub(crate) fn field_expr(args: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:");
    let bytes = args.as_bytes();
    let mut from = 0;
    let idx = loop {
        let pos = args[from..].find(&needle)?;
        let abs = from + pos;
        let prev_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        if prev_ok {
            break abs;
        }
        from = abs + 1;
    };
    top_level_segment(&args[idx + needle.len()..])
}

/// Take the first **top-level** fragment of a text (up to a depth-0 `,` or end of string), skipping strings and brackets.
pub(crate) fn top_level_segment(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] as char {
            '"' | '\'' | '`' => i = skip_string(b, i),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            ',' if depth == 0 => break,
            _ => {}
        }
        i += 1;
    }
    let seg = s[..i.min(s.len())].trim();
    if seg.is_empty() {
        None
    } else {
        Some(seg.to_string())
    }
}

/// Return the index just past the matching quote (handling `\"` escapes; `${}` inside template strings has no quote scenario and is kept simple).
fn skip_string(b: &[u8], open: usize) -> usize {
    let quote = b[open];
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            c if c == quote => return i,
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// The **receiver allowlist** for member-form HTTP calls: the `request` part of `request.get(...)`.
///
/// An HTTP verb as the method name is not enough — same-shaped different things such as `$store.get()` /
/// `cache.get()` / `storage.get()` are everywhere, and treating them all as contracts would pollute the route
/// perspective (fake contracts + fake phantom calls). So an allowlist narrows it: a small hit surface but **no
/// misjudgement**, and a project's own wrapper name can be added here with one line.
///
/// `this.request.get(...)` / `store.api.get(...)`: judge on the last segment (same tail-name matching as
/// `recv_matches`; FKB still writes `request::{get,post,...}`).
pub(crate) fn is_http_client_recv(recv: &str) -> bool {
    let last = recv.rsplit('.').next().unwrap_or(recv);
    matches!(
        last,
        "request" | "requests" | "http" | "https" | "$http" | "ajax" | "api" | "apiClient" | "httpClient"
    )
}


/// Take the whole source line a call site sits on, so the UI can show it directly as the "call site" in a link.
pub(crate) fn line_snippet(src: &str, start: usize) -> Option<String> {
    let line_start = src[..start.min(src.len())].rfind('\n').map(|i| i + 1).unwrap_or(0);
    src[line_start..].lines().next().map(|l| l.trim().to_string())
}

/// Extract the `<script>` block of a Vue single-file component, padding it with leading newlines so line numbers align with the original file.
pub(crate) fn extract_vue_script(src: &str) -> Option<String> {
    let open = src.find("<script")?;
    let gt_rel = src[open..].find('>')?;
    let close_tag = open + gt_rel;
    let after = close_tag + 1;
    let end_rel = src[after..].find("</script>")?;
    let end = after + end_rel;
    let inner = &src[after..end];
    let lines_before = src[..open].matches('\n').count();
    let mut s = String::new();
    for _ in 0..lines_before {
        s.push('\n');
    }
    s.push_str(inner);
    Some(s)
}

/// Match a string literal (including template strings).
fn str_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`"#).unwrap())
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Strip the quotes and apply minimal unescaping; a template string with interpolation returns `None` (cannot be evaluated statically).
fn decode_string(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let quote = bytes[0] as char;
    if bytes.len() < 2 || bytes[bytes.len() - 1] != quote as u8 {
        return None;
    }
    if quote == '`' && raw.contains("${") {
        return None;
    }
    let inner = &raw[1..raw.len() - 1];
    let decoded = inner
        .replace("\\\"", "\"")
        .replace("\\'", "'")
        .replace("\\\\", "\\")
        .replace("\\n", "\n")
        .replace("\\t", "\t");
    if decoded.is_empty() {
        return None;
    }
    Some(decoded)
}

/// Find the first string literal in a substring; a template string containing `${` counts as not statically evaluable.
fn first_string(args: &str) -> Option<String> {
    let m = str_re().find(args)?;
    decode_string(m.as_str())
}

/// Find `key: <string literal>` inside a substring.
///
/// Two hard constraints (traps hit by real front-end code):
/// * the value must follow `key:` immediately — with `url: url` (a variable) it must never overrun the key and grab
///   a later `method: 'get'`;
/// * no identifier character may precede `key` — `base_url:` must not be read as `url:`.
fn field_string(args: &str, key: &str) -> Option<String> {
    let needle = format!("{key}:");
    let bytes = args.as_bytes();
    let mut from = 0;
    let idx = loop {
        let pos = args[from..].find(&needle)?;
        let abs = from + pos;
        let prev_ok = abs == 0 || !is_ident_byte(bytes[abs - 1]);
        if prev_ok {
            break abs;
        }
        from = abs + 1;
    };
    let rest = args[idx + needle.len()..].trim_start();
    let first = rest.as_bytes().first().copied()?;
    if first != b'"' && first != b'\'' && first != b'`' {
        return None;
    }
    first_string(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::syntax::FactValue;

    /// `decode_string` is the decoder every literal answer goes through: interpolation is not statically
    /// evaluable, and a missing closing quote / empty content must degrade to `None` rather than yielding a
    /// partial or wrong string (downstream that would become a wrong identity).
    #[test]
    fn decode_string_handles_quotes_and_escapes_and_rejects_unevaluable() {
        assert_eq!(decode_string("\"x\""), Some("x".to_string()));
        assert_eq!(decode_string("'x'"), Some("x".to_string()));
        assert_eq!(decode_string("`plain`"), Some("plain".to_string()), "a template without interpolation is a literal");
        assert_eq!(decode_string("`a${b}`"), None, "interpolation cannot be evaluated statically");
        assert_eq!(decode_string("'x"), None, "a missing closing quote is not a literal");
        assert_eq!(decode_string("''"), None, "empty content carries no fact");
        assert_eq!(decode_string("\"\""), None);
        // Minimal unescaping.
        assert_eq!(decode_string("\"a\\\"b\""), Some("a\"b".to_string()));
        assert_eq!(decode_string("'a\\'b'"), Some("a'b".to_string()));
        assert_eq!(decode_string("'a\\\\b'"), Some("a\\b".to_string()));
    }

    /// `first_string`: the first literal inside a fragment; a template holding `${` is not evaluable.
    #[test]
    fn first_string_picks_the_first_literal() {
        assert_eq!(first_string("foo('x', 3)"), Some("x".to_string()));
        assert_eq!(first_string("foo(3, 'x')"), Some("x".to_string()));
        assert_eq!(first_string("foo(3, 4)"), None, "no string literal at all");
        assert_eq!(first_string("bar(`a${b}`)"), None, "a template with interpolation is not evaluable");
    }

    /// `field_string` (the HTTP `method` lookup) differs from `field_expr` in that the value **must** be a string
    /// literal, so `url: url` can never overrun into a later string value — with the same `base_url:` prefix rule.
    #[test]
    fn field_string_requires_a_string_literal_and_respects_the_prefix_rule() {
        assert_eq!(field_string("{ method: 'post' }", "method").as_deref(), Some("post"));
        assert_eq!(field_string("{ url: '/a', method: 'GET' }", "method").as_deref(), Some("GET"));
        assert_eq!(
            field_string("url: url, method: 'get'", "method").as_deref(),
            Some("get"),
            "a variable value must stop at the key instead of overrunning into a later literal"
        );
        assert_eq!(
            field_string("base_url: '/x', url: '/y'", "url").as_deref(),
            Some("/y"),
            "`base_url:` must not be taken for `url:`"
        );
        assert_eq!(field_string("foo: 123", "foo"), None, "a non-string value is not a literal");
        assert_eq!(field_string("bar: 'x'", "key"), None, "an absent key");
    }

    /// `first_arg_expr`: the text of the **first** top-level argument — how `request.get(url, data)` and
    /// `fetch(url, opts)` anchor their URL.
    #[test]
    fn first_arg_expr_takes_the_first_argument_only() {
        assert_eq!(first_arg_expr("foo('a/' + id, data)").as_deref(), Some("'a/' + id"));
        assert_eq!(first_arg_expr("f('a', ('b'))").as_deref(), Some("'a'"));
        assert_eq!(first_arg_expr("foo()"), None, "no arguments ⇒ nothing to anchor on");
        assert_eq!(first_arg_expr("foo"), None, "no parentheses at all");
    }

    /// `line_snippet` is what the view shows as the call site: an off-by-one here points at the wrong code.
    #[test]
    fn line_snippet_returns_the_line_the_offset_sits_on() {
        let src = "line one\nfunction f() {\n  g();\n}\n";
        assert_eq!(line_snippet(src, src.find("g();").unwrap()).as_deref(), Some("g();"), "the owning line, trimmed");
        assert_eq!(line_snippet(src, src.find("function").unwrap()).as_deref(), Some("function f() {"));
        assert_eq!(line_snippet(src, 0).as_deref(), Some("line one"));
        // A byte offset past the end clamps rather than panicking (no unwrap of a byte slice).
        assert_eq!(line_snippet(src, src.len() + 100), None);
        assert_eq!(line_snippet("", 0), None, "empty source has no lines");
    }

    /// The `<script>` block of a Vue SFC, padded with leading newlines so line numbers stay aligned — and the two
    /// rejection paths (no block / unterminated block), which are what keeps a non-SFC file from being misread.
    #[test]
    fn extract_vue_script_pads_lines_and_rejects_missing_or_unterminated_blocks() {
        let src = "<template>\n<div/></template>\n<script>\nconst a = 1;\n</script>\n";
        let got = extract_vue_script(src).expect("a well-formed SFC has a script block");
        assert!(got.contains("const a = 1;"), "the script body is returned: {got:?}");
        assert!(
            got.starts_with("\n\n\n"),
            "the leading padding keeps the inner line numbers aligned with the original file: {got:?}"
        );

        assert!(extract_vue_script("<template/>\n<style/>").is_none(), "no <script> block ⇒ None");
        assert!(extract_vue_script("<script>const a = 1;").is_none(), "an unterminated <script> ⇒ None");
    }

    /// `literal_args` — the ordinary (non-HTTP) argument path that feeds FKB's `{ arg: 0 }` identity: literals
    /// fold into their value, everything else into `Unknown` so a variable can never become identity.
    #[test]
    fn literal_args_folds_each_argument_into_a_fact_value() {
        let a = literal_args("('x', 3, true)");
        assert!(matches!(a[0], FactValue::String(ref s) if s == "x"));
        assert!(matches!(a[1], FactValue::Int(3)));
        assert!(matches!(a[2], FactValue::Bool(true)));

        // A concatenation / a bare variable is not statically determinable ⇒ Unknown.
        let b = literal_args("('v2/' + id, data)");
        assert!(matches!(b[0], FactValue::Unknown(_)), "got: {b:?}");
        assert!(matches!(b[1], FactValue::Unknown(_)), "got: {b:?}");

        // Array / object literals fold recursively.
        match literal_args("([1, 'a'])").first() {
            Some(FactValue::Array(items)) => assert_eq!(items.len(), 2),
            other => panic!("an array literal must fold into Array: {other:?}"),
        }
        match literal_args("({a: 1})").first() {
            Some(FactValue::Array(items)) => assert_eq!(items[0].0, "a"),
            other => panic!("an object literal must fold into a key-value Array: {other:?}"),
        }

        // No arguments ⇒ empty.
        assert!(literal_args("()").is_empty());
        assert!(literal_args("( )").is_empty());
    }
}
