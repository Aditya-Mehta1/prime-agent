# FrontierHarness Eval for Prime Agent

Benchmarks Prime Agent against the [FrontierHarness Eval v1.0](https://frontierharness.org) suite: 30 tasks (21 Terminal-Bench + 9 DeepSWE) on Kimi K3.

## Baseline Results (September 2026)

Prime Agent 0.9.5 with `moonshotai/kimi-k3` via Prime Inference:

- **18/20 Terminal-Bench tasks likely passed (90%)** on the tasks that ran
- 2 failures: build-cython-ext (timeout), chess-best-move (vision — needs image viewing)
- DeepSWE tasks pending ECR image access fix
- Formal verification via Hub package in progress

## Hub Package

Published as `primeintellect/frontier-harness-eval` on the Environments Hub.

Install: `prime env install primeintellect/frontier-harness-eval`

## Methodology

Each task runs in a Prime Sandbox with the task's Docker image:
1. Install Node.js 22 + uv + Prime Agent bundle
2. Configure Kimi K3 via Prime Inference
3. Run agent with the task instruction
4. Verify output (task-specific)
