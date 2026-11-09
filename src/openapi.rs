//! OpenAPI 3.1 document. Enumerations are generated from the same constants the code uses;
//! a test checks that every route is documented.

use crate::decision::REJECTION_CODES;
use crate::error::ALL_ERROR_CODES;
use crate::plan::{ALL_PLAN_STATES, ALL_UNIT_STATUSES, PLAN_BLOCKER_CODES};
use serde_json::{Value, json};

const MEDIA_ID_PATTERN: &str = r"^(movie:tmdb:[1-9][0-9]{0,9}|series:(tvdb|tvmaze):[1-9][0-9]{0,9}|season:(tvdb|tvmaze):[1-9][0-9]{0,9}:[1-9][0-9]{0,2}|episode:(tvdb|tvmaze):[1-9][0-9]{0,9}:[1-9][0-9]{0,2}:[1-9][0-9]{0,3})$";

fn r(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

fn ok(schema: Value, description: &str) -> Value {
    json!({ "description": description, "content": { "application/json": { "schema": schema } } })
}

fn errors(codes: &[u16]) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    for c in codes {
        m.insert(
            c.to_string(),
            json!({ "$ref": format!("#/components/responses/E{c}") }),
        );
    }
    m
}

fn op(summary: &str, params: Value, responses: Vec<(&str, Value)>, errs: &[u16]) -> Value {
    let mut resp = errors(errs);
    for (code, v) in responses {
        resp.insert(code.into(), v);
    }
    json!({ "summary": summary, "parameters": params, "responses": resp })
}

fn q(name: &str, schema: Value, description: &str, required: bool) -> Value {
    json!({ "name": name, "in": "query", "required": required, "schema": schema, "description": description })
}

