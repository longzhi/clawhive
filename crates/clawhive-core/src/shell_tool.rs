use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_bus::BusPublisher;
use clawhive_provider::ToolDef;
use clawhive_schema::{ApprovalDecision, BusMessage};
use corral_core::{
    start_broker, BrokerConfig, Permissions, PolicyEngine, Sandbox, SandboxConfig, ServiceHandler,
    ServicePermission,
};

use super::access_gate::{AccessGate, AccessLevel};
use super::approval::ApprovalRegistry;
use super::config::{
    ExecAskMode, ExecSecurityConfig, ExecSecurityMode, SandboxNetworkMode, SandboxPolicyConfig,
};
use super::tool::{ToolContext, ToolExecutor, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 50_000;

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
    gate: Arc<AccessGate>,
    exec_security: ExecSecurityConfig,
    sandbox_config: SandboxPolicyConfig,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    bus: Option<BusPublisher>,
    agent_id: String,
}

impl ExecuteCommandTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: PathBuf,
        default_timeout: u64,
        gate: Arc<AccessGate>,
        exec_security: ExecSecurityConfig,
        sandbox_config: SandboxPolicyConfig,
        approval_registry: Option<Arc<ApprovalRegistry>>,
        bus: Option<BusPublisher>,
        agent_id: String,
    ) -> Self {
        Self {
            workspace,
            default_timeout,
            gate,
            exec_security,
            sandbox_config,
            approval_registry,
            bus,
            agent_id,
        }
    }

    async fn wait_for_approval(
        &self,
        command: &str,
        source_info: Option<(&str, &str, &str)>,
    ) -> Result<Option<String>> {
        let Some(registry) = self.approval_registry.as_ref() else {
            return Ok(Some(
                "Command not in allowlist and no approval UI available".to_string(),
            ));
        };

        let trace_id = uuid::Uuid::new_v4();
        tracing::info!(command, %trace_id, "requesting exec approval");

        let rx = registry
            .request(trace_id, command.to_string(), self.agent_id.clone())
            .await;

        if let (Some(bus), Some((ch_type, conn_id, conv_scope))) = (self.bus.as_ref(), source_info)
        {
            let _ = bus
                .publish(BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: format!("Command requires approval: {command}"),
                    agent_id: self.agent_id.clone(),
                    command: command.to_string(),
                    network_target: None,
                    source_channel_type: Some(ch_type.to_string()),
                    source_connector_id: Some(conn_id.to_string()),
                    source_conversation_scope: Some(conv_scope.to_string()),
                })
                .await;
        }

        match rx.await {
            Ok(ApprovalDecision::AllowOnce) => Ok(None),
            Ok(ApprovalDecision::AlwaysAllow) => {
                let first_token = command.split_whitespace().next().unwrap_or(command);
                let pattern = format!("{first_token} *");
                registry
                    .add_runtime_allow_pattern(&self.agent_id, pattern.clone())
                    .await;
                tracing::info!(pattern, "adding to exec allowlist");
                Ok(None)
            }
            Ok(ApprovalDecision::Deny) | Err(_) => Ok(Some("Command denied by user".to_string())),
        }
    }

    async fn wait_for_network_approval(
        &self,
        command: &str,
        host: &str,
        port: u16,
        source_info: Option<(&str, &str, &str)>,
    ) -> Result<Option<String>> {
        let Some(registry) = self.approval_registry.as_ref() else {
            return Ok(Some(
                "Network access requires approval but no approval UI available".to_string(),
            ));
        };

        let target = format!("{host}:{port}");
        let trace_id = uuid::Uuid::new_v4();
        tracing::info!(command, %trace_id, target, "requesting network approval");

        let rx = registry
            .request(trace_id, command.to_string(), self.agent_id.clone())
            .await;

        if let (Some(bus), Some((ch_type, conn_id, conv_scope))) = (self.bus.as_ref(), source_info)
        {
            let _ = bus
                .publish(BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: format!("Network access: {target}"),
                    agent_id: self.agent_id.clone(),
                    command: command.to_string(),
                    network_target: Some(target.clone()),
                    source_channel_type: Some(ch_type.to_string()),
                    source_connector_id: Some(conn_id.to_string()),
                    source_conversation_scope: Some(conv_scope.to_string()),
                })
                .await;
        }

        match rx.await {
            Ok(ApprovalDecision::AllowOnce) => Ok(None),
            Ok(ApprovalDecision::AlwaysAllow) => {
                registry
                    .add_network_allow_pattern(&self.agent_id, target)
                    .await;
                tracing::info!(host, port, "adding to network allowlist");
                Ok(None)
            }
            Ok(ApprovalDecision::Deny) | Err(_) => Ok(Some(format!(
                "Network access to {host}:{port} denied by user"
            ))),
        }
    }

    fn is_command_allowed(&self, command: &str) -> bool {
        let cmd_lower = command.to_lowercase();
        let first_token = command.split_whitespace().next().unwrap_or("");
        let basename = std::path::Path::new(first_token)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(first_token);

        if self.exec_security.safe_bins.iter().any(|b| b == basename) {
            return true;
        }

        self.exec_security.allowlist.iter().any(|pattern| {
            if pattern.ends_with(" *") {
                let prefix = &pattern[..pattern.len() - 2];
                basename == prefix || first_token == prefix
            } else {
                cmd_lower == pattern.to_lowercase() || basename == pattern
            }
        })
    }
}

