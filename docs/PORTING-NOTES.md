### Merge with #207 (durable compaction_outcome seam)

- #207's typed `CompactionOutcomeReason`/`CompactionOutcomeKind` + the
  `record_compaction_outcome` seam supersede this lane's string-constant
  row factory; the overflow arm now records through it (durable append +
  live-context push + broadcast), like the threshold/requested arms.
- `emit_unsuccessful_compaction` gained a `custom_instructions` parameter:
  TS `_endCompactionUnsuccessfully` threads the consumed pending
  compaction's instructions onto the `compaction_end` event, which the
  overflow arm honors.
- Severity reconciliation (TS-exact, kept from this lane): automatic-arm
  failures carry NO `errorSeverity` on the wire (TS passes none); #207's
  e2e expectation encoded the pre-fix divergence and was updated.
- `AgentSession::last_assistant_message` skips trailing non-assistant rows
  (a compaction outcome disclosure) instead of matching them — without the
  fix, #207's live-context row push hid the stale overflow error from the
  pre-turn recovery arm.

## Overflow compact-and-retry arm (lane overflow-compact)

Reference: TS `core/agent-session.ts` (`_checkCompaction` Case 1,
`_runAutoCompaction` overflow/will-retry branches, `_endCompactionUnsuccessfully`,
`_overflowRecovery` resets) + `packages/ai/src/utils/overflow.ts` (the
classifier, already ported in pa-ai).

- The overflow arm fires at both TS boundaries: the settled-turn error path
  (`agent_end`) and before the next admitted prompt
  (`_runPreTurnCompaction`, whose Case 1 covers a stale overflow error from
  the previous run). The Rust turn loop (pa-daemon `run_turns`) wires the
  same two points around the #204 threshold arm.
- One recovery attempt per overflow (`_overflowRecovery`: idle → attempted
  → reported). Resets: prompt admission and every settled non-error
  assistant turn (TS resets at agent-run message starts and non-error
  assistant message ends). The retry re-issues the loop WITHOUT a new user
  message (TS `agent.continue()`): `run_model_turn` gains a `TurnAdmission`
  (fresh prompt vs continuation).
- Guards in TS order: the message may not predate the latest compaction
  boundary, `settings.enabled` (or a pending model-requested compaction,
  which the run consumes with its instructions), same-model, and the shared
  overflow classifier. The error turn leaves the loop context before the
  compaction and again after the rebuild (the kept tail re-adds it), both
  TS-exact drops.
- Failure surface (shared `_endCompactionUnsuccessfully` port, now also
  used by the threshold and requested arms): the durable `compaction_outcome`
  custom row first, then the `compaction_end` event; automatic-arm failures
  carry NO `errorSeverity` on the wire (TS passes none — the pre-existing
  threshold/requested `error` severity was a wire divergence, fixed here).
  Skip keeps the TS `warning`. The reported-overflow text is TS-verbatim:
  "Context overflow recovery failed after one compact-and-retry attempt.
  Try reducing context or switching to a larger-context model."
- Boundary with the quick-retry/failover loops (TS `_isRetryableError`
  excludes overflow): both pa-core drivers classify context-overflow
  failures as non-retryable and hand them to the compact-and-retry recovery.
- Residues left to other lanes: the pa-cli print path's own threshold loop
  (`print_runtime.rs`) has no overflow arm yet; the threshold/requested
  compaction arms do not feed `note_compaction` (adoption-telemetry seam)
  — the overflow arm does, matching the TS compaction_end counting.
- Verification: faux-driven daemon tests (the compact-and-retry cycle,
  retry recovery, skip warning, pre-turn stale-overflow recovery,
  non-overflow and disabled-settings negatives) and the f7_compaction
  battery rows (B-31: the scripted overflow probe drives both binaries and
  diffs the projected wire surface).

## Print-mode overflow compact-and-retry (lane print-overflow, 2026-09-20)

