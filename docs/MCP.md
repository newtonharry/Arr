# MCP: stdio adapter and remote Streamable HTTP

Two transports serve the same six tools with the same contracts, both built on rmcp 3.6.0,
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
| `media_status` | `media_id`: a movie, series, season or episode | `GET /api/v1/status/{media_id}` |
| `requests` | `limit`? (1 to 50, default 20) | `GET /api/v1/queue?limit=N`, then `GET /api/v1/requests` (first N shown, with `total` and `omitted`) |
| `release_choices` | `media_id`: a movie, season or episode | `GET /api/v1/media/{media_id}/releases` |
| `start_download` | `media_id`, `release_ref` (`rel_` + 24 lowercase hex) | `POST /api/v1/grabs {"media_id", "release_ref"}` |

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

**Annotations.** Reads are `readOnlyHint: true`. Writes are `destructiveHint: false` and
`idempotentHint: true`.

**Not expressible** as tool arguments:
- `native_handoff_confirmed`, `preserve_monitoring` and adoption in general;
- `monitored`, `monitor`, `quality_profile`, `root_folder` and `metadata`;
- `search: false` and `refresh`;
- paths, URLs and endpoints;
- deletion, queue or torrent control, categories and plan execution.

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
- **The per-request flag is dropped.** `adoption.native_handoff_confirmed` only says
  whether this request carried the operator's acknowledgement, which the adapter never
  sends. It read as if no handoff existed, so it is removed.
- **What the success establishes.** For an adopted native title in coexistence mode,
  `adoption.handoff` is `"on_record"`: a confirmed handoff covers the scope. The core's
  note is kept as `adoption.operator_note`.
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
- `error` is the API's typed error unchanged (`not_adopted`, `native_handoff_required`,
  `already_queued` and the rest), or one of the adapter's codes: `invalid_arguments`,
  `api_unreachable`, `api_timeout`, `api_bad_response` or `api_response_too_large`.
- An unknown tool name is a protocol error.

**`outcome`** says what the failure means for the request:

| outcome | when |
|---|---|
| `nothing_changed` | any read |
| `not_sent` | invalid arguments, or no connection to the API was made |
| `not_applied` | a typed refusal the API returns before changing anything (an explicit list in `src/mcp.rs`): request-level refusals; `native_handoff_required` and `unsupported_scope` for adds; for grabs, the validation, `release_not_found`, `not_adopted`, execution-blocker, `already_queued`, `duplicate_torrent`, `blocklisted`, payload and upstream refusals |
| `unknown` | any other write failure: a timeout after connecting, a lost or unusable reply, a redirect, or a typed error that can follow a committed add |

`unknown` comes with guidance: check `media_status` or `requests` before repeating. A write
is never retried, and there is no fallback of any kind.

**A typed error after a committed add.** `POST /library` can commit an add and then answer
with a typed error from its automatic search: for example `502 upstream_unavailable` when
Prowlarr fails. That is reported as `unknown`. `media_status` then shows the title adopted,
and a later `add_media` searches again without duplicating it.

**Redaction.** Response bodies have any occurrence of the token replaced by `[redacted]`
before use (defence in depth; the API never echoes it).

## Coexistence routing

**Native titles not yet adopted** (imported and *observed*):
- `add_media` gets `409 native_handoff_required`;
- `start_download` gets `409 not_adopted`;
- titles this server cannot acquire get `409 unsupported_scope`.

These are `not_applied`, with a `routing` note: the title stays with Sonarr/Radarr. The
adapter never confirms a handoff and never falls back to Sonarr/Radarr.

**Adoption is the operator's step**, outside the assistant: the native ledger and pause
first, then `POST /api/v1/library` with `native_handoff_confirmed`.

**Once the operator has confirmed a scope,** `add_media` inside it works without a
confirmation and acquires it. This is a core change from this pass. The confirmation is
recorded durably, per scope:
- **Scopes.** The whole title (including `preserve_monitoring`), a season, or an episode.
- **Lifetime.** Until the title is released with `DELETE /library`. Current monitoring
  flags play no part: an unmonitored episode inside a confirmed season, or a whole title
  handed over with its native flags off, stays this server's.
- **Wider scopes.** A sibling episode, its season or the series is refused again, as is a
  released title. An unmonitored registration is not a confirmation.
- **Visibility.** `GET /api/v1/ownership` lists each title's `handoffs`.
- **Upgrade step (operator).** Scopes adopted before this build have no record. Confirm each
  exact scope once more with `POST /api/v1/library` and `native_handoff_confirmed: true,
  search: false` (or `preserve_monitoring` for a whole title); nothing is searched.
