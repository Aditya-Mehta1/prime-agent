

## Daemon discovery containment (operator directive, 2026-09-17)

The OS-level daemon discovery (`pa-cli` `daemon_discovery`) was killing this box's live
mission daemon: a test process that inherits the ambient `HOME`/`TMPDIR` resolves
`current_state_root()` onto the mission daemon's real socket dirs (`/tmp/prime-agent-1000`,
`/tmp/mission-tmp/prime-agent-1000`), so a scan or `--force` residual sweep found, probed,
and SIGTERM'd the mission supervisor's workers. Two structural fixes, both deliberate
divergences from the TS `cli/daemon-ps.ts` shape:

- Every scan, probe, unlink, and kill is scoped to an explicit `DaemonStateRoot` handed in
  by the caller (the CLI passes the env-resolved current root; tests pass only fixture
  directories they created). The root filter runs inside the OS census, before any probe or
  signal, so a daemon outside the given root is never even a candidate.
- `NEVER_TOUCH_SOCKET_DIRS` is a hard exclusion list checked unconditionally in the scan,
  probe, socket-dir sweep, and unlink paths, even when a state root deliberately points at
  them. It lists this sandbox's mission paths, including `/tmp/prime-agent-1000` — which is
  also the product-default socket dir for a uid-1000 Linux user with `TMPDIR=/tmp`. That
  product-parity trade-off is accepted for this mission sandbox per the operator directive;
  revisit before any release cut of the binary.


## Child-session stream hang (observed in production, 2026-09-16)

Three child sessions hung mid-turn: `isStreaming=true` with zero progress for 20+ minutes,
provider endpoint healthy (parent session streamed fine concurrently), steer-mode messages
ignored. Daemon restart was required. Relevance to the pa-daemon redesign:
- The supervisor must own per-worker stream deadlines: a worker whose provider stream makes no
  progress for N minutes must be aborted and the turn restarted with a bounded context, not left
  streaming forever. The TS daemon's quiescence wait "cancels" but does not kill the stream.
- A steer/interrupt arriving while a stream is hung must hard-abort the underlying provider request;
  queued nudges are not enough.
- Worktree/branch state survived every hang and restart; per-unit commits by workers are the
  mitigation while this bug exists in the TS daemon we operate under.


## Hang recurrence + supervisor adoption failures (2026-09-16, post-bounce)

- Child streams hung again ~15 min after a clean daemon bounce (same signature: streaming=true,
  zero message-count progress, endpoint healthy for parent). Steers queued but ignored.