Reference: TS `core/agent-session.ts` `_checkCompaction` Case 1 (ported for
the interactive daemon path in the overflow-compact lane) driven by
`modes/print-mode.ts`'s `InProcessAgentConnection` — the TS print mode runs
the session's own turn-boundary checks, so the compact-and-retry recovery
applies there unchanged. Ground truth captured with the TS binary against
`scripts/battery/mock_provider.py` (scripted 400 `prompt is too long`
probes, isolated agent dirs, `--mode json`/text):

- Full cycle: `compaction_start` (reason "overflow") -> `compaction_end`
  (`willRetry: true`, result with `details`) -> the retried turn without a
  new user message -> the reported row pair (`message_start`/`message_end`
  custom `compaction_outcome`) then `compaction_end` (`willRetry: false`,
  the TS failure text, NO `errorSeverity`). Text mode: the assistant error
  line then the outcome row on stderr, exit 1. json mode: exit 0 — TS print
  mode never derives the json exit code from the terminal selection (only
  the autonomous gates or a thrown error do).
- Skip (nothing compactable): the dropped error turn leaves no primary —
  TS text mode prints ONLY the warning row and exits 0 (the Rust
  "No response produced." branch was an invented surface, removed here).
- Pre-turn recovery (`_runPreTurnCompaction`): a fresh process resuming a
  session whose context ends with an unresolved overflow (compaction was
  disabled in the earlier run) compacts BEFORE the admitted prompt, with
  `willRetry: true` on the end event — verified with `--continue`.
- In-process stale state: after a skip/failure the error turn is dropped
  from the loop context, so the next prompt's pre-turn arm no-ops (TS
  verified); a reported double-overflow leaves state `reported`, which
  no-ops the pre-turn arm without a second row.
- TS multi-prompt + retry race (not ported): TS `promptAndWait` can submit
  the next message while the retry's re-issued turn still streams
  (`Agent is already processing` error, exit 1). The Rust print loop awaits
  the retry to quiescence at the boundary, so the sequential print loop has
  no equivalent race; deliberately not replicated.

Port shape: `crates/pa-cli/src/print_boundary.rs` (`TurnBoundary`) owns the
print loop's boundary checks — the overflow arm (same guards as the daemon
arm: not-predating, enabled-or-pending-request, same-model, the shared
classifier; one attempt; the `continue_run` re-issue; the durable
`compaction_outcome` surface; json-mode events), the requested
compaction/refinement consumption, and the threshold arm. pa-core's
`consume_turn_boundary_requests` split into
`consume_pending_compaction`/`consume_pending_refinement` so the print
loop mirrors the TS `_checkCompaction` ordering exactly (the overflow arm
consumes a pending requested compaction when it runs; a reported arm
leaves it pending for the next boundary — TS-exact).