- **Ordering.** Library mutations (add, `PATCH`, `DELETE`) share one lock, so a release
  cannot fall between another request's handoff check and its change.
- **Grab against release.** Every grab re-reads adoption from the stored rows in the
  transaction that records it. A release committed while the torrent was fetched therefore
  leaves nothing recorded or submitted.

**Titles new to the native apps** (TMDB/TVmaze) follow the existing one-call add.

## Tests

`tests/mcp.rs` (9 tests) drives the real binary over its stdin/stdout with the official
rmcp client (`client` feature). The adapter's raw stdout bytes are recorded too. Every
child also gets HTTP(S)/ALL proxy variables pointing at a counting sentinel, so a request
to anything but `--api-url` would be seen.

- **`tools_are_listed_with_strict_schemas_under_every_negotiated_protocol`.**
  - **Lifecycles.** Four of them; the versions above are asserted against the SDK's own
    constants.
  - **Listing.** The same six tools under each.
  - **Schemas.** Closed schemas with exact properties and required fields; annotations.
    No forbidden property appears anywhere in any schema. The published `media_id`
    patterns accept exactly the ids each tool takes.
  - **Wording.** `add_media` is the default; `release_choices` and `start_download` are
    manual mode only; no description promises that something changes nothing.
- **`a_new_title_is_found_added_and_tracked_and_repeating_creates_no_duplicate`.** Against a
  real API in coexistence mode with TMDB:
  - search, add (TMDB metadata, grab), qBittorrent submission in the dedicated category,
    then status (adopted, one download) and requests;
  - a repeated add, `release_choices` (`covers_queued`), and a repeated download of the
    queued release (`already_queued`, `not_applied`);
  - still one grab and one client add.
- **`an_add_committed_before_its_search_failed_is_reported_as_unknown`.** A real `502` after
  a committed add is `unknown` with guidance, and status shows the title adopted. Once the
  indexer answers, a repeat grabs once.
- **`an_operator_adopted_scope_is_acquired_through_mcp_and_cannot_be_widened`.**
  - After an out-of-band adoption of one episode, `add_media` acquires it with no
    confirmation: one grab, in the TV category.
  - A sibling episode and the season are refused with `native_handoff_required`.
  - After the operator confirms the season and unmonitors one of its episodes, `add_media`
    for that episode still needs no confirmation.
  - `/ownership` lists exactly the two confirmed scopes.
- **`an_automatic_add_without_a_release_reads_as_waiting_and_a_real_shadow_plan_stays_shadow`.**
  - **Execution enabled.** An operator-adopted episode is added through `add_media` with an
    empty indexer. The test checks the exact fields:
    - the adoption keys, with `handoff: "on_record"` and no `native_handoff_confirmed`;
    - plan `blocked`, `download_submission: "enabled"`, `mode: "not_executed"`, no grabs;
    - `automatic.status: "not_downloaded_yet"`;
    - no "shadow plan" or `/execute` anywhere.

    `requests` shows the core's active request. The REST response for the same add is
    unchanged.
  - **Execution disabled.** The plan stays `mode: "shadow"` with its note, and the status is
    `execution_disabled`.
- **`an_unadopted_native_title_is_refused_with_the_core_error_and_nothing_happens`.**
  - Observed movie, season and episode adds get `native_handoff_required`.
  - Passing `native_handoff_confirmed` is refused as an unknown argument.
  - Listing releases works, but downloading one gets `not_adopted`.
  - Then: no client add, no torrent fetched, an empty queue and request list, nothing
    adopted, and status still `observed`.
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

**Review regression tests.**
- `tests/mcp_parent_review.rs`: a committed `POST` whose reply is delayed past the timeout,
  or replaced by an HTML `502`, is committed once (never retried), reported as uncertain
  with inspect-first guidance, and says there is no fallback.
- In `tests/migration_review.rs`:
  - `confirmed_scope_replays_without_handoff_but_never_expands_via_sibling_or_parent`:
    the replay works, a wider scope is refused, and a release revokes the confirmation;
  - `unmonitored_registration_cannot_stand_in_for_a_confirmed_native_handoff`;
  - `a_single_known_episode_does_not_authorize_its_seasons_future_episodes`;
  - `an_explicit_whole_title_handoff_survives_disabled_monitoring_and_reactivation`
    (including the TVmaze alias).

