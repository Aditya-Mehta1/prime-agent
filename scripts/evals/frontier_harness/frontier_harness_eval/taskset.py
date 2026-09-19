"""FrontierHarness Taskset: 30 tasks (21 Terminal-Bench + 9 DeepSWE).

Each task runs in a Prime Sandbox with the task's Docker image.
Prime Agent (candidate harness) works inside with Kimi K3 via Prime Inference.
The verifier checks task-specific output.
"""
from __future__ import annotations

import json
from pathlib import Path

TASKS_PATH = Path(__file__).parent / "tasks.json"


def load_tasks() -> list[dict]:
    """Load the 30 FrontierHarness task definitions."""
    return json.loads(TASKS_PATH.read_text())


def get_task_by_name(name: str) -> dict | None:
    """Get a single task definition by name."""
    for task in load_tasks():
        if task["name"] == name:
            return task
    return None


class FrontierHarnessTaskset:
    """Placeholder for the Verifiers-compatible taskset.

    The full integration uses verifiers.v1.Taskset with:
    - Sandbox environment per task (the task's Docker image)
    - PrimeAgentHarness as the candidate
    - FrontierHarnessVerifier for task-specific scoring
    """

    @staticmethod
    def tasks() -> list[dict]:
        return load_tasks()

    @staticmethod
    def terminal_bench_tasks() -> list[dict]:
        return [t for t in load_tasks() if t["task_type"] == "terminal-bench"]

    @staticmethod
    def deep_swe_tasks() -> list[dict]:
        return [t for t in load_tasks() if t["task_type"] == "deepswe"]