Known residues left to other lanes (verified against the TS binary, out of
this lane's scope): the print json stream has no `compaction_start`/
`compaction_end` events for the THRESHOLD and REQUESTED arms (only the
overflow arm emits them now); the threshold arm's skip/failure surface in
print mode still logs to stderr without the durable
`compaction_outcome` row the daemon records; TS print mode also emits
`refine_failed` (auto-refine after settled turns) which the Rust print
loop does not run.

## PR/git context (roadmap item 5, 2026-09-19)

Reference: TS `packages/coding-agent/src/utils/git.ts` (`captureGitContext`,
`runGit`) + `core/session-manager.ts` (`recordGitStateIfChanged`) +
`core/agent-session.ts` (`_emitExtensionEvent` recording git state on
`agent_start`/`agent_end`).

- Scope correction (audited against TS 0.9.5, installed binary and all repo
  branches): the TS product has NO PR-number extraction, NO review status,
  and never shells out to `gh`. Its git context is exactly three
  repo-identity fields (repoUrl, commit, branch). Porting a PR/review
  surface would violate the parity rule ("if Rust shows something TS does
  not, that is also a parity bug"), so this lane implements the TS strategy
  only. A future PR/review surface needs a TS release that carries it (or
  an explicit product decision against TS).
- `capture_git_context` (pa-core session/manager.rs) now matches TS
  byte-for-byte in behavior: `git --no-optional-locks` probes with
  ignore/pipe/ignore stdio; `branch --show-current` (detached HEAD yields no
  branch, not "HEAD"); every field independently optional with the context
  present when ANY probe succeeds (a fresh repo without commits still
  reports its branch); the remote URL normalized through the shared
  `packages::parse_git_url` port (same primitive TS `parseGitUrl` provides)
  and kept verbatim when it does not parse (scp-like ssh remotes).
- Git state recording is wired where the TS has it: the session engine's
  persistence listener records a `git_state` entry on `agent_start` and
  `agent_end` (the TS `_emitExtensionEvent` calls
  `recordGitStateIfChanged` on both), so a commit or branch switch made
  during a run (e.g. via bash) lands in the session file. The unchanged
  context dedupes via the leaf-to-root walk, exactly like TS.
- The TS `FooterDataProvider` (git-branch watching for extension custom
  footers) is intentionally NOT ported: the Rust TUI has no extension UI
  API yet, the TS `FooterComponent` renders nothing by default (footer is
  intentionally empty in the prime brand), so the port would be dead code
  with no consumer. It ports with the extension UI surface when that lane
  lands.
- User-visible surface audit (why there is no pane-capture evidence): the
  TS TUI shows no git/PR information anywhere by default (top bar = chat
  name + spend, tray = goal/heartbeat/model/context %, footer = empty);
  git context is data-only (session header `git` field, `git_state`
  entries, trace upload headers). The Rust port matches: same data, no
  new visible UI.
- Verifiers: `crates/pa-core/tests/git_context.rs` (fixture-repo capture:
  branch+commit+normalized URL, detached HEAD, no origin, scp-like ssh
  verbatim, non-git dir, fresh repo, dirty tree; session lifecycle:
  header capture, no-change dedupe, commit-change entry, branch-path
  re-record, git_state stays out of the LLM context; byte-parity pin of a
  real TS-binary session header) + `session_engine` run-boundary test.
- Parity evidence: `prime-agent -p --session-dir ... "say hi"` in a
  fixture repo (https remote, one commit) writes the header git context
  `{"repoUrl":"https://github.com/acme/widgets.git","commit":"5a0875...",
  "branch":"main"}` and zero git_state entries (unchanged context); the
  pinned test replays that exact header line through the Rust parser.

## Update boot sweep + roster restore + re-arm (slice 5, 2026-09-19)

Reference: `docs/update-flow-state-machine.md` (§6/§8/§10) over the TS
`restoreDaemonUpdateRestart` (package-manager-cli.ts) and the TS
supervisor's scheduled-wake machinery (daemon-supervisor.ts).

- The supervisor owns the boot side (spec §3/§6): the sweep, the
  roster-via-env restore, and the scheduled-work re-arm. TS drove restore
  from the coordinator over the client wire (`create` replay + a
  `restore_actions` RPC + a continuation `prompt`); the Rust design keeps
  the durable truth in the workers' recovery journals and session files, so
  restore is create-or-adopt in place: kept descriptors relaunch (the
  slice-3 adoption pass) and the supervisor's restore pass covers the rest
  from the roster row's captured create command.
- Divergences (spec, recorded):
  - The TS `restore_actions`/`resume_queue` RPCs are not transcribed: the
    worker's create replay rehydrates the session store and the persisted
    queue snapshot from the recovery journal, which IS the restore for
    both the relaunch and the roster-row create path.
  - The boot sweep (§6 step 1) is unconditional - it deletes a live
    coordinator's `status.json` scratch file too. The pa-cli status writer
    recreates the parent dir on every persist and the CLI tail graces a
    mid-tail missing file (`TAIL_SWEEP_GRACE_MS`); the coordinator's epoch
    keeps rising so late writes cannot regress state.
  - The scheduled-work re-arm is a boot pass (and a post-restore pass):
    due active jobs of sessions with no live worker are woken once
    (create-by-session-file, client id `scheduled-wake`, TS literal).
    Live sessions need no wake - their in-process scheduler claims due
    jobs itself. TS's permanent recompute-on-change wake timer
    (`recomputeScheduledSessionWake` on job mutations) is not transcribed
    in this slice: it is a general (non-update) daemon feature.
  - The continuation treatment sends the TS
    `UPDATE_RESTART_CONTINUATION_PROMPT` verbatim to a restored row that
    was mid-turn (`in_flight.streaming`); a queued-work row counts as
    resumed via its journal replay. The TS update-complete
    `append_custom_message` notice (origin-session marker) is slice 6's
    UX surface.
  - `hello.update_resume` (§10.3) is a Rust-only extension over the TS
    hello (the TS close frame carries no resume contract); TS clients
    ignore unknown hello fields.
  - The `update_restore_status` RPC (the coordinator's `Restoring` report
    input) is Rust-owned wire, like the slice-2/3 prepare/commit RPCs.
- Ownership: pa-daemon `update_restore.rs` (sweep, RestoreProgress, restore
  pass, re-arm, queued-attach + status surfaces); pa-cli `update_flow`
  (status-writer sweep survival, tail grace, restore report poll);
  pa-types (the `update_restore_status` command, the hello
  `update_resume` contract type).

## Update staged activation + coordinator (slice 4, 2026-09-19)

Reference: `docs/update-flow-state-machine.md` (§3/§4/§7/§9) over the TS
`daemon-update-restart.ts`, `native-update.ts`, `version-check.ts`,
`native-installation.ts`.

- The TS coordinator was a daemon-restart-only helper: TS's `pi update` ran
  the npm/install.sh self-update first and the coordinator then stopped and
  restored the daemon (phases starting/preparing/stopping/starting_daemon/
  restoring/complete, `proper-lockfile` registry). The spec redesign moves
  the whole flow into one coordinator FSM (`Acquire..Complete`) and makes
  the binary swap a coordinator phase (`Activating`) instead of an
  installer side effect. The Rust port keeps the TS surfaces that ARE the
  contract - the status-file schema (camelCase, plus `updateId`/`state`/
  `epoch`), the 5 s heartbeat, the Join relay, the `--internal-update-
  restart-*` flags, the probe messages - and replaces the phase machine.
- Divergences (spec, recorded):
  - `intent.json` is the lock (spec §4) - not TS's separate
    `update-restart-coordinators/` registry with `proper-lockfile`; the
    joining process reads the holder's `status_path` from the intent
    record's `status_path` field.
  - Rust release payloads (installer-ci-design.md §5) do not ship TS's
    `package.json`/`install.sh`; staging writes `.archive-sha256` and
    `.install-source` itself and validates the Rust payload list. The
    coordinator owns the symlink swap; the TS install.sh recovery marker
    check is not transcribed.
  - `Restoring` first shipped as adoption-based counts measured from the
    live successor; slice 5 replaced the phase body with the supervisor's
    restore pass reported over the `update_restore_status` RPC (see the
    slice-5 section). Counts are measured, never faked.
  - `update --rollback` runs the same FSM with the previous release as the
    candidate (the launcher swap repoints `bin/prime-agent` at
    `bin/previous`'s target; `bin/previous` then names the rolled-back-from
    release): the spec's `Rollback` state stays reachable only from the
    after-stop failure paths (`Stopped`/`Activating`/`Booting`), which is
    exactly the state table pa-types carries.
  - Telemetry: the `update completed` adoption event is emitted by the
    invoking CLI at the terminal status (coordinator mode never emits);
    spec §13.6 wires the remaining update-flow UX events.
- Ownership: pa-core `update` module (version policy, install-root layout,
  manifest fetch, staging) is daemon-free client support; pa-cli
  `update_flow` owns the coordinator FSM, the status/intent writers, and
  the activation swap; pa-daemon is untouched by this slice (the prepare/
  commit/stop drivers are slices 2-3).


## Update graceful stop + roster (slice 3, 2026-09-19)

The TS-era worker prepare/commit/cancel frames (`worker_prepare_update`/
`worker_commit_update`/`worker_cancel_update`, `daemon-mode.ts`'s
`createUpdateRestartSession`) are NOT transcribed. The Rust update flow
(spec `docs/update-flow-state-machine.md` §5/§8, replacing the TS manifest
flow) splits them into: a read-only `update_snapshot` worker command (the
worker reports its queue lanes, in-flight flags, and durable session id,
and flushes its recovery journal before replying - no freeze, no cancel
round-trip; a busy session keeps running and the graceful-stop budget owns
the exit), and the existing acked `shutdown` worker command as the
graceful-stop frame (its handler is already the flush barrier: journal
record + telemetry finalize before the reply, then the process exits).
Key divergences from TS, deliberate:
- The roster's per-session `next_turn` is empty on this build: the Rust
  engine has no separate next-turn custom-message lane (pending prompts
  ride the steering/follow-up lanes, persisted to the worker recovery
  journal and restored on respawn); `queue.actions` carries the lane
  snapshot.
- The roster's `in_flight` granularity is the honest superset: provider
  streaming, bash work, and retries all live inside a busy turn, so
  `streaming` = busy and `bash_running`/`retrying`/`prompt_in_flight` are
  false (restore treats `busy` as the continuation signal). `rlm_children`
  comes from the spawn ledger (supervisor-side).
- TS `UPDATE_RESTART_WORKER_REQUEST_TIMEOUT_MS` (90 s) bounds the
  supervisor->worker snapshot RPC, always within the remaining prepare
  deadline.
- The stop budget maps to the spec §9 table: the acked request gets
  `worker_stop_ms` (30 s) and the exit wait gets `worker_stop_extension_ms`
  (30 s); a miss on either ABANDONS the update (the supervisor resumes
  Serving, refused sessions untouched, stopped workers relaunched) - no
  SIGKILL of a session, ever.
- The all-stopped exit keeps worker descriptors on disk (the new
  supervisor's create-or-adopt restore), unlike `begin_shutdown` which
  deletes them for a terminal stop.


## Update-prepare transaction (spec redesign over the TS prepare RPC, 2026-09-18)

`pa-daemon/src/update_prepare.rs` is a deliberate divergence from the TS
`daemon-supervisor.ts` `prepareUpdateRestart` path (documented in
`docs/update-flow-state-machine.md` §2/§5, not a transcription): TS runs the whole
prepare as one blocking in-process RPC (drain -> fence -> worker prepare -> manifest
persist -> commit -> stop) with a single 90 s deadline and no recovery state, so a
coordinator death between persisting and clearing its fence wedges the next boot.
The Rust side keeps the TS vocabulary and wire compat but restructures it:

- `prepare_update_restart` is idempotent on `updateId` (a repeat reports the current
  state; a different id is a typed refusal — TS refused concurrent prepares with the
  plain `"Daemon is already preparing an update restart"` string, which is kept as the
  message and now carries `DaemonErrorInfo::UpdatePrepareRefused`).
- The admission gate, mutation-drain latch (`MutationDrainLatch`, TS
  `mutation-drain-latch.ts`), gate refusal string, and the TS `UPDATE_RESTART_DRAIN_COMMANDS`
  pass-through during `Draining` are ports; the drain commands table and
  `READ_ONLY_DAEMON_COMMANDS` classification live in `pa-types::daemon::plane`
  (TS `daemon-protocol.ts`).
- Watchdog states (`Fenced`/`Snapshotted` TS never had) carry a durable
  `prepared/<update-id>/marker.json` self-expiry (45 s) in addition to the hard
  90 s prepare deadline, re-checked on a timer and on any later command, with
  `Aborted -> Serving` as the failure default — the structural fix for the wedged
  prepare.

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


## System prompt: layered redesign supersedes TS-prompt parity (roadmap item 3, 2026-09-18)

The Rust product ships the redesigned layered system prompt as its native prompt
(adopting Sebastian's draft text), not the TS product's base prompt. TS-prompt
parity is explicitly superseded for the system prompt only; every other
model-surface row still compares against the TS binary:

- The prompt is assembled from human-editable layer files (`pa-core`
  `prompts/layers/`): `core.md` (harness description + the full programmatic-tool
  API), `usage.md` (mandatory rules), `opinionated.md` (overridable guidelines),
  `per_model.md` (per-model instruction map, shipped empty). These form the
  cache-stable prefix.
- Every session-specific value (packages, project context, skills inventory,
  MCP servers, environment, session role) is appended strictly after the prefix
  as the dynamic tail, so providers can cache the prefix across sessions.
- `prime-agent prompt [--model] [--cwd] [--json]` dumps the fully-assembled
  effective prompt with the per-layer breakdown (cached prefix vs dynamic tail).
- The golden system-prompt test now pins the Rust prompt itself
  (`PA_UPDATE_GOLDEN=1` regenerates) instead of the TS text; the battery f2 row
  checks the layered shape (static layers, then the dynamic tail) and keeps the
  raw TS prompt in `protocol-request-diff.txt` as reference evidence. The
  `prompt` CLI command is Rust-only until the TS product adopts one; the
  differential CLI corpus normalizes it out of the help comparisons.

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

## Compaction outcome rows (2026-09-20)

Reference: TS `core/messages.ts` (`createCompactionOutcomeMessage`,
`convertToLlm`), `core/agent-session.ts` (`_endCompactionUnsuccessfully`,
`_persistCompactionOutcome`, `_mergeUnpersistedOutcomes`, `compact`), and the
TS suite pin `agent-session-compaction.test.ts`
("emits a warning and persists the outcome outside model context").

- The durable `compaction_outcome` row is a user-facing disclosure, NEVER
  model context: TS `convertToLlm` filters it (the TS test asserts
  `convertToLlm([outcome]) === []`), so the KV-cacheable provider prefix is
  unaffected. The Rust `convert_to_llm` had the exclusion already; the seam
  (`AgentSession::record_compaction_outcome`) pins it in tests. The live
  push mirrors TS `agent.state.messages.push` (the loop's default converter
  filters custom rows out of the provider request).
- Manual `/compact` records NO outcome row (TS `compact()` emits
  `compaction_end` and throws to the caller; only `_runAutoCompaction` ->
  `_endCompactionUnsuccessfully` persists). The Rust manual paths
  (compaction.rs, the `/compact` session command) stay event-only by design.
- The TS `_unpersistedOutcomes` fallback (a failed session-file append keeps
  the row in memory, timestamp-merged into rebuilt contexts) is held
  structurally in Rust: `SessionManager` keeps fire-and-forthing persistence
  (a failed disk write cannot remove the in-memory entry), and every
  in-process rebuild reads that entry chain. The failed-write test pins the
  guarantee.
- Parity fix found by this lane: TS throws `Summarization failed: <error>`
  when the compaction summarizer returns an error-stop assistant message
  (compaction.ts); the Rust `execute_compaction` ignored `stop_reason` and
  "succeeded" with an empty summary. The check is now ported (the failure
  arm of every auto-compaction path).
- The overflow arm stays unported (no Rust overflow recovery path yet); the
  reason vocabulary (`threshold`/`overflow`/`requested`,
  `skipped`/`cancelled`/`failed`) rides the row details exactly like TS.