- Supervisor log: after bounce, 5 of the old workers failed adoption ("Session worker process is no
  longer running"); workers are NOT idempotently recoverable across supervisor restarts.
- Design implications for pa-daemon: (1) adopt/recovery journal must cover in-flight provider
  streams, not only tool ops ("recovered without replaying uncertain operations" loses stream state);
  (2) hung child streams need a supervisor-side progress watchdog with hard abort; (3) supervisor
  restart must be able to re-spawn workers from durable descriptors rather than fail adoption.


## Attach event stream: deferred model-surface diffs (2026-09-17)

The f6 attach cross-side fingerprint (battery `run_battery.py`) locks the projected
event sequence: agent_start/turn_start, the user message_start+message_end pair,
assistant start/updates/end ordering, turn_end/agent_end presence, session_status.
Two TS wire behaviors are deliberately out of that row's scope and still open:

- The per-turn harness digest rides TS turns as a `custom` message pair
  (`message_start`/`message_end` with `customType: harness_digest`); the Rust
  session engine composes the digest into the request only and never projects it
  on the wire. Fixing this couples to custom-message session-entry parity
  (f3/f8 resume surfaces) — model-surface lane.
- TS `turn_end` carries the final assistant message (and `agent_end` the message
  list); the Rust worker emits both bare. Same coupling: the payload is the
  durable turn record, owned by the session engine.

Both are wire-projection only (no request/cache-prefix effect). The battery
filter that excludes them is annotated in `run_battery.py` and must be removed
when the model-surface lane lands custom-message wire parity.

## Lock convention: proper-lockfile directory locks (2026-09-17)

TS ground truth (`proper-lockfile` 4.1.2, used by auth.json / settings.json /
cron state): a lock is an EMPTY DIRECTORY at `<file>.lock`, mtime probed to
"next second + 5ms", judged stale from mtime alone (10s default, 30s cron),
reclaimed by rmdir + retry (one fresh attempt; a reappearing rival is
ELOCKED). Release rmdirs it. A regular FILE at the lock path is fatal to the
TS release path (ENOTDIR): pre-compat Rust flock files wedged real installs
by making `AuthStorage.reload()` fail, silently losing auth. `pa-core
platform::lock_dir` implements the convention; the Rust binary additionally
heals stale or unheld legacy lock FILES (a held legacy flock reads as
contention), which the TS binary cannot do.

Deliberate divergences (candidates for the lock-audit follow-up unit):
Rust does not refresh the held lock's mtime (TS updates every stale/2) and
does not detect compromise (TS `onCompromised`); Rust lock holds are short
read-modify-write cycles, so staleness takeover only sees genuinely crashed
holders. The cron `with_state_locks` still runs its action unlocked when
acquisition fails (pre-existing shape; now logged at warn) because the store
API has no failure channel; TS throws there.


## Kernel packaging lane notes

- Packaged layout (TS install.sh native path + copy-binary-assets.mjs): the
  release artifact is the binary plus exe-adjacent `package.json` (the
  version manifest: `{"version", "piConfig"}`), `prime-agent-runtime/` (the
  vendored sidecar), `skills/`, `docs/`, `README.md`, and `LICENSE`. Runtime
  resolution (`pa-core/src/kernel/bootstrap/venv.rs` `packaged_runtime_dir`,
  moved with #118's bootstrap split) is
  `PI_PACKAGE_DIR` -> binary directory -> `dist/` -> source-checkout root
  (TS `runtimeCandidateDirs` module-relative candidates; the compile-time
  workspace root replaces them and never resolves on a user machine).
- Relation to the installer-ci lane's `scripts/release/assemble_artifacts.py`
  (merged in #114): that script assembles the CI distribution tarball +
  `manifest.json` (release-pipeline contract, staged at `ci/workflows/`),
  while `scripts/package_release.py` is the TS-installer-packaging dry run
  (exe-adjacent layout, dev-cache exclusions, version pin, `SHA256SUMS` +
  `binaries.json`); `make release-dry-run` runs the former, `make package`
  the latter.
- `scripts/package_release.py` ports `assemble-release-archives.mjs` +
  `copy-binary-assets.mjs`: staging walk rejects symlinks and the TS
  exclusion set (`node_modules`, `.venv`, `__pycache__`, `*.pyc`,
  `*.egg-info`, caches, `.git`, `.DS_Store`), `validateBinaryAssets`-style
  required-asset checks, version pinning (the binary's compiled `--version`
  must equal the release version - cargo embeds it, where TS stamps
  `package.json` post-build via `setBinaryVersion`), `SHA256SUMS` +
  `binaries.json` (`{platform, file, sha256, executableSha256}`), and a
  flat tarball `prime-agent-<version>-<platform>.tar.gz`. `make package` is
  the entry point; `--root` re-anchors assets for the e2e's synthetic tree.
- `--version` reads the packaged `package.json` at runtime (TS `VERSION`
  is `getPackageJsonPath()`-based) with the compiled-in version as the
  fallback (dev checkouts). `--prime-agent-bootstrap` is the TS
  `runtime-bootstrap.ts` installer handoff: `ensureKernelPython` + prints
  `kernel python: <path>`; the TS fd/rg preloads (`ensureTool`) are not
  ported (no tools-manager in this build yet).
- Missing-sidecar failure UX: bootstrap failures keep the TS
  `formatBootstrapFailure` text and append a hint naming the executable
  directory when the packaged sidecar is absent (the registry fallback the
  TS keeps would otherwise surface a bare pip error; the runtime is not on
  a registry).
- Verifier: `crates/pa-cli/tests/packaged_layout_e2e.rs` - a staged layout
  boots a kernel session with `PI_PACKAGE_DIR` removed (the ipython cell
  runs with a live `rlm`, the staged marker skill reaches the skill
  inventory, the staged manifest reports the pinned version), the
  missing-sidecar and bad-override failure UX, and the packaging dry-run
  (staging, exclusions, version pin, `SHA256SUMS`/`binaries.json`, tarball
  integrity). The ignored test bootstraps a fresh venv from the packaged
  sidecar over uv + network.
- Print-mode faux scripts accept content-block entries (tool calls) through
  the shared `pa_ai::faux::script::parse_faux_script` (the daemon worker
  seam already used it), so binary-level e2e can script full kernel turns.
