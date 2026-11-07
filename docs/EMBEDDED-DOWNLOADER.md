# Embedded downloader (librqbit 9.0.1)

The embedded downloader is an optional download backend: librqbit's BitTorrent engine runs
inside the `arr` process instead of qBittorrent.
- **Dependency.** `librqbit = { version = "=9.0.1", default-features = false, features =
  ["rust-tls"] }`, used through `librqbit::Session`.
- **No daemon or UI.** There is no rqbit HTTP daemon, web UI, extra frontend or stub.
- **Selection.** It is selected by an `[embedded]` section; then no qBittorrent section,
  URL, credential or API is involved.
- **Shared paths.** Acquisition, monitoring, import, cleanup and the coexistence gates are
  the same code paths as with qBittorrent, through a small client enum (`src/client.rs`).
  The qBittorrent backend is unchanged.

**Status.** Implemented and tested against real BitTorrent peers on loopback. That means
authored torrents over real playable media and controlled peers in the test process. It is
not production evidence, and no performance claim is made.

## Configuration

`[embedded]` replaces `[qbittorrent]`. Configuring both is refused, and there is never a
fallback from one to the other. Execution needs exactly one of them.

```toml
[execution]
enabled = true                       # the engine runs only with execution enabled
download_cleanup = true

[embedded]
download_dir = "/data/um-embedded"   # required: absolute, dedicated (see the rules below)
listen = "0.0.0.0:6881"              # required: BitTorrent TCP listen address (port 0 = random)
trackers = true                      # default true: announce to the torrent's trackers
dht = false                          # default false: mainline DHT (state in STATE/embedded/dht.json)
local_discovery = false              # default false: LSD multicast
initial_peers = []                   # default none: explicit peers for every torrent (controlled swarms)
# peer_limit = 50                    # per torrent, 1..1000
# upload_limit_bytes_per_sec = 1048576     # session-wide, positive; omit for none
# download_limit_bytes_per_sec = 4194304   # session-wide, positive; omit for none
max_active_downloads = 3             # 1..100; further grabs wait in the outbox, no attempt counted
# ratio_limit = 2.0                  # 0..1000: share limit when the indexer sets no seed ratio
# seeding_time_limit_minutes = 4320  # 0..5256000: same, for seeding time
metadata_timeout_secs = 60           # 5..600: magnet metadata wait
# movie_category = "um-movies"       # labels; default: the imported native client category
# tv_category = "um-tv"
```

**Refused at load:**
- both backends configured;
- execution with no client;
- a relative `download_dir`, or `/`;
- a `download_dir` inside or around the state directory, or overlapping a library root;
- a malformed `listen` or `initial_peers` entry;
- a zero rate limit, or an out-of-range limit;
- unknown keys (for example `upnp`; UPnP is never enabled).

**Refused at engine start** (and so at `serve` startup):
- a symlink anywhere in `download_dir` or its ancestors;
- a second engine on the same state directory;
- a changed `download_dir` for a state that already has one;
- persisted engine torrents that do not match this app's records;
- a listen address in use.

## Startup, state and binding

**When it runs.** `arr serve` starts the engine only with `[execution] enabled =
true`. In shadow mode there is no engine client at all: no BitTorrent listener opens and
no saved transfer resumes. `check` never starts it.

**Order at startup.**
1. The API socket is bound first: with the API port taken, `serve` exits before any engine
   or worker starts.
2. The engine starts. A failure after the session was created stops the session again, so
   nothing is left running.
3. Only then does the worker start.

**Order at SIGINT/SIGTERM.**
1. HTTP stops.
2. The worker finishes its current pass and exits (it is aborted after 120 s).
3. The engine records every torrent's counters and stops its session.
4. Running piece checks finish.
5. Only then is the state's engine lock released.

The same happens when HTTP serving itself fails.

**State under `STATE/embedded/`** (private, mode 0700):
- `session/`: librqbit's JSON persistence and fast-resume bitfields;
- `dht.json`: only with `dht = true`;
- `engine.lock`: an exclusive `flock`. The engine object holds it from start until its
  shutdown has finished (session stopped, piece checks drained). It is also released when
  the object is dropped, or when the process dies.