/// Extract target hosts from command arguments (best-effort URL parsing)
fn extract_network_targets(command: &str) -> Vec<(String, u16)> {
    let mut targets = Vec::new();
    for token in command.split_whitespace() {
        if let Ok(url) = reqwest::Url::parse(token) {
            if let Some(host) = url.host_str() {
                let port = url.port_or_known_default().unwrap_or(443);
                targets.push((host.to_string(), port));
            }
        }
    }
    targets
}

/// Known package manager commands and their registry domains
fn package_manager_domains(command: &str) -> Vec<String> {
    let first_token = command.split_whitespace().next().unwrap_or("");
    let basename = std::path::Path::new(first_token)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first_token);
    match basename {
        "npm" | "npx" | "yarn" | "pnpm" => {
            vec!["registry.npmjs.org".into(), "registry.yarnpkg.com".into()]
        }
        "pip" | "pip3" => vec!["pypi.org".into(), "files.pythonhosted.org".into()],
        "cargo" => vec!["crates.io".into(), "static.crates.io".into()],
        "gem" => vec!["rubygems.org".into()],
        "go" => vec!["proxy.golang.org".into()],
        _ => vec![],
    }
}

/// Check if a network target matches a domain pattern from the whitelist
fn domain_matches(pattern: &str, host: &str) -> bool {
    if pattern == host {
        return true;
    }
    // Wildcard: *.example.com matches sub.example.com
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host.ends_with(suffix) && host.len() > suffix.len();
    }
    false
}

struct RemindersHandler;

