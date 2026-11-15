# MCP: stdio adapter and remote Streamable HTTP

Two transports serve the same tools with the same contracts, both built on rmcp 3.6.0,
the official Rust SDK, which does all JSON-RPC framing, lifecycle and version negotiation:

- **stdio** — `arr mcp`, a separate process that is a client of a running server's
  authenticated REST API and of nothing else (features `server`, `transport-io`). Unchanged.
- **remote** — `[mcp] enabled = true` in `serve`: Streamable HTTP at `POST /mcp` in the same
  process (feature `transport-streamable-http-server`, `StreamableHttpService` in stateless
  mode with JSON responses). See [Remote MCP](#remote-mcp-streamable-http) below.

Neither keeps state of its own or knows a Sonarr, Radarr, Prowlarr or qBittorrent address.

## Invocation

```bash
arr mcp --api-url http://127.0.0.1:8787/ --token-file /private/state/secrets/api_token [--timeout-secs 120]
```

An MCP client launches it as a stdio server, for example:

```json
{ "command": "arr", "args": ["mcp", "--api-url", "http://127.0.0.1:8787/", "--token-file", "/private/state/secrets/api_token"] }
```

**Operator configuration only.** The model cannot set or see any of it.

- **`--api-url`.** The base URL of a running `arr serve`:
  - `http` or `https`, with a host;
  - no user info, query or fragment (refused at start, without echoing the URL);
  - a path prefix is kept.

  Use loopback, or `https` across hosts.
- **`--token-file`.** The API bearer token:
  - **Permissions:** mode 600; group or other permissions are refused.
  - **Contents:** UTF-8, at most 4 KiB, no control characters.
  - **Handling:** read once at start, and never printed.
- **Where the token goes.** It is sent only as `Authorization` to `--api-url`. Proxy
  environment variables are ignored, and redirects are not followed.
- **`--timeout-secs`.** Per API call, 1 to 600 (default 120). The connect timeout is at most
  10 seconds. A release search can take as long as the server's Prowlarr settings allow.
- **Output.**
  - Stdout carries only MCP messages.
  - Logs go to stderr at `warn` unless `RUST_LOG` says otherwise. They never contain the
    token: the header is marked sensitive, and the adapter logs nothing itself.
  - A start-up refusal writes the error to stderr and exits non-zero, with nothing on
    stdout.

**Protocol.** The adapter advertises every revision the SDK knows and leaves negotiation to
the SDK:

| client | negotiated |
|---|---|
| `server/discover` | 2026-07-28 |
| `initialize` asking for 2026-07-28 | 2025-11-25 |
| `initialize` asking for 2025-06-18 or 2024-11-05 | that version |

## Tools

| tool | arguments | REST call |
|---|---|---|
| `search_titles` | `kind` (`movie`, `series`), `q` (1 to 100 printable characters), `year`? (1870 to 2100) | `GET /api/v1/discover` |
| `add_media` | `media_id`: a movie, season or episode | `POST /api/v1/library {"media_id"}` |
| `stop` | `media_id`: a movie, season or episode | `POST /api/v1/stop {"media_id"}`; the result gains a `message` saying how many downloads were cancelled and how many are still finishing. Never deletes anything in Plex or the library |
| `media_status` | `media_id`: a movie, series, season or episode | `GET /api/v1/status/{media_id}` |
| `requests` | `limit`? (1 to 50, default 20) | `GET /api/v1/queue?limit=N`, then `GET /api/v1/requests` (first N shown, with `total` and `omitted`) |
| `library` | `kind` (`movie`, `series`), `q`? (1 to 100 printable characters), `limit`? (1 to 100, default 50), `offset`? | `GET /api/v1/library/movies` or `/series` with `q`, `limit`, `offset`; returns a compact page (`media_id`, title, year; movies: `owned`, `quality`, `monitored`, `downloading`; series: `episodes` and per-season `owned`/`episodes`) with `total` and `next_offset` |
| `release_choices` | `media_id`: a movie, season or episode | `GET /api/v1/media/{media_id}/releases` |
| `start_download` | `media_id`, `release_ref` (`rel_` + 24 lowercase hex) | `POST /api/v1/grabs {"media_id", "release_ref"}` |

### Everything the web app can do

An assistant can do anything a person can do in the web app, through the same API routes, so
with the same checks, confirmations and refusals. Each tool is one REST call, and its result
is the API's JSON: no field is added, hidden or reworded. These tools come after the
original eight.

| tool | arguments | REST call |
|---|---|---|
| `get_title` | `media_id` (movie, series, season or episode), `level`? (`smaller`, `standard`, `best`) | `POST /api/v1/quick-add`: the web app's Get button. A show is followed and its latest season fetched. The answer is the web app's (`outcome`, `message`), not `automatic` |
| `set_quality` | `media_id` (movie or series), `level` | `POST /api/v1/quality` |
| `follow_series` | `media_id` (series), `on`, `level`? | `POST /api/v1/follow` |
| `remove_title` | `media_id` (movie or series) | `POST /api/v1/remove`: stops tracking, cancels unfinished downloads, touches no file |
| `copies` | `media_id` (movie, season or episode), `level`? | `GET /api/v1/copies/{media_id}`: plain-words copies with the server's `recommended` marked; the choice is the person's |
| `get_copy` | `media_id`, `id` (`rel_` + 24 hex), `level`? | `POST /api/v1/copies/{media_id}/get` |
| `retry_download` | `grab_id` | `POST /api/v1/queue/{grab_id}/retry` |
| `title_details` | `media_id` (movie, series or season) | `GET /api/v1/movie/{id}`, `/show/{id}` or `/show/{id}/season/{n}` |
| `people_search`, `person` | `q`; `person_id` | `GET /api/v1/people?q=`, `GET /api/v1/people/{person_id}` |
| `feed`, `attention`, `ideas`, `storage`, `system_health` | none | `GET /api/v1/feed`, `/attention`, `/ideas`, `/storage`, `/system/health` |
| `hide_idea` | `media_id` (series), `hidden`? (default true) | `POST /api/v1/ideas/hide`, or `DELETE /api/v1/ideas/hide/{media_id}` with `hidden: false` |
| `calendar` | `start`?, `end`? (RFC 3339) | `GET /api/v1/calendar` |
| `wanted`, `history` | `media_id`?, `limit`?, `offset?` (`history` also `kind`?) | `GET /api/v1/wanted`, `GET /api/v1/history` |
| `sport_search` | `q` | `GET /api/v1/sport/search` |
| `sport_leagues`, `sport_following` | none | `GET /api/v1/sport/leagues`, `/sport/following` |
| `sport_league` | `league` | `GET /api/v1/sport/league/{slug}` |
| `sport_follow`, `sport_unfollow` | `league`, `team_id`? | `POST /api/v1/sport/follow`, `/sport/unfollow` |
| `sport_events` | `from`?, `to`?, `league`? | `GET /api/v1/sport/events` |
| `sport_copies` | `event_id`, `part`?, `fresh`? | `GET /api/v1/sport/event/{id}/copies` |
| `sport_get` | `event_id`, `id` (`rel_` + 24 hex), `confirm`? | `POST /api/v1/sport/event/{id}/get` |
| `download` | `grab_id` (`grb_` + 24 hex) | `GET /api/v1/queue/{grab_id}`: one download with its events and import steps (a sport download shows the event's name, never release text) |
| `pause_download`, `resume_download`, `cancel_download` | `grab_id` | `POST /api/v1/queue/{grab_id}/pause`, `/resume`, `/cancel` (no body: data is kept, nothing is blocklisted). A refusal other than "no such download" is `unknown`, because the API may have acted on the download client first |
| `blocklist` | `media_id`? | `GET /api/v1/blocklist`. A sport entry shows the stored event's name and a reason from a closed list (anything else is `other`); the API does this, the tool adds nothing |
| `unblock` | `id` | `DELETE /api/v1/blocklist/{id}` |
| `run_command` | `name` (`rescan`, `search_missing`, `rss_sync`, `refresh_metadata`, `search`, `plex_refresh`), `media_id`? | `POST /api/v1/commands`. `search` and `plex_refresh` need a title, the others take none; `poll_downloads` and `cleanup_downloads` are not offered. Queued, not finished |
| `command_status` | `id`? (`cmd_` + 24 hex), `limit`?, `offset`? | `GET /api/v1/commands/{id}`, or `GET /api/v1/commands` without an id |
| `library_folders` | none | `GET /api/v1/library/roots` |
| `delete_preview` | `media_id` | `POST /api/v1/delete/preview` |
| `delete_files` | `media_id`, `token`, `confirmed`, `keep_wanted`? | `POST /api/v1/delete/preview` again, then `POST /api/v1/delete` |
| `bin` | none | `GET /api/v1/bin` |
| `bin_restore` | `delete_id` | `POST /api/v1/bin/{delete_id}/restore` |
| `bin_empty` | `delete_id`, `bytes`, `confirmed` | `GET /api/v1/bin`, then `DELETE /api/v1/bin/{delete_id}` |

**Sport is hand-pick only.** Nothing recommends or picks a copy: `sport_copies` lists what
the API lists and `sport_get` downloads the one the person chose. A copy with a caution
answers `409 confirmation_needed` (`not_applied`) with a `routing` note telling the
assistant to show the person the caution and to call again with `confirm: true` only after
they say yes. Scores and results are never returned (the API does not return them), and no
release text, link, magnet or hash is either. Sport events are still refused by the
movie and series tools, with a pointer to the `sport_*` tools.

**Deleting files is two steps and needs the person's yes.**
- `delete_preview` shows what would go (files, sizes, the downloads that only seed them,
  anything blocking) and a `token`. It deletes nothing.
- `delete_files` is described as destructive and tells the assistant to show the preview
  and get an explicit yes to exactly that scope. It requires `confirmed: true` and the
  preview's `token`.
- **Bound to its preview.** The tool previews the scope again itself and compares the
  token with the one it was given. The API's token is a digest over the media id, each
  file's path, kind, size and modified time, and each download the preview lists with its
  `removed` decision, so a download that appears after the preview refuses the delete too. A token for another scope, a stale one or an
  invented one is refused locally (`changed`, `not_applied`) before anything is sent to
  `POST /delete`, and the API checks the same token once more before it moves a file. A
  preview with no files, or one that is `blocked`, is refused the same way.
- **What it does.** Files go to the bin (`<library folder>/.arr-bin`), never straight to
  nothing. Downloads that only seed them are removed with their data. By default Arr stops
  wanting the title; `keep_wanted: true` makes it fetch a new copy.
- **Putting it back.** `bin` lists entries (`delete_id`, title, files, bytes, `purge_at`);
  `bin_restore` moves the files back within 7 days (a file whose place was taken stays in
  the bin and is reported; nothing is wanted again).
- **Emptying for good.** `bin_empty` is permanent. It reads the bin first and only empties
  an entry that still has the `bytes` the person was shown, with `confirmed: true`.
- **Failure.** A delete or bin failure with an unknown outcome comes with guidance to check
  `bin` and `media_status` and to preview again.

**Annotations (all tools).** Reads and `delete_preview` are `readOnlyHint: true`. The
original writes are `destructiveHint: false`, `idempotentHint: true`; `remove_title`,
`delete_files` and `bin_empty` are `destructiveHint: true`.

**Parity.** The table `PARITY` in `src/mcp.rs` lists every API route (and every path the web
app calls) with the tool that covers it, or a reason it has none:
- **Web only, on purpose:** the web app's sign-in and session (`/app/api/me`, `signout`),
  push notifications (`/notifications/key`, `subscribe`), installing an update (`/update`)
  and changing server settings (`/settings/app`).
- **Operator:** routes that serve probes, scripts and client developers rather than a
  person's request (liveness and readiness, openapi, capabilities, diagnostics, the raw
  settings and migration report, first-run setup, plans, the timers list, the RSS poller,
  bulk ownership, the raw catalog record, `PATCH /library/{id}`), each with the reason a
  person would not need it in a chat. Things a person does with Arr itself (pausing or
  cancelling a download, the blocklist, background jobs, library folders) are tools.

The test also scans `src/` and fails when a route is registered anywhere but the shared route
lists in `src/api.rs` (other routers, such as the web app's pages, the setup wizard and the
OAuth endpoints, are allowlisted by file with a reason), and its reader of `app.js` refuses
what it cannot see (an aliased helper, an extra `fetch`, a computed route name it does not
know).

`tests/mcp_parity.rs` fails when a route of the real router, or a path `src/webapp/app.js`
calls, has no entry, when an entry names a tool that is not registered, or when a tool is in
no entry. Adding a route means adding its tool (or its reason) in the same change; see
`AGENTS.md`.

**Default and manual mode.** The server instructions and the tool descriptions say this,
and the tools test checks the key phrases:
- **Default.** `add_media` is the automatic way to add what the user asked for. The server
  monitors it, then searches and downloads it by its own policy.
- **Exact scope.** The assistant adds exactly the scope requested. It must not replace a
  requested season with individual episodes unless the user agrees.
- **Manual mode.** `release_choices` and `start_download` are for when the user explicitly
  asks to choose a release. `start_download` downloads only the exact release the user
  chose, never one picked on the user's behalf.
- **Repeats.** Repeating an add or a download creates no duplicate; nothing promises that a
  repeat changes nothing.

**Schemas.** Every input schema is a closed object (`additionalProperties: false`).

**Ids.** `media_id` is the core's canonical form, checked by the core's own parser: no
aliases, leading zeros, case changes, whitespace or season 0. Whole series cannot be added
here: the refusal tells the assistant to ask the user which season or episode they want.
`add_media` sends only `media_id`, so the REST defaults apply: monitored, and searched
automatically.

**Not expressible** as tool arguments:
- `native_handoff_confirmed` and `preserve_monitoring` (retired REST fields) and adoption in general;
- `monitored`, `monitor`, `quality_profile`, `root_folder` and `metadata`;
- `search: false` and `refresh`;
- paths, URLs and endpoints;
- paths, torrent control, categories and plan execution. Files are deleted only through
  `delete_preview` and `delete_files` as described above.

An unknown property is refused as `invalid_arguments` before any request.

**`start_download` and release refs.** A `release_ref` behaves like this:
- It comes from `release_choices`, which keeps link material only for approved releases.
- It is bound to the target it was listed for: any other target gets `404 release_not_found`.
- It expires after 7 days (worker housekeeping).
- Repeating a download is refused (`already_queued`, `duplicate_torrent`). After a grab,
  `release_choices` lists the target's releases as rejected `covers_queued`.

## Results and errors

**Success.** A success is the API's JSON as structured content. One result is reshaped:
a successful `add_media`. The REST API and every other tool's results are unchanged.
- **The retired flag is dropped.** `adoption.native_handoff_confirmed` (always `false` now)
  is removed, and the core's note is kept as `adoption.operator_note`.
- **Plans with nothing to execute.** With execution enabled, a plan that was not executed
  because it is `blocked`, `already_satisfied` or `in_progress` gets
  `plan.execution.mode: "not_executed"`. Its note no longer says to execute the plan by
  hand. `download_submission`, `submitted`, `grabs`, `refused`, blockers and units are
  kept as they are.
- **`automatic`** summarises the outcome from the plan alone, as `{status, message}`:

  | status | when |
  |---|---|
  | `download_queued` | executed, with grabs |
  | `partially_queued` | executed, with grabs, and some candidates refused |
  | `nothing_queued` | executed, and every candidate refused |
  | `not_downloaded_yet` | blocked: no release chosen. The message says execution is enabled, and that a waiting request, if `requests` or `media_status` shows one, is retried and RSS can pick up a release; no manual step. It does not claim a request exists. |
  | `already_owned` | already satisfied |
  | `in_progress` | owned or being downloaded |
  | `execution_disabled` | the real shadow plan, with its `mode: "shadow"` and note unchanged |

**Failure.** A failure is a tool error (`isError: true`) shaped as
`{"error": {...}, "http_status"?, "outcome", "guidance"?, "routing"?}`:
- `error` is the API's typed error unchanged (`not_adopted`,
  `already_queued` and the rest), or one of the adapter's codes: `invalid_arguments`,
  `api_unreachable`, `api_timeout`, `api_bad_response` or `api_response_too_large`.
