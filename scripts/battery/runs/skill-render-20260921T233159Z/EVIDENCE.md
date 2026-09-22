# skill-render lane parity evidence (2026-09-21; re-run post-merge with #282, origin/main 4d008209)

Verifiers:
1. `scripts/custom_message_parity.py` (extended with the persisted
   `<skill>` block user message + args): PASS `a_collapsed-120x90` and
   `b_expanded-120x90` — TS resume vs Rust replay frames identical after
   the shared normalization; the reach asserts hold on both sides (the
   `[skill] web-search` summary collapsed, the markdown body only
   expanded, the `find rust tuis` args user block, the raw block never
   visible). Frames: `replay-{ts,rust}-{a_collapsed,b_expanded}-120x90.txt`.
2. `scripts/skill_invocation_parity.py` (new, the user-typed slash case):
   both binaries run their real daemon with a scripted faux provider and
   a fixture `web-search` skill in the sandbox agent dir; the same
   `/skill:web-search find parity tuis` typed in tmux at 120x90. PASS
   `a_collapsed-120x90` and `b_expanded-120x90` (frame diff equal after
   the shared normalization + the sandbox agent-dir scrub, the only diff
   was each side's own sandbox path inside the fixture skill's
   references line). Frames:
   `live-{ts,rust}-{a_collapsed,b_expanded}-120x90.txt`.
3. `skill used` telemetry event verified live in the Rust run
   (name/properties only, no content): `{"name": "skill used",
   "properties": {"skill_name": "web-search", "skill_kind": "markdown",
   "source": "prompt", ...}}`.
