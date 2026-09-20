# System Router GBA demo (node-mgba)

This example drives the [Prime Agent system router](../../docs/system-router.md)
against a real Game Boy Advance ROM through
[ARISE-Foundation/node-mgba](https://github.com/ARISE-Foundation/node-mgba), the
headless libmGBA binding (1,000+ FPS headless, button injection, direct memory
reads, PNG capture).

- `adapter.mjs` — the environment adapter: loads the ROM, saves a reset state,
  observes memory (EWRAM digest + configurable reads), presses buttons, optional
  PNG screenshots. It speaks the router's JSON-lines adapter protocol and
  supplies the default GBA action space (`press_a`, `press_b`, `press_l`, `press_r`,
`press_up`, `press_down`, `press_left`, `press_right`, `press_start`, `press_select`, plus `wait`).
- `demo.mjs` — a local demo driver that runs the real router loop (compiled
  `dist/`) with a scripted decision plan, so the full plumbing — emulator,
  adapter protocol, budgets, trace — runs without a model.

The ROM is yours and stays out of the repository: pass its path with
`--rom`/`SYSTEM_ROUTER_ROM`. Nothing in CI installs node-mgba or reads a ROM.

## Platform

node-mgba publishes prebuilt native shims for Linux and Windows x64 only
(`"os": ["linux", "win32"], "cpu": ["x64"]`); `npm install node-mgba` fails on
macOS with `EBADPLATFORM` (verified on darwin-arm64). The adapter therefore runs
inside a Linux container on macOS while the router loop stays on the host —
the adapter is a subprocess, so the boundary is the container command.

## Run (Linux x64)

```sh
npm run build   # once, from the repository root
cd packages/coding-agent/examples/system-router-gba
npm install node-mgba@0.2.9   # keep this out of the repository's package.json
cd ../../..   # back to the repository root: the default adapter path is root-relative
node packages/coding-agent/examples/system-router-gba/demo.mjs \
  --rom /path/to/your.gba --plan press_a,wait,press_start,finish
```

(The demo spawns the adapter itself via the default `node,examples/system-router-gba/adapter.mjs` command.)

## Run (macOS, adapter in a Linux container)

```sh
npm run build
ROM_DIR=$(dirname "$SYSTEM_ROUTER_ROM")
node demo.mjs \
  --rom "$SYSTEM_ROUTER_ROM" \
  --plan press_a,wait,press_start,finish \
  --adapter "$(printf '["docker","run","--rm","-i","--platform","linux/amd64","-v","%s:/roms:ro","-e","SYSTEM_ROUTER_ROM=/roms/game.gba","-v","%s:/app","-w","/app","node:22-slim","sh","-c","cd /app/examples/system-router-gba && npm install node-mgba@0.2.9 && node adapter.mjs"]' "$ROM_DIR" "$PWD")"
```

## Model-driven steering (the real System 1 / System 2 split)

In a Prime Agent session, the session model is System 2 and the router skill
does the stepping. Ask the agent something like:

> Use the system-router skill to play the opening of the game. Action model
> internal/glm-5.3-fast, goal: get through the intro into the overworld. Run a
> 25-step segment, review the trace, then continue or adjust.

The agent calls `await system_router.run({...})` with the adapter command above
as the environment, reviews the returned trace between segments, and steers.
See `docs/system-router.md` for the full steering contract.