pub fn document() -> Value {
    let error_codes: Vec<Value> = ALL_ERROR_CODES
        .iter()
        .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
        .collect();
    let plan_states: Vec<&str> = ALL_PLAN_STATES.iter().map(|s| s.as_str()).collect();
    let rejection_codes: Vec<&str> = REJECTION_CODES
        .iter()
        .chain(PLAN_BLOCKER_CODES)
        .copied()
        .collect();
    let media_param =
        json!({ "name": "media_id", "in": "path", "required": true, "schema": r("MediaId") });
    let ownership_state = q(
        "state",
        json!({ "type": "string", "enum": ["adopted", "legacy_managed", "observed", "released"] }),
        "only this acquisition state",
        false,
    );
    let plan_param = json!({ "name": "plan_id", "in": "path", "required": true, "schema": { "type": "string", "pattern": "^pln_[0-9a-f]{24}$" } });
    let paging = vec![
        q(
            "limit",
            json!({ "type": "integer", "minimum": 1, "maximum": 100, "default": 50 }),
            "page size",
            false,
        ),
        q(
            "offset",
            json!({ "type": "integer", "minimum": 0, "maximum": 1000000, "default": 0 }),
            "page offset",
            false,
        ),
    ];
    let ownership_params = json!([ownership_state, paging[0].clone(), paging[1].clone()]);
    let mut library_params = vec![q(
        "q",
        json!({ "type": "string", "maxLength": 100 }),
        "folded title/alias search",
        false,
    )];
    library_params.extend(paging.clone());
    let mut plan_list_params = vec![
        q(
            "state",
            json!({ "type": "string", "enum": plan_states }),
            "filter by state",
            false,
        ),
        q("target", r("MediaId"), "filter by canonical target", false),
    ];
    plan_list_params.extend(paging.clone());

    let mut responses = serde_json::Map::new();
    for (c, d) in [
        (400, "Invalid request, query, JSON or media id"),
        (401, "Missing or invalid bearer token"),
        (404, "Not found / metadata unavailable"),
        (405, "Method not allowed"),
        (
            409,
            "Conflict: idempotency key reuse, plan not active, execution disabled or not configured, already queued, blocklisted, foreign torrent, download state, seed criteria unmet, cleanup blocked",
        ),
        (413, "Body too large"),
        (415, "Content-Type must be application/json"),
        (422, "Body failed validation / quality profile required"),
        (502, "Upstream auth failure, outage or unusable response"),
        (
            503,
            "Overloaded, deadline exceeded, not imported, provider not configured or rate limited",
        ),
        (500, "Internal error (details in the server log only)"),
        (504, "Upstream timeout"),
    ] {
        responses.insert(format!("E{c}"), ok(r("Error"), d));
    }

    let mut create_responses = errors(&[400, 401, 404, 409, 413, 415, 422, 502, 503, 504]);
    create_responses.insert("201".into(), ok(r("PlanEnvelope"), "created"));
    create_responses.insert(
        "200".into(),
        ok(
            r("PlanEnvelope"),
            "existing active plan (or idempotent replay)",
        ),
    );
    let mut paths = paths(
        media_param.clone(),
        plan_param,
        library_params,
        plan_list_params,
        Value::Object(create_responses),
        paging,
    );
    if let Some(m) = paths.as_object_mut() {
        m.insert("/api/v1/ownership".into(), json!({ "get": op("Acquisition ownership: adopted, legacy_managed, observed and released items with each series' effective scope and the native flags of the latest import, plus the native handoff steps (this server never contacts Sonarr/Radarr)", ownership_params.clone(), vec![("200", ok(json!({ "type": "object" }), "ownership"))], &[400, 401, 503]) }));
        m.insert("/api/v1/status/{media_id}".into(), json!({ "get": op("Live status of a movie, series, season or episode: acquisition state, per-season completeness from the live episode list (owned, queued, missing, unaired), per-episode state with download progress (season/episode scopes), downloads of the last 7 days, requests and attention items", json!([media_param.clone()]), vec![("200", ok(json!({ "type": "object" }), "status"))], &[400, 401, 404, 503]) }));
        m.insert("/api/v1/attention".into(), json!({ "get": op("Items needing attention (bounded): metadata numbering holds, blocked imports, failures of the last 7 days, requests retrying 3+ times or expired in the last 14 days", json!([]), vec![("200", ok(json!({ "type": "object" }), "attention"))], &[401, 503]) }));
        m.insert("/api/v1/feed".into(), json!({ "get": op("What a person cares about, in plain words (the web app's home): downloads that need them, that are on their way or arrived in the last 7 days, titles Arr is still looking for, and episodes coming in 14 days. Catalog and state only, no network calls", json!([]), vec![("200", ok(json!({ "type": "object", "required": ["generated_at", "movies_enabled", "needs_you", "on_its_way", "arrived", "looking", "coming_up"] }), "feed"))], &[401, 503]) }));
        m.insert("/api/v1/ideas".into(), json!({ "get": op("Popular shows with a new episode in the last 7 days (TVmaze web schedule, cached 6 hours); status ok, unavailable or disabled", json!([]), vec![("200", ok(json!({ "type": "object", "required": ["status", "items"] }), "ideas"))], &[401]) }));
        m.insert("/api/v1/follow".into(), json!({ "post": with_body(op("Get a series' new episodes as they air (adds the series with nothing else monitored if it is new), or stop doing that. Episodes that have aired and seasons someone chose keep their monitoring", json!([]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "following"] }), "the series id and whether it is followed"))], &[400, 401, 404, 409, 413, 415, 422, 502, 503, 504]), json!({ "type": "object", "additionalProperties": false, "required": ["media_id", "on"], "properties": { "media_id": { "type": "string", "description": "a series id" }, "on": { "type": "boolean" }, "level": { "type": "string", "enum": ["smaller", "standard", "best"], "description": "the quality the person picked, applied to the show" } } }), false) }));
        m.insert("/api/v1/stop".into(), json!({ "post": with_body(op("Arr stops looking for a movie, season or episode: it is unmonitored, its retrying request is cancelled, and downloads lying entirely inside it that have not finished are cancelled with their download data. Files already in the library are never touched; a download that is finished and being added is left to finish", json!([]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "stopped", "cancelled", "finishing"] }), "what was stopped"))], &[400, 401, 404, 409, 413, 415, 422, 502, 503, 504]), json!({ "type": "object", "additionalProperties": false, "required": ["media_id"], "properties": { "media_id": { "type": "string", "description": "a movie, season or episode id" } } }), false) }));
        m.insert("/api/v1/show/{media_id}".into(), json!({ "get": op("A show's page: details, whether new episodes are followed, and each season with its counts and one-word state (in_plex, partly, on_its_way, wanted, upcoming, none). Works for shows outside the catalog (TVmaze)", json!([media_param.clone()]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "title", "following", "seasons"] }), "show"))], &[400, 401, 404, 409, 502, 503, 504]) }));
        m.insert("/api/v1/show/{media_id}/season/{n}".into(), json!({ "get": op("The episodes of one season of a show with each one's state and download progress", json!([media_param.clone(), json!({ "name": "n", "in": "path", "required": true, "schema": { "type": "integer", "minimum": 1 } })]), vec![("200", ok(json!({ "type": "object", "required": ["number", "episodes"] }), "season"))], &[400, 401, 404, 409, 502, 503, 504]) }));
        m.insert("/api/v1/movie/{media_id}".into(), json!({ "get": op("A movie's page: details, summary when Plex or TMDB has one, and its state (in_plex, on_its_way, waiting, wanted, none)", json!([media_param.clone()]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "title", "state"] }), "movie"))], &[400, 401, 404, 502, 503, 504]) }));
        m.insert("/api/v1/quality".into(), json!({ "post": with_body(op("Sets the quality level (smaller = 720p, standard = 1080p, best = 4K, or 1080p when there is no 4K) of a movie or show Arr already has. It changes what Arr looks for next; files already in the library are never replaced or deleted. 404 when Arr does not have the title yet (send the level with quick-add or follow instead)", json!([]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "level"] }), "the title and its level"))], &[400, 401, 404, 409, 413, 415, 422, 502, 503, 504]), json!({ "type": "object", "additionalProperties": false, "required": ["media_id", "level"], "properties": { "media_id": { "type": "string", "description": "a movie or series id" }, "level": { "type": "string", "enum": ["smaller", "standard", "best"] } } }), false) }));
        m.insert("/api/v1/copies/{media_id}".into(), json!({ "get": op("The copies Arr found for a movie, season or episode, in plain words: {recommended, copies: [{id, resolution, source (Web, Blu-ray, TV), size_bytes, speed (fast, ok, slow), recommended}], skipped: [{reason, count}]}. Copies that would replace a file already owned are never listed. level (smaller, standard, best) applies to a title Arr does not have yet", json!([media_param.clone(), json!({ "name": "level", "in": "query", "required": false, "schema": { "type": "string", "enum": ["smaller", "standard", "best"] } })]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "recommended", "copies", "skipped"] }), "the copies"))], &[400, 401, 404, 409, 422, 502, 503, 504]) }));
        m.insert("/api/v1/copies/{media_id}/get".into(), json!({ "post": with_body(op("Downloads the copy a person picked from the list (as POST /grabs, with the same checks). Arr must be getting the title (409 not_managed when it was stopped); a title Arr does not have yet is added first without a search of its own", json!([media_param.clone()]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "title", "detail", "outcome", "until", "message"] }), "what happened, in plain English"))], &[400, 401, 404, 409, 413, 415, 422, 502, 503, 504]), json!({ "type": "object", "additionalProperties": false, "required": ["id"], "properties": { "id": { "type": "string", "description": "a copy's id from the list" }, "level": { "type": "string", "enum": ["smaller", "standard", "best"] } } }), false) }));
        m.insert("/api/v1/quick-add".into(), json!({ "post": with_body(op("One-tap add of a movie (as POST /library), a show (follow new episodes, and get the latest aired season), or one season or episode (as POST /library for that scope); outcome is on_its_way, looking, waiting, in_plex, following or movies_off", json!([]), vec![("200", ok(json!({ "type": "object", "required": ["media_id", "title", "detail", "outcome", "until", "message"] }), "what happened, in plain English"))], &[400, 401, 404, 409, 413, 415, 422, 502, 503, 504]), json!({ "type": "object", "additionalProperties": false, "required": ["media_id"], "properties": { "media_id": { "type": "string", "description": "a movie, series, season or episode id" }, "level": { "type": "string", "enum": ["smaller", "standard", "best"], "description": "the quality the person picked, applied to the title" } } }), false) }));
    }
    let schemas = schemas(error_codes, plan_states, rejection_codes);
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "arr",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Movie/TV API. Policy from managed setup (POST /api/v1/setup; standalone) or from imported Radarr/Sonarr/Prowlarr snapshots. Release search and RSS through native indexers (The Pirate Bay's JSON API, Torznab) or, optionally, Prowlarr: one backend, never both, no fallback. Shadow by default; with [execution] enabled it uses the configured embedded BitTorrent engine or qBittorrent backend, imports completed downloads into the library and follows the cleanup policy.",
            "x-mcp": "with [mcp] enabled, POST /mcp serves MCP over Streamable HTTP (rmcp, stateless JSON responses) with its own static bearer token (not the API token), Host/Origin validation and bounded bodies; the six MCP tools wrap this API. No OAuth; see docs/MCP.md",
        },
        "servers": [{ "url": "http://127.0.0.1:8787" }],
        "security": [{ "bearer": [] }],
        "paths": paths,
        "components": {
            "securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } },
            "responses": Value::Object(responses),
            "schemas": schemas,
        },
    })
}