**Bindings** (in `STATE/state.sqlite` `meta`):
- **`download_backend`.** Set by the first start with a client. A state is never run with
  the other backend: its grabs are torrents of one client and are never reinterpreted. A
  state with grabs from before this binding existed is qBittorrent's. Use a fresh state
  directory for the embedded backend.
- **`embedded_download_dir`.** Set by the first engine start. Persisted torrents resume at
  the paths they were added with, so a different `download_dir` is refused.

**Before the engine restores anything,** every torrent in its `session.json` must match this
app's record: same info-hash, not removed, the exact recorded folder, below `download_dir`,
reached without symlinks. Otherwise nothing starts.

## Mapping of the imported native client template

The imported download client stays a policy copy; no live qBittorrent is needed. The
template must be qBittorrent's (its fields are what is mapped).

| template field | embedded engine |
|---|---|
| category | label of the grab (override with `embedded.movie_category` / `tv_category`) |
| initialState 0 / 1 (force start) / 2 (paused) | start / start (there is no client queue to bypass) / add paused |
| contentLayout 0 or 1 (Original) | Original layout (below) |
| contentLayout 2 (Subfolder) | refused: `client_not_configured` |
| sequentialOrder, firstAndLast | refused: `client_not_configured` |
| removeCompletedDownloads / removeFailedDownloads | the same cleanup policy as with qBittorrent |
| indexer seed ratio / seed time | per-torrent share limits, enforced by this app |

The coexistence "category shared with a native client" check applies only to qBittorrent.
That shared client is what exposes downloads to the native apps; the embedded engine is
private to this process.

## Adding, layout and data safety

**Adding.**
- **Payload.** A `.torrent` (fetched by the existing `resolve_payload`), or a validated
  magnet. A magnet's metadata is first fetched from peers in list-only mode, so nothing is
  written yet. Its info-hash must match.
- **Validation.** Names are checked with the engine's own parser, so the names checked are
  the names written. Refused: path traversal, separators in components, unsafe names, and
  BEP-47 symlink entries.
- **Record first.** A durable record (identity, category, `unified-media` tag, share limits)
  is written before the engine sees the torrent. Reconciliation by hash then works exactly
  as with qBittorrent: a torrent without this app's record is foreign and never touched.

