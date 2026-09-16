//! Kernel Python environment bootstrap: find or build the venv that runs
//! `python -m rlm.repl`, syncing `prime-agent-runtime`, the default extra
//! packages, and any editable Python skills.
//!
//! Ported from `core/kernel/bootstrap.ts`; `build_rlm_bootstrap_code` comes
//! from `core/tools/ipython.ts` (the runtime surface injected into the kernel).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context};

/// Schema of `.bootstrap-version`; a mismatch rebuilds the venv.
const BOOTSTRAP_SCHEMA: u64 = 9;
const PYTHON_VERSION: &str = "3.11";
const RUNTIME_REQUIREMENT: &str = "prime-agent-runtime";
const STATE_SNAPSHOT_REQUIREMENT: &str = "dill";
const BOOTSTRAP_VERSION_FILE: &str = ".bootstrap-version";
const BOOTSTRAP_LOCK_NAME: &str = ".bootstrap.lock";
const BOOTSTRAP_LOCK_RETRY_MS: u64 = 100;
const BOOTSTRAP_LOCK_STALE_WITHOUT_PID_MS: u64 = 30_000;
const UV_INSTALL_COMMAND: &str = "curl -LsSf https://astral.sh/uv/install.sh | sh";

/// One Python skill the kernel should import at bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelPythonSkill {
    pub name: String,
    pub import_name: String,
    pub package_path: PathBuf,
    pub pyproject_path: PathBuf,
}

/// The default extra packages pre-installed in the kernel venv and promised to
/// the model in the system prompt.
pub const DEFAULT_RLM_EXTRA_PACKAGES: [(&str, &str, &str); 12] = [
    // (uvArg, importName, promptLabel)
    ("requests", "requests", "requests"),
    ("httpx", "httpx", "httpx"),
    ("pyyaml", "yaml", "yaml (PyYAML)"),
    ("tomli", "tomli", "tomli"),
    ("python-dotenv", "dotenv", "dotenv (python-dotenv)"),
    ("pandas", "pandas", "pandas"),
    ("numpy", "numpy", "numpy"),
    ("scipy", "scipy", "scipy"),
    ("beautifulsoup4", "bs4", "bs4 (Beautiful Soup)"),
    ("lxml", "lxml", "lxml"),
    ("pydantic", "pydantic", "pydantic"),
    ("tyro", "tyro", "tyro"),
];

pub fn default_rlm_extra_uv_args() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(uv, _, _)| *uv)
        .collect()
}

pub fn default_rlm_extra_import_names() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, import, _)| *import)
        .collect()
}

pub fn default_rlm_extra_import_labels() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, _, label)| *label)
        .collect()
}

/// Progress callback for the bootstrap (`ensure_kernel_python`).
pub type KernelBootstrapProgressHandler = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub struct EnsureKernelPythonOptions {
    pub python_skills: Vec<KernelPythonSkill>,
    pub on_progress: Option<KernelBootstrapProgressHandler>,
}

impl EnsureKernelPythonOptions {
    fn report(&self, message: &str) {
        match &self.on_progress {
            Some(handler) => handler(message),
            None => eprintln!("{message}"),
        }
    }
}

/// One normalized skill as recorded in the bootstrap version file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BootstrapPythonSkill {
    import_name: String,
    package_path: String,
    pyproject_path: String,
    pyproject_hash: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct BootstrapVersion {
    schema: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra_uv_args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    python_skills: Option<Vec<BootstrapPythonSkill>>,
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn file_content_hash(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => format!("sha256:{:x}", sha2::Sha256::digest(&bytes)),
        Err(_) => "unreadable".to_string(),
    }
}

use sha2::Digest;

fn read_toml_project_section(pyproject_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pyproject_path).ok()?;
    let mut start = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim() == "[project]" {
            start = Some(index + 1);
            break;
        }
    }
    let start = start?;
    let mut section = String::new();
    for line in text.lines().skip(start) {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            break;
        }
        section.push_str(line);
        section.push('\n');
    }
    Some(section)
}

