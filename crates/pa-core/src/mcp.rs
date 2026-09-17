//! Host side of MCP integrations. The protocol itself runs Python-side in the
//! kernel; the host only gates integration skills by auth and serves `mcp.*`
//! host requests. Port of core/mcp/mcp-manager.ts plus the TS MCP catalog.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::manager::AuthStorage;
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// A built-in MCP integration we ship a skill package for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCatalogEntry {
    /// Matches the skill package import name and the `mcp:<server>` auth key.
    pub server: String,
    pub label: String,
    pub url: String,
}

/// The built-in MCP catalog (user servers go in `mcpServers` instead).
pub const BUILTIN_MCP_CATALOG: &[(&str, &str, &str)] = &[
    ("linear", "Linear", "https://mcp.linear.app/mcp"),
    ("notion", "Notion", "https://mcp.notion.com/mcp"),
];

pub fn get_catalog_entry(server: &str) -> Option<McpCatalogEntry> {
    BUILTIN_MCP_CATALOG
        .iter()
        .find(|(name, _, _)| *name == server)
        .map(|(server, label, url)| McpCatalogEntry {
            server: server.to_string(),
            label: label.to_string(),
            url: url.to_string(),
        })
}

/// A user-declared MCP server config (the `mcpServers` setting).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpServerConfig {
    #[serde(rename_all = "camelCase")]
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        /// Env var holding a static bearer token (skips OAuth).
        #[serde(
            rename = "bearerTokenEnvVar",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        bearer_token_env_var: Option<String>,
        /// Use the generic OAuth login flow for this server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<bool>,
        /// Force-disable even when credentials exist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(
            rename = "enabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        enabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "disabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        disabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "startupTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        startup_timeout_ms: Option<u64>,
        #[serde(
            rename = "callTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        call_timeout_ms: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Environment variables resolved from the kernel environment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, EnvRef>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(
            rename = "enabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        enabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "disabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        disabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "startupTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        startup_timeout_ms: Option<u64>,
        #[serde(
            rename = "callTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        call_timeout_ms: Option<u64>,
    },
}

impl McpServerConfig {
    pub fn server_type(&self) -> &'static str {
        match self {
            McpServerConfig::Http { .. } => "http",
            McpServerConfig::Stdio { .. } => "stdio",
        }
    }

    fn is_enabled(&self) -> bool {
        match self {
            McpServerConfig::Http { enabled, .. } | McpServerConfig::Stdio { enabled, .. } => {
                *enabled != Some(false)
            }
        }
    }
}

/// `{ "env": "<name>" }` stdio env reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

/// Session-scoped server supplied by the active ACP client. This is the
/// TS `AcpMcpServerConfig` wire shape (core/mcp/acp-mcp-types.ts): literal
/// environment values and headers, no settings-only fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AcpMcpServerConfig {
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        cwd: String,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        name: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl AcpMcpServerConfig {
    pub fn name(&self) -> &str {
        match self {
            AcpMcpServerConfig::Stdio { name, .. } | AcpMcpServerConfig::Http { name, .. } => name,
        }
    }
}

/// A resolved integration: catalog/user entry plus auth state.
#[derive(Debug, Clone)]
struct ResolvedIntegration {
    server: String,
    label: String,
    config: McpServerConfig,
    uses_oauth: bool,
    /// True when this came from the `mcpServers` setting.
    user_declared: bool,
}

/// Options for constructing an [`McpManager`].
pub type BeginLoginFuture = Box<dyn Future<Output = anyhow::Result<()>> + Send>;
pub type BeginLoginFn = Arc<dyn Fn(String) -> BeginLoginFuture + Send + Sync>;

pub struct McpManagerOptions {
    pub auth_storage: AuthStorage,
    /// Reads the current `mcpServers` setting; re-read on refresh.
    pub get_user_servers: Box<dyn Fn() -> Option<HashMap<String, McpServerConfig>> + Send + Sync>,
    /// Start an interactive host-side login for a server (UI mode supplies it).
    pub begin_login: Option<BeginLoginFn>,
}

/// Host-side MCP manager: auth gating, config resolution, `mcp.*` host
/// requests, and ACP session servers.
pub struct McpManager {
    auth_storage: Arc<tokio::sync::Mutex<AuthStorage>>,
    get_user_servers: Box<dyn Fn() -> Option<HashMap<String, McpServerConfig>> + Send + Sync>,
    begin_login: Option<BeginLoginFn>,
    integrations: HashMap<String, ResolvedIntegration>,
    acp_servers: std::sync::Arc<std::sync::Mutex<HashMap<String, AcpMcpServerConfig>>>,
    acp_owner_id: std::sync::Mutex<Option<String>>,
}