**Layout** (as qBittorrent's Original layout):
- a single-file torrent goes in `download_dir/`;
- a multi-file torrent goes in `download_dir/<name>/`.

**What is never done:**
- writing through a symlink: every existing component from `download_dir` to the output
  folder and to each file is checked;
- overwriting a file the engine did not create (admission, below).

**Admission.** A record alone proves nothing about the files at its destination. It may be
a reservation left by a crash before the engine opened anything.
- **When a record is admitted.** Its `admitted_at` is set only once the engine has accepted
  the torrent without overwriting anything, or has restored it from its own persistence
  (which only follows such an add).
- **Resuming over existing files.** Only an admitted, active record of the same torrent at
  the same folder may resume over existing files: the engine's own partial or complete
  data.
- **Any other add** first checks every destination file name. An existing file is refused,
  is not recorded, and stays untouched.
- **A failed add** that was not a trusted resume removes its record again.

**A released record acquired again** (after `DELETE /library`, cleanup or a forget-only
cancel) starts a new lifecycle:
- lifetime counters, completion, the share-limit stop, verification and admission all
  reset;
- the same active transfer resumed (for example after its engine persistence was lost)
  keeps its counters.

Data kept by a forget-only cancel is not reused by a later grab: its files are not admitted
any more, so the grab is refused until they are removed.

**Hash verification before import.** The engine checks every piece as it arrives. After a
restart, though, it checks only the first claimed piece of each file and a small random
sample against its fast-resume bitfield (librqbit 9.0.1). So this app checks every piece
from the files on disk, streaming, before a completed torrent is reported complete:
- every file must be a regular file of exactly its length, reached without symlinks;
- until that check passes, the torrent is reported `checking`, so nothing is imported;
- a mismatch is reported as an error.

**When it runs again.**
- **Every engine start, before restore.** The check of every completed download not yet
  imported is reset, because its files could have changed while the engine was stopped.
  Imported downloads keep theirs: the library has the data. Transfer counters are never
  touched.
- **Stale results.** Each check run carries a fresh token, issued and bound to the record
  under the serialised engine operations before the check's task is spawned. The task only
  reads the pieces and records its result if its token is still the record's. Removal,
  re-adding and a new engine session clear the token, so a late result never vouches for a
  different transfer, and no check binds itself to one later.

**Import gate.** Import also requires the client to report a complete state, for both
backends: a torrent being checked, or in error, waits even at 100 %.

**Removal.**
- The engine only forgets the torrent; it never deletes data.
- For torrent-and-data removals, and for cleanup, the existing journaled removal deletes
  exactly the tracked files, and only while each is the same regular file (same inode and
  size), confined and not symlinked.
- Library files are hardlinks or copies and stay, with both forget-only and
  torrent-and-data removal.

## Engine versus app durability, and the metrics reported

| what | where | across restarts |
|---|---|---|
| torrent list, paused flag, output folder, fast-resume | the engine (`STATE/embedded/session`) | restored by the engine after the checks above |
| identity, ownership, share limits, verification | `embedded_torrents` (SQLite) | durable |
| lifetime uploaded / downloaded bytes, seeding seconds | `embedded_torrents` (SQLite) | durable; never reset |
| upload/download counters of a live torrent | the engine, in memory | restart at 0 at every pause and session start |

**Counters.**
- **Increments only.** Each observation (every worker pass, plus before a pause, removal or
  shutdown) adds only what grew within the same live state of the engine. An abrupt stop
  can lose the bytes moved since the last observation; nothing is ever invented.
- **One reading of one live state.** Counters are read once per observation from one owned
  live state (`stats_snapshot()`), never paired with another one.
- **Stable identity.** A live state is identified by a weak reference, so its address
  cannot be reused while it is remembered.
- **Serialised.** Observations and the engine controls (add, pause, resume, removal,
  shutdown) are serialised by one lock. Concurrent controls therefore cannot attribute one
  live state's bytes to another.
- **Replayed adds.** An add of a torrent the engine already manages (a worker retry after a
  crash) changes nothing: the record keeps its counters and verification, and the
  observation of the live state stays, so its bytes are not counted a second time. It is
  only confirmed admitted, as at a restore.
- **Seeding time.** It counts only intervals whose two ends saw the torrent complete and
  live in the same live state. Sub-second intervals add up through a millisecond carry.
  The carry is kept in memory, so a restart drops less than a second. Time is measured with
  a monotonic clock, so a suspended host adds nothing.
- **Ratio.** Lifetime uploaded divided by the larger of lifetime downloaded and the content
  size, so it is never above a full download's ratio.
- **Lost counters.** A counter that ever goes backwards marks the record invalid. Then
  ratio, uploaded, downloaded and seeding time are reported as unknown, and unknown share
  data never lets cleanup remove a download.

**Share limits.** Effective limits are the indexer goal recorded at grab time, else
`ratio_limit` / `seeding_time_limit_minutes`, else none. When either limit is met, the
engine pauses the torrent, as qBittorrent's pause action does. The unchanged cleanup logic
then sees a completed, stopped torrent with a met limit and removes the download.

**States.** These are reported in the shared vocabulary, from the engine's real state:

| engine | reported |
|---|---|
| initializing | `checking` |
| complete, piece check pending | `checking` |
| error, or a failed piece check | `error` |
| live, complete | `seeding` |
| live, no peers and no data | `stalled` |
| live | `downloading` |
| paused, complete | `completed_paused` |
| paused | `paused` |

**Stall timeout.** The worker's stall timeout does not depend on these labels. A download
reported `downloading` because peers are connected, but which makes no progress, times out
like a `stalled` one (see `ARCHITECTURE.md`, Stalls).
- **Progress** here is whole verified pieces (librqbit's `have_bytes`); speed alone never
  counts.
- **Time while the app, and so the engine, is not running** does not count.

**Other reported values.** Progress, size, bytes left, speed and ETA come from the engine.
The file list comes from the engine's metadata (padding entries skipped), with per-file
progress.

**Error privacy.** librqbit's own messages can carry tracker URLs (with passkeys), magnet
links or peer addresses. No engine failure is passed on as text: it is described by stage
and kind only, for example `fetching magnet metadata failed (engine error)` or
`starting the engine session failed (i/o: AddrInUse)`. This app's own refusals (lock,
download-directory binding, persisted state, symlinks, admission) are stated in full.

**Visibility.**
- `GET /api/v1/diagnostics` gives `download_client.backend` and `embedded`:
  - engine, status, listen address and transport (`tcp`);
  - the DHT, LSD, UPnP, PEX and tracker settings;
  - aggregates of the engine's own state: `torrents`, `engine_states` (checking,
    downloading, seeding, paused, error, complete), `live_peers`, and the measured
    `download_bytes_per_sec` / `upload_bytes_per_sec`.

  It is read-only: no counter is recorded. It shows no hashes, paths or peer addresses.
  `engine_states` are librqbit's states; the app's own pending piece check shows per torrent
  as `checking` in the queue.
