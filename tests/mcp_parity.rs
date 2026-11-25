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
/// computes at run time. A computed segment stands for an id, so it may match only a route
/// parameter (`{media_id}`), never a literal segment such as `health`.
fn path_matches(pattern: &str, path: &str) -> bool {
    let (p, a): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
    p.len() == a.len()
        && p.iter()
            .zip(&a)
            .all(|(p, a)| p == a || (p.starts_with('{') && !a.is_empty()))
}

/// Computed route names the web app is known to use, keyed by the exact expression in
/// `app.js`: `P.kind` is the page kind (a movie or a show page) and `S.kind` the list kind
/// (movies or series). Any other expression in the place of a route name is refused.
const KNOWN_NAMES: &[(&str, &[&str])] = &[
    ("P.kind", &["movie", "show"]),
    ("S.kind", &["movies", "series"]),
];

/// Resolves the computed segments (`{expr}`) of a path read from `app.js`. A known expression
/// stands for its route names. Any other computed segment must follow a literal segment (it is
/// an id, and the table must have a route parameter there); a computed segment that does not
/// (a route name such as `{section}` right after the root or after another computed segment)
/// is an error.
fn resolve(path: &str) -> Result<Vec<String>, String> {
    let mut variants: Vec<Vec<String>> = vec![vec![]];
    let mut prev_literal = false;
    for seg in path.split('/') {
        let computed = seg.contains('{');
        let options: Vec<String> = if !computed {
            vec![seg.to_string()]
        } else if let Some(values) = seg
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .and_then(|e| KNOWN_NAMES.iter().find(|(k, _)| *k == e))
            .map(|(_, v)| *v)
        {
            values.iter().map(|v| v.to_string()).collect()
        } else if prev_literal {
            vec!["{}".to_string()]
        } else {
            return Err(format!(
                "tests/mcp_parity.rs cannot tell what the computed route name {seg} in `{path}` stands for; use a literal, or add its exact expression to KNOWN_NAMES"
            ));
        };
        variants = variants
            .into_iter()
            .flat_map(|v| {
                options.iter().map(move |o| {
                    let mut v = v.clone();
                    v.push(o.clone());
                    v
                })
            })
            .collect();
        prev_literal = !computed || !options.iter().any(|o| o == "{}");
        if seg.is_empty() {
            prev_literal = false;
        }
    }
    Ok(variants.into_iter().map(|v| v.join("/")).collect())
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
                let mut depth = 0;
                let mut end = rest.len();
                for (j, c) in rest[i + 1..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = i + 1 + j + 1;
                                out.push_str(&format!("\u{1}{}\u{2}", rest[i + 2..end - 1].trim()));
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
        _ => format!("\u{1}{p}\u{2}"),
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
            // The query string starts at the first `?` outside a computed expression.
            let (mut cut, mut inside) = (joined.len(), false);
            for (i, c) in joined.char_indices() {
                match c {
                    '\u{1}' => inside = true,
                    '\u{2}' => inside = false,
                    '?' if !inside => {
                        cut = i;
                        break;
                    }
                    _ => {}
                }
            }
            // Render literals as they are and computed pieces as `{expression}`; computed
            // pieces side by side (an id and a trailing query expression) are one.
            let mut out = String::new();
            let mut prev_expr = false;
            for chunk in joined[..cut].split('\u{1}') {
                match chunk.split_once('\u{2}') {
                    Some((expr, lit)) => {
                        if !prev_expr {
                            out.push_str(&format!("{{{expr}}}"));
                        }
                        out.push_str(lit);
                        prev_expr = lit.is_empty();
                    }
                    None => {
                        out.push_str(chunk);
                        prev_expr = false;
                    }
                }
            }
            out
        })
        .collect()
}

