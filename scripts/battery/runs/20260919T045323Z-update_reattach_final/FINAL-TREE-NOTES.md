# Final merged-tree evidence (dfb340f)

All three batteries rerun against a release binary built from the final
merged tree (dfb340f = slice-6 + main #164's update_flow status timing
fix, whose overlapping test fixes superseded this branch's parallel ones -
status.rs/successor.rs/swap.rs resolved to main's reviewed versions).

- update_reattach (T1/G4/K1/K2): all green.
- update_staged rerun (F1/F2/G1/G2/G3/P1/P2/P3/D1): all green.
- update_prepare rerun (P1-P4/D1-D4/E1-E3): all green.

Gates on the same tree (Prime sandbox, rust:1.98.1-bookworm):
fmt PASS, clippy --workspace --all-targets -D warnings PASS,
cargo test -p pa-core -p pa-daemon + -p pa-cli --lib --bins PASS
(includes the update_flow tests both #164 and this branch fixed),
release build PASS.
