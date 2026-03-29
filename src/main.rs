use rmcp::{
    handler::server::router::prompt::PromptRouter,
    handler::server::router::tool::ToolRouter,
    model::*,
    prompt_handler, schemars,
    service::{NotificationContext, RequestContext},
    tool_handler, ErrorData as McpError, Peer, RoleServer, ServerHandler, ServiceExt,
};
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
    pub stdout_path: String,
    pub stderr_path: Option<String>,
    pub stdout_pos: u64,
    pub stderr_pos: u64,
    pub exit_code: Option<i32>,
}

pub type Sessions = Arc<Mutex<HashMap<String, Session>>>;

#[derive(Clone)]
pub struct Runner {
    pub sessions: Sessions,
    pub next_id: Arc<AtomicUsize>,
    pub peer: Arc<tokio::sync::OnceCell<Peer<RoleServer>>>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl Runner {
    fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicUsize::new(1)),
            peer: Arc::new(tokio::sync::OnceCell::new()),
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
    /// Text input to send. For newline/Enter: write "command\\n" in your JSON (single backslash). WRONG: "command\\\\n" sends literal backslash-n. Use bytes=[...,10] if unsure.
    #[serde(default)]
    pub input: Option<String>,
    /// Raw bytes to send (0-255). Use for control characters or when newline escaping is confusing: [10]=newline, [13]=CR, [1,24]=Ctrl-A Ctrl-X. Example: "attach 1" + newline = [97,116,116,97,99,104,32,49,10]
    #[serde(default)]
    pub bytes: Option<Vec<u8>>,
    /// If true, prompt the user directly via MCP elicitation (for passwords/secrets - input never touches the LLM).
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
                .build(),
        )
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