- `GET /api/v1/capabilities` names the client-dependent capabilities `embedded_*`, plus
  `embedded_download_engine`, all `loopback_peers`. Evidence recorded for qBittorrent never
  counts for the engine.

**Networking.**
- **Off by default:** DHT and LSD. UPnP is never on.
- **Trackers:** on by default.
- **Transport:** TCP only, IPv4-only when `listen` is IPv4. librqbit can also do uTP; this
  app does not configure it.
- **Private torrents:** librqbit uses no DHT, LSD or PEX for them, and only their first
  tracker.
- **PEX** cannot be disabled in librqbit 9.0.1 for public torrents. It only learns
  addresses from peers already connected.

## Tests

The engine tests need `/usr/bin/ffmpeg` and `/usr/bin/ffprobe`. They use loopback only, and
each test uses its own temporary directories and ephemeral ports.

```bash
cargo test --locked --test embedded                    # 14 tests, about 6 s
cargo test --locked --test embedded -- --ignored       # opt-in: the real binary, SIGKILL mid-download
cargo test --locked --lib embedded                      # piece check, inspection, counter deltas, error privacy, check tokens
cargo test --locked --lib embedded_section              # configuration
cargo test --locked --test embedded_parent_review --test embedded_process   # review regression cases
```

- **`a_new_movie_arrives_over_bittorrent_imports_and_seeds_to_ratio_two_across_restarts`.**
  - A TMDB movie new to the native apps is added in one coexistence call. Its multi-file
    torrent (video, sample, nfo) downloads from the controlled peer, whose upload counter
    proves the bytes went over TCP.
  - Every piece is verified, then the movie is imported, hardlinked and playable, without
    the sample.
  - Diagnostics and capabilities report the engine, and nothing names qBittorrent.
  - Controlled leechers then download from the app's engine only. Across a graceful
    restart the persisted counters are unchanged; across an abrupt engine stop they are
    kept and keep growing.
  - At ratio 2 the engine pauses the torrent and the normal cleanup removes the download;
    the library file stays playable. A replayed add downloads nothing.
- **`a_season_pack_pauses_resumes_and_survives_an_abrupt_engine_stop`.**
  - A Fargo S05 handoff is followed by the normal add. The 10-file season pack is rate
    limited so the steps land mid-download.
  - An API pause stops fetching; resume continues.
  - An abrupt engine stop mid-download, then a new process state: the engine re-checks the
    data, reconnects to the explicit peer and finishes. Ten episodes are imported.
- **`a_magnet_resolves_from_its_peer_and_unsafe_foreign_or_unaccounted_downloads_fail_closed`.**
  - A magnet-only release gets its metadata from the peer, then is imported.
  - Invalidated counters give an unknown ratio, and a torrent-and-data cancel is refused
    `seed_criteria_unmet`.
  - A foreign file at a new torrent's destination: the grab fails (`invalid_torrent`), and
    the file is byte-for-byte untouched.
- **`grabs_beyond_the_active_download_limit_wait_in_the_outbox`.** With
  `max_active_downloads = 1`, the second grab stays queued with no attempt until the first
  finishes downloading, then is imported.