#[async_trait]
impl ServiceHandler for RemindersHandler {
    async fn handle(
        &self,
        method: &str,
        params: &serde_json::Value,
        policy: &PolicyEngine,
    ) -> Result<serde_json::Value> {
        match method {
            "list" => {
                let list = params.get("list").and_then(|v| v.as_str());
                if let Some(list_name) = list {
                    policy.check_reminders_scope_result(list_name)?;
                }
                policy.check_service_result("reminders", "list", params)?;

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("list");
                if let Some(list_name) = list {
                    cmd.arg(list_name);
                }
                cmd.arg("--json").arg("--no-input");

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl list failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "add" => {
                let list = params
                    .get("list")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.add requires 'list'"))?;
                let title = params
                    .get("title")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.add requires 'title'"))?;

                policy.check_service_result("reminders", "add", params)?;
                policy.check_reminders_scope_result(list)?;

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("add")
                    .arg("--title")
                    .arg(title)
                    .arg("--list")
                    .arg(list)
                    .arg("--json")
                    .arg("--no-input");

                if let Some(due) = params.get("dueDate").and_then(|v| v.as_str()) {
                    cmd.arg("--due").arg(due);
                }
                if let Some(notes) = params.get("notes").and_then(|v| v.as_str()) {
                    cmd.arg("--notes").arg(notes);
                }
                if let Some(priority) = params.get("priority").and_then(|v| v.as_str()) {
                    cmd.arg("--priority").arg(priority);
                }

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl add failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "update" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.update requires 'id'"))?;

                policy.check_service_result("reminders", "update", params)?;
                if let Some(list_name) = params.get("list").and_then(|v| v.as_str()) {
                    policy.check_reminders_scope_result(list_name)?;
                }

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("edit").arg(id).arg("--json").arg("--no-input");

                if let Some(title) = params.get("title").and_then(|v| v.as_str()) {
                    cmd.arg("--title").arg(title);
                }
                if let Some(list_name) = params.get("list").and_then(|v| v.as_str()) {
                    cmd.arg("--list").arg(list_name);
                }
                if let Some(due) = params.get("dueDate").and_then(|v| v.as_str()) {
                    cmd.arg("--due").arg(due);
                }
                if params.get("clearDue").and_then(|v| v.as_bool()) == Some(true) {
                    cmd.arg("--clear-due");
                }
                if let Some(notes) = params.get("notes").and_then(|v| v.as_str()) {
                    cmd.arg("--notes").arg(notes);
                }
                if let Some(priority) = params.get("priority").and_then(|v| v.as_str()) {
                    cmd.arg("--priority").arg(priority);
                }

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl edit failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "complete" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.complete requires 'id'"))?;

                policy.check_service_result("reminders", "complete", params)?;

                let output = tokio::process::Command::new("remindctl")
                    .arg("complete")
                    .arg(id)
                    .arg("--json")
                    .arg("--no-input")
                    .output()
                    .await?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl complete failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "delete" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.delete requires 'id'"))?;

                policy.check_service_result("reminders", "delete", params)?;

                let output = tokio::process::Command::new("remindctl")
                    .arg("delete")
                    .arg(id)
                    .arg("--force")
                    .arg("--json")
                    .arg("--no-input")
                    .output()
                    .await?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl delete failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            _ => Err(anyhow!("Unknown reminders method: {method}")),
        }
    }

    fn namespace(&self) -> &str {
        "reminders"
    }
}

fn collect_env_vars(env_inherit: &[String]) -> HashMap<String, String> {
    let mut env_vars = HashMap::new();
    for key in env_inherit {
        if key == "PATH" {
            let inherited = std::env::var("PATH").unwrap_or_default();
            let merged = augment_path_like_host(&inherited, &default_path_candidates());
            env_vars.insert(key.clone(), merged);
            continue;
        }
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.clone(), val);
        }
    }
    env_vars
}

pub fn default_path_candidates() -> Vec<String> {
    let mut candidates = vec![
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/local/sbin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];

    if let Ok(home) = std::env::var("HOME") {
        candidates.extend([
            format!("{home}/.clawhive/bin"),
            format!("{home}/.cargo/bin"),
            format!("{home}/.bun/bin"),
            format!("{home}/.local/bin"),
            format!("{home}/bin"),
        ]);
    }

    candidates
}

pub fn augment_path_like_host(current_path: &str, candidates: &[String]) -> String {
    let mut entries: Vec<PathBuf> = std::env::split_paths(current_path).collect();
    let mut seen: HashSet<OsString> = entries
        .iter()
        .map(|p| p.as_os_str().to_os_string())
        .collect();

    for candidate in candidates {
        if candidate.trim().is_empty() {
            continue;
        }
        let path = PathBuf::from(candidate);
        let key = path.as_os_str().to_os_string();
        if seen.insert(key) {
            entries.push(path);
        }
    }

    match std::env::join_paths(entries) {
        Ok(os) => os.to_string_lossy().into_owned(),
        Err(_) => current_path.to_string(),
    }
}

/// On macOS `/tmp`, `/var`, `/etc` are symlinks to `/private/{…}`.
/// Returns the alternate form so sandbox patterns cover both.
fn macos_symlink_alias(path: &str) -> Option<String> {
    for prefix in &["/tmp", "/var", "/etc"] {
        let private = format!("/private{prefix}");
        if let Some(rest) = path.strip_prefix(private.as_str()) {
            if rest.is_empty() || rest.starts_with('/') {
                return Some(format!("{prefix}{rest}"));
            }
        }
        if let Some(rest) = path.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with('/') {
                return Some(format!("{private}{rest}"));
            }
        }
    }
    None
}