fn provider_id(server: &str) -> String {
    format!("mcp:{server}")
}

fn uses_oauth(config: &McpServerConfig) -> bool {
    matches!(
        config,
        McpServerConfig::Http {
            oauth: Some(true),
            ..
        }
    )
}

impl McpManager {
    pub fn new(options: McpManagerOptions) -> Self {
        let mut manager = Self {
            auth_storage: Arc::new(tokio::sync::Mutex::new(options.auth_storage)),
            get_user_servers: options.get_user_servers,
            begin_login: options.begin_login,
            integrations: HashMap::new(),
            acp_servers: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            acp_owner_id: std::sync::Mutex::new(None),
        };
        manager.resolve_integrations();
        manager
    }

    /// Re-read settings and re-resolve integrations; call after a reload.
    pub fn refresh(&mut self) {
        self.resolve_integrations();
    }

    fn resolve_integrations(&mut self) {
        let mut integrations = HashMap::new();
        for (server, label, url) in BUILTIN_MCP_CATALOG {
            integrations.insert(
                server.to_string(),
                ResolvedIntegration {
                    server: server.to_string(),
                    label: label.to_string(),
                    config: McpServerConfig::Http {
                        url: url.to_string(),
                        headers: None,
                        bearer_token_env_var: None,
                        oauth: Some(true),
                        enabled: None,
                        enabled_tools: None,
                        disabled_tools: None,
                        startup_timeout_ms: None,
                        call_timeout_ms: None,
                    },
                    uses_oauth: true,
                    user_declared: false,
                },
            );
        }
        if let Some(user_servers) = (self.get_user_servers)() {
            for (server, config) in user_servers {
                integrations.insert(
                    server.clone(),
                    ResolvedIntegration {
                        server: server.clone(),
                        label: server,
                        uses_oauth: uses_oauth(&config),
                        user_declared: true,
                        config,
                    },
                );
            }
        }
        self.integrations = integrations;
    }

    pub fn can_release_acp_servers(&self, owner_id: &str) -> bool {
        self.acp_owner_id
            .lock()
            .unwrap()
            .as_deref()
            .is_none_or(|owner| owner == owner_id)
    }

    pub fn replace_acp_servers(
        &self,
        servers: &[AcpMcpServerConfig],
        owner_id: &str,
    ) -> anyhow::Result<bool> {
        if owner_id.is_empty() {
            anyhow::bail!("ACP MCP owner id is required");
        }
        let owner_fence = self.acp_owner_id.lock().unwrap().clone();
        if servers.is_empty() && owner_fence.as_deref() != Some(owner_id) {
            return Ok(false);
        }
        if !servers.is_empty()
            && owner_fence
                .as_deref()
                .is_some_and(|owner| owner != owner_id)
        {
            anyhow::bail!("ACP MCP configuration is owned by another client");
        }
        let mut next: HashMap<String, AcpMcpServerConfig> = HashMap::new();
        for server in servers {
            if next.contains_key(server.name()) {
                anyhow::bail!("Duplicate ACP MCP server: {}", server.name());
            }
            next.insert(server.name().to_string(), server.clone());
        }
        let mut acp_servers = self.acp_servers.lock().unwrap();
        let unchanged = next.len() == acp_servers.len()
            && next.iter().all(|(name, config)| {
                acp_servers.get(name).is_some_and(|current| {
                    serde_json::to_value(current).ok() == serde_json::to_value(config).ok()
                })
            });
        if unchanged {
            return Ok(false);
        }
        *acp_servers = next;
        *self.acp_owner_id.lock().unwrap() = (!servers.is_empty()).then(|| owner_id.to_string());
        Ok(true)
    }

