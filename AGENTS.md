# Working on Arr

Notes for anyone (person or coding agent) changing this repository.

## The MCP server keeps parity with Arr

People use Arr from the web app and from an AI assistant through the MCP server
(`src/mcp.rs`, described in [docs/MCP.md](docs/MCP.md)). Both must be able to do the same
things. A capability that ships in the API or the web app ships in MCP in the same change.

- **Every user-facing route has an MCP tool, or is listed as web-only on purpose.** The parity
  table in `src/mcp.rs` maps each `/api/v1` route the web app or a person can use to the tool
  that covers it, or to a stated reason it is web-only. The parity test fails when a route is
  added without an entry, so adding a route means adding its tool (or its reason) in the same
  pull request.
  - The table is `PARITY` in `src/mcp.rs`; the test is `tests/mcp_parity.rs`. It reads the
    routes from the real router (`api::route_paths`, the list the router is built from), the
    paths the web app calls from `src/webapp/app.js`, and the registered tools.
  - A tool lives in `src/mcp/web.rs` (or `src/mcp.rs` for the original eight). Add its row
    to docs/MCP.md and a test that drives it.
  - An entry is `Tool(name)`, `WebOnly(reason)` or `Operator(reason)`. `Operator` is for
    routes only the person who runs the server or automation uses (probes, setup,
    diagnostics, plans); the web app and the household never use them.
- **Web-only is for things that make no sense in a chat**: the web app's own sign-in and
  session, push-notification keys and subscriptions, installing an update, and changing
  server settings. A feature is not web-only because it is new, hard, or risky.
- **Same rules in both places.** A tool goes through the same route as the web app, so it gets
  the same checks: confirmations (a sport copy with a caution, a delete), refusals, and what is
  never shown (scores and results, release names, links, hashes, secrets). A tool never
  bypasses a check the web app relies on.
- **Destructive tools say so.** A tool that deletes or cannot be undone tells the assistant, in
  its description, to confirm the exact scope with the person first. It is built so the
  scope it acts on is the scope that was previewed.
- **Documented and tested.** Each tool gets a row in docs/MCP.md and a test that drives it
  against the real router.

## Other rules

- This is a public repository: no IP addresses, user names, host names, or personal or local
  paths in code, tests, docs or commit messages.
- Movies and series must keep working exactly as before when a new kind of media (such as
  sport) is added; give every new `MediaId` arm an explicit branch.
- Results and scores are never shown for sport (no spoilers).
- CI pins the Rust toolchain (see `.github/workflows`); run `cargo fmt`, `cargo clippy` and
  `cargo test` on it before asking for review. Windows is built in CI, so keep Unix-only code
  behind `cfg(unix)`.
