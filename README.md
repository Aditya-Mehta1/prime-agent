# prime-agent-rs

Prime Agent, rewritten in Rust. Private until ready — see [MISSION.md](./MISSION.md) for the full mission brief.

Work happens in the `prime-agent-rust` Prime sandbox (permanent VM, state synced to `kevinjosethomas/prime-agent-state` every 15 minutes).

See ARCHITECTURE.md for the hard ownership rules: per-crate scope/non-goals/public APIs, one shared types crate, cycle-free layered dependencies, and the anti-god-module rule.
