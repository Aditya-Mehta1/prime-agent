# Evidence notes (trimmed)

The fixture install roots kept their full layout (releases/<ver>-<platform>-<sha>/,
bin symlinks, .archive-sha256, .install-source, package metadata, per-phase
status files) minus the copied 130M binaries and the candidate tarball
(byte-identical copies of the Rust release binary / TS binary; unshippable in
git). All row data lives in update_staged-report.json and the per-phase
status.json files.

Slice-5 rows (spec §6/§10):
- G1 boot sweep: the update's successor daemon is stopped over the wire, stale
  scratch is planted (this socket's update-restarts/prepared subtree + the
  TS-era legacy daemon-update-restarts/ + daemon-update-restart.json), and a
  fresh normal boot removes all of it while still serving (I2 live). The
  hello capture shows the normal boot's restore/re-arm pass settling
  (complete flips false -> true).
- G2 hello resume contract: the update-boot hello carries the update id with
  complete=true; the settled normal boot carries no id.
- G3 scheduled-work re-arm: the side's own detached worker is killed by cwd
  (scoped to the fixture root), a due active job (nextRunAt 60s in the past)
  sits in session-artifacts/<id>/scheduled-jobs.json, and a normal boot wakes
  the session once; the job file is never archived. The `killed_workers_before_boot`
  count proves the wake (not an adopted live worker) brought the session up.

The update_prepare_rerun directory (same timestamp) is the slice-3 battery
rerun against the slice-5 supervisor: all rows unchanged (no regression in the
prepare/commit/stop surfaces from the boot-side changes).