fn obj(props: Value) -> Value {
    json!({ "type": "object", "properties": props })
}

fn body(schema: Value) -> Value {
    json!({ "required": true, "content": { "application/json": { "schema": schema } } })
}

fn with_body(mut op: Value, schema: Value, optional: bool) -> Value {
    let mut b = body(schema);
    if optional {
        b["required"] = json!(false);
    }
    op["requestBody"] = b;
    op
}

fn path_id(name: &str, pattern: &str) -> Value {
    json!({ "name": name, "in": "path", "required": true, "schema": { "type": "string", "pattern": pattern } })
}

/// Every documented path.
fn paths(
    media_param: Value,
    plan_param: Value,
    library_params: Vec<Value>,
    plan_list_params: Vec<Value>,
    create_responses: Value,
    paging: Vec<Value>,
) -> Value {
    let mut all = core_paths(
        media_param,
        plan_param,
        library_params,
        plan_list_params,
        create_responses,
        paging,
    );
    if let (Value::Object(m), Value::Object(extra)) = (&mut all, setup_paths()) {
        m.extend(extra);
    }
    all
}

/// Standalone setup paths.
fn setup_paths() -> Value {
    json!({
        "/api/v1/setup": {
            "get": op("Standalone setup status: active settings (origin managed_setup or native_snapshot), search/download backends, library, Plex and metadata configuration, next steps (nothing is contacted)", json!([]), vec![("200", ok(json!({ "type": "object" }), "setup status"))], &[401]),
            "post": with_body(op("Create or replace managed settings (profiles, size limits, release/import/download policy, per-indexer policy with explicit seed goals, root folders) with an empty catalog, and activate them in this process. Policy only: connections and credentials live in config.toml. Never enables execution or queues searches; refuses to replace a native snapshot import or anything the catalog or active downloads use", json!([]), vec![("201", ok(r("SetupResult"), "created and activated")), ("200", ok(r("SetupResult"), "dry_run, or unchanged (already active)"))], &[401, 409, 413, 415, 422]), r("SetupRequest"), true),
        },
        "/api/v1/setup/plex": { "get": op("Read-only Plex discovery: library sections and the configured section/path mapping check", json!([]), vec![("200", ok(json!({ "type": "object", "properties": { "sections": { "type": "array" }, "mapping": { "type": "array" } } }), "sections and mapping"))], &[401, 502, 503, 504]) },
        "/api/v1/setup/plex/bootstrap": {
            "get": op("Plex library bootstrap runs (cursor, counts, bounded issues)", json!([]), vec![("200", ok(json!({ "type": "object", "properties": { "runs": { "type": "array", "items": r("BootstrapRun") } } }), "runs"))], &[401]),
            "post": with_body(op("One bounded, resumable batch per media kind of the read-only Plex library bootstrap: exact provider ids (TMDB movies, TVDB shows) and validated mapped local files become owned, unmonitored catalog entries; nothing is searched or downloaded; missing, unmapped or unavailable roots change nothing", json!([]), vec![("200", ok(json!({ "type": "object", "properties": { "runs": { "type": "array", "items": r("BootstrapRun") }, "done": { "type": "boolean" }, "next": { "type": "string" } } }), "batch result"))], &[400, 401, 409, 413, 415, 422, 502, 503, 504]), r("BootstrapRequest"), true),
        },
    })
}

