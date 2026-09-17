//! Wires the session runtime (goal + rlm-heartbeat host bridge) and the
//! Python kernel into the session engine build. This is the product-path
//! equivalent of the TS `AgentSession` host-request controllers: the kernel
//! provisioner receives the host-handler registry, and the agent loop gains
//! the `ipython` tool backed by that kernel.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::cron::store::AgentCronJobStore;
use crate::kernel::bootstrap::KernelPythonSkill;
use crate::kernel::provisioner::{
    IpythonKernelProvisioner as KernelProvisioner, IpythonKernelProvisionerOptions,
};
use crate::kernel::shared::HostRequestHandlers;
use crate::session::manager::SessionManager;
use crate::skills::{get_python_skill_runtime_info, Skill};
use crate::tools::ipython::{
    IpythonKernelProvisioner, IpythonToolOptions, KernelAttachment, KernelErrorInfo,
    KernelExecError, KernelExecuteOptions, KernelExecutor,
};

use super::host_requests::SessionBinding;
use super::rlm_host::{register_rlm_host_handlers, RlmHostBridge, RlmSubagentHost};
use super::runtime::SessionRuntime;

/// RLM inputs the session composition supplies: a shared model registry and
/// the daemon child-session host. Both optional; defaults are derived from
/// `agent_dir` (registry) or the no-children behavior (host).
#[derive(Default)]
pub struct RlmWiring {
    /// Registry `rlm.find_models` searches. Defaults to the agent_dir catalog.
    pub model_registry: Option<Arc<crate::models::registry::ModelRegistry>>,
    /// Child-session machinery backing `rlm.spawn`/`rlm.create_session` and
    /// the roster/collect/delete surface.
    pub subagent_host: Option<Arc<dyn RlmSubagentHost>>,
}

/// Session-scoped runtime wiring: the shared session manager handle, the
/// kernel host-handler registry, and the runtime itself.
pub struct SessionKernelWiring {
    pub session: Arc<tokio::sync::Mutex<SessionManager>>,
    pub handlers: HostRequestHandlers,
    pub runtime: Arc<SessionRuntime>,
    /// The RLM bridge: progress-note state the daemon roster reads.
    pub rlm: Arc<RlmHostBridge>,
}

/// Build the session runtime and register the `goal.*`, `rlm_heartbeat.*`,
/// and `rlm.*` host handlers the kernel reaches through its registry.
pub fn wire_session_runtime(
    session: SessionManager,
    agent_dir: &std::path::Path,
    rlm: RlmWiring,
) -> SessionKernelWiring {
    let binding = SessionBinding {
        session_id: session.get_session_id().to_string(),
        session_file: session
            .get_session_file()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        cwd: session.get_cwd().display().to_string(),
    };
    let active_session_id = session.get_session_id().to_string();
    let cron_store = Arc::new(AgentCronJobStore::new(agent_dir.join("cron-jobs.json")));
    let runtime = Arc::new(SessionRuntime::new(
        &session,
        cron_store,
        active_session_id,
        binding,
    ));
    let session = Arc::new(tokio::sync::Mutex::new(session));
    let mut handlers = HostRequestHandlers::default();
    runtime.register_host_handlers(session.clone(), &mut handlers);
    let model_registry = rlm.model_registry.unwrap_or_else(|| {
        let auth = crate::auth::AuthStorage::create(agent_dir);
        Arc::new(crate::models::registry::ModelRegistry::create(
            auth,
            agent_dir.join("models.json"),
        ))
    });
    let rlm_bridge = Arc::new(RlmHostBridge::new(model_registry, rlm.subagent_host));
    register_rlm_host_handlers(&mut handlers, &rlm_bridge);
    SessionKernelWiring {
        session,
        handlers,
        runtime,
        rlm: rlm_bridge,
    }
}

/// Kernel-side Python skill modules, pre-imported at bootstrap.
pub fn kernel_python_skills(skills: &[Skill]) -> Vec<KernelPythonSkill> {
    get_python_skill_runtime_info(skills)
        .into_iter()
        .map(|info| KernelPythonSkill {
            name: info.name,
            import_name: info.import_name,
            package_path: info.package_path,
            pyproject_path: info.pyproject_path,
        })
        .collect()
}

