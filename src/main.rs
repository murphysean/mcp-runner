use rmcp::{
    model::*,
    prompt_handler, schemars,
    service::{NotificationContext, RequestContext},
    tool_handler, ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
};
use serde_json;
use std::collections::HashMap;
use std::future::Future;
use std::process::Child;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::Mutex;

mod http;
mod prompts;
mod resources;
mod tools;
pub mod util;

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ElicitedInput {
    /// The input text
    pub input: String,
}
rmcp::elicit_safe!(ElicitedInput);

pub enum ProcessHandle {
    Pipe(Child),
    Pty {
        child: tokio::process::Child,
        pty_writer: Arc<tokio::sync::Mutex<pty_process::OwnedWritePty>>,
    },
}

pub struct Session {
    pub process: Option<ProcessHandle>,
    pub is_pty: bool,
    pub label: Option<String>,
    pub command: String,
    pub stdout_path: String,
    pub stderr_path: Option<String>,
    pub stdout_pos: u64,
    pub stderr_pos: u64,
    pub exit_code: Option<i32>,
    pub stream_log: bool,
}

pub type Sessions = Arc<Mutex<HashMap<String, Session>>>;

#[derive(Clone)]
pub struct Runner {
    pub sessions: Sessions,
    pub next_id: Arc<AtomicUsize>,
    pub peer: Arc<tokio::sync::OnceCell<Peer<RoleServer>>>,
    pub log_level: Arc<std::sync::Mutex<LoggingLevel>>,
    pub http_port: Arc<std::sync::atomic::AtomicU16>,
}

pub fn level_value(level: LoggingLevel) -> u8 {
    match level {
        LoggingLevel::Debug => 0,
        LoggingLevel::Info => 1,
        LoggingLevel::Notice => 2,
        LoggingLevel::Warning => 3,
        LoggingLevel::Error => 4,
        LoggingLevel::Critical => 5,
        LoggingLevel::Alert => 6,
        LoggingLevel::Emergency => 7,
    }
}

