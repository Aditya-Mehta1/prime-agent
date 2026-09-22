# Draft replies for PR #2340 review threads (for Kevin to post — I don't comment as you)

## Macroscope — prime-onboarding-splash.ts (nested panel replaced the login dialog)
Fixed in 3db357208 / c1d69a693. Panels now live on a stack: a selector opened over the
login mounts on top and the login is restored when it closes. Focus follows the restored
panel, since the block ignores keys while a panel is mounted.

## Macroscope — interactive-mode.ts (session reset during onboarding)
Fixed in 3db357208 and completed in c1d69a693. resetExtensionUI() now tears the block down
and settles the step it unmounts: the login dialog cancels itself, and the picker, trace
question and team selector settle through an onReset callback, so the flow unwinds instead
of awaiting a dead panel.

## Macroscope + Cursor — agent-session-services.ts / main.ts (telemetry notice)
Fixed in a7ffebda1, corrected in 3db357208. The deferral is derived from the session's
execution mode, so the daemon-hosted interactive session defers too, and print, json, rpc
and acp sessions disclose immediately.

## Macroscope — auth-flows.ts (single-team auto-adopt)
Fixed in a7ffebda1. The shortcut is limited to onboarding; /login still offers the personal
account alongside the single team.

## Macroscope — interactive-mode.ts:1858 (flag persisted after a cancelled sign-in)
Fixed in 8db3f2af5. runOnboardingFlow now returns whether every step ran, and only that
result persists onboardingShown. A user who already had a working model no longer looks
"ready" straight after cancelling.

## Macroscope — login-dialog.ts:182 (accumulating spacers)
Fixed in e9cf3bcb1. The blank row above the key hints is retained and moved, so a second
prompt no longer stacks another one. Covered by a test that fails on the old behaviour.

## Cursor — escape cancels onboarding between steps
Fixed in a7ffebda1. Cancel is unbound on the block: sign-in is the only way forward, and the
gaps between steps must not drop a user into an unconfigured chat.

## Cursor — unused export / stale comment / copy snapshots
Fixed in a7ffebda1. parseHexColor is no longer exported, the stale getRows comment is gone,
and the tests no longer assert marketing copy.

## Macroscope — interactive-mode.ts:1916 (padding condition looks inverted) — NOT CHANGING
Intentional: onboarding owns the pane from the welcome screen to the last question, so the
prompt dock stays hidden until the flow ends. The condition read as inverted because its
collapse branch was dead; 2a17d2224 removed the unreachable progress state, so the padding
is now unconditional while the block is mounted.