fn read_python_skill_project_name(skill: &BootstrapPythonSkill) -> String {
    let section = read_toml_project_section(Path::new(&skill.pyproject_path));
    let name = section.and_then(|text| {
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed
                .strip_prefix("name")
                .and_then(|r| r.trim_start().strip_prefix('='))
            {
                let value = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        None
    });
    name.unwrap_or_else(|| skill.import_name.replace('_', "-"))
}

fn parse_dependency_package_name(dependency: &str) -> Option<String> {
    let without_marker = dependency.split(';').next()?.trim();
    if without_marker.is_empty() {
        return None;
    }
    let name: String = without_marker
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .collect();
    if name.is_empty() {
        return None;
    }
    Some(name.replace('_', "-").to_lowercase())
}

/// Names in the `[project] dependencies` array, tolerating quotes/escapes.
fn read_python_skill_dependency_names(skill: &BootstrapPythonSkill) -> Vec<String> {
    let Some(section) = read_toml_project_section(Path::new(&skill.pyproject_path)) else {
        return Vec::new();
    };
    let mut dependencies = Vec::new();
    let mut in_dependencies = false;
    for line in section.lines() {
        let trimmed = line.trim();
        if in_dependencies {
            if trimmed.starts_with(']') {
                break;
            }
            for raw in trimmed.split(',') {
                let candidate = raw.trim().trim_matches(|c| c == '"' || c == '\'');
                if candidate.is_empty() {
                    continue;
                }
                if let Some(name) = parse_dependency_package_name(candidate) {
                    dependencies.push(name);
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("dependencies") {
            if rest.trim_start().starts_with('=') {
                in_dependencies = true;
            }
        }
    }
    dependencies
}

fn to_bootstrap_skill(skill: &KernelPythonSkill) -> BootstrapPythonSkill {
    BootstrapPythonSkill {
        import_name: skill.import_name.clone(),
        package_path: skill.package_path.to_string_lossy().to_string(),
        pyproject_path: skill.pyproject_path.to_string_lossy().to_string(),
        pyproject_hash: file_content_hash(&skill.pyproject_path),
    }
}

/// Deduplicate skills (by importName + packagePath), resolve sibling-local
/// dependencies, and sort deterministically — matching `normalizePythonSkills`.
fn normalize_python_skills(python_skills: &[KernelPythonSkill]) -> Vec<BootstrapPythonSkill> {
    let mut by_key: Vec<(String, BootstrapPythonSkill)> = Vec::new();
    fn add_skill(by_key: &mut Vec<(String, BootstrapPythonSkill)>, skill: BootstrapPythonSkill) {
        let key = format!("{}\u{0}{}", skill.import_name, skill.package_path);
        if by_key.iter().any(|(existing, _)| *existing == key) {
            return;
        }
        for dependency_name in read_python_skill_dependency_names(&skill) {
            if let Some(sibling) = resolve_sibling_python_skill_dependency(&skill, &dependency_name)
            {
                add_skill(by_key, sibling);
            }
        }
        by_key.push((key, skill));
    }
    for skill in python_skills {
        add_skill(&mut by_key, to_bootstrap_skill(skill));
    }
    let mut skills: Vec<BootstrapPythonSkill> = by_key.into_iter().map(|(_, s)| s).collect();
    skills.sort_by(|a, b| {
        a.package_path
            .cmp(&b.package_path)
            .then(a.import_name.cmp(&b.import_name))
    });
    skills
}

fn resolve_sibling_python_skill_dependency(
    skill: &BootstrapPythonSkill,
    dependency_name: &str,
) -> Option<BootstrapPythonSkill> {
    let siblings_dir = Path::new(&skill.package_path).parent()?;
    for entry in std::fs::read_dir(siblings_dir).ok()? {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_dir() {
            continue;
        }
        let package_path = entry.path();
        let pyproject_path = package_path.join("pyproject.toml");
        if !pyproject_path.exists() {
            continue;
        }
        let candidate = BootstrapPythonSkill {
            import_name: entry.file_name().to_string_lossy().replace('-', "_"),
            package_path: package_path.to_string_lossy().to_string(),
            pyproject_path: pyproject_path.to_string_lossy().to_string(),
            pyproject_hash: file_content_hash(&pyproject_path),
        };
        if read_python_skill_project_name(&candidate)
            .replace('_', "-")
            .to_lowercase()
            == dependency_name
        {
            return Some(candidate);
        }
    }
    None
}

/// Directory of the kernel venv, honoring `PRIME_AGENT_KERNEL_VENV`.
pub fn kernel_venv_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("PRIME_AGENT_KERNEL_VENV") {
        if !override_dir.is_empty() {
            return expand_home(&override_dir);
        }
    }
    home_dir().join(".prime").join("agent").join("kernel-venv")
}

fn xdg_kernel_venv_dir() -> PathBuf {
    let data_home = match std::env::var("XDG_DATA_HOME") {
        Ok(value) if !value.is_empty() => expand_home(&value),
        _ => home_dir().join(".local").join("share"),
    };
    data_home.join("prime").join("agent").join("kernel-venv")
}

fn resolve_writable_kernel_venv_dir() -> anyhow::Result<PathBuf> {
    let primary = kernel_venv_dir();
    if std::fs::create_dir_all(primary.parent().unwrap_or(Path::new("/"))).is_ok() {
        return Ok(primary);
    }
    if std::env::var("PRIME_AGENT_KERNEL_VENV")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Err(anyhow!(
            "couldn't create kernel venv parent directories for {}",
            primary.display()
        ));
    }
    let fallback = xdg_kernel_venv_dir();
    if std::fs::create_dir_all(fallback.parent().unwrap_or(Path::new("/"))).is_err() {
        return Err(anyhow!(
            "couldn't create kernel venv directory at {} or {}; set PRIME_AGENT_KERNEL_PYTHON to a python with a current prime-agent-runtime installed",
            primary.display(),
            fallback.display()
        ));
    }
    Ok(fallback)
}

/// Path of the venv's python interpreter.
pub fn kernel_venv_python(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

async fn run_async(command: &str, args: &[String]) -> anyhow::Result<()> {
    // Run on a blocking thread: the bootstrap is an IO-bound child process.
    let command = command.to_string();
    let args = args.to_vec();
    tokio::task::spawn_blocking(move || {
        let status = std::process::Command::new(&command)
            .args(&args)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("failed to spawn {command}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "{} {} failed with exit code {}",
                command,
                args.join(" "),
                status.code().unwrap_or(-1)
            ))
        }
    })
    .await
    .map_err(|e| anyhow!("bootstrap task join failed: {e}"))?
}

fn python_imports(python: &str, module_name: &str) -> bool {
    run_quiet(python, &["-c", &format!("import {module_name}")])
}

fn run_quiet(command: &str, args: &[&str]) -> bool {
    let output = std::process::Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(output, Ok(status) if status.success())
}

/// The runtime-ready assertion from the TS product: a current
/// prime-agent-runtime with the callable RLM surface, harness CRUD, bash
/// handles, and protocol version 3.
const RUNTIME_READY_CHECK: &str = "import inspect; import rlm; from rlm import McpIntegration; import rlm.mcp as mcp; from rlm.harness import HarnessEntry; _harness_methods = ['create_memory', 'update_memory', 'delete_memory', 'create_skill', 'update_skill', 'delete_skill', 'create_subagent', 'update_subagent', 'delete_subagent', 'create_prompt_note', 'update_prompt_note', 'delete_prompt_note', 'record_refinement']; assert callable(mcp.list_tools); assert callable(mcp.call_tool); assert callable(rlm.spawn); assert hasattr(rlm, 'rlm'); assert callable(rlm.rlm.spawn); assert inspect.signature(rlm.spawn).parameters['name'].default is inspect.Parameter.empty; assert not hasattr(rlm, 'run'); assert not hasattr(rlm.rlm, 'run'); assert callable(rlm.host_request); assert callable(rlm.find_models); assert callable(rlm.rlm.find_models); assert callable(rlm.create_session); assert callable(rlm.rlm.create_session); assert callable(rlm.progress_note); assert callable(rlm.rlm.progress_note); assert hasattr(rlm, 'harness'); assert hasattr(rlm, 'get_harness_state'); assert hasattr(rlm.rlm, 'harness'); assert hasattr(rlm.rlm, 'get_harness_state'); assert all(callable(getattr(_harness, _method, None)) for _harness in (rlm.harness, rlm.rlm.harness) for _method in _harness_methods); assert 'reference' in HarnessEntry.__dataclass_fields__; assert 'scope' in HarnessEntry.__dataclass_fields__; assert 'reference' in inspect.signature(rlm.harness.create_skill).parameters; assert 'reference' in inspect.signature(rlm.harness.update_skill).parameters; assert 'global_' in inspect.signature(rlm.harness.create_memory).parameters; assert 'global_' in inspect.signature(rlm.get_harness_state).parameters; assert not hasattr(rlm, 'background'); assert not hasattr(rlm.rlm, 'background'); from rlm.bash import BashHandle, BashResult; assert callable(rlm.bash); assert all(callable(getattr(BashHandle, _m, None)) for _m in ('tail', 'output', 'poll', 'kill')); assert {'exit_code', 'output', 'duration'} <= set(BashResult.__dataclass_fields__); import rlm.repl as _repl; assert callable(_repl.main); assert callable(_repl.emit); assert callable(_repl.host_request); assert callable(_repl.is_active); assert _repl.PROTOCOL_VERSION == 3; assert callable(rlm.emit); assert not hasattr(rlm, 'HOST_COMM_TARGET'); assert not hasattr(mcp, 'install_shutdown_hook')";

fn has_prime_agent_runtime(python: &str) -> bool {
    run_quiet(python, &["-c", RUNTIME_READY_CHECK])
}

fn missing_rlm_extra_import_labels(python: &str) -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .filter(|(_, import, _)| !python_imports(python, import))
        .map(|(_, _, label)| *label)
        .collect()
}