/// `(method, path)` of every call the web app makes through its helpers, or why the source
/// cannot be read.
fn web_calls_in(js: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = vec![];
    let method_re =
        regex::Regex::new(r#"method:\s*['"]([A-Z]+)['"]"#).map_err(|e| e.to_string())?;
    for (helper, prefix) in [
        ("api", "/app/api"),
        ("sportReq", "/app/api/v1/sport"),
        ("quietGet", "/app/api"),
    ] {
        let mut from = 0;
        while let Some(i) = js[from..].find(helper) {
            let at = from + i;
            from = at + helper.len();
            let before = js[..at].chars().next_back().unwrap_or(' ');
            let after = js[from..].chars().next();
            if before.is_alphanumeric() || matches!(before, '_' | '.' | '$' | '\'' | '"' | '/') {
                continue;
            }
            if after.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$') {
                continue;
            }
            // Whitespace or a newline may sit between the name and its parenthesis.
            let rest = &js[from..];
            let gap = rest.len() - rest.trim_start().len();
            if !rest.trim_start().starts_with('(') {
                // Not a call: a definition's name is followed by one, a use as a value
                // (an alias passed around) would hide calls from this test.
                if js[..at].trim_end().ends_with("function") || rest.trim_start().starts_with(':') {
                    continue;
                }
                return Err(format!(
                    "app.js uses `{helper}` other than by calling it ({:.60}); call it directly so tests/mcp_parity.rs can see the path",
                    js[at..].replace('\n', " ")
                ));
            }
            if js[..at].trim_end().ends_with("function") {
                continue;
            }
            let open = from + gap;
            let args = call_args(js, open)
                .ok_or_else(|| format!("an unbalanced {helper} call in app.js"))?;
            let parts = split_top(args, ',');
            let method = method_re
                .captures(args)
                .map_or("GET".to_string(), |c| c[1].to_string());
            let paths = paths_of(parts[0]);
            if !paths.iter().all(|p| p.starts_with('/')) {
                return Err(format!(
                    "tests/mcp_parity.rs cannot read the path of this app.js call (keep the first argument a string, a template or a `+` of them, or teach the test): {helper}({args:.80}"
                ));
            }
            for p in &paths {
                for p in resolve(p)? {
                    out.push((method.clone(), format!("{prefix}{p}")));
                }
            }
        }
    }
    // Every other way to reach the server would bypass the helpers above.
    let fetches = regex::Regex::new(r"\bfetch\s*\(")
        .map_err(|e| e.to_string())?
        .find_iter(js)
        .count();
    if fetches != 3 {
        return Err(format!(
            "app.js has {fetches} `fetch(` calls; the three inside the api, sportReq and quietGet helpers are the only ones this test knows. A new one must go through a helper, or teach the test."
        ));
    }
    Ok(out)
}

/// The calls whose method and path no table entry covers (`app/api` paths also match their
/// `/api` pass-through).
fn unmatched(calls: &[(String, String)], table: &BTreeSet<(String, String)>) -> BTreeSet<String> {
    let mut missing = BTreeSet::new();
    for (method, path) in calls {
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
    missing
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
    let calls = web_calls_in(APP_JS).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        calls.len() > 40,
        "read only {} calls from app.js: {calls:?}",
        calls.len()
    );
    let missing = unmatched(&calls, &entries());
    assert!(
        missing.is_empty(),
        "the web app calls paths with no parity entry: {missing:?}{HOW_TO}"
    );
}