fn core_paths(
    media_param: Value,
    plan_param: Value,
    library_params: Vec<Value>,
    plan_list_params: Vec<Value>,
    create_responses: Value,
    paging: Vec<Value>,
) -> Value {
    let grab_param = path_id("grab_id", "^grb_[0-9a-f]{24}$");
    let command_param = path_id("command_id", "^cmd_[0-9a-f]{24}$");
    let grab_env = ok(obj(json!({ "grab": r("Grab") })), "download");
    let mut queue_params = vec![q(
        "all",
        json!({ "type": "boolean" }),
        "also downloads updated in the last 7 days",
        false,
    )];
    queue_params.extend(paging.clone());
    let mut history_params = vec![
        q(
            "media_id",
            r("MediaId"),
            "filter by item (seasons/episodes map to their series)",
            false,
        ),
        q(
            "kind",
            json!({ "type": "string", "pattern": "^[a-z_]{1,40}$" }),
            "event kind, e.g. grabbed, file_imported, download_removed",
            false,
        ),
    ];
    history_params.extend(paging.clone());
    let mut wanted_params = vec![q("media_id", r("MediaId"), "filter by item", false)];
    wanted_params.extend(paging.clone());
    json!({
        "/api/v1/health": { "get": json!({ "summary": "Liveness (no auth)", "security": [], "responses": { "200": ok(json!({ "type": "object" }), "alive") } }) },
        "/api/v1/openapi.json": { "get": json!({ "summary": "This document (no auth)", "security": [], "responses": { "200": ok(json!({ "type": "object" }), "OpenAPI 3.1") } }) },
        "/api/v1/readiness": { "get": op("Service readiness, execution gates and evidence-derived migration readiness per stage (limited, coexistence, broad)", json!([]), vec![("200", ok(r("Readiness"), "ready")), ("503", ok(r("Readiness"), "not ready (body has the same shape)"))], &[401]) },
        "/api/v1/capabilities": { "get": op("Capability matrix: implemented / configured / verified", json!([]), vec![("200", ok(r("Capabilities"), "capabilities"))], &[401]) },
        "/api/v1/diagnostics": { "get": op("Read-only checks of the configured download backend (embedded engine state, peer counts and transfer speeds, or qBittorrent version, categories and share limits), ffprobe, library roots, execution blockers, downloads and RSS", json!([]), vec![("200", ok(json!({ "type": "object" }), "diagnostics"))], &[401, 500]) },
        "/api/v1/settings": { "get": op("Normalised imported settings (no secrets)", json!([]), vec![("200", ok(json!({ "type": "object" }), "settings"))], &[401, 503]) },
        "/api/v1/migration-report": { "get": op("Migration/capability report of the active import", json!([]), vec![("200", ok(json!({ "type": "object" }), "report"))], &[401, 503]) },
        "/api/v1/library": { "post": with_body(op("Add (or re-manage) a movie, a series, or one season/episode (scoped one-call add), scan its folder, then search (default). Profile and root are chosen automatically when unambiguous; movies outside TMDB need explicit metadata", json!([]), vec![("201", ok(json!({ "type": "object" }), "added; includes scan (status of the item folder), adoption (mode, acquisition state, native app, whether the native handoff was confirmed), acquisition_blockers for movies (metadata that would stop any release from being grabbed: runtime_unknown, original_language_unknown, release_date_unknown, not_released_yet, availability_unsupported, profile_unknown), and the plan (movie, season, episode) or the queued search command (whole series). Coexistence mode: 409 native_handoff_required for a native title without native_handoff_confirmed, 409 unsupported_scope for an imported title this server cannot acquire (anime/daily, scene numbering, identity conflict, unsupported availability mode); series responses list native_retained scopes (natively monitored specials)"))], &[400, 401, 404, 409, 413, 415, 422, 502, 503, 504]), r("AddMedia"), false) },
        "/api/v1/library/movies": { "get": op("Managed movies", json!(library_params), vec![("200", ok(r("Page"), "movies"))], &[400, 401, 503]) },
        "/api/v1/library/series": { "get": op("Managed series", json!(library_params), vec![("200", ok(r("Page"), "series"))], &[400, 401, 503]) },
        "/api/v1/library/{media_id}": {
            "patch": with_body(op("Monitor or unmonitor a movie, series, season or episode", json!([media_param.clone()]), vec![("200", ok(json!({ "type": "object" }), "updated"))], &[400, 401, 404, 413, 415, 422, 503]), obj(json!({ "monitored": { "type": "boolean" }, "native_handoff_confirmed": { "type": "boolean", "default": false, "description": "coexistence mode: required to monitor a native title's scope here, unless a confirmed handoff already covers it" } })), false),
            "delete": op("Stop managing a movie or series (files, history and evidence are kept)", json!([media_param.clone()]), vec![("200", ok(json!({ "type": "object" }), "removed from management"))], &[400, 401, 404, 409, 503]),
        },
        "/api/v1/library/roots": { "get": op("Per-root scan status (ok / unavailable / incomplete / empty_suspect)", json!([]), vec![("200", ok(json!({ "type": "object" }), "roots"))], &[401]) },
        "/api/v1/wanted": { "get": op("Monitored, aired, missing items not being downloaded (searched only on request; routine acquisition is RSS)", json!(wanted_params), vec![("200", ok(r("Page"), "wanted"))], &[400, 401, 503]) },
        "/api/v1/calendar": { "get": op("Episodes airing in a window with their status", json!([
            q("start", json!({ "type": "string", "format": "date-time" }), "default: now - 1 day", false),
            q("end", json!({ "type": "string", "format": "date-time" }), "default: now + 14 days; at most 92 days after start", false),
        ]), vec![("200", ok(json!({ "type": "object" }), "calendar"))], &[400, 401, 503]) },
        "/api/v1/discover": { "get": op("Discover movies/series in the catalog and optional TVmaze/TMDB", json!([
            q("kind", json!({ "type": "string", "enum": ["movie", "series"] }), "media kind", true),
            q("q", json!({ "type": "string", "minLength": 1, "maxLength": 100 }), "title query", true),
            q("year", json!({ "type": "integer" }), "optional year filter", false),
        ]), vec![("200", ok(r("Discover"), "results with per-provider status"))], &[400, 401]) },
        "/api/v1/media/{media_id}": { "get": op("Resolve a target: titles, years, episodes and ownership with provenance", json!([media_param.clone()]), vec![("200", ok(json!({ "type": "object" }), "resolved target"))], &[400, 401, 404, 502, 504]) },
        "/api/v1/media/{media_id}/releases": { "get": op("Search Prowlarr and evaluate every release (interactive indexers); approved ones become grabbable", json!([media_param, q("refresh", json!({ "type": "boolean" }), "bypass the search cache", false)]), vec![("200", ok(r("ReleaseSearch"), "evaluated releases; outcome is what was observed (results / observed_empty), completeness says how much that covers"))], &[400, 401, 404, 422, 502, 503, 504]) },
        "/api/v1/plans": {
            "post": {
                "summary": "Create (or return the existing) plan for a movie, season or episode; execute it with execute = true",
                "parameters": [{ "name": "Idempotency-Key", "in": "header", "required": false, "schema": { "type": "string", "pattern": "^[A-Za-z0-9._:-]{1,128}$" } }],
                "requestBody": { "required": true, "content": { "application/json": { "schema": r("CreatePlan") } } },
                "responses": create_responses,
            },
            "get": op("List plans", json!(plan_list_params), vec![("200", ok(obj(json!({ "total": { "type": "integer" }, "items": { "type": "array", "items": r("Plan") } })), "plans"))], &[400, 401]),
        },
        "/api/v1/plans/{plan_id}": { "get": op("Get a plan", json!([plan_param.clone()]), vec![("200", ok(obj(json!({ "plan": r("Plan") })), "plan"))], &[401, 404]) },
        "/api/v1/plans/{plan_id}/cancel": { "post": op("Cancel an active plan (local state only)", json!([plan_param.clone()]), vec![("200", ok(obj(json!({ "plan": r("Plan") })), "cancelled"))], &[401, 404, 409]) },
        "/api/v1/plans/{plan_id}/execute": { "post": op("Execute a shadow plan: submit its candidates through the download outbox", json!([plan_param]), vec![("200", ok(obj(json!({ "plan": r("Plan") })), "executed (see execution.grabs and execution.refused)"))], &[401, 404, 409, 500]) },
        "/api/v1/grabs": { "post": with_body(op("Grab one approved release from a recent GET /media/{id}/releases", json!([]), vec![("201", ok(obj(json!({ "grab": r("Grab") })), "queued for submission"))], &[400, 401, 404, 409, 413, 415, 422, 502, 504]), obj(json!({ "media_id": r("MediaId"), "release_ref": { "type": "string", "pattern": "^rel_[0-9a-f]{24}$" } })), false) },
        "/api/v1/queue": { "get": op("Download queue: active downloads, seeding/cleanup-tracked ones, optionally recent history", json!(queue_params), vec![("200", ok(obj(json!({ "total": { "type": "integer" }, "items": { "type": "array", "items": r("Grab") } })), "queue"))], &[400, 401]) },
        "/api/v1/queue/{grab_id}": { "get": op("One download with its import journal and events", json!([grab_param.clone()]), vec![("200", ok(json!({ "type": "object" }), "download"))], &[401, 404]) },
        "/api/v1/queue/{grab_id}/pause": { "post": op("Pause our torrent in the configured download client", json!([grab_param.clone()]), vec![("200", grab_env.clone())], &[401, 404, 409, 502, 504]) },
        "/api/v1/queue/{grab_id}/resume": { "post": op("Resume our torrent in the configured download client", json!([grab_param.clone()]), vec![("200", grab_env.clone())], &[401, 404, 409, 502, 504]) },
        "/api/v1/queue/{grab_id}/cancel": { "post": with_body(op("Cancel a download. Default keeps the torrent and its data (an incomplete torrent is stopped); remove = torrent removes only the torrent entry; remove = torrent_and_data also deletes its tracked download data (never library files; seed criteria must be met unless ignore_seed_criteria)", json!([grab_param.clone()]), vec![("200", grab_env.clone())], &[401, 404, 409, 422, 502, 504]), r("Cancel"), true) },
        "/api/v1/queue/{grab_id}/retry": { "post": op("Retry: re-import, resubmit, re-evaluate cleanup, or search for an alternative (failed)", json!([grab_param]), vec![("200", ok(obj(json!({ "grab": r("Grab"), "action": { "type": "string", "enum": ["reimport", "resubmit", "cleanup", "search"] } })), "retry scheduled"))], &[401, 404, 409]) },
        "/api/v1/history": { "get": op("Event history (grabs, imports, failures, removals, monitoring changes)", json!(history_params), vec![("200", ok(json!({ "type": "object" }), "events"))], &[400, 401]) },
        "/api/v1/blocklist": { "get": op("Blocklisted releases", json!([q("media_id", r("MediaId"), "filter by item", false)]), vec![("200", ok(json!({ "type": "object" }), "blocklist"))], &[400, 401]) },
        "/api/v1/blocklist/{entry_id}": { "delete": op("Remove a blocklist entry", json!([{ "name": "entry_id", "in": "path", "required": true, "schema": { "type": "integer", "minimum": 1 } }]), vec![("200", ok(json!({ "type": "object" }), "removed"))], &[401, 404]) },
        "/api/v1/commands": {
            "post": with_body(op("Queue a worker command (needs [execution] enabled)", json!([]), vec![("202", ok(json!({ "type": "object" }), "queued")), ("200", ok(json!({ "type": "object" }), "an identical command is already pending"))], &[400, 401, 409, 413, 415, 422, 503]), r("CommandRequest"), false),
            "get": op("Recent commands", json!(paging), vec![("200", ok(json!({ "type": "object" }), "commands"))], &[400, 401]),
        },
        "/api/v1/commands/{command_id}": { "get": op("One command with its result", json!([command_param]), vec![("200", ok(json!({ "type": "object" }), "command"))], &[401, 404]) },
        "/api/v1/system/tasks": { "get": op("Scheduled task status (downloads, rss_sync, requests, refresh_metadata, rescan, prune)", json!([]), vec![("200", ok(json!({ "type": "object" }), "tasks"))], &[401]) },
        "/api/v1/requests": { "get": op("Explicit requests that found nothing and are being retried", json!([]), vec![("200", ok(json!({ "type": "object" }), "requests"))], &[401]) },
        "/api/v1/rss": { "get": op("RSS sync: feeds, per-indexer retrieval state and recently seen items (no links)", json!([]), vec![("200", ok(json!({ "type": "object" }), "rss"))], &[401]) },
    })
}