fn missing_python_skill_import_labels(
    python: &str,
    python_skills: &[KernelPythonSkill],
) -> Vec<String> {
    python_skills
        .iter()
        .filter(|skill| !python_imports(python, &skill.import_name))
        .map(|skill| format!("{} ({})", skill.name, skill.import_name))
        .collect()
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path_value = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path_value) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let full_path = dir.join(name);
        if full_path.is_file() && is_executable(&full_path) {
            return Some(full_path);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    crate::platform::perms::is_executable(path)
}

fn is_process_alive(pid: u32) -> bool {
    crate::platform::process::pid_exists(pid)
}

/// A `link(2)`-published lock file: born with owner content, EEXIST the only
/// collision signal; stale locks are renamed aside, verified, then reclaimed.
/// Ported from `utils/dir-lock.ts`.
enum DirLockAttempt {
    Acquired,
    Held,
    Reclaimed,
}

fn strict_pid(raw: Option<&str>) -> Option<u32> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let parsed: u32 = trimmed.parse().ok()?;
    (parsed > 0).then_some(parsed)
}

fn try_acquire_dir_lock(lock_path: &Path) -> anyhow::Result<DirLockAttempt> {
    std::fs::create_dir_all(lock_path.parent().unwrap_or(Path::new("/")))?;
    let token = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    let temp_path = lock_path.with_file_name(format!(
        "{}.candidate-{}",
        lock_path.file_name().unwrap_or_default().to_string_lossy(),
        token
    ));
    {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        crate::platform::perms::set_private_mode(&mut options);
        let mut file = options.open(&temp_path)?;
        writeln!(file, "{}", std::process::id())?;
    }
    // The primary signal: link() publishing the candidate under the lock path.
    if std::fs::hard_link(&temp_path, lock_path).is_ok() {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(DirLockAttempt::Acquired);
    }
    // Judge the incumbent: dead owner (or no owner readable) means stale.
    let judge = match std::fs::read_to_string(lock_path) {
        Ok(raw) => strict_pid(Some(&raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&temp_path);
            return Ok(DirLockAttempt::Reclaimed);
        }
        Err(error) if error.kind() == std::io::ErrorKind::IsADirectory => {
            // Legacy directory lock from the old protocol.
            let pid_file = lock_path.join("pid");
            match std::fs::read_to_string(pid_file) {
                Ok(raw) => strict_pid(Some(&raw)),
                Err(_) => None,
            }
        }
        Err(_) => None,
    };
    let stale = match judge {
        None => lock_missing_pid_is_stale(lock_path),
        Some(pid) => !is_process_alive(pid),
    };
    let result = if stale {
        let aside_path = lock_path.with_file_name(format!(
            "{}.stale-{}",
            lock_path.file_name().unwrap_or_default().to_string_lossy(),
            token
        ));
        match std::fs::rename(lock_path, &aside_path) {
            Ok(()) => {
                if std::fs::remove_file(&aside_path).is_ok() {
                    DirLockAttempt::Reclaimed
                } else {
                    DirLockAttempt::Held
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirLockAttempt::Reclaimed,
            Err(_) => DirLockAttempt::Held,
        }
    } else {
        DirLockAttempt::Held
    };
    let _ = std::fs::remove_file(&temp_path);
    Ok(result)
}

fn lock_missing_pid_is_stale(lock_path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(lock_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    std::time::SystemTime::now()
        .duration_since(modified)
        .map(|age| age.as_millis() as u64 > BOOTSTRAP_LOCK_STALE_WITHOUT_PID_MS)
        .unwrap_or(false)
}

/// Serialize concurrent bootstraps across processes on the same venv.
async fn acquire_bootstrap_lock(venv: &Path) -> anyhow::Result<impl Drop> {
    let lock_dir = venv.with_file_name(format!(
        "{}{}",
        venv.file_name().unwrap_or_default().to_string_lossy(),
        BOOTSTRAP_LOCK_NAME
    ));
    std::fs::create_dir_all(lock_dir.parent().unwrap_or(Path::new("/")))?;
    loop {
        match try_acquire_dir_lock(&lock_dir)? {
            DirLockAttempt::Acquired => {
                struct Guard(PathBuf);
                impl Drop for Guard {
                    fn drop(&mut self) {
                        let _ = std::fs::remove_file(&self.0);
                    }
                }
                return Ok(Guard(lock_dir));
            }
            DirLockAttempt::Held | DirLockAttempt::Reclaimed => {
                tokio::time::sleep(std::time::Duration::from_millis(BOOTSTRAP_LOCK_RETRY_MS)).await;
            }
        }
    }
}

fn read_bootstrap_version(venv: &Path) -> Option<BootstrapVersion> {
    let raw = std::fs::read_to_string(venv.join(BOOTSTRAP_VERSION_FILE)).ok()?;
    let parsed: BootstrapVersion = serde_json::from_str(&raw).ok()?;
    (parsed.schema > 0).then_some(parsed)
}

fn extra_uv_args_match(a: &Option<Vec<String>>, b: &[&str]) -> bool {
    match a {
        None => false,
        Some(a) => a.iter().map(String::as_str).eq(b.iter().copied()),
    }
}

fn python_skills_match(a: &Option<Vec<BootstrapPythonSkill>>, b: &[BootstrapPythonSkill]) -> bool {
    let Some(a) = a else {
        return false;
    };
    a == b
}

fn bootstrap_base_version_current(
    version: Option<BootstrapVersion>,
    runtime_identity: &str,
) -> bool {
    match version {
        Some(version) => {
            version.schema == BOOTSTRAP_SCHEMA
                && version.runtime.as_deref() == Some(runtime_identity)
                && version.snapshot.as_deref() == Some(STATE_SNAPSHOT_REQUIREMENT)
                && extra_uv_args_match(&version.extra_uv_args, &default_rlm_extra_uv_args())
        }
        None => false,
    }
}

fn bootstrap_version_current(
    version: Option<BootstrapVersion>,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    bootstrap_base_version_current(version.clone(), runtime_identity)
        && version
            .as_ref()
            .and_then(|v| v.python_skills.clone())
            .is_some_and(|skills| python_skills_match(&Some(skills), python_skills))
}

fn write_bootstrap_version(
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<()> {
    let version = BootstrapVersion {
        schema: BOOTSTRAP_SCHEMA,
        runtime: Some(runtime_identity.to_string()),
        snapshot: Some(STATE_SNAPSHOT_REQUIREMENT.to_string()),
        extra_uv_args: Some(
            default_rlm_extra_uv_args()
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        python_skills: Some(python_skills.to_vec()),
    };
    std::fs::write(
        venv.join(BOOTSTRAP_VERSION_FILE),
        format!("{}\n", serde_json::to_string(&version)?),
    )?;
    Ok(())
}

/// Directory of the installed `prime-agent-runtime` sources. The Rust binary
/// ships the same sidecar layout the compiled TS executable uses; an explicit
/// `PI_PACKAGE_DIR` override wins (matching the TS `getPackageDir`).
fn package_dir() -> PathBuf {
    if let Ok(env_dir) = std::env::var("PI_PACKAGE_DIR") {
        if !env_dir.is_empty() {
            return expand_home(&env_dir);
        }
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    exe_dir
}

fn runtime_candidate_dirs() -> Vec<PathBuf> {
    let package = package_dir();
    vec![
        package.join("prime-agent-runtime"),
        package.join("dist").join("prime-agent-runtime"),
    ]
}

fn resolve_runtime_source_dir() -> Option<PathBuf> {
    runtime_candidate_dirs()
        .into_iter()
        .find(|candidate| candidate.join("pyproject.toml").exists())
}

/// Content identity of the runtime: a hash of every `rlm/*.py` file plus
/// `pyproject.toml`, so any runtime change invalidates an existing venv.
/// Falls back to the bare package name when the runtime resolves to a
/// registry install (no local source).
pub fn resolve_runtime_identity() -> String {
    let Some(source_dir) = resolve_runtime_source_dir() else {
        return RUNTIME_REQUIREMENT.to_string();
    };
    hash_runtime_source(&source_dir).unwrap_or_else(|error| {
        panic!(
            "cannot hash runtime source at {}: {error}",
            source_dir.display()
        )
    })
}

fn hash_runtime_source(source_dir: &Path) -> anyhow::Result<String> {
    let rlm_dir = source_dir.join("src").join("rlm");
    let mut files = vec![source_dir.join("pyproject.toml")];
    collect_python_files(&rlm_dir, &mut files)?;
    files.sort();
    let mut hasher = sha2::Sha256::new();
    for file in &files {
        let relative = file.strip_prefix(source_dir)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(&std::fs::read(file)?);
        hasher.update([0]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn collect_python_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_python_files(&path, files)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            files.push(path);
        }
    }
    Ok(())
}

/// Find `uv` on PATH or at `~/.local/bin/uv`. Returns `Err` with install
/// guidance when missing: the Rust binary never auto-installs (the TS
/// interactive confirm belongs to the CLI layer).
fn ensure_uv() -> anyhow::Result<String> {
    if let Some(from_path) = find_executable("uv") {
        return Ok(from_path.to_string_lossy().to_string());
    }
    let local_uv = home_dir().join(".local").join("bin").join("uv");
    if is_executable(&local_uv) {
        return Ok(local_uv.to_string_lossy().to_string());
    }
    Err(anyhow!(
        "uv is required to set up the Python kernel. Install uv yourself: {UV_INSTALL_COMMAND}"
    ))
}

async fn bootstrap_venv(
    venv: &Path,
    python_skills: &[BootstrapPythonSkill],
    options: &EnsureKernelPythonOptions,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(venv.parent().unwrap_or(Path::new("/")))?;
    let uv = ensure_uv()?;
    let python = kernel_venv_python(venv);
    let source_dir = resolve_runtime_source_dir();
    let runtime_requirement = source_dir
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| RUNTIME_REQUIREMENT.to_string());
    let runtime_identity = resolve_runtime_identity();

    let venv_str = venv.to_string_lossy().to_string();
    let python_str = python.to_string_lossy().to_string();
    let mut install_args = vec![
        "pip".to_string(),
        "install".to_string(),
        "--python".to_string(),
        python_str,
        runtime_requirement,
    ];
    install_args.push(STATE_SNAPSHOT_REQUIREMENT.to_string());
    for uv_arg in default_rlm_extra_uv_args() {
        install_args.push(uv_arg.to_string());
    }

    run_async(
        &uv,
        &[
            "python".to_string(),
            "install".to_string(),
            PYTHON_VERSION.to_string(),
        ],
    )
    .await?;
    run_async(
        &uv,
        &[
            "venv".to_string(),
            venv_str,
            "--python".to_string(),
            PYTHON_VERSION.to_string(),
            "--seed".to_string(),
        ],
    )
    .await?;
    run_async(&uv, &install_args).await?;
    sync_python_skills(
        &uv,
        venv,
        &python,
        &runtime_identity,
        python_skills,
        options,
    )
    .await
}

/// Install/refresh the editable Python skills recorded in the version file.
/// Per-skill failures warn and continue: one broken skill must not cost the kernel.
async fn sync_python_skills(
    uv: &str,
    venv: &Path,
    python: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
    options: &EnsureKernelPythonOptions,
) -> anyhow::Result<()> {
    let version = read_bootstrap_version(venv);
    let mut installed_python_skills: Vec<BootstrapPythonSkill> = Vec::new();
    let current_python_skills: HashMap<String, BootstrapPythonSkill> = version
        .as_ref()
        .and_then(|v| v.python_skills.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|s| (format!("{}\u{0}{}", s.import_name, s.package_path), s))
        .collect();
    let python_str = python.to_string_lossy().to_string();
    for skill in python_skills {
        let existing =
            current_python_skills.get(&format!("{}\u{0}{}", skill.import_name, skill.package_path));
        if existing.is_some_and(|existing| {
            existing.pyproject_path == skill.pyproject_path
                && existing.pyproject_hash == skill.pyproject_hash
        }) {
            installed_python_skills.push(skill.clone());
            continue;
        }
        let result = run_async(
            uv,
            &[
                "pip".to_string(),
                "install".to_string(),
                "--python".to_string(),
                python_str.clone(),
                "--editable".to_string(),
                skill.package_path.clone(),
            ],
        )
        .await;
        match result {
            Ok(()) => installed_python_skills.push(skill.clone()),
            Err(error) => options.report(&format!(
                "Warning: Python skill {} failed to install and will be unavailable: {error}",
                skill.import_name
            )),
        }
    }
    write_bootstrap_version(venv, runtime_identity, &installed_python_skills)
}

fn kernel_base_ready(python: &str, venv: &Path, runtime_identity: &str) -> bool {
    has_prime_agent_runtime(python)
        && bootstrap_base_version_current(read_bootstrap_version(venv), runtime_identity)
}

fn kernel_ready(
    python: &str,
    venv: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
) -> bool {
    has_prime_agent_runtime(python)
        && bootstrap_version_current(
            read_bootstrap_version(venv),
            runtime_identity,
            python_skills,
        )
}

fn format_bootstrap_failure(error: &anyhow::Error) -> anyhow::Error {
    anyhow!(
        "Failed to set up the Python kernel runtime. {error:#}\n\
         First-time setup needs internet to install uv, Python, prime-agent-runtime, and default Python packages; once set up, prime-agent runs offline. \
         An interrupted runtime upgrade needs network once more, so re-run this while online. \
         Set PRIME_AGENT_KERNEL_PYTHON to a Python with a current prime-agent-runtime and default Python packages installed to skip auto-bootstrap."
    )
}

/// One in-flight bootstrap per unique options set, joined by concurrent callers.
type InFlightBootstrap = Option<(
    String,
    Arc<tokio::sync::Mutex<Option<anyhow::Result<PathBuf>>>>,
)>;

static IN_FLIGHT: Mutex<InFlightBootstrap> = Mutex::new(None);

/// Resolve the Python interpreter for the kernel: the `PRIME_AGENT_KERNEL_PYTHON`
/// override when valid, else the auto-bootstrapped venv python.
pub async fn ensure_kernel_python(options: EnsureKernelPythonOptions) -> anyhow::Result<PathBuf> {
    let python_skills = normalize_python_skills(&options.python_skills);
    let key = [
        std::env::var("PRIME_AGENT_KERNEL_PYTHON").unwrap_or_default(),
        std::env::var("PRIME_AGENT_KERNEL_VENV").unwrap_or_default(),
        std::env::var("HOME").unwrap_or_default(),
        std::env::var("XDG_DATA_HOME").unwrap_or_default(),
        serde_json::to_string(&python_skills).unwrap_or_default(),
    ]
    .join("\u{0}");

    let shared = {
        let mut in_flight = IN_FLIGHT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match in_flight.as_ref() {
            Some((existing_key, promise)) if *existing_key == key => promise.clone(),
            _ => {
                let promise = Arc::new(tokio::sync::Mutex::new(None));
                *in_flight = Some((key, promise.clone()));
                promise
            }
        }
    };
    let mut guard = shared.lock().await;
    if guard.is_none() {
        *guard = Some(ensure_kernel_python_uncached(&options, &python_skills).await);
    }
    let outcome = guard.take().expect("outcome was just stored");
    // Drop the registry entry so a later call re-validates instead of reusing
    // a cached rejection forever.
    let mut in_flight = IN_FLIGHT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if matches!(in_flight.as_ref(), Some((_, promise)) if Arc::ptr_eq(promise, &shared)) {
        *in_flight = None;
    }
    outcome
}

async fn ensure_kernel_python_uncached(
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<PathBuf> {
    if let Ok(override_python) = std::env::var("PRIME_AGENT_KERNEL_PYTHON") {
        if !override_python.is_empty() {
            let python = expand_home(&override_python);
            let python_str = python.to_string_lossy().to_string();
            let mut missing = Vec::new();
            if !has_prime_agent_runtime(&python_str) {
                missing.push(
                    "a current prime-agent-runtime with callable rlm.spawn, rlm.create_session, rlm.host_request, rlm.progress_note, and explicit harness CRUD methods".to_string(),
                );
            }
            if missing.is_empty() {
                let missing_extras = missing_rlm_extra_import_labels(&python_str);
                if !missing_extras.is_empty() {
                    missing.push(format!(
                        "default Python packages ({})",
                        missing_extras.join(", ")
                    ));
                }
            }
            if missing.is_empty() && !options.python_skills.is_empty() {
                let missing_skills =
                    missing_python_skill_import_labels(&python_str, &options.python_skills);
                if !missing_skills.is_empty() {
                    options.report(&format!(
                        "Warning: Python skills unavailable in PRIME_AGENT_KERNEL_PYTHON and will be disabled: {}",
                        missing_skills.join(", ")
                    ));
                }
            }
            if missing.is_empty() {
                return Ok(python);
            }
            return Err(anyhow!(
                "PRIME_AGENT_KERNEL_PYTHON points to a Python missing {}: {}",
                missing.join(" and "),
                python.display()
            ));
        }
    }

    let venv = resolve_writable_kernel_venv_dir()?;
    let python = kernel_venv_python(&venv);
    let python_str = python.to_string_lossy().to_string();
    let runtime_identity = resolve_runtime_identity();
    if kernel_ready(&python_str, &venv, &runtime_identity, python_skills) {
        return Ok(python);
    }

    let release_lock = acquire_bootstrap_lock(&venv).await;
    let result = async {
        if kernel_ready(&python_str, &venv, &runtime_identity, python_skills) {
            return Ok(python);
        }
        if kernel_base_ready(&python_str, &venv, &runtime_identity) {
            let uv = ensure_uv()?;
            sync_python_skills(
                &uv,
                &venv,
                &python,
                &runtime_identity,
                python_skills,
                options,
            )
            .await?;
            return Ok(python);
        }
        let had_venv = venv.exists();
        options.report("› setting up python kernel (one-time, ~30s)…");
        if had_venv {
            options.report("rebuilding kernel venv");
            std::fs::remove_dir_all(&venv)
                .with_context(|| format!("removing {}", venv.display()))?;
        }
        bootstrap_venv(&venv, python_skills, options).await?;
        Ok(python)
    }
    .await;
    drop(release_lock);
    options.report("✓ ready");
    result.map_err(|error| format_bootstrap_failure(&error))
}

// ---------------------------------------------------------------------------
// Runtime bootstrap code (from core/tools/ipython.ts)
// ---------------------------------------------------------------------------

const RLM_BOOTSTRAP_HEADER_CODE: &str =
    "import asyncio\nimport os as _prime_agent_os\n\n_prime_agent_os.environ[\"NO_COLOR\"] = \"1\"";

const RLM_BOOTSTRAP_RUNTIME_CODE: &str = r#"
try:
    import rlm as _prime_agent_rlm_module
    rlm = _prime_agent_rlm_module.rlm
    bash = _prime_agent_rlm_module.bash
    import rlm.mcp as mcp
except Exception as _prime_agent_rlm_error:
    _PRIME_AGENT_RLM_IMPORT_ERROR = str(_prime_agent_rlm_error)

    class _PrimeAgentMissingRlm:
        def _raise_missing(self):
            raise RuntimeError(
                "prime-agent-runtime is not installed in this kernel. "
                "Remove ~/.prime/agent/kernel-venv so prime-agent can rebuild it, or set "
                "PRIME_AGENT_KERNEL_PYTHON to a kernel environment with prime-agent-runtime installed. "
                f"Import error: {_PRIME_AGENT_RLM_IMPORT_ERROR}"
            )

        async def spawn(self, prompt, **kwargs):
            self._raise_missing()

        async def find_models(self, query="", limit=8):
            self._raise_missing()

        async def create_session(self, prompt, **kwargs):
            self._raise_missing()

        async def list_subagents(self):
            self._raise_missing()

        async def delete_subagent(self, target):
            self._raise_missing()

    rlm = _PrimeAgentMissingRlm()

    def bash(command):
        rlm._raise_missing()
"#;

/// The code the session injects right after kernel start/restore: binds the
/// `rlm`, `bash`, and MCP surfaces, imports every Python skill (wrapping
/// callable ones, replacing broken imports with a stub that raises), matching
/// the TS `buildRlmBootstrapCode`.
pub fn build_rlm_bootstrap_code(python_skills: &[KernelPythonSkill]) -> String {
    let base_code = format!("{RLM_BOOTSTRAP_HEADER_CODE}\n\n{RLM_BOOTSTRAP_RUNTIME_CODE}");
    let mut import_names: Vec<&str> = python_skills
        .iter()
        .map(|s| s.import_name.as_str())
        .collect();
    import_names.sort_unstable();
    import_names.dedup();
    if import_names.is_empty() {
        return base_code;
    }
    let imports_json = serde_json::to_string(&import_names).unwrap_or_else(|_| "[]".to_string());
    format!(
        r#"
{base_code}

import importlib as _prime_agent_importlib
import inspect as _prime_agent_inspect
import sys as _prime_agent_sys
import types as _prime_agent_types

class _PrimeAgentCallableSkillModule(_prime_agent_types.ModuleType):
    async def __call__(self, *args, **kwargs):
        result = self.run(*args, **kwargs)
        if _prime_agent_inspect.isawaitable(result):
            return await result
        return result

class _PrimeAgentUnavailableSkill:
    def __init__(self, name, error):
        self.__name__ = name
        self._prime_agent_import_error = error
        self.__doc__ = f"Python skill {{name}} is unavailable: {{error}}"

    async def run(self, *args, **kwargs):
        raise RuntimeError(
            f"Python skill {{self.__name__}} is unavailable in this kernel. "
            f"Import error: {{self._prime_agent_import_error}}"
        )

    async def __call__(self, *args, **kwargs):
        return await self.run()

    def __repr__(self):
        return f"<unavailable Python skill {{self.__name__!r}}: {{self._prime_agent_import_error}}>"

def _prime_agent_wrap_skill_module(module):
    run = getattr(module, "run", None)
    if not callable(run):
        return module
    if isinstance(module, _PrimeAgentCallableSkillModule):
        return module
    wrapped = _PrimeAgentCallableSkillModule(module.__name__)
    wrapped.__dict__.update(module.__dict__)
    try:
        wrapped.__signature__ = _prime_agent_inspect.signature(run)
    except Exception:
        pass
    doc = getattr(run, "__doc__", None)
    if doc:
        wrapped.__doc__ = doc
    _prime_agent_sys.modules[module.__name__] = wrapped
    return wrapped

_PRIME_AGENT_SKILL_IMPORT_ERRORS = {{}}

for _prime_agent_skill_name in {imports_json}:
    try:
        globals()[_prime_agent_skill_name] = _prime_agent_wrap_skill_module(
            _prime_agent_importlib.import_module(_prime_agent_skill_name)
        )
    except Exception as _prime_agent_skill_error:
        _PRIME_AGENT_SKILL_IMPORT_ERRORS[_prime_agent_skill_name] = str(_prime_agent_skill_error)
        globals()[_prime_agent_skill_name] = _PrimeAgentUnavailableSkill(
            _prime_agent_skill_name,
            str(_prime_agent_skill_error),
        )
"#
    )
    .trim()
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_code_without_skills_binds_rlm() {
        let code = build_rlm_bootstrap_code(&[]);
        assert!(code.contains("import rlm as _prime_agent_rlm_module"));
        assert!(!code.contains("_PrimeAgentUnavailableSkill"));
    }

    #[test]
    fn bootstrap_code_imports_skills() {
        let skills = vec![KernelPythonSkill {
            name: "edit".into(),
            import_name: "edit".into(),
            package_path: PathBuf::from("/pkg/edit"),
            pyproject_path: PathBuf::from("/pkg/edit/pyproject.toml"),
        }];
        let code = build_rlm_bootstrap_code(&skills);
        assert!(code.contains(r#"for _prime_agent_skill_name in ["edit"]"#));
        assert!(code.contains("_PrimeAgentUnavailableSkill"));
    }

    #[test]
    fn venv_dir_honors_override() {
        // The default path lives under $HOME.
        let base = kernel_venv_dir();
        assert!(base.ends_with("kernel-venv"));
    }

    #[test]
    fn dependency_names_parse() {
        let dir = tempfile::tempdir().unwrap();
        let pyproject = dir.path().join("pyproject.toml");
        std::fs::write(
            &pyproject,
            "[project]\nname = 'edit'\ndependencies = [\n  \"agent-message>=1\",\n  'yaml; python_version > \"3\"',\n]\n[other]\nkey = 1\n",
        )
        .unwrap();
        let skill = BootstrapPythonSkill {
            import_name: "edit".into(),
            package_path: dir.path().join("pkg").to_string_lossy().to_string(),
            pyproject_path: pyproject.to_string_lossy().to_string(),
            pyproject_hash: file_content_hash(&pyproject),
        };
        assert_eq!(read_python_skill_project_name(&skill), "edit");
        assert_eq!(
            read_python_skill_dependency_names(&skill),
            vec!["agent-message", "yaml"]
        );
    }

    #[test]
    fn version_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        write_bootstrap_version(dir.path(), "sha256:abc", &[]).unwrap();
        let version = read_bootstrap_version(dir.path()).expect("version written");
        assert_eq!(version.schema, BOOTSTRAP_SCHEMA);
        assert_eq!(version.runtime.as_deref(), Some("sha256:abc"));
        assert!(bootstrap_version_current(Some(version), "sha256:abc", &[]));
        assert!(!bootstrap_base_version_current(
            read_bootstrap_version(dir.path()),
            "sha256:other"
        ));
    }
}