fn base_permissions(
    workspace: &Path,
    extra_dirs: &[(PathBuf, AccessLevel)],
    exec_allow: &[String],
    network_allowed: bool,
    env_inherit: &[String],
) -> Permissions {
    let workspace_self = workspace.display().to_string();
    let workspace_pattern = format!("{workspace_self}/**");
    // Include the directory itself (for opendir) AND its contents (for files within)
    let mut read_patterns = vec![workspace_self.clone(), workspace_pattern.clone()];
    let mut write_patterns = vec![workspace_self, workspace_pattern];

    for (dir, level) in extra_dirs {
        let dir_self = dir.display().to_string();

        // Collect the canonical path and its symlink alias (if any).
        // On macOS /tmp, /var, /etc are symlinks to /private/{…}; the sandbox
        // checks literal strings, so we must include both forms.
        let mut paths = vec![dir_self.clone()];
        if let Some(alias) = macos_symlink_alias(&dir_self) {
            paths.push(alias);
        }

        for p in &paths {
            read_patterns.push(p.clone());
            read_patterns.push(format!("{p}/**"));
            if *level == AccessLevel::Rw {
                write_patterns.push(p.clone());
                write_patterns.push(format!("{p}/**"));
            }
        }
    }

    let mut builder = Permissions::builder()
        .fs_read(read_patterns)
        .fs_write(write_patterns)
        .exec_allow(exec_allow.iter().map(|s| s.as_str()));

    if network_allowed {
        builder = builder.network_allow(["*:*"]);
    } else {
        builder = builder.network_deny();
    }

    builder
        .env_allow(env_inherit.iter().map(|s| s.as_str()))
        .build()
}

fn make_sandbox(
    workspace: &Path,
    extra_dirs: &[(PathBuf, AccessLevel)],
    sandbox_cfg: &SandboxPolicyConfig,
) -> Result<Sandbox> {
    tracing::debug!(
        workspace = %workspace.display(),
        extra_dirs_count = extra_dirs.len(),
        network_mode = ?sandbox_cfg.network,
        exec_allow_count = sandbox_cfg.exec_allow.len(),
        "building sandbox with permissions"
    );
    let network_allowed = match sandbox_cfg.network {
        SandboxNetworkMode::Allow | SandboxNetworkMode::Ask => true,
        SandboxNetworkMode::Deny => false,
    };
    let config = SandboxConfig {
        permissions: base_permissions(
            workspace,
            extra_dirs,
            &sandbox_cfg.exec_allow,
            network_allowed,
            &sandbox_cfg.env_inherit,
        ),
        work_dir: workspace.to_path_buf(),
        data_dir: None,
        timeout: Duration::from_secs(sandbox_cfg.timeout_secs),
        max_memory_mb: Some(sandbox_cfg.max_memory_mb),
        env_vars: collect_env_vars(&sandbox_cfg.env_inherit),
        broker_socket: None,
    };
    Sandbox::new(config)
}

