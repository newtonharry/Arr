//! The MCP server keeps parity with the API and the web app (see `AGENTS.md`).
//!
//! Three things must hold, and each fails with what to add:
//!
//! 1. every route the real router serves has an entry in `arr::mcp::PARITY` (the router is
//!    built from `api::route_paths`'s own list, and each method of each path is found by
//!    asking the running router, so a route cannot exist unseen);
//! 2. every path the web app (`src/webapp/app.js`) calls has an entry;
//! 3. every entry that names a tool names a registered one, and every registered tool is in an
//!    entry.
#![recursion_limit = "256"]

mod common;

use arr::mcp::{PARITY, Parity};
use common::*;
use std::collections::BTreeSet;

const APP_JS: &str = include_str!("../src/webapp/app.js");
const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

const HOW_TO: &str = "\n\nTo fix this, add the route to the parity table `PARITY` in src/mcp.rs. Give it the MCP tool that does it (add the tool in src/mcp/web.rs, a row in docs/MCP.md and a test), or, only if it truly makes no sense in a chat, WebOnly or Operator with a reason. A feature is not web-only because it is new, hard or risky. See AGENTS.md (\"The MCP server keeps parity with Arr\").";

/// `{id}` placeholders filled in so a path can be requested.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|s| if s.starts_with('{') { "x" } else { s })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether a table path (a router pattern) matches a path with `{}` for what the web app
/// computes at run time.
fn path_matches(pattern: &str, path: &str) -> bool {
    let (p, a): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
    p.len() == a.len()
        && p.iter()
            .zip(&a)
            .all(|(p, a)| p.starts_with('{') || *a == "{}" || p == a)
}