/// Build the kernel provisioner for a session: host handlers for the
/// goal/heartbeat bridge plus the pre-imported Python skills.
///
/// The session's agent dir is propagated explicitly into the kernel env
/// (PRIME_AGENT_CODING_AGENT_DIR): ambient inheritance is correct for the
/// product paths, but an embedding host whose ambient env differs from the
/// session's agent dir must not leak its own paths into the kernel. Same
/// discipline as the daemon worker env (#109).
pub fn kernel_provisioner(
    session_id: String,
    handlers: HostRequestHandlers,
    python_skills: Vec<KernelPythonSkill>,
    agent_dir: &std::path::Path,
) -> Arc<KernelProvisioner> {
    let mut env = HashMap::with_capacity(1);
    env.insert(
        "PRIME_AGENT_CODING_AGENT_DIR".to_string(),
        agent_dir.to_string_lossy().to_string(),
    );
    Arc::new(KernelProvisioner::new(
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        IpythonKernelProvisionerOptions {
            python: None,
            env,
            command_prefix: None,
            shell_path: None,
            session_id: Some(session_id),
            host_handlers: handlers,
            python_skills,
            snapshot_dir: None,
            ready_gate: None,
            on_restore: None,
        },
    ))
}

impl IpythonKernelProvisioner for KernelProvisioner {
    fn ensure(
        &self,
        on_progress: Option<crate::tools::ipython::BootstrapProgressHandler>,
        signal: Option<crate::tools::tool_definition::AbortSignal>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn KernelExecutor>>> + Send>> {
        let this = self.clone();
        Box::pin(async move {
            // The tool contract uses the raw cancellation token; the kernel
            // wraps it in its own abort signal.
            let signal = signal.map(crate::kernel::cancellation::AbortSignal::from_token);
            let manager = this.ensure(on_progress, signal).await?;
            Ok(Box::new(KernelManagerExecutor { manager }) as Box<dyn KernelExecutor>)
        })
    }

    fn kill(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let this = self.clone();
        Box::pin(async move {
            this.kill().await;
        })
    }
}

/// Adapts the kernel manager to the ipython tool's executor contract,
/// converting the kernel protocol result to the tool-facing shape.
struct KernelManagerExecutor {
    manager: crate::kernel::manager::ReplKernelManager,
}

impl KernelExecutor for KernelManagerExecutor {
    fn execute(
        &self,
        code: &str,
        options: KernelExecuteOptions<'_>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<crate::tools::ipython::ExecuteResult, KernelExecError>>
                + Send,
        >,
    > {
        let manager = self.manager.clone();
        let code = code.to_string();
        let signal = options.signal.clone();
        Box::pin(async move {
            let result = manager
                .execute(
                    &code,
                    crate::kernel::shared::ExecuteOptions {
                        signal: signal.map(crate::kernel::cancellation::AbortSignal::from_token),
                        ..Default::default()
                    },
                )
                .await
                .map_err(KernelExecError::Other)?;
            Ok(convert_execute_result(result))
        })
    }
}

fn convert_status(
    status: crate::kernel::shared::ExecuteStatus,
) -> crate::tools::ipython::ExecuteStatus {
    match status {
        crate::kernel::shared::ExecuteStatus::Ok => crate::tools::ipython::ExecuteStatus::Ok,
        crate::kernel::shared::ExecuteStatus::Error => crate::tools::ipython::ExecuteStatus::Error,
        crate::kernel::shared::ExecuteStatus::Aborted => {
            crate::tools::ipython::ExecuteStatus::Aborted
        }
    }
}

fn convert_execute_result(
    result: crate::kernel::shared::ExecuteResult,
) -> crate::tools::ipython::ExecuteResult {
    crate::tools::ipython::ExecuteResult {
        status: convert_status(result.status),
        stdout: result.stdout,
        stderr: result.stderr,
        result: result.result,
        duration_ms: Some(result.duration_ms),
        background_output: result.background_output,
        error: result.error.map(|error| KernelErrorInfo {
            ename: error.ename,
            evalue: error.evalue,
            traceback: error.traceback,
        }),
        attachments: result
            .attachments
            .unwrap_or_default()
            .into_iter()
            .map(|attachment| KernelAttachment {
                mime_type: attachment.mime_type,
                data: attachment.data,
            })
            .collect(),
    }
}

/// Build the ipython tool options for a wired kernel provisioner.
pub fn ipython_tool_options(provisioner: Arc<KernelProvisioner>) -> IpythonToolOptions {
    IpythonToolOptions {
        provisioner,
        ui: None,
    }
}