- **`a_state_is_bound_to_its_first_download_backend`.** A state bound to the embedded
  engine refuses `[qbittorrent]`. Also, no engine starts until it is used.
- **`the_server_process_killed_mid_download_resumes_and_imports`** (opt-in). The real `serve`
  process with its worker is SIGKILLed mid-download and started again over the same state.
  The movie is imported byte-identical and playable.
- **`a_record_never_admitted_does_not_authorise_overwriting_a_file`.**
  - The state a crash leaves between recording and the engine opening its files: a record,
    never admitted, plus an unrelated file at the destination. The add is refused, and the
    file is byte-identical.
  - Once the destination is free, the engine creates the file and the record is admitted.
- **`a_released_record_acquired_again_starts_a_fresh_seeding_lifecycle`.** A released record
  whose earlier lifecycle had met its goal is acquired again. It comes back with zero
  counters, no completion, no share-limit stop, and a new admission.
- **`concurrent_controls_fabricate_no_bytes_and_seeding_time_accrues_below_a_second`.**
  - After a real upload, 60 observations and 20 pause/resume calls run concurrently with
    nobody connected: the lifetime upload is unchanged, the counters stay valid, and there
    is no deadlock.
  - Readings 300 ms apart while seeding add at least 2 s of seeding time.
  - Diagnostics show the aggregates and record nothing.
- **`a_replayed_add_of_an_active_torrent_counts_nothing_again_and_keeps_its_check`.**
  - After import, a new live state uploads one full copy to a controlled leecher and
    fetches nothing (ratio 1 of 2).
  - The same add is replayed three times, each followed by an observation. Uploaded and
    downloaded stay unchanged, the check stands, and the torrent keeps seeding with no
    share-limit stop.
  - Against the earlier code, which cleared the observation on every add, the first replay
    counted the live state's upload again (35409 to 70818 bytes, ratio 2).
- **`a_download_with_peers_connected_but_no_data_fails_at_the_stall_timeout`.**
  - The only peer is a raw loopback peer: it connects to the engine, claims every piece
    and never unchokes. The engine reports `downloading` (a peer connected, no data,
    progress 0), never `stalled`.
  - With `stall_timeout_minutes = 30` and simulated worker time: 20 min are observed, then
    the app stops for 100 min (the engine with it) and restarts. That time does not count,
    and the download is still downloading 9 min after the restart.
  - At 31 min observed, the download fails as `stalled` and the release is blocklisted
    with reason `stalled`. The normal cleanup of a failed incomplete download removes it
    from the engine.
  - Against the earlier poll, which timed only `stalled` and metadata labels, no stall
    clock ever started. Without the unanswered-time rule, the download failed at the first
    poll after the restart ("120 min").
- **`a_completed_payload_changed_while_stopped_is_checked_again_before_any_import`.**
  - A completed, verified, not yet imported download stops. One byte in the middle changes
    while the size and modification time stay, and the peer is gone.
  - After the restart, the earlier check no longer counts, and nothing is imported. In
    three runs librqbit's fast-resume missed the change and the app's full check caught it:
    "piece 1 does not match its hash".
- **`engine_failures_never_carry_its_own_text_out`.** A canary passkey in a magnet's tracker
  URL appears neither in the refusal of a malformed magnet nor in the engine's
  unresolvable-magnet failure.
- **`the_engine_lock_is_held_until_its_session_has_actually_stopped`.** While a first engine
  is shutting down, a second one on the same state is refused; once it has stopped, the
  second starts.
- **`an_occupied_api_port_starts_no_engine`.** The real `serve` binary, with its API port
  taken, exits without opening the engine listener or taking the engine lock.
- **Unit tests.**
  - The piece check accepts exact data and rejects a flipped byte, a longer file, a missing
    file and a symlink.
  - Inspection refuses traversal, unsafe names and symlink entries.
  - Counter deltas count only one live state's growth: a new live state counts from zero, a
    backwards counter is distrusted, and sub-second seeding time carries over.
  - Engine failures keep only stage and kind, even with a canary URL in the error chain.
  - `a_check_is_bound_before_it_runs_and_a_superseded_one_records_nothing`: the
    observation that first sees a transfer complete has bound its check's token before the
    check's task runs. After removal, and after the torrent is acquired again, the old
    token's result (pass or failure) records nothing, and no new check binds before the new
    transfer completes. With the bind moved back into the task, the test fails.
  - The configuration is explicit, exclusive and validated.
