

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