- An unknown tool name is a protocol error.

**`outcome`** says what the failure means for the request:

| outcome | when |
|---|---|
| `nothing_changed` | any read |
| `not_sent` | invalid arguments, or no connection to the API was made |
| `not_applied` | a typed refusal the API returns before changing anything (an explicit list in `src/mcp.rs`): request-level refusals; for grabs, the validation, `release_not_found`, `not_adopted`, execution-blocker, `already_queued`, `duplicate_torrent`, `blocklisted`, payload and upstream refusals |
| `unknown` | any other write failure (each write names its own list in `src/mcp.rs`; a delete's is the shortest, because a refusal after it cancelled downloads is not a no-op): a timeout after connecting, a lost or unusable reply, a redirect, or a typed error that can follow a committed add |

`unknown` comes with guidance: check `media_status` or `requests` (`bin` for files) before repeating. A write
is never retried, and there is no fallback of any kind.

**A typed error after a committed add.** `POST /library` can commit an add and then answer
with a typed error from its automatic search: for example `502 upstream_unavailable` when
Prowlarr fails. That is reported as `unknown`. `media_status` then shows the title adopted,
and a later `add_media` searches again without duplicating it.

**Redaction.** Response bodies have any occurrence of the token replaced by `[redacted]`
before use (defence in depth; the API never echoes it).

## Routing

`start_download` for a title that is not adopted and monitored here (stopped, or never
added) gets `409 not_adopted`. It is `not_applied`, with a `routing` note: add it with
`add_media` first. There is no hand-over from Sonarr/Radarr any more, and no
`native_handoff_required` or `unsupported_scope` error.

Library mutations (add, `PATCH`, `DELETE`) share one lock. Every grab re-reads adoption from
the stored rows in the transaction that records it, so a release committed while the torrent
was fetched leaves nothing recorded or submitted.

## Tests

`tests/mcp.rs` drives the real binary over its stdin/stdout with the official
rmcp client (`client` feature). The adapter's raw stdout bytes are recorded too. Every
child also gets HTTP(S)/ALL proxy variables pointing at a counting sentinel, so a request
to anything but `--api-url` would be seen.

- **`tools_are_listed_with_strict_schemas_under_every_negotiated_protocol`.**
  - **Lifecycles.** Four of them; the versions above are asserted against the SDK's own
    constants.
  - **Listing.** The same tools under each, the original eight first.
  - **Schemas.** Closed schemas with exact properties and required fields; annotations.
    No forbidden property appears anywhere in any schema. The published `media_id`
    patterns accept exactly the ids each tool takes.
  - **Wording.** `add_media` is the default; `release_choices` and `start_download` are
    manual mode only; no description promises that something changes nothing.
- **`a_new_title_is_found_added_and_tracked_and_repeating_creates_no_duplicate`.** Against a
  real API with TMDB:
  - search, add (TMDB metadata, grab), qBittorrent submission in the dedicated category,
    then status (adopted, one download) and requests;
  - a repeated add, `release_choices` (`covers_queued`), and a repeated download of the
    queued release (`already_queued`, `not_applied`);
  - still one grab and one client add.
- **`an_add_committed_before_its_search_failed_is_reported_as_unknown`.** A real `502` after
  a committed add is `unknown` with guidance, and status shows the title adopted. Once the
  indexer answers, a repeat grabs once.
- **`an_automatic_add_without_a_release_reads_as_waiting_and_a_real_shadow_plan_stays_shadow`.**
  - **Execution enabled.** A monitored episode is added through `add_media` with an
    empty indexer. The test checks the exact fields:
    - the adoption keys (`acquisition`, `mode`, `native_app`, `operator_note`), without `native_handoff_confirmed`;
    - plan `blocked`, `download_submission: "enabled"`, `mode: "not_executed"`, no grabs;
    - `automatic.status: "not_downloaded_yet"`;
    - no "shadow plan" or `/execute` anywhere.

    `requests` shows the core's active request. The REST response for the same add is
    unchanged.
  - **Execution disabled.** The plan stays `mode: "shadow"` with its note, and the status is
    `execution_disabled`.
- **`a_title_that_is_not_monitored_is_refused_with_the_core_error_and_nothing_happens`.**
  - Passing `native_handoff_confirmed` is refused as an unknown argument.
  - Listing releases works, but downloading one for a stopped movie gets `not_adopted`.
  - Then: no client add, no torrent fetched, an empty queue.
- **`malformed_or_overreaching_arguments_never_reach_the_api`.** 32 malformed or overreaching
  calls and an unknown tool reach a recording stand-in API zero times. A 300 KiB message is
  dropped unanswered, and the session stays usable. Six valid calls produce exactly the
  expected method, path (under a base-path prefix), query, body and bearer header.
- **`an_unreachable_hanging_or_broken_api_fails_closed`.**
  - A closed port and an HTML `502` give an error with the right outcome on every tool.
  - A hanging server (1 s timeout) and a redirect give one on `add_media`.
  - No proxy is used, and the redirect target is never contacted.
- **`the_token_never_leaves_through_stdout_stderr_or_results`.**
  - A hostile stand-in echoes the Authorization header: results show `[redacted]`.
  - With `RUST_LOG=trace`, neither stdout nor stderr contains the token, and every stdout
    line is a JSON-RPC 2.0 message.
  - Start-up refusals (mode 644, credentials or a query in the URL, a non-HTTP scheme)
    write nothing to stdout and leak nothing on stderr.

**Parity tests.**
- `tests/mcp_parity.rs` (see Parity above): every route of the real router (its methods are
  found by asking the running router, its paths come from the list the router is built
  from), every path `src/webapp/app.js` calls, and every tool is in `PARITY`. The OpenAPI
  route list is compared with the router too. The failure message says what to add.
- `tests/mcp.rs`, for the web-app tools:
  - `the_read_tools_say_what_the_web_apps_routes_say`: each read tool answers what its
    route answers (or the same typed refusal, `nothing_changed`), and arguments that could
    escape a path are refused before any request;
  - `titles_get_a_quality_follow_and_hide_through_the_same_routes_as_the_web_app`,
    `get_title_is_the_web_apps_get_with_a_quality_and_repeating_it_creates_no_duplicate` and
    `a_copy_picked_by_hand_is_downloaded_through_the_same_route_as_the_web_app`: the
    quality, follow, hide, retry, remove, get and copy tools, with their outcomes;
  - `sport_is_found_followed_and_picked_by_hand_with_the_caution_shown_first`: follow,
    schedule, copies without any recommendation or leak, `confirmation_needed` with its
    routing note, the unlisted copy refused, one grab, a repeat that creates no duplicate;
  - `files_are_deleted_only_as_previewed_and_confirmed_then_restored_or_emptied_from_the_bin`:
    no yes means no request; a made-up, stale or other scope's token moves nothing; the
    previewed scope is deleted, restored and emptied, and a bin entry whose size differs
    from what the person saw is not emptied.

- `downloads_are_paused_resumed_cancelled_and_shown_and_a_sport_one_shows_no_release_text`
  and `background_jobs_are_asked_for_by_a_few_names_and_read_back_and_folders_are_listed`:
  the download, blocklist, command and folder tools; a sport download and a sport blocklist
  entry carrying a release's text come out as the event's name and a closed reason only.

**Review regression tests.**
- `tests/mcp_parent_review.rs`: a committed `POST` whose reply is delayed past the timeout,
  or replaced by an HTML `502`, is committed once (never retried), reported as uncertain
  with inspect-first guidance, and says there is no fallback.

**Core ordering regression.**
`a_release_sent_during_an_add_is_ordered_after_it_and_refused_by_its_download` and
`a_release_committed_while_a_torrent_is_fetched_records_and_submits_nothing`
(`tests/failures.rs`): a release sent while an add is held in its search runs after it and is
refused as an active download; a release committed while a torrent is fetched makes the
grab fail with `409 not_adopted` from the transaction check, leaving no grab, outbox entry or
client add.

**Negative controls.** Each guard was disabled in turn, and at least one test failed:
- no-proxy;
- redaction;
- outage fallback;
- unknown fields allowed;
- redirects followed;
- whole series accepted;
- the 256 KiB line limit;
- every add error treated as `not_applied`;
- every failure treated as `not_sent`;
- the library lock removed from `DELETE`: the release lands mid-add, and the add then fails
  with `409`;
- the grab transaction's adoption re-check disabled: the grab is recorded (`201`) after the
  release;
- the add-result reshaping bypassed: the raw `mode: "shadow"` and the "execute it with POST …" note reappear.

Two guards are not observable: the canonical-id round trip, which the core parser already
enforces (so it was removed), and the sensitive header flag, since no log prints header
values (it is kept as a one-line defence).

## Limits

- **One API, no failover.** With the API down, every tool errors.
- **Scope.**
  - No whole-series adds.
  - No monitoring changes, removal, blocklist or queue control.
  - These stay REST operations for the operator.
- **Responses pass through.** A large series status can be long; it is bounded at 2 MiB.
  Messages are bounded at 256 KiB: longer lines are dropped by the SDK transport, and the
  request gets no answer.

## Remote MCP (Streamable HTTP)

```toml
[mcp]
enabled = true
token_file = "secrets/mcp_token"        # 0600, >= 24 characters, NOT the API token
allowed_hosts = ["localhost", "127.0.0.1"]   # default for a loopback listener; required otherwise
allowed_origins = []                    # browser origins allowed; any other Origin is refused
max_body_bytes = 65536
max_concurrent = 4
timeout_secs = 120
```

**Request path.** Every `/mcp` request passes, in this order, before rmcp parses anything:
`Host` validation (exact host or host:port from `allowed_hosts`; DNS-rebinding protection),
`Origin` validation (refused unless listed; requests without `Origin` pass), the MCP bearer
token (constant-time; `401` with `WWW-Authenticate: Bearer`), a `Content-Length` bound
(`413`), a concurrency bound (`503`) and a deadline (`504`). rmcp then applies its own Host,
Origin and streamed body-size checks with the same settings. The endpoint is stateless (no
sessions to exhaust; `GET` and `DELETE` answer `405`); `initialize` and the newer stateless
discover lifecycle both work.

**Same tools, same core.** The remote adapter calls this process's API router in-process: the
same bearer auth (with the API token held in memory), load shedding, request deadline, body
limit and handlers as any HTTP request, without a loopback socket and without depending on the
configured listen address (a `0.0.0.0` listen is never dialled). Shutdown stops the MCP service
with the server; nothing re-enters startup.

**Separate credential.** The MCP token reaches only the tools; it is refused by the REST
API, and the API token is refused at `/mcp`. Startup refuses an MCP token equal to the API
token, a missing token file, and a non-loopback listener without explicit `allowed_hosts`.
With `allow_non_loopback`, `/mcp` is still never served unauthenticated.

**TLS.** The server has no TLS listener. Put an HTTPS reverse proxy or tunnel in front for
anything beyond loopback and list the public host in `allowed_hosts`.

### OAuth sign-in (ChatGPT connectors)

A static bearer token cannot be entered in a ChatGPT connector, which signs in with OAuth.
With an `[mcp.oauth]` section, Arr is the OAuth 2.1 authorization server for `/mcp` and uses
Google only to prove who is signing in. Without the section nothing changes: only the static
token works and no OAuth endpoint exists.

```toml
[mcp.oauth]
public_url = "https://media.example.org"   # https origin only; its host must be in allowed_hosts
google_client_id = "1234567890-abc.apps.googleusercontent.com"
google_client_secret_file = "secrets/google_client_secret"   # 0600, relative to the config dir
allowed_emails = ["you@example.com"]       # 1 to 32 exact addresses, case-insensitive, no wildcards
# Optional, defaults shown. Every URL must be https (http only for loopback hosts, for tests).
# trusted_client_metadata = ["https://chatgpt.com/oauth/"]
# google_authorization_endpoint = "https://accounts.google.com/o/oauth2/v2/auth"
# google_token_endpoint = "https://oauth2.googleapis.com/token"
# access_token_ttl_secs = 3600
# refresh_token_ttl_days = 30
```

Startup refuses a `public_url` whose host is not in `mcp.allowed_hosts`, a non-https URL, an
empty or wildcard `allowed_emails` entry, and a group- or world-readable secret file.

**What it serves** (same `Host` allow-list as `/mcp`; every response is `Cache-Control:
no-store`; POSTs refuse any `Origin` other than `public_url`; bodies are limited to 16 KiB):

| Path | Purpose |
| --- | --- |
| `/.well-known/oauth-protected-resource` (and `.../mcp`) | resource `<public_url>/mcp`, issuer `<public_url>/` |
| `/.well-known/oauth-authorization-server` | endpoints, S256 PKCE, `private_key_jwt` (RS256) or `none`, `client_id_metadata_document_supported`; no registration endpoint |
| `GET /authorize`, `POST /authorize/consent` | validates the client, shows a consent page, then sends the user to Google |
| `GET /auth/callback` | Google's redirect target; checks the ID token and issues an authorization code |
| `POST /token` | `authorization_code` and `refresh_token` grants |

**Flow.** ChatGPT identifies itself with a Client ID Metadata Document: its `client_id` is an
https URL. Arr fetches the document (no redirects, no proxy, 5 KiB, 5 s, cached for an hour),
requires the document's `client_id` to equal the URL and the request's `redirect_uri` to be
one of its `redirect_uris`, and only then shows the consent page naming the client. Until both
are valid, errors are shown on a plain page and never redirected. After approval the user signs
in with Google (`openid email`, `prompt=select_account`). Arr exchanges Google's code, checks
the ID token (issuer, audience and `azp`, expiry, `email_verified`, and that the lowercase
address is in `allowed_emails`), and returns an authorization code to the connector. At
`/token` ChatGPT authenticates with a signed `private_key_jwt` assertion verified against the
`jwks_uri` of its metadata document (same origin as the `client_id`; RS256; `aud`, `exp`,
`iat`/`nbf` and a never-repeated `jti` are enforced). The code is single use, short lived,
bound to the client and redirect address, and redeemed with a PKCE verifier. Refresh tokens
rotate on every use.

**Using it with Google.** In the Google Cloud console, create an OAuth client of type
**Web application** and add the authorized redirect URI `<public_url>/auth/callback`. Keep the
OAuth consent screen in **Testing** and add the allowed users as test users. Put the client
secret in the secret file and the client id in the config.

**Adding the connector in ChatGPT.** Add a custom connector with the URL `<public_url>/mcp` and
authentication **OAuth**. ChatGPT discovers the rest, opens the consent page, and the user
signs in with an allowed Google account.

**Security properties and limits.**

- Only addresses listed in `allowed_emails` can sign in, and the list is checked again every
  time an access token is used and every time a refresh token is redeemed: removing an
  address (and restarting) ends its access immediately.
- Dynamic client registration is not supported. Only clients whose `client_id` matches a
  `trusted_client_metadata` prefix (`<prefix>client.json` or `<prefix><id>/client.json` with
  `id` of 1 to 128 of `A-Za-z0-9_-`) are accepted; the default trusts ChatGPT only.
- Access tokens (`arr_at_...`) and refresh tokens (`arr_rt_...`) are random; only their
  SHA-256 is stored, in `oauth.sqlite` in the state directory (mode 0600), so they survive a
  restart but cannot be read from disk. Authorization codes, consent transactions and used
  assertion ids are in memory only and are lost on restart.
- OAuth access tokens reach `/mcp` only. The REST API refuses them, as it refuses the MCP
  token; `/mcp` still accepts the static MCP token. A refused `/mcp` request answers `401` with
  `WWW-Authenticate: Bearer resource_metadata="<public_url>/.well-known/oauth-protected-resource/mcp"`.
- The ID token's signature is not verified: OIDC Core 3.1.3.7 allows this when the token
  comes straight from the token endpoint over TLS, which is the only way it reaches Arr. Its
  claims are still checked.
- Logs carry outcome codes only: never tokens, codes, assertions, secrets or addresses. A
  refused sign-in reveals nothing about the allow-list to the client.

**Tests.** `tests/standalone.rs::remote_mcp_serves_the_same_tools_over_streamable_http_with_its_own_bearer`
drives the endpoint with rmcp's official Streamable HTTP client (initialize and discover
lifecycles; list, read, write and typed-error tool calls) and checks with raw HTTP: missing
token `401` before initialisation, the API token refused, a foreign `Host` and `Origin` `403`,
an oversized body `413`, `GET` `405`, and the MCP token refused by the REST API.
`tests/oauth.rs` covers the OAuth sign-in end to end against loopback mocks (discovery, client
validation, consent, Google round trip, assertions, PKCE, refresh rotation, restart, removal
of an address).
