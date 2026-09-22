# prime-agent-rs

Prime Agent, rewritten in Rust. Private until ready — see [MISSION.md](./MISSION.md) for the full mission brief.

Work happens in the `prime-agent-rust` Prime sandbox (permanent VM, state synced to `kevinjosethomas/prime-agent-state` every 15 minutes).

See ARCHITECTURE.md for the hard ownership rules: per-crate scope/non-goals/public APIs, one shared types crate, cycle-free layered dependencies, and the anti-god-module rule.

## Continuous builds (share with coworkers)

Every push to `main` republishes prebuilt binaries to the rolling
[`continuous` release](https://github.com/kevinjosethomas/prime-agent-rs/releases/tag/continuous).
The artifacts are overwritten per push, so the stable URLs below always serve
the latest build of `main`. No Rust toolchain is needed: the kernel runtime
sidecar (`prime-agent-runtime/`) ships inside the tarball, next to the binary
(the same exe-adjacent layout the TS installer expects).

Pick your platform, extract, and run:

```sh
# macOS Apple Silicon (M1/M2/M3/M4)
mkdir prime-agent && cd prime-agent &&   curl -fsSL https://github.com/kevinjosethomas/prime-agent-rs/releases/download/continuous/prime-agent-0.1.0-aarch64-apple-darwin.tar.gz | tar xz &&   ./prime-agent

# macOS Intel
mkdir prime-agent && cd prime-agent &&   curl -fsSL https://github.com/kevinjosethomas/prime-agent-rs/releases/download/continuous/prime-agent-0.1.0-x86_64-apple-darwin.tar.gz | tar xz &&   ./prime-agent

# Linux x64
mkdir prime-agent && cd prime-agent &&   curl -fsSL https://github.com/kevinjosethomas/prime-agent-rs/releases/download/continuous/prime-agent-0.1.0-x86_64-unknown-linux-gnu.tar.gz | tar xz &&   ./prime-agent

# Linux ARM64
mkdir prime-agent && cd prime-agent &&   curl -fsSL https://github.com/kevinjosethomas/prime-agent-rs/releases/download/continuous/prime-agent-0.1.0-aarch64-unknown-linux-gnu.tar.gz | tar xz &&   ./prime-agent
```

The repo is private, so anonymous `curl` gets a 404 — run `gh auth login` once
and let the GitHub CLI handle authentication (same archive, same layout):

```sh
mkdir prime-agent && cd prime-agent &&   gh release download continuous -R kevinjosethomas/prime-agent-rs     -p 'prime-agent-0.1.0-aarch64-apple-darwin.tar.gz' &&   tar xzf prime-agent-0.1.0-aarch64-apple-darwin.tar.gz && ./prime-agent
```

Notes:

- `./prime-agent --version` reports the exact commit the binary was built
  from (`0.1.0-continuous.<commit-sha>`) — report that string when filing
  issues. The release body also states "Built from `<sha>`" for the push.
- Verify the download against the release's `SHA256SUMS` if you want a
  checksum (same directory on the release page).
- The bundled runtime needs `uv` on PATH only when a kernel venv has to be
  (re)bootstrapped (`brew install uv` / `curl -LsSf https://astral.sh/uv/install.sh | sh`).
- The `0.1.0` in the asset names is the workspace version; it changes when
  `Cargo.toml` bumps and the README one-liners move with it (the
  `continuous` release drops the old names automatically).