#[test]
fn the_web_call_reader_cannot_be_walked_around() {
    let table = entries();
    // Boilerplate the reader needs to be satisfied by the three known fetch helpers.
    let fetches = "fetch('/app/api'); fetch('/app/api'); fetch('/app/api');";
    let run = |body: &str| web_calls_in(&format!("{fetches}\n{body}"));
    // A space or a newline between the helper and its parenthesis is still a call.
    for src in [
        "await api ('/v1/nonexistent-demo');",
        "await api\n  ('/v1/nonexistent-demo');",
        "await api\t(\n'/v1/nonexistent-demo');",
    ] {
        let calls = run(src).unwrap();
        assert_eq!(calls.len(), 1, "{src}");
        assert_eq!(unmatched(&calls, &table).len(), 1, "{src}");
    }
    // A computed segment stands for an id: it does not match a literal route such as health.
    let calls = run("await api(`/v1/${name}`);").unwrap();
    assert_eq!(calls, [("GET".to_string(), "/app/api/v1/{}".to_string())]);
    assert_eq!(unmatched(&calls, &table).len(), 1);
    let calls = run("await api(`/v1/library/${x}`, { method: 'POST' });").unwrap();
    assert!(
        !unmatched(&calls, &table).is_empty(),
        "no POST on library/movies"
    );
    // The known computed names are resolved explicitly, and still need entries.
    let calls = run("await api(`/v1/${P.kind}/${id}`);").unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        unmatched(&calls, &table).is_empty(),
        "movie and show pages exist"
    );
    // The computed name must be one the test knows by its exact expression; `section` is not.
    assert!(run("await api(`/v1/${section}/${id}`);").is_err());
    let calls = run("await api(`/v1/library/${S.kind}?${qs}`);").unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        unmatched(&calls, &table).is_empty(),
        "movies and series lists exist"
    );
    // A `fetch` with whitespace or a newline before its parenthesis is counted too.
    assert!(run("fetch\n('/app/api/x');").is_err());
    assert!(run("fetch ('/app/api/x');").is_err());
    // Aliasing a helper or a stray fetch would hide calls, so it is refused.
    assert!(run("const a = api; a('/v1/health');").is_err());
    assert!(web_calls_in("fetch('/x');").is_err());
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

// ------------------------------------------------------------------ route registration guard

/// Files that register routes of their own, none of them under `/api/v1`, with why they may.
const OTHER_ROUTERS: &[(&str, &str)] = &[
    (
        "src/webapp.rs",
        "the web app's pages and its /app/api routes (checked against PARITY below)",
    ),
    (
        "src/wizard.rs",
        "the setup wizard's own local server, a different program from the API",
    ),
    (
        "src/oauth.rs",
        "the OAuth sign-in endpoints of the remote MCP mount",
    ),
    ("src/mcp.rs", "the /mcp mount"),
    ("src/plex.rs", "a stand-in Plex server inside unit tests"),
    (
        "src/selfupdate.rs",
        "a stand-in release server inside unit tests",
    ),
];

/// Registrations in the other routers whose path is not a string literal, by exact text, with
/// nothing to say about `/api/v1`: none today.
const KNOWN_COMPUTED: &[&str] = &[];

/// The text of a plain (`"..."`) or raw (`r"..."`, `r#"..."#`) string literal.
fn string_literal(s: &str) -> Option<&str> {
    let raw = s.strip_prefix('r').unwrap_or(s);
    let hashes = raw.len() - raw.trim_start_matches('#').len();
    let body = raw[hashes..].strip_prefix('"')?;
    let end = format!("\"{}", "#".repeat(hashes));
    let body = body.strip_suffix(end.as_str())?;
    (s.starts_with('r') || hashes == 0).then_some(body)
}

/// What `src/api.rs` may pass to a registration call: the folds over the shared route lists
/// and the merges of whole routers that are not API routes.
const API_RS_ARGS: &[&str] = &["path, handlers", "protected", "api", "web"];

