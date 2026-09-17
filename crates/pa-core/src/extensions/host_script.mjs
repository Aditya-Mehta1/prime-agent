// Extension host script, stage 1 (protocol 1).
//
// The Rust half of this pair is `crates/pa-core/src/extensions/`
// (`host.rs` spawns this script and owns the lifecycle; `client.rs` + 
// `framing.rs` own the dispatch). Design: `docs/extensions-runner-design.md`
// section 2.2. The protocol is private and versioned; both ends ship in the
// same release, so they always match.
//
// Stage 1 is the protocol peer only: handshake, ping, event no-op, shutdown.
// Extension module loading (vendored jiti/static + the pi API shim) is
// stage 2 and rewrites this bundle; until then `hello` always reports zero
// extensions.
//
// Wire: newline-delimited JSON over stdio, both directions.
//   host -> sidecar: `{"id":N,"method":...,"params":...}` requests,
//                    `{"method":...,"params":...}` notifications (no reply).
//   sidecar -> host: `{"id":N,"result":...}` / `{"id":N,"error":{...}}` replies.
// Replies are serialized in request order; handlers run one at a time so a
// hanging `event` cannot starve a later `shutdown`.

import readline from "node:readline";

// Keep in sync with `pa_types::extension_rpc::EXTENSION_RPC_PROTOCOL`.
const PROTOCOL = 1;
// Keep in sync with `EXTENSION_HOST_NODE_MAJOR_FLOOR`.
const NODE_FLOOR = 18;

function writeReply(id, result, error, done) {
  const envelope = error !== undefined ? { id, error } : { id, result };
  process.stdout.write(JSON.stringify(envelope) + "\n", done);
}

function nodeMajor() {
  return Number(process.versions.node.split(".")[0] || 0);
}

async function handleRequest(msg) {
  switch (msg.method) {
    case "hello": {
      const params = msg.params || {};
      if (params.protocol !== PROTOCOL) {
        return {
          error: {
            message:
              "extension host protocol mismatch: sidecar speaks " +
              PROTOCOL +
              ", host speaks " +
              params.protocol,
          },
        };
      }
      if (nodeMajor() < NODE_FLOOR) {
        return {
          error: {
            message:
              "extension host requires Node >= " +
              NODE_FLOOR +
              " (found " +
              process.versions.node +
              ")",
          },
        };
      }
      // Stage 2 loads extension modules here; stage 1 registers nothing.
      return { result: { protocol: PROTOCOL, extensions: [], errors: [] } };
    }
    case "ping":
      return { result: { ok: true } };
    case "event":
      // No handlers exist yet (stage 3 adds real dispatch); the
      // accumulated event result is null, matching an event with no
      // handlers.
      return { result: null };
    case "shutdown":
      // Reply first, exit after the reply flushes. The host waits briefly
      // and kills us if this never happens.
      return { result: { ok: true }, exit: 0 };
    default:
      return {
        error: { message: "extension host received unknown method '" + msg.method + "'" },
      };
  }
}

const rl = readline.createInterface({ input: process.stdin, terminal: false });
let queue = Promise.resolve();

rl.on("line", (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch (err) {
    process.stderr.write("extension host: unparsable line: " + line + " (" + err + ")\n");
    process.exit(1);
  }
  if (!("id" in msg)) {
    // Notifications from the host (`cancel` in stage 1): there is no
    // in-flight dispatch to cancel yet, so they are no-ops.
    return;
  }
  queue = queue
    .then(() => handleRequest(msg))
    .then((outcome) => {
      writeReply(msg.id, outcome.result, outcome.error, () => {
        if (outcome.exit !== undefined) {
          process.exit(outcome.exit);
        }
      });
    })
    .catch((err) => {
      writeReply(msg.id, undefined, { message: String((err && err.message) || err) });
    });
});

// Stdin closed: the host is gone, stop.
rl.on("close", () => process.exit(0));