    /// True when valid credentials exist for the integration (drives enablement).
    fn is_authed(&self, integration: &ResolvedIntegration) -> bool {
        if !integration.config.is_enabled() {
            return false;
        }
        // A user override of a catalog server is not enableable.
        if integration.user_declared && get_catalog_entry(&integration.server).is_some() {
            return false;
        }
        if matches!(integration.config, McpServerConfig::Stdio { .. }) {
            return true;
        }
        let McpServerConfig::Http {
            bearer_token_env_var,
            ..
        } = &integration.config
        else {
            return true;
        };
        if !integration.uses_oauth && bearer_token_env_var.is_none() {
            return true;
        }
        if let Some(env_var) = bearer_token_env_var {
            if std::env::var(env_var)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            {
                return true;
            }
        }
        let all = self.auth_storage_blocking_snapshot();
        let Some(cred) = all.get(&provider_id(&integration.server)) else {
            return false;
        };
        // Builtin URLs are code-constant; only user-declared endpoints can be
        // retargeted, so only their tokens must prove where they belong.
        if !integration.user_declared {
            return true;
        }
        // Only user-declared endpoints can be retargeted, so their tokens
        // must prove where they belong.
        cred.get("endpoint").and_then(Value::as_str) == integration.config_url()
    }

    fn auth_storage_blocking_snapshot(&self) -> serde_json::Map<String, Value> {
        // The auth storage data map, read without refresh side effects.
        let storage = self.auth_storage.clone();
        let handle = storage.blocking_lock();
        let all = handle.get_all();
        match serde_json::to_value(&all) {
            Ok(Value::Object(map)) => map,
            _ => Default::default(),
        }
    }

    /// `-<server>/SKILL.md` overrides for every built-in integration the user
    /// is not logged into.
    pub fn get_disabled_builtin_skill_overrides(&self) -> Vec<String> {
        BUILTIN_MCP_CATALOG
            .iter()
            .filter_map(|(server, _, _)| {
                let integration = self.integrations.get(*server)?;
                (!self.is_authed(integration)).then(|| format!("-{server}/SKILL.md"))
            })
            .collect()
    }

    /// Register the `mcp.*` host-request handlers onto a handler map.
    pub fn register_host_handlers(&self, handlers: &mut HostRequestHandlers) {
        let auth = self.auth_storage.clone();
        let acp_servers = self.acp_servers.clone();
        handlers.register(
            "mcp.refresh",
            host_handler(move |payload| {
                let auth = auth.clone();
                Box::pin(async move {
                    let server = payload
                        .data
                        .get("server")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if server.is_empty() {
                        return Err(anyhow::anyhow!("mcp.refresh requires a server"));
                    }
                    let key = auth.lock().await.get_api_key(&provider_id(&server));
                    if key.is_none() {
                        return Err(anyhow::anyhow!(
                            "Could not refresh credentials for {server}"
                        ));
                    }
                    Ok(json!({}))
                })
            }),
        );
        let integrations = self.integrations.clone();
        handlers.register(
            "mcp.config",
            host_handler(move |payload| {
                let integrations = integrations.clone();
                let acp_servers = acp_servers.clone();
                Box::pin(async move {
                    let server = payload
                        .data
                        .get("server")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if server.is_empty() {
                        return Err(anyhow::anyhow!("mcp.config requires a server"));
                    }
                    let acp = acp_servers.lock().unwrap().get(&server).cloned();
                    if let Some(acp) = acp {
                        let mut config = serde_json::to_value(acp).unwrap_or(Value::Null);
                        if let Value::Object(map) = &mut config {
                            map.insert("credentialSource".to_string(), json!("acp"));
                        }
                        return Ok(config);
                    }
                    let Some(integration) = integrations.get(&server) else {
                        return Ok(json!({}));
                    };
                    if !integration.user_declared || get_catalog_entry(&server).is_some() {
                        return Ok(json!({}));
                    }
                    serde_json::to_value(&integration.config).map_err(anyhow::Error::new)
                })
            }),
        );
        // Only expose begin_login when an interactive login is actually wired.
        if let Some(begin_login) = self.begin_login.clone() {
            handlers.register(
                "mcp.begin_login",
                host_handler(move |payload| {
                    let begin_login = begin_login.clone();
                    Box::pin(async move {
                        let server = payload
                            .data
                            .get("server")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if server.is_empty() {
                            return Err(anyhow::anyhow!("mcp.begin_login requires a server"));
                        }
                        let mut future = std::pin::Pin::from(begin_login(server));
                        future.as_mut().await?;
                        Ok(json!({}))
                    })
                }),
            );
        }
    }

    /// Session-scoped servers supplied by the active ACP client.
    pub fn get_acp_servers(&self) -> Vec<AcpMcpServerConfig> {
        self.acp_servers.lock().unwrap().values().cloned().collect()
    }

