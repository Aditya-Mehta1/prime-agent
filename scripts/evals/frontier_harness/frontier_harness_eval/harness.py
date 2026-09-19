"""Prime Agent harness for FrontierHarness tasks.

Installs the Prime Agent bundle in the sandbox, configures Kimi K3,
and runs the agent with the task instruction.
"""
from __future__ import annotations

import os


CREDENTIAL_ENV = (
    "PRIME_API_KEY",
    "PRIME_TEAM_ID",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "HF_TOKEN",
)


def process_env() -> dict[str, str]:
    """Return the environment needed by the harness (API keys only)."""
    env = {}
    for key in ("PRIME_API_KEY", "PRIME_TEAM_ID"):
        value = os.environ.get(key, "")
        if value:
            env[key] = value
    return env


def agent_command(instruction: str, timeout_sec: int = 480) -> str:
    """Build the command that runs Prime Agent with the instruction."""
    escaped = instruction.replace("'", "'\''").replace('"', '\"')
    return (
        f"cd /app 2>/dev/null || cd /\n"
        f"exec timeout {timeout_sec} node /tmp/bundle/cli.js "
        f"-p --provider prime-inference --model moonshotai/kimi-k3 "
        f"'{escaped}' 2>&1"
    )


def setup_command() -> str:
    """Build the sandbox setup command (install node, uv, extract bundle)."""
    return (
        "export DEBIAN_FRONTEND=noninteractive; "
        "apt-get update -qq 2>/dev/null; "
        "apt-get install -y -qq curl ca-certificates 2>/dev/null; "
        "curl -fsSL https://deb.nodesource.com/setup_22.x | sh - 2>/dev/null; "
        "apt-get install -y -qq nodejs 2>/dev/null; "
        "curl -LsSf https://astral.sh/uv/install.sh | sh 2>/dev/null; "
        "mkdir -p /tmp/bundle; tar xzf /tmp/bundle.tar.gz -C /tmp/bundle; "
        "cd /tmp/bundle; npm install undici --silent 2>/dev/null; "
        "echo '{\"type\":\"module\"}' > package.json; "
        "node --version"
    )
