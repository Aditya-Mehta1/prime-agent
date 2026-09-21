#!/usr/bin/env python3
"""Generate and validate the bundled catalog assets for prime-agent releases.

Port of the TS catalog asset tool (packages/coding-agent/scripts/catalog-assets.mjs
on the TS client's `feat/catalog-client` line) for the Rust release pipeline.
The two assets are the client's no-cold-start layer 2 (spec §3.2):

  models.bundled.json        byte-identical copy of the catalog repo's
                             models/catalog.v1.json ({schemaVersion, models})
  mcp-services.bundled.json  byte-identical copy of plugins/catalog.v2.json
                             ({version, counts, entries})

They are generated at BUILD time and never committed: `--catalog-dir` copies
from a local catalog checkout, the network mode fetches the live catalog repo
for CI/packaging, and `--fixture` emits a synthetic snapshot that still
passes the full validation gates so offline builds work end to end.

Validation (the release packer runs the same functions before packing):
  models   schemaVersion == 1, models is an array, and at least
           MIN_MODEL_TRANSPORT_TUPLES distinct (provider, api, baseUrl) tuples
  plugins  version == 2, entries is an array, at least MIN_MCP_SERVICES entries

Usage:
    python3 scripts/release/bundle_catalog.py generate \
        [--catalog-dir DIR | --network | --fixture] [--out DIR] \
        [--models-url URL] [--mcp-services-url URL] [--allow-small-fixture]
    python3 scripts/release/bundle_catalog.py verify [--out DIR] [--allow-small-fixture]

Network mode reads GITHUB_TOKEN / PRIME_CATALOG_REPO_TOKEN (optional Bearer;
the catalog repo is public), aborts after 5 s, refuses redirects, and caps
responses at MAX_REMOTE_CATALOG_BYTES.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

# The two bundled asset names (spec §3.2; TS bundledCatalogFiles).
BUNDLED_CATALOG_FILES = ("models.bundled.json", "mcp-services.bundled.json")

# Release-gate minimums (spec §3.2/§3.9). The shipped TS tool still carries
# MIN_BUNDLED_MCP_SERVICES = 20 from before the service count grew; this port
# uses the verified current contract: >= 42 transport tuples, >= 68 services.
MIN_MODEL_TRANSPORT_TUPLES = 42
MIN_MCP_SERVICES = 68

DEFAULT_MODEL_CATALOG_URL = (
    "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog"
    "/main/models/catalog.v1.json"
)
DEFAULT_MCP_SERVICE_CATALOG_URL = (
    "https://raw.githubusercontent.com/PrimeIntellect-ai/prime-agent-catalog"
    "/main/plugins/catalog.v2.json"
)

# Build-time fetch limits (TS parity: 20 MiB cap, 5 s hard timeout, no
# redirects — a moved catalog must be a client change).
MAX_REMOTE_CATALOG_BYTES = 20 * 1024 * 1024
FETCH_TIMEOUT_SECONDS = 5

# The 14 derived count keys of the plugins v2 envelope (spec §1.2).
COUNTS_KEYS = (
    "total", "http", "httpTemplate", "sse", "stdio",
    "ready", "requiresSetup", "metadataReviewed",
    "oauthStrategy", "apiKeyStrategy",
    "readinessOauthReady", "readinessUserSetup",
    "readinessPrimeRestricted", "readinessUnknown",
)
READINESS_TO_COUNTS_KEY = {
    "oauth-ready": "readinessOauthReady",
    "user-setup": "readinessUserSetup",
    "prime-restricted": "readinessPrimeRestricted",
    "unknown": "readinessUnknown",
}


def fail(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


# --------------------------------------------------------------------------
# Validation (imported by the release packer: assemble_artifacts.py,
# package_release.py, verify_release.py)
# --------------------------------------------------------------------------

def validate_bundled_model_catalog(path: Path, allow_small_fixture: bool = False) -> dict:
    """Gate the bundled model catalog: version, shape, and transport-tuple minimum.

    Build-time parity with the TS validateBundledModelCatalog: strict schema
    enforcement is the runtime's job (skip-invalid + fail-closed there); the
    build gate checks the envelope and the transport-tuple minimum.
    """
    try:
        catalog = json.loads(Path(path).read_text())
    except (OSError, ValueError) as error:
        fail(f"Invalid bundled model catalog {path}: {error}")
    if not isinstance(catalog, dict) or catalog.get("schemaVersion") != 1 \
            or not isinstance(catalog.get("models"), list):
        fail(f"Invalid bundled model catalog: {path}")
    tuples = set()
    for model in catalog["models"]:
        if not isinstance(model, dict):
            continue
        provider = model.get("provider")
        api = model.get("api")
        base_url = model.get("baseUrl")
        if isinstance(provider, str) and isinstance(api, str) and isinstance(base_url, str):
            tuples.add((provider, api, base_url))
    if not allow_small_fixture and len(tuples) < MIN_MODEL_TRANSPORT_TUPLES:
        fail(
            f"Bundled model catalog has {len(tuples)} transport tuples; "
            f"expected at least {MIN_MODEL_TRANSPORT_TUPLES}"
        )
    return {"models": len(catalog["models"]), "transportTuples": len(tuples)}


def validate_bundled_mcp_catalog(path: Path, allow_small_fixture: bool = False) -> dict:
    """Gate the bundled MCP service catalog: version, shape, and entry minimum."""
    try:
        catalog = json.loads(Path(path).read_text())
    except (OSError, ValueError) as error:
        fail(f"Invalid bundled MCP service catalog {path}: {error}")
    if not isinstance(catalog, dict) or catalog.get("version") != 2 \
            or not isinstance(catalog.get("entries"), list):
        fail(f"Invalid bundled MCP service catalog: {path}")
    if not allow_small_fixture and len(catalog["entries"]) < MIN_MCP_SERVICES:
        fail(
            f"Bundled MCP service catalog has {len(catalog['entries'])} entries; "
            f"expected at least {MIN_MCP_SERVICES}"
        )
    return {"services": len(catalog["entries"])}


def validate_bundled_catalog_dir(directory, allow_small_fixture: bool = False) -> dict:
    """Validate both bundled assets in a directory (the release packer gate)."""
    directory = Path(directory)
    for name in BUNDLED_CATALOG_FILES:
        if not (directory / name).is_file():
            fail(f"Missing bundled catalog asset: {directory / name}")
    return {
        "models": validate_bundled_model_catalog(
            directory / "models.bundled.json", allow_small_fixture),
        "mcpServices": validate_bundled_mcp_catalog(
            directory / "mcp-services.bundled.json", allow_small_fixture),
    }


# --------------------------------------------------------------------------
# Fixture: a synthetic snapshot that passes the FULL gates (offline builds)
# --------------------------------------------------------------------------

# The compiled transport tuples of the shipped registry (43 distinct
# (provider, api, baseUrl) triples across 32 providers, derived from
# crates/pa-ai/src/models.generated.json). Fixture models use real tuples so
# they survive runtime transport pinning; nothing here can introduce a
# transport the client does not implement.
FIXTURE_TRANSPORT_TUPLES = [
    ("amazon-bedrock", "bedrock-converse-stream", "https://bedrock-runtime.ap-northeast-1.amazonaws.com"),
    ("amazon-bedrock", "bedrock-converse-stream", "https://bedrock-runtime.ap-southeast-2.amazonaws.com"),
    ("amazon-bedrock", "bedrock-converse-stream", "https://bedrock-runtime.eu-central-1.amazonaws.com"),
    ("amazon-bedrock", "bedrock-converse-stream", "https://bedrock-runtime.us-east-1.amazonaws.com"),
    ("anthropic", "anthropic-messages", "https://api.anthropic.com"),
    ("azure-openai-responses", "azure-openai-responses", ""),
    ("cerebras", "openai-completions", "https://api.cerebras.ai/v1"),
    ("cloudflare-ai-gateway", "anthropic-messages", "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/anthropic"),
    ("cloudflare-ai-gateway", "openai-responses", "https://gateway.ai.cloudflare.com/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai"),
    ("cloudflare-workers-ai", "openai-completions", "https://api.cloudflare.com/client/v4/accounts/{CLOUDFLARE_ACCOUNT_ID}/ai/v1"),
    ("deepseek", "openai-completions", "https://api.deepseek.com"),
    ("fireworks", "anthropic-messages", "https://api.fireworks.ai/inference"),
    ("github-copilot", "anthropic-messages", "https://api.individual.githubcopilot.com"),
    ("github-copilot", "openai-completions", "https://api.individual.githubcopilot.com"),
    ("github-copilot", "openai-responses", "https://api.individual.githubcopilot.com"),
    ("google", "google-generative-ai", "https://generativelanguage.googleapis.com/v1beta"),
    ("google-vertex", "google-vertex", "https://{location}-aiplatform.googleapis.com"),
    ("groq", "openai-completions", "https://api.groq.com/openai/v1"),
    ("huggingface", "openai-completions", "https://router.huggingface.co/v1"),
    ("kimi-coding", "anthropic-messages", "https://api.kimi.com/coding"),
    ("minimax", "anthropic-messages", "https://api.minimax.io/anthropic"),
    ("minimax-cn", "anthropic-messages", "https://api.minimaxi.com/anthropic"),
    ("mistral", "mistral-conversations", "https://api.mistral.ai"),
    ("moonshotai", "openai-completions", "https://api.moonshot.ai/v1"),
    ("moonshotai-cn", "openai-completions", "https://api.moonshot.cn/v1"),
    ("openai", "openai-responses", "https://api.openai.com/v1"),
    ("openai-codex", "openai-codex-responses", "https://chatgpt.com/backend-api"),
    ("opencode", "anthropic-messages", "https://opencode.ai/zen"),
    ("opencode", "google-generative-ai", "https://opencode.ai/zen/v1"),
    ("opencode", "openai-completions", "https://opencode.ai/zen/v1"),
    ("opencode", "openai-responses", "https://opencode.ai/zen/v1"),
    ("opencode-go", "anthropic-messages", "https://opencode.ai/zen/go"),
    ("opencode-go", "openai-completions", "https://opencode.ai/zen/go/v1"),
    ("opencode-go", "openai-responses", "https://opencode.ai/zen/go/v1"),
    ("openrouter", "openai-completions", "https://openrouter.ai/api/v1"),
    ("prime-inference", "openai-completions", "https://api.pinference.ai/api/v1"),
    ("vercel-ai-gateway", "anthropic-messages", "https://ai-gateway.vercel.sh"),
    ("xai", "openai-completions", "https://api.x.ai/v1"),
    ("xiaomi", "anthropic-messages", "https://api.xiaomimimo.com/anthropic"),
    ("xiaomi-token-plan-ams", "anthropic-messages", "https://token-plan-ams.xiaomimimo.com/anthropic"),
    ("xiaomi-token-plan-cn", "anthropic-messages", "https://token-plan-cn.xiaomimimo.com/anthropic"),
    ("xiaomi-token-plan-sgp", "anthropic-messages", "https://token-plan-sgp.xiaomimimo.com/anthropic"),
    ("zai", "openai-completions", "https://api.z.ai/api/coding/paas/v4")
]


def fixture_model(provider: str, api: str, base_url: str, index: int, **extra) -> dict:
    model = {
        "id": f"fixture-{provider}-{index}",
        "name": f"Fixture {provider} model {index}",
        "api": api,
        "provider": provider,
        "baseUrl": base_url,
        "reasoning": False,
        "input": ["text"],
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0},
        "contextWindow": 128000,
        "maxTokens": 4096,
    }
    model.update(extra)
    return model


def fixture_models() -> list:
    """One model per compiled transport tuple, plus optional-field exercises.

    Every entry matches the strict model schema (spec §1.1): the optional
    fields (thinkingLevelMap, featured, compat) follow the documented shapes
    so the runtime's deny_unknown_fields parse succeeds.
    """
    models = []
    for index, (provider, api, base_url) in enumerate(FIXTURE_TRANSPORT_TUPLES):
        models.append(fixture_model(provider, api, base_url, index))
    # Reasoning model with an explicit thinking level map (off explicitly
    # unsupported: hidden + clamped away; xhigh/max absent: provider default).
    models.append(fixture_model(
        "anthropic", "anthropic-messages", "https://api.anthropic.com", 100,
        id="fixture-anthropic-reasoning", name="Fixture Claude (reasoning)",
        reasoning=True, input=["text", "image"], featured=True,
        thinkingLevelMap={"off": None, "low": "low", "medium": "medium", "high": "high"},
    ))
    # openai-completions compat surface (maxTokensField + thinkingFormat).
    models.append(fixture_model(
        "deepseek", "openai-completions", "https://api.deepseek.com", 101,
        id="fixture-deepseek-compat", name="Fixture DeepSeek (compat)",
        compat={"maxTokensField": "max_tokens", "thinkingFormat": "deepseek"},
    ))
    # openai-responses compat surface (sendSessionIdHeader, long cache).
    models.append(fixture_model(
        "openai", "openai-responses", "https://api.openai.com/v1", 102,
        id="fixture-openai-responses-compat", name="Fixture OpenAI (responses compat)",
        compat={"sendSessionIdHeader": False, "supportsLongCacheRetention": True},
    ))
    return models


def fixture_service(index: int, transport_kind: str, auth_strategy: str,
                     setup_status: str, readiness: str, verified: bool) -> dict:
    server = f"fixture-svc-{index:02d}"
    url = f"https://mcp-{server}.example.com/mcp"
    auth = {
        "oauth": {"strategy": "oauth", "clientRegistration": "dynamic"},
        "api_key": {"strategy": "api_key", "clientRegistration": "unknown"},
        "none": {"strategy": "none", "clientRegistration": "unknown"},
        "unknown": {"strategy": "unknown", "clientRegistration": "unknown"},
    }[auth_strategy]
    if auth_strategy == "oauth" and index % 8 == 0:
        auth["reviewedScopes"] = ["mcp:fixture"]
    transport = {
        "http": {"type": "http", "url": url},
        "http-template": {
            "type": "http-template",
            "template": f"https://mcp-{server}.example.com/{{region}}/mcp",
            "variables": ["region"],
        },
        "sse": {"type": "sse", "url": url},
        "stdio": {
            "type": "stdio",
            "servers": [{"name": server, "command": f"{server}-mcp"}],
        },
    }[transport_kind]
    setup = {"status": setup_status, "readiness": readiness}
    if setup_status == "requires-setup":
        setup["reason"] = f"Fixture service {index} needs its fixture credential."
        if auth_strategy == "api_key":
            setup["requirement"] = "bearer-token"
            setup["fields"] = [{
                "id": f"FIXTURE_SVC_{index:02d}_TOKEN",
                "label": f"Fixture service {index} token",
                "required": True,
                "kind": "bearer-token",
                "credentialSet": f"fixture-svc-{index:02d}",
            }]
        elif transport_kind == "stdio":
            setup["requirement"] = "local-runtime"
    return {
        "server": server,
        "service": "fixture",
        "label": f"Fixture Service {index:02d}",
        "url": "" if transport_kind == "stdio" else url,
        "description": f"Synthetic fixture MCP service {index:02d} for offline builds.",
        "category": "Fixture",
        "aliases": [] if index % 5 else [f"fixture service {index:02d}"],
        "publisher": "Prime Intellect",
        "transport": transport,
        "auth": auth,
        "setup": setup,
        "verification": {"status": "metadata-reviewed" if verified else "unverified"},
        "legacyBuiltin": False,
        "provenance": [{"source": "prime"}],
        **({"oauth": {"kind": "oauth"}} if auth_strategy == "oauth" else {}),
    }


def fixture_special_services() -> list:
    """Hand-shaped entries exercising the paste-flow and builtin-shadow
    semantics (spec §1.2/§3.6) that the runtime lane's tests consume:

    - a pasteable bearer-token service whose two alternative credential
      field ids share one credentialSet (alias names, one credential);
    - a bearer-token service with TWO distinct credential sets (fail-closed:
      NOT pasteable — no single credential resolves);
    - a requires-setup api-key service (kind "api-key" field);
    - the legacy builtin ids (linear, notion) with legacyBuiltin: true.
    """
    def entry(server, label, auth, setup, **extra):
        service = fixture_service(
            99, "http", auth, setup, "user-setup", False)
        service.update({
            "server": server,
            "label": label,
            "url": f"https://mcp-{server}.example.com/mcp",
            "description": f"Synthetic fixture MCP service for offline builds.",
            "setup": setup,
        })
        service["transport"] = {"type": "http", "url": service["url"]}
        service.update(extra)
        return service

    def field(field_id, label, kind, credential_set):
        return {
            "id": field_id,
            "label": label,
            "required": True,
            "kind": kind,
            "credentialSet": credential_set,
        }

    paste_setup = {
        "status": "requires-setup",
        "readiness": "user-setup",
        "reason": "Paste a fixture personal access token.",
        "requirement": "bearer-token",
        "fields": [
            field("FIXTURE_PAT_TOKEN", "Fixture personal access token",
                  "bearer-token", "fixture-pat"),
            field("FIXTURE_PERSONAL_ACCESS_TOKEN", "Fixture personal access token (alias)",
                  "bearer-token", "fixture-pat"),
        ],
    }
    two_cred_setup = {
        "status": "requires-setup",
        "readiness": "user-setup",
        "reason": "Requires two distinct credentials (never pasteable).",
        "requirement": "bearer-token",
        "fields": [
            field("FIXTURE_CLIENT_ID", "Fixture client id", "client-id", "fixture-cred-a"),
            field("FIXTURE_CLIENT_SECRET", "Fixture client secret",
                  "client-secret", "fixture-cred-b"),
        ],
    }
    api_key_setup = {
        "status": "requires-setup",
        "readiness": "user-setup",
        "reason": "Requires a fixture API key.",
        "requirement": "api-key",
        "fields": [
            field("FIXTURE_API_KEY", "Fixture API key", "api-key", "fixture-api-key"),
        ],
    }
    return [
        entry("fixture-paste-single", "Fixture Pasteable (single credential)",
              "api_key", paste_setup),
        entry("fixture-paste-multi-cred", "Fixture Two-Credential (not pasteable)",
              "api_key", two_cred_setup),
        entry("fixture-api-key", "Fixture API Key (requires-setup)", "api_key", api_key_setup),
        entry("linear", "Linear", "oauth", {"status": "ready", "readiness": "oauth-ready"},
              legacyBuiltin=True, service="linear", category="Developer tools"),
        entry("notion", "Notion", "oauth", {"status": "ready", "readiness": "oauth-ready"},
              legacyBuiltin=True, service="notion", category="Developer tools"),
    ]


def fixture_mcp_services() -> list:
    """68 services covering every transport kind, auth strategy, setup
    status, readiness value, and verification status, plus the special
    paste-flow/builtin-shadow entries the runtime lane consumes."""
    entries = []
    # (transport, auth, setup status, readiness, verified) round-robin so the
    # full shape matrix is exercised deterministically.
    matrix = [
        ("http", "oauth", "ready", "oauth-ready", True),
        ("http-template", "api_key", "requires-setup", "user-setup", False),
        ("sse", "none", "requires-setup", "unknown", False),
        ("stdio", "unknown", "requires-setup", "prime-restricted", False),
    ]
    for index in range(MIN_MCP_SERVICES):
        transport_kind, auth_strategy, setup_status, readiness, verified = \
            matrix[index % len(matrix)]
        entries.append(fixture_service(
            index, transport_kind, auth_strategy, setup_status, readiness, verified))
    return entries + fixture_special_services()


def fixture_counts(entries: list) -> dict:
    counts = dict.fromkeys(COUNTS_KEYS, 0)
    counts["total"] = len(entries)
    for entry in entries:
        transport_key = {"http": "http", "http-template": "httpTemplate",
                           "sse": "sse", "stdio": "stdio"}[entry["transport"]["type"]]
        counts[transport_key] += 1
        counts["ready" if entry["setup"]["status"] == "ready" else "requiresSetup"] += 1
        if entry["verification"]["status"] == "metadata-reviewed":
            counts["metadataReviewed"] += 1
        auth_strategy = entry["auth"]["strategy"]
        strategy_key = {"oauth": "oauthStrategy",
                        "api_key": "apiKeyStrategy"}.get(auth_strategy)
        if strategy_key is not None:
            counts[strategy_key] += 1
        readiness_key = READINESS_TO_COUNTS_KEY[entry["setup"]["readiness"]]
        counts[readiness_key] += 1
    return counts


def fixture_catalog_bodies() -> dict:
    """The two synthetic asset bodies (2-space JSON, trailing newline — the
    TS fixture formatting)."""
    services = fixture_mcp_services()
    models = {"schemaVersion": 1, "models": fixture_models()}
    plugins = {
        "version": 2,
        "counts": fixture_counts(services),
        "entries": services,
    }
    return {
        "models.bundled.json": json.dumps(models, indent=2) + "\n",
        "mcp-services.bundled.json": json.dumps(plugins, indent=2) + "\n",
    }


# --------------------------------------------------------------------------
# Source modes
# --------------------------------------------------------------------------

def catalog_source_paths(catalog_dir: Path) -> dict:
    return {
        "models.bundled.json": catalog_dir / "models" / "catalog.v1.json",
        "mcp-services.bundled.json": catalog_dir / "plugins" / "catalog.v2.json",
    }


def bundled_targets(out_dir: Path) -> dict:
    return {name: out_dir / name for name in BUNDLED_CATALOG_FILES}


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(
            req.full_url, code,
            f"refused redirect to {newurl} (catalog moves require a client change)",
            headers, fp)


def fetch_catalog(url: str, label: str) -> str:
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("PRIME_CATALOG_REPO_TOKEN")
    request = urllib.request.Request(url, headers={
        "accept": "application/json",
        "cache-control": "no-cache",
        **({"authorization": f"Bearer {token}"} if token else {}),
    })
    opener = urllib.request.build_opener(_NoRedirect)
    try:
        response = opener.open(request, timeout=FETCH_TIMEOUT_SECONDS)
    except urllib.error.HTTPError as error:
        hint = ""
        if error.code in (401, 404):
            hint = (" The catalog repo is private or absent; set GITHUB_TOKEN "
                    "or PRIME_CATALOG_REPO_TOKEN.")
        elif 300 <= error.code < 400:
            hint = f" ({error.reason})"
        fail(f"Failed to fetch {label} catalog from {url}: HTTP {error.code}.{hint}")
    except (urllib.error.URLError, OSError, TimeoutError) as error:
        fail(f"Failed to fetch {label} catalog from {url}: {error}")
    with response:
        content_length = response.headers.get("content-length")
        if content_length is not None:
            try:
                if int(content_length) > MAX_REMOTE_CATALOG_BYTES:
                    fail(
                        f"{label} catalog is too large: {content_length} bytes "
                        f"exceeds {MAX_REMOTE_CATALOG_BYTES}"
                    )
            except ValueError:
                pass
        body = bytearray()
        while True:
            chunk = response.read(64 * 1024)
            if not chunk:
                break
            body.extend(chunk)
            if len(body) > MAX_REMOTE_CATALOG_BYTES:
                fail(
                    f"{label} catalog is too large: exceeds "
                    f"{MAX_REMOTE_CATALOG_BYTES} bytes"
                )
        text = body.decode("utf-8")
    return text if text.endswith("\n") else f"{text}\n"


def generate_bundled_catalog_assets(options) -> dict:
    out_dir = Path(options.out_dir).resolve() if options.out_dir \
        else (ROOT / "target" / "catalog-assets")
    out_dir.mkdir(parents=True, exist_ok=True)
    targets = bundled_targets(out_dir)
    modes = [mode for mode, chosen in (
        ("--fixture", options.fixture),
        ("--catalog-dir", options.catalog_dir is not None),
        ("--network", options.network),
    ) if chosen]
    if len(modes) > 1:
        fail(f"pick one source mode ({', '.join(modes)} are mutually exclusive)")
    if options.fixture:
        for name, body in fixture_catalog_bodies().items():
            targets[name].write_text(body)
    elif options.catalog_dir is not None:
        sources = catalog_source_paths(Path(options.catalog_dir))
        for name, source in sources.items():
            if not source.is_file():
                fail(f"catalog checkout missing {source}; pass a valid --catalog-dir")
            shutil.copyfile(source, targets[name])
    else:
        # Network mode is the default for CI/packaging (TS parity: fetch both
        # live files and write the bodies verbatim).
        targets["models.bundled.json"].write_text(
            fetch_catalog(options.models_url or DEFAULT_MODEL_CATALOG_URL, "model"))
        targets["mcp-services.bundled.json"].write_text(
            fetch_catalog(options.mcp_services_url or DEFAULT_MCP_SERVICE_CATALOG_URL,
                          "MCP service"))
    allow_small = options.allow_small_fixture or options.fixture
    result = validate_bundled_catalog_dir(out_dir, allow_small_fixture=allow_small)
    result["outDir"] = str(out_dir)
    return result


def parse_args(argv=None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("command", nargs="?", default="generate",
                        choices=("generate", "verify"),
                        help="generate (default) or verify an existing --out dir")
    parser.add_argument("--out", dest="out_dir", default=None,
                        help="asset output directory (default: target/catalog-assets)")
    parser.add_argument("--catalog-dir", default=None,
                        help="copy from a local prime-agent-catalog checkout")
    parser.add_argument("--network", action="store_true",
                        help="fetch the live catalog repo (CI/packaging default)")
    parser.add_argument("--fixture", action="store_true",
                        help="emit the synthetic full-gate snapshot (offline builds)")
    parser.add_argument("--models-url", default=None,
                        help="override the model catalog URL")
    parser.add_argument("--mcp-services-url", default=None,
                        help="override the MCP service catalog URL")
    parser.add_argument("--allow-small-fixture", action="store_true",
                        help="skip the minimum-count gates (smoke fixtures only)")
    return parser.parse_args(argv)


def main(argv=None) -> int:
    args = parse_args(argv)
    out_dir = args.out_dir or (ROOT / "target" / "catalog-assets")
    if args.command == "verify":
        result = validate_bundled_catalog_dir(
            out_dir, allow_small_fixture=args.allow_small_fixture)
        result["outDir"] = str(out_dir)
        print(json.dumps(result, indent=2))
        return 0
    print(json.dumps(generate_bundled_catalog_assets(args), indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