/// The text of a call's argument list (after the opening parenthesis at `open`) and where it
/// ends, skipping strings and template literals.
fn call_args(js: &str, open: usize) -> Option<&str> {
    let b = js.as_bytes();
    let (mut depth, mut i) = (0i32, open);
    let mut quote: Option<u8> = None;
    let mut tmpl_depth: Vec<i32> = vec![];
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 1;
                } else if q == b'`' && c == b'$' && b.get(i + 1) == Some(&b'{') {
                    tmpl_depth.push(depth);
                    depth += 1;
                    quote = None;
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if c == b'}' && tmpl_depth.last() == Some(&depth) {
                        tmpl_depth.pop();
                        quote = Some(b'`');
                    }
                    if depth == 0 {
                        return Some(&js[open + 1..i]);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// Splits `s` at top-level occurrences of `sep` (outside strings, templates and brackets).
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut depth, mut start) = (0i32, 0usize);
    let mut quote: Option<u8> = None;
    let mut tmpl_depth: Vec<i32> = vec![];
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match quote {
            Some(q) => {
                if c == b'\\' {
                    i += 1;
                } else if q == b'`' && c == b'$' && b.get(i + 1) == Some(&b'{') {
                    tmpl_depth.push(depth);
                    depth += 1;
                    quote = None;
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'\'' | b'"' | b'`' => quote = Some(c),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if c == b'}' && tmpl_depth.last() == Some(&depth) {
                        tmpl_depth.pop();
                        quote = Some(b'`');
                    }
                }
                c if c as char == sep && depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// One piece of a path expression: a literal as its text (with `{}` for `${...}`), anything
/// else as `{}`.
fn piece(p: &str) -> String {
    let p = p.trim();
    let (first, last) = (p.chars().next(), p.chars().last());
    match (first, last) {
        (Some(q @ ('\'' | '"')), Some(l)) if q == l && p.len() >= 2 => p[1..p.len() - 1].into(),
        (Some('`'), Some('`')) if p.len() >= 2 => {
            let inner = &p[1..p.len() - 1];
            let mut out = String::new();
            let mut rest = inner;
            while let Some(i) = rest.find("${") {
                out.push_str(&rest[..i]);
                out.push_str("{}");
                let mut depth = 0;
                let mut end = rest.len();
                for (j, c) in rest[i + 1..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = i + 1 + j + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                rest = &rest[end..];
            }
            out.push_str(rest);
            out
        }
        _ => "{}".into(),
    }
}

/// The paths a call's first argument can be: both branches of a ternary, pieces joined by `+`,
/// anything computed as `{}`, the query string cut off.
fn paths_of(arg: &str) -> Vec<String> {
    let branches: Vec<&str> = match split_top(arg, '?').as_slice() {
        [_, rest] => split_top(rest, ':').into_iter().map(str::trim).collect(),
        _ => vec![arg],
    };
    branches
        .into_iter()
        .map(|b| {
            let joined: String = split_top(b, '+').into_iter().map(piece).collect();
            let joined = joined.split('?').next().unwrap_or_default().to_string();
            joined.replace("{}{}", "{}")
        })
        .collect()
}

/// `(method, path)` of every call the web app makes through its helpers.
fn web_calls() -> Vec<(String, String)> {
    let mut out = vec![];
    let method_re = regex::Regex::new(r#"method:\s*['"]([A-Z]+)['"]"#).unwrap();
    for (helper, prefix) in [
        ("api(", "/app/api"),
        ("sportReq(", "/app/api/v1/sport"),
        ("quietGet(", "/app/api"),
    ] {
        let mut from = 0;
        while let Some(i) = APP_JS[from..].find(helper) {
            let at = from + i;
            from = at + helper.len();
            let before = APP_JS[..at].chars().next_back().unwrap_or(' ');
            if before.is_alphanumeric() || before == '_' || before == '.' {
                continue;
            }
            if APP_JS[..at].ends_with("function ") {
                continue;
            }
            let args = call_args(APP_JS, at + helper.len() - 1)
                .unwrap_or_else(|| panic!("an unbalanced {helper} call in app.js"));
            let parts = split_top(args, ',');
            let method = method_re
                .captures(args)
                .map_or("GET".to_string(), |c| c[1].to_string());
            let paths = paths_of(parts[0]);
            assert!(
                paths.iter().all(|p| p.starts_with('/')),
                "tests/mcp_parity.rs cannot read the path of this app.js call (keep the first \
                 argument a string, a template or a `+` of them, or teach the test): {helper}{args:.80}"
            );
            for p in paths {
                out.push((method.clone(), format!("{prefix}{p}")));
            }
        }
    }
    out
}

fn entries() -> BTreeSet<(String, String)> {
    PARITY
        .iter()
        .map(|(m, p, _)| (m.to_string(), p.to_string()))
        .collect()
}

#[tokio::test]
async fn every_route_of_the_router_has_a_parity_entry() {
    let e = start_exec(ExecOptions::default()).await;
    // Without a token a route that exists answers 401 (or 200 when public) and anything else
    // 404 or 405, before any handler runs: so the router itself says which methods each of
    // its paths has.
    let anon = reqwest::Client::new();
    let mut served = BTreeSet::new();
    let paths = arr::api::route_paths();
    assert_eq!(
        paths.len(),
        paths.iter().collect::<BTreeSet<_>>().len(),
        "a path is listed twice in the router's route list"
    );
    for path in &paths {
        for method in METHODS {
            let url = format!("{}{}", e.base, concrete(path));
            let r = anon
                .request(method.parse().unwrap(), url)
                .send()
                .await
                .unwrap();
            if !matches!(r.status().as_u16(), 404 | 405) {
                served.insert((method.to_string(), path.to_string()));
            }
        }
    }
    assert!(served.len() > 80, "found only {} routes", served.len());
    let table: BTreeSet<_> = entries()
        .into_iter()
        .filter(|(_, p)| p.starts_with("/api/"))
        .collect();
    let missing: Vec<_> = served.difference(&table).collect();
    assert!(
        missing.is_empty(),
        "these API routes have no parity entry: {missing:?}{HOW_TO}"
    );
    let stale: Vec<_> = table.difference(&served).collect();
    assert!(
        stale.is_empty(),
        "these parity entries name routes the router does not serve (renamed or removed?): {stale:?}\n\nRemove or correct them in `PARITY` in src/mcp.rs. See AGENTS.md."
    );
    // The OpenAPI list is checked against the same router, so it cannot drift either.
    let documented: BTreeSet<_> = arr::api::ROUTES
        .iter()
        .map(|(m, p)| (m.to_uppercase(), p.to_string()))
        .collect();
    assert_eq!(
        served, documented,
        "api::ROUTES (the OpenAPI list) differs from the router: add the route there and to src/openapi.rs"
    );
}

#[test]
fn every_path_the_web_app_calls_has_a_parity_entry() {
    let calls = web_calls();
    assert!(
        calls.len() > 40,
        "read only {} calls from app.js: {calls:?}",
        calls.len()
    );
    let table = entries();
    let mut missing = BTreeSet::new();
    for (method, path) in &calls {
        // The app's proxy passes `/app/api/v1/...` to `/api/v1/...`; its own routes
        // (sign-in, notifications) are listed under `/app/api`.
        let candidates = [path.clone(), path.replacen("/app/api", "/api", 1)];
        let found = table
            .iter()
            .any(|(m, p)| m == method && candidates.iter().any(|c| path_matches(p, c)));
        if !found {
            missing.insert(format!("{method} {path}"));
        }
    }
    assert!(
        missing.is_empty(),
        "the web app calls paths with no parity entry: {missing:?}{HOW_TO}"
    );
}

#[test]
fn every_entry_names_a_registered_tool_and_every_tool_has_an_entry() {
    let registered: BTreeSet<String> = arr::mcp::tool_names().into_iter().collect();
    let mut used = BTreeSet::new();
    for (method, path, how) in PARITY {
        match how {
            Parity::Tool(name) => {
                assert!(
                    registered.contains(*name),
                    "{method} {path} names the tool {name:?}, which is not registered in src/mcp.rs (tools())\n\nRegister it, or correct the entry. See AGENTS.md."
                );
                used.insert(name.to_string());
            }
            Parity::WebOnly(why) | Parity::Operator(why) => assert!(
                why.len() >= 20,
                "{method} {path} needs a real reason, not {why:?}"
            ),
        }
    }
    let unused: Vec<_> = registered.difference(&used).collect();
    assert!(
        unused.is_empty(),
        "these tools are in no parity entry: {unused:?}\n\nA tool exists to cover a route: add its `Parity::Tool(..)` entry (or remove the tool). See AGENTS.md."
    );
    let keys = entries();
    assert_eq!(
        keys.len(),
        PARITY.len(),
        "a route is listed twice in PARITY"
    );
}