- **Review regression cases** (`tests/embedded_parent_review.rs`, 6;
  `tests/embedded_process.rs`, 1, a real-process SIGKILL with upload accounting):
  - shadow mode opens no listener;
  - a symlinked output folder is refused, and so is a symlinked root;
  - a second runtime on the same state is refused;
  - a changed `download_dir` is refused;
  - a tampered `session.json` is refused before restoration.

**Negative controls (earlier pass).** Each guard was disabled once, and the named test
failed. The admission, counter-identity, re-verification, error-privacy and lifecycle
invariants added later were not put through a mutation campaign. Their tests target the
earlier code's failure modes (an unadmitted record granting overwrite, a check surviving a
restart, a lock released before the stop finished), but were not run against that code.
The two latest fixes are the exception: the replayed-add and check-token tests were each
run once against the reverted fix and failed.

The guards disabled were:
- counters replaced instead of accumulated;
- new torrents allowed to overwrite;
- a ratio from invalid counters;
- no backend binding;
- no re-attachment of explicit peers after a restart;
- no pause at the share limit (cleanup then waited with "still seeding (the client has not
  stopped it at a share limit)");
- no outbox limit;
- no symlink check, no engine lock, no persisted-state check, no root check.

**Two guards no test isolates.** Counting a new live state's bytes before its first
observation (dropping them only undercounts). The download-directory binding (the
persisted-state check also refuses a moved directory).

## Migration and rollback contract

**Separate instances.**
- Run the embedded backend on a fresh, dedicated state directory and its own
  `download_dir`.
- Existing qBittorrent states, earlier deployments, their transfers, seed goals and ledgers
  are not touched.
- A state cannot switch backends.

**Seeding needs the process.** The engine seeds only while `serve` runs, so seeding time
accrues only then. A stopped server stops seeding until it starts again; torrents resume
(unpaused ones unpaused).

**Rollback of a scope.** This is the same API as before:
1. Leave or cancel the download: `remove: torrent` forgets it in the engine and keeps the
   data.
2. Release the scope (`DELETE /library`). It is refused while a download is active.
3. Restore native monitoring from the ledger.

The download data and the engine state live in the instance's own directories. Library
files are separate hardlinks or copies.

**Retiring an instance.** Stop it, then archive or remove its state directory and
`download_dir`. Remove the download data only after its imports are verified and no
seeding obligation remains.

## Limits

- **Not production evidence.** No live swarm, private tracker, real indexer or production
  library was used. Performance and memory at scale are unmeasured.
- **Restart evidence.**
  - **Tested:** process restarts, both graceful and SIGKILL (this suite's opt-in harness
    and `tests/embedded_process.rs`), plus an abrupt in-process engine stop.
  - **Not tested:** OS crash or power loss, where unflushed librqbit persistence or recent
    SQLite writes could be lost. The checks then refuse whatever does not match, or the
    full piece check re-runs; they never silently accept it.
- **Not implemented:** sequential download, first/last-piece priority and the Subfolder
  layout (refused); per-torrent rate limits (session-wide only); queue priorities (only the
  outbox limit). uTP is supported by librqbit, but this app configures TCP only.
- **Startup cost.**
  - With `initial_peers`, every incomplete torrent is re-added at engine start, which
    re-checks its data. This is needed because librqbit 9.0.1 neither persists explicit
    peers nor lets them be added later.
  - The full piece check reads the whole payload when a torrent completes, and again after
    each engine start for every completed download not yet imported.
- **Kept data is not reused.** Data kept by a forget-only cancel is not reused by a later
  grab of the same release (it is refused as unadmitted existing files).
- **Magnet wait.** Magnet metadata is fetched during submission, up to
  `metadata_timeout_secs`.
- **Counters are approximate.** Within one observation interval they undercount, never
  overcount.
- **Private torrents.** librqbit uses only the first tracker of a private multi-tracker
  torrent.
