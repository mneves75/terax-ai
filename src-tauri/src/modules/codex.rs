use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Channel;

use crate::modules::workspace::{
    authorize_spawn_cwd, resolve_path, WorkspaceEnv, WorkspaceRegistry,
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use tungstenite::{client, Message, WebSocket};

const MIN_CODEX_CLI_VERSION: &str = "0.134.0";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexStatus {
    supported: bool,
    cli_available: bool,
    cli_version_supported: bool,
    cli_path: String,
    cli_version: Option<String>,
    min_cli_version: &'static str,
    daemon_running: bool,
    error: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CodexAppServerEvent {
    AgentDelta {
        delta: String,
    },
    PlanUpdated {
        text: String,
    },
    ItemStarted {
        item: Value,
    },
    ItemCompleted {
        item: Value,
    },
    ApprovalRequest {
        request_id: u64,
        method: String,
        params: Value,
    },
    TurnCompleted {
        turn: Value,
    },
    Error {
        message: String,
    },
    Status {
        message: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexStartTurnParams {
    session_id: String,
    thread_id: Option<String>,
    cwd: Option<String>,
    workspace_roots: Vec<String>,
    workspace: Option<WorkspaceEnv>,
    prompt: String,
    cli_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexApprovalParams {
    session_id: String,
    request_id: u64,
    approved: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexInterruptParams {
    session_id: String,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CodexRunResult {
    Completed {
        thread_id: String,
    },
    AwaitingApproval {
        thread_id: String,
        turn_id: Option<String>,
        request_id: u64,
    },
}

#[cfg(unix)]
type CodexWs = WebSocket<UnixStream>;

#[cfg(unix)]
enum CodexConnection {
    Ws(CodexWs),
    Stdio {
        child: Child,
        stdin: std::process::ChildStdin,
        stdout: BufReader<std::process::ChildStdout>,
    },
}

#[cfg(unix)]
struct PendingRequest {
    id: u64,
    method: String,
    params: Value,
}

#[cfg(unix)]
struct CodexClient {
    conn: CodexConnection,
    next_id: u64,
    thread_id: Option<String>,
    active_turn_id: Option<String>,
    pending: Option<PendingRequest>,
    cli_path: String,
    scope: Option<CodexScope>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexScope {
    cwd: Option<String>,
    workspace_roots: Vec<String>,
}

#[cfg(unix)]
#[derive(Clone)]
struct ActiveCodexTurn {
    cli_path: String,
    thread_id: String,
    turn_id: String,
}

#[cfg(unix)]
static CLIENTS: OnceLock<Mutex<HashMap<String, Arc<Mutex<CodexClient>>>>> = OnceLock::new();
#[cfg(unix)]
static ACTIVE_TURNS: OnceLock<Mutex<HashMap<String, ActiveCodexTurn>>> = OnceLock::new();
static FALLBACK_ID: AtomicU64 = AtomicU64::new(1);

#[tauri::command]
pub fn codex_app_server_status(cli_path: Option<String>) -> CodexStatus {
    let cli = codex_cli(cli_path);
    if !cfg!(unix) {
        return CodexStatus {
            supported: false,
            cli_available: false,
            cli_version_supported: false,
            cli_path: cli,
            cli_version: None,
            min_cli_version: MIN_CODEX_CLI_VERSION,
            daemon_running: false,
            error: Some(
                "Codex CLI provider is supported on macOS and Linux in this release".into(),
            ),
        };
    }
    let version = command_output(&cli, &["--version"]);
    let (cli_available, cli_version, cli_version_supported, version_error) = match version {
        Ok(v) => {
            let version_supported = codex_version_supported(&v);
            let error = if version_supported {
                None
            } else {
                Some(format!(
                    "Codex CLI version is below the required {MIN_CODEX_CLI_VERSION}"
                ))
            };
            (true, Some(v), version_supported, error)
        }
        Err(e) => (false, None, false, Some(e)),
    };
    let daemon_running = command_output(&cli, &["app-server", "daemon", "version"]).is_ok();
    CodexStatus {
        supported: true,
        cli_available,
        cli_version_supported,
        cli_path: cli,
        cli_version,
        min_cli_version: MIN_CODEX_CLI_VERSION,
        daemon_running,
        error: version_error,
    }
}

#[tauri::command]
pub fn codex_app_server_start_turn(
    params: CodexStartTurnParams,
    on_event: Channel<CodexAppServerEvent>,
    registry: tauri::State<'_, WorkspaceRegistry>,
) -> Result<CodexRunResult, String> {
    start_turn_impl(params, on_event, &registry)
}

#[tauri::command]
pub fn codex_app_server_respond_approval(
    params: CodexApprovalParams,
    on_event: Channel<CodexAppServerEvent>,
) -> Result<CodexRunResult, String> {
    respond_approval_impl(params, on_event)
}

#[tauri::command]
pub fn codex_app_server_interrupt(params: CodexInterruptParams) -> Result<(), String> {
    interrupt_impl(params)
}

fn codex_cli(cli_path: Option<String>) -> String {
    let requested = cli_path
        .and_then(|s| {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .unwrap_or_else(|| "codex".into());
    resolve_cli_path(&requested).unwrap_or(requested)
}

fn resolve_cli_path(requested: &str) -> Option<String> {
    let requested = expand_home(requested);
    if has_path_separator(&requested) {
        return is_executable_file(Path::new(&requested)).then_some(requested);
    }
    find_executable_on_paths(&requested, std::env::var_os("PATH").as_ref())
        .or_else(|| resolve_with_login_shell(&requested))
        .or_else(|| find_executable_on_paths(&requested, None))
        .map(|p| p.to_string_lossy().into_owned())
}

fn has_path_separator(value: &str) -> bool {
    value.contains('/') || value.contains('\\')
}

fn expand_home(value: &str) -> String {
    let Some(rest) = value.strip_prefix("~/") else {
        return value.to_string();
    };
    dirs::home_dir()
        .map(|home| home.join(rest).to_string_lossy().into_owned())
        .unwrap_or_else(|| value.to_string())
}

fn find_executable_on_paths(name: &str, env_path: Option<&std::ffi::OsString>) -> Option<PathBuf> {
    let paths: Vec<PathBuf> = env_path
        .map(std::env::split_paths)
        .map(Iterator::collect)
        .unwrap_or_else(default_cli_search_paths);
    paths
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn default_cli_search_paths() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".local/bin"));
        paths.push(home.join(".cargo/bin"));
        paths.push(home.join("bin"));
        paths.extend(nvm_node_bin_paths(&home));
    }
    paths.push(PathBuf::from("/Applications/Codex.app/Contents/Resources"));
    paths
}

fn nvm_node_bin_paths(home: &Path) -> Vec<PathBuf> {
    let versions = home.join(".nvm/versions/node");
    let mut bins: Vec<PathBuf> = fs::read_dir(versions)
        .into_iter()
        .flat_map(Iterator::flatten)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|path| path.join("bin"))
        .collect();
    bins.sort_by_key(|path| std::cmp::Reverse(node_version_key(path)));
    bins
}

fn node_version_key(bin_path: &Path) -> (u32, u32, u32) {
    let version = bin_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .trim_start_matches('v');
    let mut parts = version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok());
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

#[cfg(unix)]
fn resolve_with_login_shell(name: &str) -> Option<PathBuf> {
    if has_path_separator(name) {
        return None;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let output = Command::new(shell)
        .args(["-lc", &format!("command -v {}", shell_quote(name))])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    if path.is_empty() || !path.starts_with('/') {
        return None;
    }
    let path = PathBuf::from(path);
    is_executable_file(&path).then_some(path)
}

#[cfg(not(unix))]
fn resolve_with_login_shell(_name: &str) -> Option<PathBuf> {
    None
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command_output(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("{program} exited with {}", out.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn codex_version_supported(output: &str) -> bool {
    let Some(version) = first_semver(output) else {
        return false;
    };
    let Some(min) = first_semver(MIN_CODEX_CLI_VERSION) else {
        return false;
    };
    version >= min
}

fn first_semver(value: &str) -> Option<(u32, u32, u32)> {
    for token in value.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-')) {
        let token = token.strip_prefix('v').unwrap_or(token);
        let core = token.split('-').next().unwrap_or(token);
        let mut parts = core.split('.');
        let Some(major) = parts.next().and_then(|p| p.parse().ok()) else {
            continue;
        };
        let Some(minor) = parts.next().and_then(|p| p.parse().ok()) else {
            continue;
        };
        let Some(patch) = parts.next().and_then(|p| p.parse().ok()) else {
            continue;
        };
        return Some((major, minor, patch));
    }
    None
}

#[cfg(not(unix))]
fn start_turn_impl(
    _params: CodexStartTurnParams,
    _on_event: Channel<CodexAppServerEvent>,
    _registry: &WorkspaceRegistry,
) -> Result<CodexRunResult, String> {
    Err("Codex CLI provider is supported on macOS and Linux in this release".into())
}

#[cfg(not(unix))]
fn respond_approval_impl(
    _params: CodexApprovalParams,
    _on_event: Channel<CodexAppServerEvent>,
) -> Result<CodexRunResult, String> {
    Err("Codex CLI provider is supported on macOS and Linux in this release".into())
}

#[cfg(not(unix))]
fn interrupt_impl(_params: CodexInterruptParams) -> Result<(), String> {
    Err("Codex CLI provider is supported on macOS and Linux in this release".into())
}

#[cfg(unix)]
fn start_turn_impl(
    params: CodexStartTurnParams,
    on_event: Channel<CodexAppServerEvent>,
    registry: &WorkspaceRegistry,
) -> Result<CodexRunResult, String> {
    if params.prompt.trim().is_empty() {
        return Err("empty Codex prompt".into());
    }
    let cli = codex_cli(params.cli_path.clone());
    let scope = sanitize_scope(&params, registry)?;
    let client = get_or_connect(&params.session_id, &cli, &scope)?;
    let mut client = client.lock().map_err(|_| "Codex client lock poisoned")?;
    if client.pending.is_some() {
        return Err("Codex is waiting for an approval response".into());
    }
    if client.thread_id.is_none() {
        let thread = client.thread_start(params.thread_id, &scope)?;
        client.thread_id = Some(thread);
    }
    let thread_id = client.thread_id.clone().ok_or("missing Codex thread id")?;
    let input = vec![json!({
        "type": "text",
        "text": params.prompt,
        "text_elements": []
    })];
    let mut body = json!({
        "threadId": thread_id,
        "input": input,
        "permissions": ":workspace",
    });
    if let Some(cwd) = scope.cwd.as_deref() {
        body["cwd"] = json!(cwd);
    }
    if !scope.workspace_roots.is_empty() {
        body["runtimeWorkspaceRoots"] = json!(&scope.workspace_roots);
    }
    let response = client.request("turn/start", body, &on_event)?;
    client.active_turn_id = response
        .get("turn")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(turn_id) = client.active_turn_id.clone() {
        remember_active_turn(&params.session_id, &cli, &thread_id, &turn_id)?;
    }
    let result = client.read_turn_events(on_event);
    if matches!(result, Ok(CodexRunResult::Completed { .. }) | Err(_)) {
        forget_active_turn(&params.session_id);
    }
    result
}

#[cfg(unix)]
fn respond_approval_impl(
    params: CodexApprovalParams,
    on_event: Channel<CodexAppServerEvent>,
) -> Result<CodexRunResult, String> {
    let clients = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let client = clients
        .lock()
        .map_err(|_| "Codex clients lock poisoned")?
        .get(&params.session_id)
        .cloned()
        .ok_or_else(|| "no active Codex session".to_string())?;
    let mut client = client.lock().map_err(|_| "Codex client lock poisoned")?;
    let pending = client
        .pending
        .take()
        .ok_or_else(|| "no pending Codex approval".to_string())?;
    if pending.id != params.request_id {
        client.pending = Some(pending);
        return Err("approval id does not match the pending Codex request".into());
    }
    let result = client.approval_result(&pending.method, &pending.params, params.approved);
    client.send_value(json!({ "id": pending.id, "result": result }))?;
    let result = client.read_turn_events(on_event);
    if matches!(result, Ok(CodexRunResult::Completed { .. }) | Err(_)) {
        forget_active_turn(&params.session_id);
    }
    result
}

#[cfg(unix)]
fn interrupt_impl(params: CodexInterruptParams) -> Result<(), String> {
    if let Some(active) = active_turn(&params.session_id)? {
        let mut control = CodexClient::connect(&active.cli_path)?;
        let _ = control.request_no_events(
            "turn/interrupt",
            json!({ "threadId": active.thread_id, "turnId": active.turn_id }),
        )?;
        return Ok(());
    }

    let clients = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let client = clients
        .lock()
        .map_err(|_| "Codex clients lock poisoned")?
        .get(&params.session_id)
        .cloned()
        .ok_or_else(|| "no active Codex session".to_string())?;
    let mut client = client.lock().map_err(|_| "Codex client lock poisoned")?;
    let Some(thread_id) = client.thread_id.clone() else {
        return Ok(());
    };
    let Some(turn_id) = client.active_turn_id.clone() else {
        return Ok(());
    };
    let _ = client.request_no_events(
        "turn/interrupt",
        json!({ "threadId": thread_id, "turnId": turn_id }),
    )?;
    Ok(())
}

#[cfg(unix)]
fn remember_active_turn(
    session_id: &str,
    cli_path: &str,
    thread_id: &str,
    turn_id: &str,
) -> Result<(), String> {
    ACTIVE_TURNS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| "Codex active turn lock poisoned")?
        .insert(
            session_id.to_string(),
            ActiveCodexTurn {
                cli_path: cli_path.to_string(),
                thread_id: thread_id.to_string(),
                turn_id: turn_id.to_string(),
            },
        );
    Ok(())
}

#[cfg(unix)]
fn active_turn(session_id: &str) -> Result<Option<ActiveCodexTurn>, String> {
    Ok(ACTIVE_TURNS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| "Codex active turn lock poisoned")?
        .get(session_id)
        .cloned())
}

#[cfg(unix)]
fn forget_active_turn(session_id: &str) {
    if let Ok(mut active) = ACTIVE_TURNS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        active.remove(session_id);
    }
}

#[cfg(unix)]
fn sanitize_scope(
    params: &CodexStartTurnParams,
    registry: &WorkspaceRegistry,
) -> Result<CodexScope, String> {
    let workspace = WorkspaceEnv::from_option(params.workspace.clone());
    let cwd = authorize_spawn_cwd(registry, params.cwd.as_deref(), &workspace)?
        .map(crate::modules::fs::to_canon);

    let mut roots = Vec::<PathBuf>::new();
    for root in &params.workspace_roots {
        let trimmed = root.trim();
        if trimmed.is_empty() {
            continue;
        }
        let resolved = resolve_path(trimmed, &workspace);
        let canonical = std::fs::canonicalize(&resolved)
            .map_err(|e| format!("workspace root not accessible: {e}"))?;
        if !canonical.is_dir() {
            return Err(format!(
                "workspace root is not a directory: {}",
                canonical.display()
            ));
        }
        if !registry.is_authorized(&canonical) {
            return Err(format!(
                "workspace root is outside the authorized workspace: {}",
                canonical.display()
            ));
        }
        roots.push(canonical);
    }

    if let Some(cwd) = cwd.as_deref() {
        let cwd_path = PathBuf::from(cwd);
        if roots.is_empty() || !roots.iter().any(|root| cwd_path.starts_with(root)) {
            roots.push(cwd_path);
        }
    }

    roots.sort();
    roots.dedup();
    if cwd.is_none() && roots.is_empty() {
        return Err("Codex requires an authorized cwd or workspace root".into());
    }

    Ok(CodexScope {
        cwd,
        workspace_roots: roots
            .into_iter()
            .map(crate::modules::fs::to_canon)
            .collect(),
    })
}

#[cfg(unix)]
fn ensure_daemon(cli: &str) -> Result<(), String> {
    if command_output(cli, &["app-server", "daemon", "version"]).is_ok() {
        return Ok(());
    }
    let status = Command::new(cli)
        .args(["app-server", "daemon", "start"])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!(
            "codex app-server daemon start exited with {status}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn get_or_connect(
    session_id: &str,
    cli: &str,
    scope: &CodexScope,
) -> Result<Arc<Mutex<CodexClient>>, String> {
    let clients = CLIENTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(client) = clients
        .lock()
        .map_err(|_| "Codex clients lock poisoned")?
        .get(session_id)
        .cloned()
    {
        let matches_scope = {
            let guard = client.lock().map_err(|_| "Codex client lock poisoned")?;
            guard.cli_path == cli && guard.scope.as_ref() == Some(scope)
        };
        if matches_scope {
            return Ok(client);
        }
        {
            let guard = client.lock().map_err(|_| "Codex client lock poisoned")?;
            if guard.pending.is_some() || guard.active_turn_id.is_some() {
                return Err(
                    "Codex session is busy; wait for the current turn before changing workspace or CLI path"
                        .into(),
                );
            }
        }
        clients
            .lock()
            .map_err(|_| "Codex clients lock poisoned")?
            .remove(session_id);
    }
    let mut connected = CodexClient::connect(cli)?;
    connected.cli_path = cli.to_string();
    connected.scope = Some(scope.clone());
    let client = Arc::new(Mutex::new(connected));
    clients
        .lock()
        .map_err(|_| "Codex clients lock poisoned")?
        .insert(session_id.to_string(), client.clone());
    Ok(client)
}

#[cfg(unix)]
fn socket_path() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".codex")))
        .ok_or_else(|| "could not resolve Codex home".to_string())?;
    Ok(home
        .join("app-server-control")
        .join("app-server-control.sock"))
}

#[cfg(unix)]
impl CodexClient {
    fn connect(cli: &str) -> Result<Self, String> {
        let mut conn = connect_connection(cli)?;
        let init_id = FALLBACK_ID.fetch_add(1, Ordering::Relaxed);
        send_json(
            &mut conn,
            json!({
                "method": "initialize",
                "id": init_id,
                "params": {
                    "clientInfo": {
                        "name": "terax",
                        "title": "Terax",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": true
                    }
                }
            }),
        )?;
        loop {
            let v = read_json(&mut conn)?;
            if v.get("id").and_then(Value::as_u64) == Some(init_id) {
                if let Some(error) = v.get("error") {
                    return Err(error_message(error));
                }
                break;
            }
        }
        send_json(&mut conn, json!({ "method": "initialized" }))?;
        let _ = cli;
        Ok(Self {
            conn,
            next_id: init_id + 1,
            thread_id: None,
            active_turn_id: None,
            pending: None,
            cli_path: cli.to_string(),
            scope: None,
        })
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send_value(&mut self, value: Value) -> Result<(), String> {
        send_json(&mut self.conn, value)
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        on_event: &Channel<CodexAppServerEvent>,
    ) -> Result<Value, String> {
        let id = self.next_request_id();
        self.send_value(json!({ "method": method, "id": id, "params": params }))?;
        loop {
            let v = read_json(&mut self.conn)?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = v.get("error") {
                    return Err(error_message(error));
                }
                return Ok(v.get("result").cloned().unwrap_or_else(|| json!({})));
            }
            self.handle_inbound(v, on_event)?;
        }
    }

    fn request_no_events(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_request_id();
        self.send_value(json!({ "method": method, "id": id, "params": params }))?;
        loop {
            let v = read_json(&mut self.conn)?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = v.get("error") {
                    return Err(error_message(error));
                }
                return Ok(v.get("result").cloned().unwrap_or_else(|| json!({})));
            }
        }
    }

    fn thread_start(
        &mut self,
        existing_thread_id: Option<String>,
        scope: &CodexScope,
    ) -> Result<String, String> {
        if let Some(id) = existing_thread_id.filter(|s| !s.trim().is_empty()) {
            let mut params = json!({
                "threadId": id,
                "permissions": ":workspace",
            });
            apply_scope_params(&mut params, scope);
            let result = self.request_no_events("thread/resume", params)?;
            return result_thread_id(&result)
                .ok_or_else(|| "Codex did not return a thread id".into());
        }
        let mut params = json!({
            "serviceName": "terax",
            "sessionStartSource": "startup",
            "permissions": ":workspace",
        });
        apply_scope_params(&mut params, scope);
        let result = self.request_no_events("thread/start", params)?;
        result_thread_id(&result).ok_or_else(|| "Codex did not return a thread id".into())
    }

    fn read_turn_events(
        &mut self,
        on_event: Channel<CodexAppServerEvent>,
    ) -> Result<CodexRunResult, String> {
        loop {
            let v = read_json(&mut self.conn)?;
            if let Some(result) = self.handle_inbound(v, &on_event)? {
                return Ok(result);
            }
        }
    }

    fn handle_inbound(
        &mut self,
        v: Value,
        on_event: &Channel<CodexAppServerEvent>,
    ) -> Result<Option<CodexRunResult>, String> {
        let Some(method) = v.get("method").and_then(Value::as_str) else {
            return Ok(None);
        };
        if let Some(id) = v.get("id").and_then(Value::as_u64) {
            let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
            let _ = on_event.send(CodexAppServerEvent::ApprovalRequest {
                request_id: id,
                method: method.to_string(),
                params: params.clone(),
            });
            self.pending = Some(PendingRequest {
                id,
                method: method.to_string(),
                params: params.clone(),
            });
            let thread_id = self.thread_id.clone().unwrap_or_default();
            return Ok(Some(CodexRunResult::AwaitingApproval {
                thread_id,
                turn_id: self.active_turn_id.clone(),
                request_id: id,
            }));
        }
        let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
        match method {
            "item/agentMessage/delta" => {
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    let _ = on_event.send(CodexAppServerEvent::AgentDelta {
                        delta: delta.to_string(),
                    });
                }
            }
            "turn/plan/updated" => {
                let _ = on_event.send(CodexAppServerEvent::PlanUpdated {
                    text: format_plan(&params),
                });
            }
            "item/started" => {
                if let Some(item) = params.get("item") {
                    if let Some(status) = format_item_status(item) {
                        let _ = on_event.send(CodexAppServerEvent::Status { message: status });
                    }
                    let _ = on_event.send(CodexAppServerEvent::ItemStarted { item: item.clone() });
                }
            }
            "item/completed" => {
                if let Some(item) = params.get("item") {
                    let _ =
                        on_event.send(CodexAppServerEvent::ItemCompleted { item: item.clone() });
                }
            }
            "turn/completed" => {
                if let Some(turn) = params.get("turn") {
                    let _ =
                        on_event.send(CodexAppServerEvent::TurnCompleted { turn: turn.clone() });
                }
                let thread_id = params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| self.thread_id.clone())
                    .unwrap_or_default();
                self.active_turn_id = None;
                return Ok(Some(CodexRunResult::Completed { thread_id }));
            }
            "error" => {
                let message = params
                    .get("error")
                    .map(error_message)
                    .unwrap_or_else(|| error_message(&params));
                let _ = on_event.send(CodexAppServerEvent::Error { message });
            }
            "mcpServer/startupStatus/updated" => {
                if let Some(message) = format_mcp_status(&params) {
                    let _ = on_event.send(CodexAppServerEvent::Status { message });
                }
            }
            "hook/started" => {
                if let Some(message) = format_hook_status(&params) {
                    let _ = on_event.send(CodexAppServerEvent::Status { message });
                }
            }
            "hook/completed" => {
                let _ = on_event.send(CodexAppServerEvent::Status {
                    message: "Codex hooks complete".into(),
                });
            }
            "warning" => {
                if let Some(message) = params.get("message").and_then(Value::as_str) {
                    let _ = on_event.send(CodexAppServerEvent::Status {
                        message: message.to_string(),
                    });
                }
            }
            "thread/started" => {
                if let Some(id) = params
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                {
                    self.thread_id = Some(id.to_string());
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn approval_result(&self, method: &str, params: &Value, approved: bool) -> Value {
        approval_result(method, params, approved, self.scope.as_ref())
    }
}

#[cfg(unix)]
impl Drop for CodexClient {
    fn drop(&mut self) {
        if let CodexConnection::Stdio { child, .. } = &mut self.conn {
            let _ = child.kill();
        }
    }
}

#[cfg(unix)]
fn connect_connection(cli: &str) -> Result<CodexConnection, String> {
    let daemon_path = socket_path()?;
    if let Ok(ws) = connect_socket(&daemon_path) {
        return Ok(CodexConnection::Ws(ws));
    }
    if ensure_daemon(cli).is_ok() {
        return connect_socket_with_retry(&daemon_path, Duration::from_secs(5))
            .map(CodexConnection::Ws);
    }
    connect_stdio(cli)
}

#[cfg(unix)]
fn connect_stdio(cli: &str) -> Result<CodexConnection, String> {
    let mut child = Command::new(cli)
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("start codex app-server: {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or("codex app-server stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("codex app-server stdout unavailable")?;
    Ok(CodexConnection::Stdio {
        child,
        stdin,
        stdout: BufReader::new(stdout),
    })
}

#[cfg(unix)]
fn connect_socket(path: &std::path::Path) -> Result<CodexWs, String> {
    let stream =
        UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    let (ws, _) = client("ws://localhost/", stream).map_err(|e| e.to_string())?;
    Ok(ws)
}

#[cfg(unix)]
fn connect_socket_with_retry(path: &std::path::Path, timeout: Duration) -> Result<CodexWs, String> {
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < timeout {
        match connect_socket(path) {
            Ok(ws) => return Ok(ws),
            Err(e) => last = e,
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(if last.is_empty() {
        format!("connect {} timed out", path.display())
    } else {
        last
    })
}

#[cfg(unix)]
fn send_json(conn: &mut CodexConnection, value: Value) -> Result<(), String> {
    match conn {
        CodexConnection::Ws(ws) => ws
            .send(Message::Text(value.to_string()))
            .map_err(|e| e.to_string()),
        CodexConnection::Stdio { stdin, .. } => {
            writeln!(stdin, "{value}").map_err(|e| e.to_string())?;
            stdin.flush().map_err(|e| e.to_string())
        }
    }
}

#[cfg(unix)]
fn read_json(conn: &mut CodexConnection) -> Result<Value, String> {
    match conn {
        CodexConnection::Ws(ws) => loop {
            match ws.read().map_err(|e| e.to_string())? {
                Message::Text(text) => {
                    return serde_json::from_str(&text).map_err(|e| e.to_string());
                }
                Message::Binary(bytes) => {
                    return serde_json::from_slice(&bytes).map_err(|e| e.to_string());
                }
                Message::Ping(bytes) => {
                    ws.send(Message::Pong(bytes)).map_err(|e| e.to_string())?;
                }
                Message::Close(_) => return Err("Codex app-server connection closed".into()),
                _ => {}
            }
        },
        CodexConnection::Stdio { stdout, .. } => {
            let mut line = String::new();
            let read = stdout.read_line(&mut line).map_err(|e| e.to_string())?;
            if read == 0 {
                return Err("Codex app-server stdio closed".into());
            }
            serde_json::from_str(line.trim_end()).map_err(|e| e.to_string())
        }
    }
}

fn result_thread_id(result: &Value) -> Option<String> {
    result
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(unix)]
fn apply_scope_params(params: &mut Value, scope: &CodexScope) {
    if let Some(cwd) = scope.cwd.as_deref() {
        params["cwd"] = json!(cwd);
    }
    if !scope.workspace_roots.is_empty() {
        params["runtimeWorkspaceRoots"] = json!(&scope.workspace_roots);
    }
}

fn approval_result(
    method: &str,
    params: &Value,
    approved: bool,
    #[cfg_attr(not(unix), allow(unused_variables))] scope: Option<&CodexScope>,
) -> Value {
    let decision = if approved { "accept" } else { "decline" };
    if method == "applyPatchApproval" || method == "execCommandApproval" {
        json!({ "decision": if approved { "approved" } else { "denied" } })
    } else if method == "item/permissions/requestApproval" {
        let permissions = if approved {
            #[cfg(unix)]
            {
                scope
                    .map(|s| scoped_permissions(params, s))
                    .unwrap_or_else(|| json!({}))
            }
            #[cfg(not(unix))]
            {
                json!({})
            }
        } else {
            json!({})
        };
        json!({
            "permissions": permissions,
            "scope": "turn"
        })
    } else {
        json!({ "decision": decision })
    }
}

#[cfg(unix)]
fn scoped_permissions(params: &Value, scope: &CodexScope) -> Value {
    let requested = params
        .get("permissions")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let Some(obj) = requested.as_object() else {
        return json!({});
    };

    let mut granted = serde_json::Map::new();
    if let Some(fs) = obj.get("fileSystem").and_then(Value::as_object) {
        let mut fs_grants = serde_json::Map::new();
        for key in ["read", "write"] {
            if let Some(paths) = fs.get(key).and_then(Value::as_array) {
                let allowed: Vec<Value> = paths
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|path| canonical_permission_path(path, scope))
                    .filter(|path| scope_allows_path(scope, path))
                    .map(|path| json!(crate::modules::fs::to_canon(path)))
                    .collect();
                if !allowed.is_empty() {
                    fs_grants.insert(key.to_string(), Value::Array(allowed));
                }
            }
        }
        if !fs_grants.is_empty() {
            granted.insert("fileSystem".to_string(), Value::Object(fs_grants));
        }
    }
    if let Some(network) = obj.get("network") {
        granted.insert("network".to_string(), network.clone());
    }
    Value::Object(granted)
}

#[cfg(unix)]
fn canonical_permission_path(path: &str, scope: &CodexScope) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = PathBuf::from(trimmed);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        PathBuf::from(scope.cwd.as_ref()?).join(candidate)
    };
    std::fs::canonicalize(candidate).ok()
}

#[cfg(unix)]
fn scope_allows_path(scope: &CodexScope, path: &Path) -> bool {
    scope
        .workspace_roots
        .iter()
        .map(PathBuf::from)
        .any(|root| path.starts_with(root))
}

fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| error.to_string())
}

fn format_mcp_status(params: &Value) -> Option<String> {
    let name = params.get("name").and_then(Value::as_str)?;
    match params.get("status").and_then(Value::as_str) {
        Some("starting") => Some(format!("Starting Codex tool: {name}")),
        Some("ready") => Some(format!("Codex tool ready: {name}")),
        Some("failed") => Some(format!("Codex tool failed: {name}")),
        _ => None,
    }
}

fn format_hook_status(params: &Value) -> Option<String> {
    let event = params
        .get("run")
        .and_then(|run| run.get("eventName"))
        .and_then(Value::as_str)?;
    Some(format!("Running Codex hook: {event}"))
}

fn format_item_status(item: &Value) -> Option<String> {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => Some("Codex is reasoning".into()),
        Some("agentMessage") => Some("Codex is answering".into()),
        Some("userMessage") => Some("Codex received the prompt".into()),
        Some("commandExecution") => Some("Codex is running a command".into()),
        Some("fileChange") => Some("Codex is preparing file changes".into()),
        _ => None,
    }
}

fn format_plan(params: &Value) -> String {
    let Some(plan) = params.get("plan").and_then(Value::as_array) else {
        return "Plan updated.".into();
    };
    let mut lines = Vec::with_capacity(plan.len() + 1);
    lines.push("Plan updated:".to_string());
    for item in plan {
        let step = item.get("step").and_then(Value::as_str).unwrap_or("step");
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        lines.push(format!("- {status}: {step}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn finds_bare_cli_on_supplied_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("codex");
        fs::write(&cli, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = fs::metadata(&cli).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&cli, perms).unwrap();
        let path = std::ffi::OsString::from(dir.path());

        assert_eq!(find_executable_on_paths("codex", Some(&path)), Some(cli));
    }

    #[test]
    fn includes_nvm_node_bins_before_app_bundle_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let v9 = dir.path().join(".nvm/versions/node/v9.99.0/bin");
        let v20 = dir.path().join(".nvm/versions/node/v20.0.0/bin");
        let v24 = dir.path().join(".nvm/versions/node/v24.15.0/bin");
        fs::create_dir_all(&v9).unwrap();
        fs::create_dir_all(&v20).unwrap();
        fs::create_dir_all(&v24).unwrap();

        let paths = nvm_node_bin_paths(dir.path());
        assert_eq!(paths, vec![v24, v20, v9]);
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("co'dex"), "'co'\\''dex'");
    }

    #[test]
    fn approval_result_uses_current_decision_names() {
        assert_eq!(
            approval_result(
                "item/commandExecution/requestApproval",
                &json!({}),
                true,
                None
            ),
            json!({ "decision": "accept" })
        );
        assert_eq!(
            approval_result("item/fileChange/requestApproval", &json!({}), false, None),
            json!({ "decision": "decline" })
        );
        assert_eq!(
            approval_result("execCommandApproval", &json!({}), true, None),
            json!({ "decision": "approved" })
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_approval_clamps_filesystem_paths_to_scope() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let allowed_file = allowed.path().join("ok.txt");
        let outside_file = outside.path().join("no.txt");
        fs::write(&allowed_file, "ok").unwrap();
        fs::write(&outside_file, "no").unwrap();
        let allowed_root = allowed.path().canonicalize().unwrap();
        let allowed_file_canonical = allowed_file.canonicalize().unwrap();
        let scope = CodexScope {
            cwd: Some(crate::modules::fs::to_canon(&allowed_root)),
            workspace_roots: vec![crate::modules::fs::to_canon(&allowed_root)],
        };
        let params = json!({
            "permissions": {
                "fileSystem": {
                    "read": [allowed_file, outside_file],
                    "write": [allowed.path(), outside.path()]
                },
                "network": { "enabled": true }
            }
        });
        assert_eq!(
            approval_result(
                "item/permissions/requestApproval",
                &params,
                true,
                Some(&scope)
            ),
            json!({
                "permissions": {
                    "fileSystem": {
                        "read": [crate::modules::fs::to_canon(&allowed_file_canonical)],
                        "write": [crate::modules::fs::to_canon(&allowed_root)]
                    },
                    "network": { "enabled": true }
                },
                "scope": "turn"
            })
        );
        assert_eq!(
            approval_result(
                "item/permissions/requestApproval",
                &params,
                false,
                Some(&scope)
            ),
            json!({ "permissions": {}, "scope": "turn" })
        );
    }

    #[test]
    fn codex_version_check_requires_minimum() {
        assert!(codex_version_supported("codex-cli 0.134.0"));
        assert!(codex_version_supported("codex-cli 1.0.0"));
        assert!(!codex_version_supported("codex-cli 0.133.9"));
        assert!(!codex_version_supported("not a version"));
    }

    #[cfg(unix)]
    #[test]
    fn sanitize_scope_rejects_unauthorized_roots_and_adds_cwd_root() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let reg = WorkspaceRegistry::default();
        reg.authorize(allowed.path()).unwrap();

        let ok = sanitize_scope(
            &CodexStartTurnParams {
                session_id: "s".into(),
                thread_id: None,
                cwd: Some(crate::modules::fs::to_canon(allowed.path())),
                workspace_roots: Vec::new(),
                workspace: None,
                prompt: "hi".into(),
                cli_path: None,
            },
            &reg,
        )
        .expect("scope");
        assert_eq!(
            ok.workspace_roots,
            vec![crate::modules::fs::to_canon(
                allowed.path().canonicalize().unwrap()
            )]
        );

        let err = sanitize_scope(
            &CodexStartTurnParams {
                session_id: "s".into(),
                thread_id: None,
                cwd: Some(crate::modules::fs::to_canon(allowed.path())),
                workspace_roots: vec![crate::modules::fs::to_canon(outside.path())],
                workspace: None,
                prompt: "hi".into(),
                cli_path: None,
            },
            &reg,
        )
        .unwrap_err();
        assert!(
            err.contains("outside the authorized workspace"),
            "got: {err}"
        );
    }

    #[test]
    fn formats_plan_steps() {
        let text = format_plan(&json!({
            "plan": [
                { "step": "Inspect", "status": "completed" },
                { "step": "Patch", "status": "inProgress" }
            ]
        }));
        assert!(text.contains("completed: Inspect"));
        assert!(text.contains("inProgress: Patch"));
    }

    #[test]
    fn formats_mcp_startup_statuses() {
        assert_eq!(
            format_mcp_status(&json!({ "name": "playwright", "status": "starting" })),
            Some("Starting Codex tool: playwright".into())
        );
        assert_eq!(
            format_mcp_status(&json!({ "name": "playwright", "status": "ready" })),
            Some("Codex tool ready: playwright".into())
        );
        assert_eq!(
            format_mcp_status(&json!({ "name": "playwright", "status": "failed" })),
            Some("Codex tool failed: playwright".into())
        );
        assert_eq!(
            format_mcp_status(&json!({ "name": "playwright", "status": "unknown" })),
            None
        );
    }

    #[test]
    fn formats_hook_status() {
        assert_eq!(
            format_hook_status(&json!({ "run": { "eventName": "UserPromptSubmit" } })),
            Some("Running Codex hook: UserPromptSubmit".into())
        );
        assert_eq!(format_hook_status(&json!({ "run": {} })), None);
    }

    #[test]
    fn formats_item_statuses() {
        assert_eq!(
            format_item_status(&json!({ "type": "userMessage" })),
            Some("Codex received the prompt".into())
        );
        assert_eq!(
            format_item_status(&json!({ "type": "reasoning" })),
            Some("Codex is reasoning".into())
        );
        assert_eq!(
            format_item_status(&json!({ "type": "agentMessage" })),
            Some("Codex is answering".into())
        );
        assert_eq!(format_item_status(&json!({ "type": "other" })), None);
    }
}