    /// Enabled user-declared servers available through the generic kernel API.
    pub fn get_enabled_persistent_generic_servers(&self) -> Vec<String> {
        let mut servers: Vec<String> = self
            .integrations
            .values()
            .filter(|integration| {
                integration.user_declared
                    && is_generic_server_name(&integration.server)
                    && get_catalog_entry(&integration.server).is_none()
                    && self.is_authed(integration)
            })
            .map(|integration| integration.server.clone())
            .collect();
        servers.sort();
        servers
    }

    /// Status for the `/mcp list` command.
    pub fn list_status(&self) -> Vec<McpServerStatus> {
        self.integrations
            .values()
            .map(|integration| McpServerStatus {
                server: integration.server.clone(),
                label: integration.label.clone(),
                enabled: self.is_authed(integration),
                uses_oauth: integration.uses_oauth,
            })
            .collect()
    }
}

/// One `/mcp list` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub server: String,
    pub label: String,
    pub enabled: bool,
    pub uses_oauth: bool,
}

fn is_generic_server_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().expect("checked non-empty");
    if !(first.is_ascii_alphanumeric()) {
        return false;
    }
    chars.all(|char| char.is_ascii_alphanumeric() || char == '_' || char == '-')
}

impl ResolvedIntegration {
    fn config_url(&self) -> Option<&str> {
        match &self.config {
            McpServerConfig::Http { url, .. } => Some(url.as_str()),
            McpServerConfig::Stdio { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_auth_storage() -> AuthStorage {
        AuthStorage::in_memory(
            Default::default(),
            std::sync::Arc::new(crate::auth::manager::NoOAuth),
        )
    }

    fn manager_with(user_servers: Option<HashMap<String, McpServerConfig>>) -> McpManager {
        McpManager::new(McpManagerOptions {
            auth_storage: test_auth_storage(),
            get_user_servers: Box::new(move || user_servers.clone()),
            begin_login: None,
        })
    }

    fn http_config(url: &str, oauth: Option<bool>, env_var: Option<&str>) -> McpServerConfig {
        McpServerConfig::Http {
            url: url.to_string(),
            headers: None,
            bearer_token_env_var: env_var.map(|value| value.to_string()),
            oauth,
            enabled: None,
            enabled_tools: None,
            disabled_tools: None,
            startup_timeout_ms: None,
            call_timeout_ms: None,
        }
    }

    #[test]
    fn builtin_catalog_and_skill_overrides() {
        // Both built-ins start disabled without credentials.
        let manager = manager_with(None);
        let overrides = manager.get_disabled_builtin_skill_overrides();
        assert_eq!(
            overrides,
            vec![
                "-linear/SKILL.md".to_string(),
                "-notion/SKILL.md".to_string()
            ]
        );
        let status = manager.list_status();
        assert_eq!(status.len(), 2);
        let linear = status.iter().find(|row| row.server == "linear").unwrap();
        assert!(!linear.enabled);
        assert!(linear.uses_oauth);
        // No user servers -> no generic persistent servers.
        assert!(manager.get_enabled_persistent_generic_servers().is_empty());
    }

    #[test]
    fn stdio_and_env_token_servers_are_enabled() {
        let mut user_servers = HashMap::new();
        user_servers.insert(
            "toolchain".to_string(),
            McpServerConfig::Stdio {
                command: "npx".to_string(),
                args: Some(vec!["-y".to_string(), "server".to_string()]),
                cwd: None,
                env: None,
                enabled: None,
                enabled_tools: None,
                disabled_tools: None,
                startup_timeout_ms: None,
                call_timeout_ms: None,
            },
        );
        user_servers.insert(
            "search".to_string(),
            http_config("https://search.example/mcp", None, Some("SEARCH_TOKEN")),
        );
        let manager = manager_with(Some(user_servers));
        let generic = manager.get_enabled_persistent_generic_servers();
        assert!(generic.contains(&"toolchain".to_string()));
        // The env-var server stays disabled until SEARCH_TOKEN is set in the
        // environment (bearer-token auth requires the token at hand).
        assert!(!generic.contains(&"search".to_string()));
        // Catalog overrides by users are not enableable.
        let mut override_servers = HashMap::new();
        override_servers.insert(
            "linear".to_string(),
            http_config("https://evil.example/mcp", None, None),
        );
        let manager = manager_with(Some(override_servers));
        assert!(!manager
            .get_enabled_persistent_generic_servers()
            .contains(&"linear".to_string()));
    }

    #[test]
    fn generic_name_pattern() {
        assert!(is_generic_server_name("a"));
        assert!(is_generic_server_name("Tool_2-go"));
        assert!(!is_generic_server_name("")); // empty
        assert!(!is_generic_server_name("-leading"));
        assert!(!is_generic_server_name("has space"));
        assert!(!is_generic_server_name(&"x".repeat(65)));
    }

    #[test]
    fn acp_server_ownership() {
        let manager = manager_with(None);
        assert!(manager.can_release_acp_servers("client-a"));
        let servers = vec![AcpMcpServerConfig::Stdio {
            name: "session-tool".to_string(),
            command: "run".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
        }];
        assert!(manager.replace_acp_servers(&servers, "client-a").unwrap());
        // Same owner, identical servers -> no change reported.
        assert!(!manager.replace_acp_servers(&servers, "client-a").unwrap());
        // Another client cannot take ownership.
        assert!(manager.replace_acp_servers(&servers, "client-b").is_err());
        assert!(!manager.can_release_acp_servers("client-b"));
        assert!(manager.can_release_acp_servers("client-a"));
        // Duplicates are rejected.
        let duplicate = vec![servers[0].clone(), servers[0].clone()];
        assert!(manager.replace_acp_servers(&duplicate, "client-a").is_err());
        // Clearing requires the owner.
        assert!(manager.replace_acp_servers(&[], "client-a").unwrap());
        assert_eq!(manager.get_acp_servers().len(), 0);
        // Owner id is required.
        assert!(manager.replace_acp_servers(&servers, "").is_err());
    }

    #[tokio::test]
    async fn config_host_handler_returns_user_stdio_server_config() {
        // The product path for a settings-declared stdio server: gating
        // enables it for the generic kernel API and `mcp.config` hands the
        // kernel the exact command/args the user declared.
        let mut user_servers = HashMap::new();
        user_servers.insert(
            "fixture-echo".to_string(),
            McpServerConfig::Stdio {
                command: "python3".to_string(),
                args: Some(vec!["fixtures/mcp_echo_server.py".to_string()]),
                cwd: None,
                env: None,
                enabled: None,
                enabled_tools: Some(vec!["echo".to_string()]),
                disabled_tools: None,
                startup_timeout_ms: None,
                call_timeout_ms: None,
            },
        );
        let manager = manager_with(Some(user_servers));
        assert_eq!(
            manager.get_enabled_persistent_generic_servers(),
            vec!["fixture-echo".to_string()]
        );
        let mut handlers = HostRequestHandlers::default();
        manager.register_host_handlers(&mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        let result = config(crate::kernel::shared::HostRequestPayload {
            data: json!({ "server": "fixture-echo" }),
            cell_source_code: None,
        })
        .await
        .unwrap();
        assert_eq!(result["type"], "stdio");
        assert_eq!(result["command"], "python3");
        assert_eq!(result["args"], json!(["fixtures/mcp_echo_server.py"]));
        assert_eq!(result["enabledTools"], json!(["echo"]));
    }

    #[tokio::test]
    async fn config_host_handler_resolves_user_and_acp_servers() {
        let manager = manager_with(None);
        let mut handlers = HostRequestHandlers::default();
        manager.register_host_handlers(&mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        // Unknown server -> empty object.
        let result = config(crate::kernel::shared::HostRequestPayload {
            data: json!({ "server": "nope" }),
            cell_source_code: None,
        })
        .await
        .unwrap();
        assert!(result.as_object().unwrap().is_empty());
        // ACP server -> config with credentialSource.
        let servers = vec![AcpMcpServerConfig::Stdio {
            name: "session-tool".to_string(),
            command: "run".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
        }];

        manager.replace_acp_servers(&servers, "client-a").unwrap();
        let mut handlers = HostRequestHandlers::default();
        manager.register_host_handlers(&mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        let result = config(crate::kernel::shared::HostRequestPayload {
            data: json!({ "server": "session-tool" }),
            cell_source_code: None,
        })
        .await
        .unwrap();
        assert_eq!(result["type"], "stdio");
        assert_eq!(result["credentialSource"], "acp");
        // Missing server argument errors.
        let error = config(crate::kernel::shared::HostRequestPayload {
            data: json!({}),
            cell_source_code: None,
        })
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "mcp.config requires a server");
        // begin_login is not registered when no login is wired.
        assert!(handlers.get("mcp.begin_login").is_none());
    }
}
