use rmcp::{
    handler::server::router::prompt::PromptRouter,
    handler::server::router::tool::ToolRouter,
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
use std::sync::{Arc, Mutex};

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
    pub log_level: Arc<Mutex<LoggingLevel>>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
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
            log_level: Arc::new(Mutex::new(LoggingLevel::Debug)),
            tool_router: tools::router(),
            prompt_router: prompts::router(),
        }
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
    /// Capture stderr separately from stdout. Set true to use read_stderr tool. Default: false (stderr merged into stdout).
    pub split_stderr: Option<bool>,
    /// Run in a pseudo-terminal (PTY). ONLY use for programs that NEED terminal features (picocom, gdb TUI, serial consoles). For simple commands, leave false for cleaner output. PTY output has ANSI cursor codes that look messy when stripped.
    pub use_pty: Option<bool>,
    /// Stream process output to the client log in real-time via MCP logging notifications. Stdout lines are sent at Info level, stderr at Warning. Default: false.
    pub stream_log: Option<bool>,
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
    /// RECOMMENDED: Wait this many ms after sending input, then return any output received. Use this to send AND get response in one call instead of separate send_input + read_output. Good values: 300-1000ms for REPLs.
    #[serde(default)]
    pub await_response_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendSignalArgs {
    /// Session ID returned by start_command
    pub session_id: String,
    /// Signal to send: SIGINT (Ctrl-C), SIGTERM (graceful stop), SIGKILL (force kill), SIGSTOP (pause), SIGCONT (resume), SIGHUP (reload), SIGQUIT (quit with core dump)
    pub signal: String,
}

#[prompt_handler]
#[tool_handler]
impl ServerHandler for Runner {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .enable_resources()
                .enable_resources_subscribe()
                .enable_resources_list_changed()
                .enable_logging()
                .build(),
        )
    }

    fn set_level(
        &self,
        request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<(), McpError>> + Send + '_ {
        *self.log_level.lock().unwrap() = request.level;
        std::future::ready(Ok(()))
    }

    fn on_initialized(
        &self,
        context: NotificationContext<RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.peer.set(context.peer).ok();
        std::future::ready(())
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

#[tokio::main]
async fn main() {
    let wrapper = Runner::new();
    let sessions_http = wrapper.sessions.clone();
    let sessions_cleanup = wrapper.sessions.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        util::cleanup_all_sessions(&sessions_cleanup);
        std::process::exit(0);
    });

    tokio::spawn(http::serve(sessions_http));

    let transport = rmcp::transport::io::stdio();
    let server = wrapper.serve(transport).await.unwrap();
    server.waiting().await.unwrap();
}