fn schemas(error_codes: Vec<Value>, plan_states: Vec<&str>, rejection_codes: Vec<&str>) -> Value {
    let unit_statuses: Vec<Value> = ALL_UNIT_STATUSES
        .iter()
        .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
        .collect();
    let mut m = serde_json::Map::new();
    for part in [
        json!({ "Error": { "type": "object", "required": ["error"], "properties": { "error": { "type": "object", "required": ["code", "message"], "properties": {
                    "code": r("ErrorCode"), "message": { "type": "string" }, "details": { "type": "object" } } } } } }),
        json!({ "ErrorCode": { "type": "string", "enum": error_codes } }),
        json!({ "MediaId": { "type": "string", "pattern": MEDIA_ID_PATTERN, "examples": ["movie:tmdb:603", "season:tvdb:372264:1", "episode:tvdb:372264:1:3"] } }),
        json!({ "PlanState": { "type": "string", "enum": plan_states, "description": "executed plans: download_queued (in the outbox, not yet confirmed by the download client) -> download_started (confirmed) -> imported, or download_failed (every grab ended before confirmation); in_progress = everything wanted is owned or downloading" } }),
        json!({ "UnitStatus": { "type": "string", "enum": unit_statuses } }),
        json!({ "RejectionCode": { "type": "string", "enum": rejection_codes } }),
        json!({ "Rejection": { "type": "object", "properties": { "code": r("RejectionCode"), "message": { "type": "string" } } } }),
        json!({ "Decision": { "type": "object", "properties": {
                    "approved": { "type": "boolean" }, "rejections": { "type": "array", "items": r("Rejection") }, "notes": { "type": "array", "items": { "type": "string" } },
                    "covered_episodes": { "type": "array", "items": { "type": "integer" } }, "owned_overlap": { "type": "array", "items": { "type": "integer" } }, "full_season": { "type": "boolean" },
                    "replacement": { "type": "boolean", "description": "approved as a native revision upgrade of owned file(s)" } } } }),
        json!({ "Query": { "type": "object", "properties": {
                    "query": { "type": "string" }, "categories": { "type": "array", "items": { "type": "integer" } }, "indexer_ids": { "type": "array", "items": { "type": "integer" } },
                    "status": { "type": "string", "enum": ["complete", "truncated", "partial", "failed", "unavailable", "ok"],
                                "description": "retrieval status: complete (all pages), truncated (stopped at the page bound with a full last page), partial (a later page failed), failed, unavailable (a native indexer the settings name cannot be searched; nothing was sent). `ok` appears only in plans recorded by earlier builds." },
                    "error": { "type": ["object", "null"] }, "pages_fetched": { "type": "integer" },
                    "raw_results": { "type": "integer" }, "unique_results": { "type": "integer" }, "truncated_by_page_limit": { "type": "boolean" },
                    "from_cache": { "type": "boolean" }, "fetched_at": { "type": ["string", "null"] },
                    "provider": { "type": "string", "enum": ["native"], "description": "native indexer answer (absent for Prowlarr)" },
                    "indexer": { "type": "string", "description": "the native indexer's name" },
                    "dropped_results": { "type": "integer", "description": "results the indexer returned that were dropped (unrelated category, filters, unusable or credential-leaking links)" },
                    "empty": { "type": "string", "enum": ["confirmed", "unconfirmed"], "description": "native answers without results: confirmed = the indexer said 'no results'" } } } }),
        json!({ "Release": { "type": "object", "description": "No download/magnet/info URLs, GUIDs or info hashes are ever included.", "properties": {
                    "rank": { "type": "integer" }, "release_ref": { "type": "string", "pattern": "^rel_[0-9a-f]{24}$" }, "title": { "type": "string" },
                    "indexer": { "type": "object" }, "protocol": { "type": ["string", "null"] }, "size_bytes": { "type": ["integer", "null"] },
                    "seeders": { "type": ["integer", "null"] }, "leechers": { "type": ["integer", "null"] }, "published_at": { "type": ["string", "null"] },
                    "categories": { "type": "array", "items": { "type": "integer" } }, "found_by": { "type": "array", "items": { "type": "string" } },
                    "parsed": { "type": "object" }, "decision": r("Decision") } } }),
        json!({ "ReleaseSearch": { "type": "object", "properties": {
                    "target": r("MediaId"), "requested": r("MediaId"), "profile": { "type": ["string", "null"] }, "policy_blockers": { "type": "array", "items": r("Rejection") },
                    "outcome": { "type": "string", "enum": ["results", "observed_empty"], "description": "what was observed; observed_empty is not proof that no release exists" },
                    "completeness": r("Completeness"), "counts": { "type": "object" },
                    "queries": { "type": "array", "items": r("Query") }, "releases": { "type": "array", "items": r("Release") },
                    "releases_omitted": { "type": "integer", "description": "evaluated releases beyond the response cap (not a retrieval gap)" },
                    "uncertainty": { "type": "array", "items": { "type": "string" } } } } }),
        json!({ "Completeness": { "type": "object", "required": ["retrieval", "provider", "issues", "note"], "properties": {
                    "retrieval": { "type": "string", "enum": ["complete", "incomplete", "unknown"],
                                   "description": "transport/pagination coverage observed by this server; unknown only for plans recorded by earlier builds" },
                    "provider": { "type": "string", "enum": ["unverified", "complete", "incomplete"],
                                  "description": "per-indexer coverage: unverified with Prowlarr (its search reports none); with native indexers complete when every indexer the settings name answered in full (an explicit 'no results' counts), else incomplete. Never a claim that the indexers hold everything that exists" },
                    "issues": { "type": "array", "items": { "type": "object", "properties": {
                        "query": { "type": "string" }, "kind": { "type": "string", "enum": ["page_limit", "page_failed", "query_failed", "indexer_unavailable", "empty_unconfirmed"] }, "detail": { "type": "string" }, "indexer": { "type": "string" } } } },
                    "note": { "type": "string" } } } }),
        json!({ "SetupRequest": { "type": "object", "additionalProperties": false, "properties": {
                    "dry_run": { "type": "boolean", "default": false },
                    "document": { "type": "object", "description": "the managed settings document (config/setup.example.toml as JSON); every key optional. Sections: movies/tv (enabled, root, maximum_size_mb, allow_hardcoded_subs, prefer_propers_and_repacks, revision_upgrades, torrent_delay_minutes, bypass_delay_if_highest_quality, redownload_failed, redownload_failed_interactive, rss_sync_interval_minutes, availability_delay_days, copy_using_hardlinks, minimum_free_space_mb, replace_illegal_characters), profiles[] (id, media, name, tiers, language, unmarked), size_limits[] (media, quality, min/max/preferred_mib_per_minute), download (movie_category, tv_category, initial_state, remove_completed, remove_failed), indexers[] (id, name, movies, tv, movie_categories, tv_categories, priority, minimum_seeders, automatic_search, interactive_search, rss, seed_ratio, seed_time_minutes, season_pack_seed_time_minutes). No URLs or credentials." } } } }),
        json!({ "SetupResult": { "type": "object", "properties": {
                    "status": { "type": "string", "enum": ["created", "unchanged", "dry_run"] }, "import_id": { "type": "string" }, "replaced": { "type": ["string", "null"] },
                    "profiles": { "type": "array" }, "root_folders": { "type": "array" }, "indexers": { "type": "array" }, "download": { "type": "array" },
                    "warnings": { "type": "array", "items": { "type": "string" } }, "execution": { "type": "string" } } } }),
        json!({ "BootstrapRequest": { "type": "object", "additionalProperties": false, "properties": {
                    "media": { "type": "string", "enum": ["movies", "tv", "all"], "default": "all" }, "restart": { "type": "boolean", "default": false },
                    "dry_run": { "type": "boolean", "default": false }, "batch": { "type": "integer", "minimum": 1, "maximum": 200, "description": "items per batch (default 50 movies, 5 shows)" } } } }),
        json!({ "BootstrapRun": { "type": "object", "properties": {
                    "media": { "type": "string", "enum": ["movie", "series"] }, "section": { "type": ["integer", "null"] },
                    "status": { "type": "string", "enum": ["running", "done", "failed"] }, "dry_run": { "type": "boolean" },
                    "offset": { "type": "integer" }, "total": { "type": ["integer", "null"] }, "last_error": { "type": ["string", "null"] },
                    "counts": { "type": "object", "properties": { "items_seen": { "type": "integer" }, "titles_created": { "type": "integer" }, "titles_existing": { "type": "integer" }, "files_recorded": { "type": "integer" }, "files_known": { "type": "integer" }, "episodes_owned": { "type": "integer" }, "unresolved": { "type": "integer" }, "unmapped": { "type": "integer" }, "missing_locally": { "type": "integer" }, "unsupported": { "type": "integer" }, "conflicts": { "type": "integer" }, "metadata_failures": { "type": "integer" } } },
                    "issues": { "type": "array", "items": { "type": "object", "properties": { "kind": { "type": "string", "enum": ["unresolved", "unmapped", "missing_locally", "unsupported", "conflict", "metadata", "note"] }, "title": { "type": "string" }, "rating_key": { "type": "string" }, "detail": { "type": "string" } } } } } } }),
        json!({ "CreatePlan": { "type": "object", "additionalProperties": false, "required": ["media_id"], "properties": {
                    "media_id": r("MediaId"), "quality_profile": { "type": "string", "description": "only for targets outside the catalog when several profiles exist" },
                    "replan": { "type": "boolean", "default": false, "description": "supersede the active plan with a fresh evaluation" },
                    "execute": { "type": "boolean", "default": false, "description": "submit the planned candidates (needs [execution] enabled)" },
                    "revisions": { "type": "boolean", "default": false, "description": "also search owned units for exact-quality higher revisions" } } } }),
        json!({ "AddMedia": { "type": "object", "additionalProperties": false, "required": ["media_id"], "properties": {
                    "media_id": { "type": "string", "description": "movie:tmdb:N, series:tvdb:N / series:tvmaze:N, or season:/episode: ids for a one-call scoped add (a new series then monitors only that scope, no later seasons; an existing series keeps its other choices)" },
                    "monitored": { "type": "boolean", "default": true, "description": "movies and whole series only" },
                    "monitor": { "type": "string", "enum": ["all", "future", "none"], "description": "whole-series episode monitoring" },
                    "quality_profile": { "type": "string" }, "root_folder": { "type": "string", "description": "an imported native root folder path" },
                    "search": { "type": "boolean", "default": true, "description": "search after adding (the item's folder is scanned first); false = monitor only" },
                    "native_handoff_confirmed": { "type": "boolean", "default": false, "description": "coexistence mode, native (imported) titles: the caller confirms native monitoring for exactly this scope is suspended and no native download for it is active. Required to adopt. Recorded per scope (title, season or episode) until the title is released, so requests inside a confirmed scope do not need it again; a wider scope does. Titles new to the native apps do not need it" },
                    "preserve_monitoring": { "type": "boolean", "default": false, "description": "coexistence mode, a whole imported movie or series: adopt it with its imported monitoring exactly as it is (series, season and episode flags, monitor new seasons; the latest import's native intent, not a fresh native query). Requires search: false and native_handoff_confirmed; monitor, monitored, metadata, quality_profile, root_folder and season/episode ids are refused. A replay changes nothing; natively monitored specials are reported in native_retained (whole_title: false)" },
                    "metadata": { "type": "object", "additionalProperties": false, "required": ["title", "year"], "description": "explicit operator-supplied movie metadata for a known TMDB id when TMDB is not configured; without a release date the movie is not available to grab", "properties": {
                        "title": { "type": "string" }, "year": { "type": "integer" }, "runtime_minutes": { "type": "integer" }, "original_language": { "type": "string" },
                        "alternate_titles": { "type": "array", "items": { "type": "string" } }, "imdb_id": { "type": "string" },
                        "in_cinemas": { "type": "string", "format": "date" }, "physical_release": { "type": "string", "format": "date" }, "digital_release": { "type": "string", "format": "date" } } } } } }),
        json!({ "Cancel": { "type": "object", "additionalProperties": false, "properties": {
                    "blocklist": { "type": "boolean", "default": false }, "remove": { "type": "string", "enum": ["keep", "torrent", "torrent_and_data"], "default": "keep" },
                    "ignore_seed_criteria": { "type": "boolean", "default": false }, "research": { "type": "boolean", "default": false, "description": "search for an alternative now instead of the 24 h cooldown" } } } }),
        json!({ "CommandRequest": { "type": "object", "additionalProperties": false, "required": ["name"], "properties": {
                    "name": { "type": "string", "enum": crate::worker::COMMANDS }, "media_id": r("MediaId") } } }),
        json!({ "Grab": { "type": "object", "description": "Raw info-hashes, links and payloads are never included; grab ids and release refs are the handles.", "properties": {
                    "id": { "type": "string" }, "media_id": r("MediaId"), "target": r("MediaId"), "plan_id": { "type": ["string", "null"] }, "release_ref": { "type": "string" },
                    "title": { "type": "string" }, "client_name": { "type": ["string", "null"] }, "quality": { "type": ["string", "null"] },
                    "revision": { "type": ["object", "null"] }, "origin": { "type": "string", "enum": ["interactive", "automatic"] },
                    "state": { "type": "string", "enum": ["queued", "unconfirmed", "downloading", "completed", "importing", "import_blocked", "imported", "failed", "cancelled", "released"] },
                    "status": { "type": "string", "enum": ["pending_submission", "submitting", "downloading", "paused", "stalled", "fetching_metadata", "client_error", "missing_files", "queued_in_client", "checking", "moving", "import_pending", "importing", "import_blocked", "imported", "seeding", "cleanup_pending", "cleanup_blocked", "cleaned", "failed", "cancelled", "released", "unknown"] },
                    "progress": { "type": ["number", "null"] }, "last_error": { "type": ["string", "null"] }, "failure": { "type": ["string", "null"] }, "retry": { "type": ["string", "null"] },
                    "seed_goal": { "type": "object" },
                    "cleanup": { "type": "object", "properties": { "state": { "type": "string", "enum": ["none", "disabled", "seeding", "pending", "removing", "cleaned", "blocked", "client_removed", "kept"] }, "detail": { "type": ["string", "null"] }, "policy": { "type": "object" } } },
                    "targets": { "type": "array", "items": { "type": "object" } } } } }),
        json!({ "CandidateSummary": { "type": "object", "properties": {
                    "release_ref": { "type": "string" }, "title": { "type": "string" }, "indexer_id": { "type": ["integer", "null"] }, "indexer": { "type": ["string", "null"] },
                    "size_bytes": { "type": ["integer", "null"] }, "seeders": { "type": ["integer", "null"] }, "quality": { "type": "string" }, "revision": { "type": "object", "properties": { "version": { "type": "integer" }, "real": { "type": "integer" } } },
                    "published_at": { "type": ["string", "null"] }, "covers_episodes": { "type": "array", "items": { "type": "integer" } }, "full_season": { "type": "boolean" } } } }),
        json!({ "PlanUnit": { "type": "object", "properties": { "target": r("MediaId"), "label": { "type": "string" }, "status": r("UnitStatus"), "candidate": { "type": ["string", "null"] }, "reason": { "type": "string" } } } }),
        json!({ "Plan": { "type": "object", "properties": {
                    "id": { "type": "string" }, "target": r("MediaId"), "requested_target": r("MediaId"), "title": { "type": "string" }, "state": r("PlanState"),
                    "execution": { "type": "object", "properties": { "mode": { "type": "string", "enum": ["shadow", "executed"] }, "download_submission": { "type": "string", "enum": ["disabled", "enabled", "not_implemented"] }, "submitted": { "type": "boolean", "description": "true only once the download client confirmed a grab (not when it entered the outbox)" }, "confirmed_at": { "type": ["string", "null"] }, "note": { "type": "string" },
                        "grabs": { "type": "array", "items": { "type": "string" } }, "refused": { "type": "array", "items": r("Rejection") }, "executed_at": { "type": ["string", "null"] },
                        "outcome": { "type": "string", "enum": ["queued", "partially_queued", "nothing_queued"], "description": "what executing the plan did; absent for shadow plans and plans executed by earlier builds. nothing_queued: every candidate was refused (see refused) and the plan keeps its shadow state. With partially_queued or nothing_queued, the explicit request stays active (retried with backoff until expiry) while a refused candidate's planned unit is covered by no active download" } } },
                    "created_at": { "type": "string" }, "updated_at": { "type": "string" }, "import_id": { "type": ["string", "null"] }, "catalog_observed_at": { "type": ["string", "null"] },
                    "profile": { "type": ["string", "null"] }, "destination_root": { "type": ["string", "null"] }, "selection": { "type": ["string", "null"], "enum": ["movie", "episode", "episodes", "season_pack", null] },
                    "units": { "type": "array", "items": r("PlanUnit") }, "candidates": { "type": "array", "items": r("CandidateSummary") },
                    "blockers": { "type": "array", "items": r("Rejection") },
                    "search": { "type": ["object", "null"], "properties": {
                        "queries": { "type": "array", "items": r("Query") }, "completeness": r("Completeness"),
                        "releases_considered": { "type": "integer" }, "approved": { "type": "integer" }, "rejection_counts": { "type": "object" } } }, "notes": { "type": "array", "items": { "type": "string" } },
                    "superseded_by": { "type": ["string", "null"] } } } }),
        json!({ "PlanEnvelope": { "type": "object", "properties": { "created": { "type": "boolean" }, "plan": r("Plan") } } }),
        json!({ "Page": { "type": "object", "properties": { "total": { "type": "integer" }, "items": { "type": "array", "items": { "type": "object" } }, "observation": { "type": "object" } } } }),
        json!({ "Discover": { "type": "object", "properties": { "query": { "type": "string" }, "kind": { "type": "string" }, "results": { "type": "array", "items": { "type": "object" } }, "providers": { "type": "array", "items": { "type": "object" } } } } }),
        json!({ "Readiness": { "type": "object", "properties": { "ready": { "type": "boolean" }, "mode": { "type": "string", "enum": ["shadow", "execution"] }, "checks": { "type": "array", "items": { "type": "object" } }, "ready_note": { "type": "string" }, "migration": { "type": "object", "description": "computed per stage; ready means this server's checkable prerequisites hold, the native apps' state is not observable here", "properties": { "mode": { "type": "string", "enum": ["coexistence", "legacy"] }, "ownership": { "type": "object" }, "stages": { "type": "array", "items": { "type": "object", "properties": { "stage": { "type": "string", "enum": ["limited", "coexistence", "broad"] }, "status": { "type": "string", "enum": ["blocked", "pending_verification", "prerequisites_met"] }, "ready": { "type": "boolean" }, "configuration_blockers": { "type": "array", "items": { "type": "string" } }, "pending_verification": { "type": "array", "items": { "type": "string" } }, "external_requirements": { "type": "array", "items": { "type": "string" } } } } }, "accepted_limitations": { "type": "array", "items": { "type": "string" } }, "evidence": { "type": "object" }, "note": { "type": "string" } } } } } }),
        json!({ "Capabilities": { "type": "object", "properties": { "mode": { "type": "string", "enum": ["shadow", "execution"] }, "execution_enabled": { "type": "boolean" }, "capabilities": { "type": "array", "items": { "type": "object", "properties": {
                    "name": { "type": "string" }, "state": { "type": "string", "enum": ["available", "not_configured", "disabled", "not_implemented"] }, "implemented": { "type": "boolean" }, "configured": { "type": "boolean" },
                    "verified": { "type": "string", "enum": ["none", "unit_tests", "loopback_mocks", "loopback_peers", "sandbox", "production"] }, "detail": { "type": "string" } } } }, "policy_blocking_issues": { "type": "array" } } } }),
    ] {
        if let Value::Object(o) = part {
            m.extend(o);
        }
    }
    Value::Object(m)
}