impl Runner {
    fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicUsize::new(1)),
            peer: Arc::new(tokio::sync::OnceCell::new()),
            log_level: Arc::new(std::sync::Mutex::new(LoggingLevel::Debug)),
            http_port: Arc::new(std::sync::atomic::AtomicU16::new(0)),
        }
    }

    pub fn http_base_url(&self) -> String {
        let port = self.http_port.load(std::sync::atomic::Ordering::Relaxed);
        format!("http://localhost:{}", port)
    }

    pub fn notify_resource_updated(&self, uri: String) {
        if let Some(peer) = self.peer.get() {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.notify_resource_updated(ResourceUpdatedNotificationParam { uri })
                    .await
                    .ok();
            });
        }
    }

    pub fn notify_resource_list_changed(&self) {
        if let Some(peer) = self.peer.get() {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.notify_resource_list_changed().await.ok();
            });
        }
    }

    pub fn notify_log(&self, level: LoggingLevel, logger: &str, data: &str) {
        let min_level = *self.log_level.lock().unwrap();
        if level_value(level) < level_value(min_level) {
            return;
        }
        if let Some(peer) = self.peer.get() {
            let peer = peer.clone();
            let param = LoggingMessageNotificationParam {
                level,
                logger: Some(logger.to_string()),
                data: serde_json::Value::String(data.to_string()),
            };
            tokio::spawn(async move {
                peer.notify_logging_message(param).await.ok();
            });
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StartCommandArgs {
    /// Command to execute (e.g., "python3", "gdb", "picocom")
    pub command: String,
    /// Command arguments (e.g., ["-m", "http.server", "8000"] or ["program.elf"])
    pub args: Option<Vec<String>>,
    /// Optional human-readable label for this session (e.g., "dev-server", "build", "debugger"). Makes it easier to identify sessions in list_sessions.
    pub label: Option<String>,
    /// Working directory for the command. If not specified, inherits the MCP server's working directory.
    pub working_dir: Option<String>,
    /// Environment variables to set for the command (key-value pairs). These are added to the inherited environment. Use to set PATH, secrets, or tool-specific config.
    pub env: Option<HashMap<String, String>>,
    /// Capture stderr separately from stdout. Set true to use read_stderr tool. Default: false (stderr merged into stdout).
    pub split_stderr: Option<bool>,
    /// Run in a pseudo-terminal (PTY). ONLY use for programs that NEED terminal features (picocom, gdb TUI, serial consoles). For simple commands, leave false for cleaner output. PTY output has ANSI cursor codes that look messy when stripped.
    pub use_pty: Option<bool>,
    /// Stream process output to the client log in real-time via MCP logging notifications. Stdout lines are sent at Info level, stderr at Warning. Default: false.
    pub stream_log: Option<bool>,
    /// Hard timeout in seconds. After this time, the process receives SIGTERM, then SIGKILL after 5s if still alive. Use for bounded tasks like builds or tests. Default: no timeout (runs until stopped or exits).
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionIdArgs {
    /// Session ID
    pub session_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadOutputArgs {
    /// Session ID returned by start_command
    pub session_id: String,
    /// Strip ANSI escape codes (colors, cursor movement, etc.). Default: true. Set false to keep raw codes.
    #[serde(default = "default_true")]
    pub strip_ansi: bool,
    /// Wait up to this many ms for new output before returning. Returns early if output stops arriving (idle for 300ms). Use this to wait for a build/command to produce output instead of polling. Good values: 5000-30000ms for builds, 1000-5000ms for interactive tools.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Wait until this pattern appears in the output (case-sensitive substring). Returns immediately when the pattern is found, or after timeout_ms if set (default: waits up to 30s). Useful for waiting until "BUILD SUCCESS", "error:", "listening on port", etc.
    #[serde(default)]
    pub wait_for: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendInputArgs {
    /// Session ID
    pub session_id: String,
    /// Text to send. Enter/newline is AUTOMATICALLY appended (correct line ending for pipe vs PTY). Just send the command text: "ls", "print('hello')", "continue". Trailing whitespace is trimmed before appending Enter. Set no_enter: true to send text exactly as-is without Enter.
    #[serde(default)]
    pub input: Option<String>,
    /// Raw bytes to send (0-255). No automatic Enter is appended. Use for control characters: [1,24]=Ctrl-A Ctrl-X, [3]=Ctrl-C, [9]=Tab. Only use this for non-text input; prefer 'input' for readable commands.
    #[serde(default)]
    pub bytes: Option<Vec<u8>>,
    /// If true, suppress the automatic Enter/newline after input text. Use for partial input, tab completion, or interactive character-by-character entry.
    #[serde(default)]
    pub no_enter: Option<bool>,
    /// If true, prompt the user directly via MCP elicitation (for passwords/secrets - input never touches the LLM). Enter is auto-appended.
    #[serde(default)]
    pub elicit: Option<bool>,
    /// Custom prompt message for elicitation. Defaults to "Enter input for process".
    #[serde(default)]
    pub elicit_message: Option<String>,
    /// Set true to wait for output after sending input. Returns when output stops arriving (1s idle). Use when you want the response but don't know the exact prompt pattern. Max 30s.
    #[serde(default)]
    pub wait: Option<bool>,
    /// Wait until this pattern appears in stdout after sending input (e.g., "$", ">>>", "(gdb)"). Returns immediately when the pattern is found. If nothing arrives for 5s, returns what it has. Max 30s. This is the RECOMMENDED approach for interactive sessions where you know the shell/REPL prompt.
    #[serde(default)]
    pub wait_for: Option<String>,
    /// Advanced: custom idle timeout in ms. Overrides the default 1s idle when using wait:true. Most agents should just use wait:true or wait_for instead.
    #[serde(default)]
    pub await_response_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunCommandArgs {
    /// Command to execute (e.g., "cargo", "make", "python3")
    pub command: String,
    /// Command arguments (e.g., ["build", "--release"] or ["test"])
    pub args: Option<Vec<String>>,
    /// Working directory for the command. If not specified, inherits the MCP server's working directory.
    pub working_dir: Option<String>,
    /// Environment variables to set for the command (key-value pairs). These are added to the inherited environment.
    pub env: Option<HashMap<String, String>>,
    /// Hard timeout in seconds. Process is killed (SIGTERM then SIGKILL) if it exceeds this. Default: 300 (5 minutes).
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendSignalArgs {
    /// Session ID returned by start_command
    pub session_id: String,
    /// Signal to send: SIGINT (Ctrl-C), SIGTERM (graceful stop), SIGKILL (force kill), SIGSTOP (pause), SIGCONT (resume), SIGHUP (reload), SIGQUIT (quit with core dump)
    pub signal: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchOutputArgs {
    /// Session ID returned by start_command
    pub session_id: String,
    /// Text pattern to search for (case-sensitive substring match). Use simple strings like "ERROR", "FAIL", "listening on".
    pub pattern: String,
    /// Search stderr instead of stdout. Only works if split_stderr was true. Default: false.
    #[serde(default)]
    pub stderr: bool,
    /// Maximum number of matching lines to return. Default: 20.
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[prompt_handler]
#[tool_handler]
impl ServerHandler for Runner {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .enable_logging()
                .build(),
        );
        info.server_info = Implementation::new("mcp-runner", env!("CARGO_PKG_VERSION"));
        info.instructions = Some("MCP Runner manages long-running processes (builds, servers, debuggers, REPLs). Start commands with start_command, read their output with read_output (supports wait_for patterns and timeout_ms for intelligent waiting), search output with search_output, and manage sessions with list_sessions. Use prompts for workflow guides.".into());
        info
    }

    fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        *self.log_level.lock().expect("log_level poisoned") = request.level;
        std::future::ready(Ok(()))
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.peer.set(context.peer).ok();
        let base_url = self.http_base_url();
        let peer = self.peer.get().cloned();
        async move {
            // Give HTTP server a moment to bind
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Some(peer) = peer {
                peer.notify_logging_message(LoggingMessageNotificationParam {
                    level: LoggingLevel::Info,
                    logger: Some("mcp-runner".to_string()),
                    data: serde_json::Value::String(format!(
                        "HTTP monitoring UI available at {}",
                        base_url
                    )),
                })
                .await
                .ok();
            }
        }
    }

    fn subscribe(
        &self,
        _request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        std::future::ready(Ok(()))
    }

    fn unsubscribe(
        &self,
        _request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        std::future::ready(Ok(()))
    }

    fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        self.list_resources(request, context)
    }

    fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_ {
        self.list_resource_templates(request, context)
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        self.read_resource(request, context)
    }
}

use clap::Parser;

#[derive(Parser)]
#[command(name = "mcp-runner", about = "MCP server for running and managing long-lived processes")]
struct Cli {
    /// Transport mode: "stdio" (default) or "http" (streamable HTTP on --port)
    #[arg(long, default_value = "stdio")]
    transport: String,

    /// Port for the HTTP monitoring interface. Default: 0 (auto-assign a free port). The assigned port is reported in tool responses so agents can direct users to it.
    #[arg(long, default_value = "0")]
    port: u16,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let wrapper = Runner::new();
    let sessions_http = wrapper.sessions.clone();
    let sessions_cleanup = wrapper.sessions.clone();
    let http_port = wrapper.http_port.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        util::cleanup_all_sessions(&sessions_cleanup).await;
        std::process::exit(0);
    });

    tokio::spawn(http::serve(sessions_http, cli.port, http_port));

    match cli.transport.as_str() {
        "stdio" => {
            let transport = rmcp::transport::io::stdio();
            let server = wrapper.serve(transport).await.unwrap();
            server.waiting().await.unwrap();
        }
        "http" => {
            use rmcp::transport::streamable_http_server::{
                StreamableHttpServerConfig, StreamableHttpService,
                session::local::LocalSessionManager,
            };
            use tokio_util::sync::CancellationToken;

            let ct = CancellationToken::new();
            let config = StreamableHttpServerConfig::default()
                .with_stateful_mode(true)
                .with_cancellation_token(ct.clone());
            let service: StreamableHttpService<Runner, LocalSessionManager> =
                StreamableHttpService::new(move || Ok(Runner::new()), Default::default(), config);
            let router = axum::Router::new().nest_service("/mcp", service);

            let mcp_port = cli.port + 1;
            let addr = std::net::SocketAddr::from(([0, 0, 0, 0], mcp_port));
            eprintln!("MCP streamable HTTP transport on http://0.0.0.0:{}/mcp", mcp_port);
            let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct.cancelled().await })
                .await
                .unwrap();
        }
        other => {
            eprintln!("Unknown transport: {}. Use 'stdio' or 'http'.", other);
            std::process::exit(1);
        }
    }
}