/// Every way a route could be registered without going through the shared lists in
/// `src/api.rs`, for one source file.
fn registration_violations(file: &str, src: &str, parity_paths: &BTreeSet<String>) -> Vec<String> {
    let mut out = vec![];
    let re =
        regex::Regex::new(r"\.(route|route_service|nest|nest_service|merge|fallback_service)\(")
            .unwrap();
    for m in re.captures_iter(src) {
        let whole = m.get(0).unwrap();
        let args = call_args(src, whole.end() - 1).unwrap_or_default().trim();
        let first = split_top(args, ',')[0].trim();
        let literal = string_literal(first);
        let what = format!("{file}: .{}({:.50})", &m[1], args.replace('\n', " "));
        if file == "src/api.rs" {
            if !API_RS_ARGS.contains(&args) {
                out.push(format!(
                    "{what}: routes of the API are registered only through `protected_routes` / `public_routes` in src/api.rs"
                ));
            }
        } else if let Some(p) = literal {
            if p.starts_with("/api/v1") {
                out.push(format!(
                    "{what}: an /api/v1 route outside src/api.rs's lists"
                ));
            } else if p.starts_with("/app/api/")
                && p != "/app/api/v1/{*rest}"
                && !parity_paths.contains(p)
            {
                out.push(format!("{what}: a /app/api route with no parity entry"));
            } else if !OTHER_ROUTERS.iter().any(|(f, _)| *f == file) {
                out.push(format!(
                    "{what}: this file is not one of the known routers; add it to OTHER_ROUTERS with a reason, or move the route into the lists"
                ));
            }
        } else if &m[1] != "merge" && !KNOWN_COMPUTED.contains(&first) {
            out.push(format!(
                "{what}: a route registered with a path that is not a string literal (list its exact text in KNOWN_COMPUTED if it is not an API route)"
            ));
        }
    }
    out
}

fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn routes_are_registered_only_through_the_shared_lists() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![];
    rust_sources(&root.join("src"), &mut files);
    assert!(files.len() > 30, "found {} sources", files.len());
    let parity: BTreeSet<String> = PARITY.iter().map(|(_, p, _)| p.to_string()).collect();
    let mut bad = vec![];
    for f in files {
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        bad.extend(registration_violations(
            &rel,
            &std::fs::read_to_string(&f).unwrap(),
            &parity,
        ));
    }
    assert!(
        bad.is_empty(),
        "routes registered outside the shared route lists: {bad:#?}\n\nAdd an API route to `protected_routes` (or `public_routes`) in src/api.rs so the parity test sees it; see AGENTS.md."
    );
}

#[test]
fn the_registration_guard_trips_on_a_route_added_outside_the_lists() {
    let parity: BTreeSet<String> = PARITY.iter().map(|(_, p, _)| p.to_string()).collect();
    let check = |file: &str, src: &str| registration_violations(file, src, &parity);
    // The shape of the real router passes.
    assert!(
        check(
            "src/api.rs",
            "x.fold(Router::new(), |r, (path, handlers)| r.route(path, handlers)).merge(protected)"
        )
        .is_empty()
    );
    // A route appended in `router()`, a nested or merged router, and a computed path trip.
    for src in [
        r#"Router::new().route("/api/v1/sneaky", get(h))"#,
        r#"router.route(
            "/api/v1/sneaky",
            get(h),
        )"#,
        "router.nest(\"/api/v1/x\", sub)",
        "router.merge(extra_api)",
        "router.route(some_path, get(h))",
    ] {
        assert!(!check("src/api.rs", src).is_empty(), "{src}");
    }
    // Another file may not register an API route, nor a /app/api route that has no entry, and
    // a file that is not a known router may not register anything.
    assert!(!check("src/webapp.rs", r#".route("/api/v1/sneaky", get(h))"#).is_empty());
    assert!(!check("src/webapp.rs", r#".route("/app/api/sneaky", get(h))"#).is_empty());
    assert!(check("src/webapp.rs", r#".route("/app/api/me", get(h))"#).is_empty());
    assert!(!check("src/new_module.rs", r#".route("/thing", get(h))"#).is_empty());
    // A raw string is a literal like any other, and a computed path is refused even in an
    // allowlisted file.
    assert!(!check("src/webapp.rs", r#".route(r"/api/v1/sneaky", get(h))"#).is_empty());
    assert!(!check("src/webapp.rs", r##".route(r#"/api/v1/sneaky"#, get(h))"##).is_empty());
    assert!(!check("src/webapp.rs", ".route(some_path, get(h))").is_empty());
    assert!(!check("src/webapp.rs", r#".route(&format!("/api/{v}/x"), get(h))"#).is_empty());
}
