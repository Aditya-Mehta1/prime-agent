# Evidence notes (trimmed)

The fixture install roots kept their full layout (releases/<ver>-<platform>-<sha>/,
bin symlinks, .archive-sha256, .install-source, package metadata) minus the
copied 130M binaries and the candidate tarball (byte-identical copies of the
Rust release binary / TS binary; sizes make them unshippable in git). All row
data lives in update_staged-report.json (status files, launcher targets,
counts, refusal texts) and the per-phase status.json files under
<phase>/agent/update-restarts/.