**Core ordering regression.**
`a_release_sent_during_an_add_is_ordered_after_it_and_refused_by_its_download`
(`tests/coexistence.rs`):
- **Setup.** An add inside a confirmed scope is held in its automatic search by a slow
  indexer answer. The test waits for the mock to log the query, so there is no timing
  guess.
- **Check.** A release sent meanwhile runs after the add, and the add's download refuses it
  (`409 active_downloads`).
- **Result.** The add succeeds, and the title and its handoff stay.

`a_release_committed_while_a_torrent_is_fetched_records_and_submits_nothing`
(`tests/coexistence.rs`):
- **Setup.** A manual grab passes its adoption check. A mock gate then holds its torrent
  fetch open until the test sees the fetch.
- **Check.** A release succeeds meanwhile. When the fetch completes, the grab gets `409
  not_adopted` from the transaction check. Then there is no grab, outbox entry or client
  add, even after a worker pass, and the title is `released` with no handoffs.
- **Re-adoption.** A fresh handoff re-adopts the title, and the same release is grabbed
  once.

**Negative controls.** Each guard was disabled in turn, and at least one test failed:
- no-proxy;
- redaction;
- auto-confirming the handoff;
- outage fallback;
- unknown fields allowed;
- redirects followed;
- whole series accepted;
- the 256 KiB line limit;
- every add error treated as `not_applied`;
- every failure treated as `not_sent`;
- the replay rule reverted, and the handoff never required;
- a release keeping its handoffs;
- coverage always granted;
- `preserve_monitoring` not recorded;
- a season handoff not covering its episodes;
- the library lock removed from `DELETE`: the release lands mid-add, and the add then fails
  with `409`;
- the grab transaction's adoption re-check disabled: the grab is recorded (`201`) after the
  release;
- the add-result reshaping bypassed: the raw `native_handoff_confirmed: false`,
  `mode: "shadow"` and the "execute it with POST …" note reappear.

Two guards are not observable: the canonical-id round trip, which the core parser already
enforces (so it was removed), and the sensitive header flag, since no log prints header
values (it is kept as a one-line defence).

## Limits

- **One API, no failover.** With the API down, every tool errors.
- **Scope.**
  - No whole-series adds.
  - No monitoring changes, removal, blocklist or queue control.
  - No adoption, no `preserve_monitoring`.
  - These stay REST operations for the operator.
- **Responses pass through.** A large series status can be long; it is bounded at 2 MiB.
  Messages are bounded at 256 KiB: longer lines are dropped by the SDK transport, and the
  request gets no answer.
- **Native state is not observable.** As with the REST API, the adapter cannot see whether
  native monitoring is paused. It only relays the core's adoption state.


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

**Separate credential.** The MCP token reaches only the six tools; it is refused by the REST
API, and the API token is refused at `/mcp`. Startup refuses an MCP token equal to the API
token, a missing token file, and a non-loopback listener without explicit `allowed_hosts`.
With `allow_non_loopback`, `/mcp` is still never served unauthenticated.

**What this is not (honest scope).**

- It is **static bearer-token** transport for MCP clients that can send an `Authorization`
  header (rmcp's client, MCP inspectors, many desktop clients). It is **not** an OAuth 2.1
  protected resource: there is no protected-resource metadata, no authorization-server
  integration, no JWT/JWKS validation and no per-user scopes.
- **Not a ChatGPT connector by itself.** ChatGPT's remote connectors require an HTTPS
  endpoint reachable from the internet and, for authentication, OAuth (authorization code
  with PKCE against an authorization server). This server has no TLS listener and no OAuth.
- To put this endpoint behind a cloud connector deliberately, an operator would need: TLS
  termination and ingress (a reverse proxy or tunnel) with the public host listed in
  `allowed_hosts`; and an OAuth-protected-resource front (an authenticating proxy that
  validates the issuer's tokens — audience/resource, scopes, expiry against the issuer's
  JWKS — and forwards to `/mcp` with the static MCP token). Implementing native JWT
  validation here (configured issuer + JWKS, resource/audience/scope checks) is a possible
  next step; it is not implemented.

**Tests.** `tests/standalone.rs::remote_mcp_serves_the_same_tools_over_streamable_http_with_its_own_bearer`
drives the endpoint with rmcp's official Streamable HTTP client (initialize and discover
lifecycles; list, read, write and typed-error tool calls) and checks with raw HTTP: missing
token `401` before initialisation, the API token refused, a foreign `Host` and `Origin` `403`,
an oversized body `413`, `GET` `405`, and the MCP token refused by the REST API.