async fn sandbox_with_broker(
    workspace: &Path,
    timeout_secs: u64,
    reminders_lists: &[String],
    extra_dirs: &[(PathBuf, AccessLevel)],
    sandbox_cfg: &SandboxPolicyConfig,
) -> Result<Sandbox> {
    tracing::debug!(
        workspace = %workspace.display(),
        extra_dirs_count = extra_dirs.len(),
        network_mode = ?sandbox_cfg.network,
        exec_allow_count = sandbox_cfg.exec_allow.len(),
        reminders_lists_count = reminders_lists.len(),
        "building sandbox with broker and reminders service"
    );
    let network_allowed = match sandbox_cfg.network {
        SandboxNetworkMode::Allow | SandboxNetworkMode::Ask => true,
        SandboxNetworkMode::Deny => false,
    };
    let mut permissions = base_permissions(
        workspace,
        extra_dirs,
        &sandbox_cfg.exec_allow,
        network_allowed,
        &sandbox_cfg.env_inherit,
    );

    let mut scope = HashMap::new();
    if !reminders_lists.is_empty() {
        scope.insert("lists".to_string(), serde_json::json!(reminders_lists));
    }
    permissions.services.insert(
        "reminders".to_string(),
        ServicePermission {
            access: "readwrite".to_string(),
            scope,
        },
    );

    let mut broker_config = BrokerConfig::new(PolicyEngine::new(permissions.clone()));
    broker_config.register_handler(Arc::new(RemindersHandler));
    let broker_handle = start_broker(broker_config).await?;

    let config = SandboxConfig {
        permissions,
        work_dir: workspace.to_path_buf(),
        data_dir: None,
        timeout: Duration::from_secs(timeout_secs.max(1)),
        max_memory_mb: Some(sandbox_cfg.max_memory_mb),
        env_vars: collect_env_vars(&sandbox_cfg.env_inherit),
        broker_socket: Some(broker_handle.socket_path.clone()),
    };

    Sandbox::new(config)
}

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "execute_command".into(),
            description: "Execute a shell command in a Corral sandbox scoped to the workspace directory. Returns stdout and stderr. Optional broker-backed reminders service can be enabled with explicit permission.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (passed to sh -c)"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30)"
                    },
                    "enable_reminders_service": {
                        "type": "boolean",
                        "description": "Enable broker-backed reminders service for this execution (default: false)"
                    },
                    "reminders_lists": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional allowed reminder lists when reminders service is enabled"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        use super::audit::ToolAuditEntry;
        use super::policy::HardBaseline;
        use std::time::Instant;

        let command = input["command"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'command' field"))?;
        let timeout_secs = input["timeout_seconds"]
            .as_u64()
            .unwrap_or(self.default_timeout);
        let enable_reminders_service = input["enable_reminders_service"].as_bool().unwrap_or(false);
        let reminders_lists = input["reminders_lists"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let source_info = ctx
            .source_channel_type()
            .zip(ctx.source_connector_id())
            .zip(ctx.source_conversation_scope())
            .map(|((channel_type, connector_id), conversation_scope)| {
                (channel_type, connector_id, conversation_scope)
            });

        match &self.exec_security.security {
            ExecSecurityMode::Deny => {
                return Ok(ToolOutput {
                    content: "Command denied: exec is disabled for this agent".to_string(),
                    is_error: true,
                });
            }
            ExecSecurityMode::Allowlist => {
                let runtime_allowed = match self.approval_registry.as_ref() {
                    Some(registry) => registry.is_runtime_allowed(&self.agent_id, command).await,
                    None => false,
                };
                let is_allowed = self.is_command_allowed(command) || runtime_allowed;
                if !is_allowed {
                    match self.exec_security.ask {
                        ExecAskMode::Off => {
                            return Ok(ToolOutput {
                                content: format!(
                                    "Command not in allowlist. To run this command, add a matching pattern to exec_security.allowlist in agent config. Command: {command}"
                                ),
                                is_error: true,
                            });
                        }
                        ExecAskMode::OnMiss | ExecAskMode::Always => {
                            if let Some(reason) =
                                self.wait_for_approval(command, source_info).await?
                            {
                                return Ok(ToolOutput {
                                    content: if reason.contains("no approval UI available") {
                                        format!("{reason}: {command}")
                                    } else {
                                        reason
                                    },
                                    is_error: true,
                                });
                            }
                        }
                    }
                } else if self.exec_security.ask == ExecAskMode::Always {
                    if let Some(reason) = self.wait_for_approval(command, source_info).await? {
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    }
                }
            }
            ExecSecurityMode::Full => {
                if self.exec_security.ask == ExecAskMode::Always {
                    if let Some(reason) = self.wait_for_approval(command, source_info).await? {
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    }
                }
            }
        }

        // Network approval flow (ask mode)
        if self.sandbox_config.network == SandboxNetworkMode::Ask {
            let targets = extract_network_targets(command);
            let pkg_domains = package_manager_domains(command);

            for (host, port) in &targets {
                let is_whitelisted = self
                    .sandbox_config
                    .network_allow
                    .iter()
                    .any(|pattern| domain_matches(pattern, host));

                // Hard baseline: block private/loopback/metadata targets (SSRF protection)
                // This is non-negotiable — cannot be bypassed via approval
                if !is_whitelisted && HardBaseline::network_denied(host, *port) {
                    tracing::warn!(
                        target: "clawhive::audit::network",
                        agent_id = %self.agent_id,
                        tool = "execute_command",
                        host = %host,
                        port = %port,
                        command = %command,
                        "network access denied by hard baseline (SSRF protection)"
                    );
                    return Ok(ToolOutput {
                        content: format!(
                            "Network access denied: {}:{} is a private/loopback address blocked by hard baseline. ", host, port
                        ),
                        is_error: true,
                    });
                }
                let is_pkg_manager = pkg_domains.iter().any(|d| domain_matches(d, host));
                let is_runtime_allowed = match self.approval_registry.as_ref() {
                    Some(reg) => reg.is_network_allowed(&self.agent_id, host, *port).await,
                    None => false,
                };

                if !is_whitelisted && !is_pkg_manager && !is_runtime_allowed {
                    if let Some(reason) = self
                        .wait_for_network_approval(command, host, *port, source_info)
                        .await?
                    {
                        tracing::warn!(
                            target: "clawhive::audit::network",
                            agent_id = %self.agent_id,
                            tool = "execute_command",
                            host = %host,
                            port = %port,
                            command = %command,
                            "network access denied"
                        );
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    }

                    tracing::info!(
                        target: "clawhive::audit::network",
                        agent_id = %self.agent_id,
                        tool = "execute_command",
                        host = %host,
                        port = %port,
                        command = %command,
                        "network access granted"
                    );
                }
            }
        }

        // Hard baseline check - applies to ALL tool origins
        if HardBaseline::exec_denied(command) {
            let entry = ToolAuditEntry::denied(
                "execute_command",
                ctx.origin(),
                &input,
                "command blocked by hard baseline",
            )
            .with_module(module_path!());
            entry.emit();
            return Ok(ToolOutput {
                content: "Command denied: matches dangerous pattern (hard baseline)".to_string(),
                is_error: true,
            });
        }

        // Policy context check (external skills need exec permission)
        if !ctx.check_exec(command) {
            let entry = ToolAuditEntry::denied(
                "execute_command",
                ctx.origin(),
                &input,
                "command not in allowed exec list",
            )
            .with_module(module_path!());
            entry.emit();
            return Ok(ToolOutput {
                content: "Command denied: not in allowed exec list for this skill".to_string(),
                is_error: true,
            });
        }

        let timeout = Duration::from_secs(timeout_secs.max(1));
        let start = Instant::now();

        // Log command execution details
        let command_preview = if command.len() > 200 {
            format!("{}...", &command[..command.floor_char_boundary(200)])
        } else {
            command.to_string()
        };
        tracing::info!(
            command = %command_preview,
            timeout_secs = timeout_secs,
            enable_reminders_service = enable_reminders_service,
            agent_id = %self.agent_id,
            "executing command in sandbox"
        );

        // Build sandbox dynamically to include current allowlist
        let extra_dirs = self.gate.allowed_dirs().await;
        let result = if enable_reminders_service {
            let sandbox = sandbox_with_broker(
                &self.workspace,
                timeout_secs,
                &reminders_lists,
                &extra_dirs,
                &self.sandbox_config,
            )
            .await?;
            sandbox.execute_with_timeout(command, timeout).await
        } else {
            let sandbox = make_sandbox(&self.workspace, &extra_dirs, &self.sandbox_config)?;
            sandbox.execute_with_timeout(command, timeout).await
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(output) => {
                let mut combined = String::new();

                if !output.stdout.is_empty() {
                    combined.push_str(&output.stdout);
                }
                if !output.stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[stderr]\n");
                    combined.push_str(&output.stderr);
                }

                if combined.len() > MAX_OUTPUT_BYTES {
                    combined.truncate(MAX_OUTPUT_BYTES);
                    combined.push_str("\n...(output truncated)");
                }

                let exit_code = output.exit_code;
                let mut is_error = !output.exit_code.eq(&0);

                tracing::debug!(
                    exit_code = exit_code,
                    duration_ms = duration_ms,
                    stdout_bytes = output.stdout.len(),
                    stderr_bytes = output.stderr.len(),
                    was_killed = output.was_killed,
                    "command execution completed"
                );

                if output.was_killed {
                    is_error = true;
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[killed: timeout exceeded]");
                }

                if exit_code != 0 {
                    combined.push_str(&format!("\n[exit code: {exit_code}]"));
                }

                let content = if combined.is_empty() {
                    format!("[exit code: {exit_code}]")
                } else {
                    combined
                };

                // Audit log successful execution
                let entry = ToolAuditEntry::success(
                    "execute_command",
                    ctx.origin(),
                    &input,
                    &content,
                    duration_ms,
                )
                .with_module(module_path!());
                entry.emit();

                Ok(ToolOutput { content, is_error })
            }
            Err(e) => {
                // Audit log failed execution
                let entry = ToolAuditEntry::error(
                    "execute_command",
                    ctx.origin(),
                    &input,
                    e.to_string(),
                    duration_ms,
                )
                .with_module(module_path!());
                entry.emit();

                Ok(ToolOutput {
                    content: format!("Failed to execute command: {e}"),
                    is_error: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalRegistry;
    use crate::config::{ExecAskMode, ExecSecurityConfig, ExecSecurityMode, SandboxPolicyConfig};
    use clawhive_schema::ApprovalDecision;
    use tempfile::TempDir;

    fn make_gate(workspace: &Path) -> Arc<AccessGate> {
        Arc::new(AccessGate::in_memory(workspace.to_path_buf()))
    }

    fn make_tool(tmp: &TempDir) -> ExecuteCommandTool {
        let gate = make_gate(tmp.path());
        ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig::default(),
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        )
    }

    fn make_full_mode_tool(tmp: &TempDir, timeout: u64) -> ExecuteCommandTool {
        let gate = make_gate(tmp.path());
        ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            timeout,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        )
    }

    #[tokio::test]
    async fn echo_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn failing_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_full_mode_tool(&tmp, 10);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "exit 1"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("exit code: 1"));
    }

    #[tokio::test]
    async fn timeout_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_full_mode_tool(&tmp, 1);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "sleep 10", "timeout_seconds": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("killed") || result.content.contains("Timeout"));
    }

    #[tokio::test]
    async fn runs_in_workspace_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("marker.txt"), "found").unwrap();
        let tool = make_tool(&tmp);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "cat marker.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("found"));
    }

    #[tokio::test]
    async fn external_context_requires_exec_permission() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("data.txt"), "hello").unwrap();

        let tool = make_tool(&tmp);

        // External context with cat allowed
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions {
                read: vec![format!("{}/**", tmp.path().display())],
                write: vec![],
            },
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec!["cat".into()],
            env: vec![],
            services: Default::default(),
        };
        let ctx = ToolContext::external(perms);

        let result = tool
            .execute(serde_json::json!({"command": "cat data.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn external_context_denies_unlisted_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // External context with only echo allowed
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions::default(),
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec!["echo".into()],
            env: vec![],
            services: Default::default(),
        };
        let ctx = ToolContext::external(perms);

        // Try to run ls (not in exec list)
        let result = tool
            .execute(serde_json::json!({"command": "ls"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn hard_baseline_blocks_dangerous_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_full_mode_tool(&tmp, 10);

        // Even builtin context should block dangerous commands
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "rm -rf /"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn denies_network_by_default_on_linux() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "curl -sS https://example.com", "timeout_seconds": 5}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn exec_security_deny_blocks_all_commands() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Deny,
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo denied"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("exec is disabled"));
    }

    #[tokio::test]
    async fn exec_security_allowlist_blocks_unlisted_commands() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec!["git *".into()],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "python --version"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("not in allowlist"));
    }

    #[tokio::test]
    async fn exec_security_full_allows_non_baseline_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                allowlist: vec![],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo allowed"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("allowed"));
    }

    #[test]
    fn is_command_allowed_matches_allowlist_patterns() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec!["git *".into(), "pwd".into()],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );

        assert!(tool.is_command_allowed("git status"));
        assert!(tool.is_command_allowed("git"));
        assert!(tool.is_command_allowed("pwd"));
        assert!(!tool.is_command_allowed("ls -la"));
    }

    #[test]
    fn is_command_allowed_accepts_safe_bins() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec![],
                safe_bins: vec!["jq".into()],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );

        assert!(tool.is_command_allowed("jq --version"));
        assert!(tool.is_command_allowed("/usr/bin/jq .foo data.json"));
        assert!(!tool.is_command_allowed("cat data.json"));
    }

    #[test]
    fn collect_env_vars_uses_configured_keys_only() {
        let key = "CLAWHIVE_EXEC_TEST_ENV";
        unsafe { std::env::set_var(key, "ok") };

        let env = collect_env_vars(&[key.to_string()]);

        assert_eq!(env.get(key), Some(&"ok".to_string()));
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn augment_path_like_host_preserves_existing_order_and_dedups() {
        let merged = augment_path_like_host(
            "/usr/bin:/bin:/opt/homebrew/bin",
            &["/opt/homebrew/bin".into(), "/usr/local/bin".into()],
        );
        assert_eq!(
            merged,
            "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin".to_string()
        );
    }

    #[test]
    fn augment_path_like_host_adds_missing_candidates() {
        let merged = augment_path_like_host(
            "/usr/bin:/bin",
            &["/opt/homebrew/bin".into(), "/usr/local/bin".into()],
        );
        assert!(merged.contains("/opt/homebrew/bin"));
        assert!(merged.contains("/usr/local/bin"));
    }

    #[test]
    fn base_permissions_apply_exec_network_and_env_config() {
        let tmp = TempDir::new().unwrap();
        let perms = base_permissions(
            tmp.path(),
            &[],
            &["sh".into(), "jq".into()],
            true,
            &["PATH".into(), "HOME".into()],
        );

        assert_eq!(perms.exec, vec!["sh".to_string(), "jq".to_string()]);
        assert_eq!(perms.network.allow, vec!["*:*".to_string()]);
        assert_eq!(perms.env, vec!["PATH".to_string(), "HOME".to_string()]);
    }

    #[test]
    fn extract_network_targets_finds_urls() {
        let targets = extract_network_targets("git clone https://github.com/user/repo.git");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "github.com");
        assert_eq!(targets[0].1, 443);
    }

    #[test]
    fn extract_network_targets_finds_http_urls() {
        let targets = extract_network_targets("curl http://example.com:8080/api");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "example.com");
        assert_eq!(targets[0].1, 8080);
    }

    #[test]
    fn extract_network_targets_no_urls() {
        let targets = extract_network_targets("ls -la /tmp");
        assert!(targets.is_empty());
    }

    #[test]
    fn package_manager_domains_npm() {
        let domains = package_manager_domains("npm install express");
        assert!(domains.iter().any(|d| d.contains("npmjs.org")));
    }

    #[test]
    fn package_manager_domains_pip() {
        let domains = package_manager_domains("pip install requests");
        assert!(domains.iter().any(|d| d.contains("pypi.org")));
    }

    #[test]
    fn package_manager_domains_unknown() {
        let domains = package_manager_domains("echo hello");
        assert!(domains.is_empty());
    }

    #[test]
    fn domain_matches_exact() {
        assert!(domain_matches("github.com", "github.com"));
        assert!(!domain_matches("github.com", "api.github.com"));
    }

    #[test]
    fn domain_matches_wildcard() {
        assert!(domain_matches("*.github.com", "api.github.com"));
        assert!(domain_matches(
            "*.github.com",
            "raw.githubusercontent.github.com"
        ));
        assert!(!domain_matches("*.github.com", "github.com"));
    }

    #[tokio::test]
    async fn allowlist_onmiss_waits_for_allow_once_and_executes() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();

        let tool_task = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf approved"}), &ctx)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(approval_registry.has_pending().await);

        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::AllowOnce)
            .await
            .unwrap();

        let output = tool_task.await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("approved"));
    }

    #[tokio::test]
    async fn allowlist_onmiss_deny_blocks_execution() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();

        let tool_task = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf denied"}), &ctx)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::Deny)
            .await
            .unwrap();

        let output = tool_task.await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("denied"));
    }

    #[tokio::test]
    async fn always_allow_persists_for_same_agent_via_registry() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());

        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate.clone(),
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();

        let first = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf persist"}), &ctx)
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::AlwaysAllow)
            .await
            .unwrap();
        let first_output = first.await.unwrap();
        assert!(!first_output.is_error);

        let tool_again = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx2 = ToolContext::builtin();
        let second = tokio::spawn(async move {
            tool_again
                .execute(serde_json::json!({"command": "printf persist"}), &ctx2)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !approval_registry.has_pending().await,
            "second execution should not require approval"
        );

        let second_output = second.await.unwrap();
        assert!(!second_output.is_error);
        assert!(second_output.content.contains("persist"));
    }

    #[tokio::test]
    async fn allowlist_onmiss_without_registry_denies() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "printf denied"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("no approval UI available"));
    }

    #[tokio::test]
    async fn hard_baseline_blocks_localhost_in_network_ask_mode() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let sandbox = SandboxPolicyConfig {
            network: SandboxNetworkMode::Ask,
            ..Default::default()
        };
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                ask: ExecAskMode::Off,
                allowlist: vec![],
                safe_bins: vec![],
            },
            sandbox,
            None,
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "curl -sS http://localhost:8001/health"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(
            result.is_error,
            "localhost should be blocked by hard baseline"
        );
        assert!(
            result.content.contains("hard baseline") || result.content.contains("denied"),
            "error should mention hard baseline, got: {}",
            result.content
        );
    }
}
