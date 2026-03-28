use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    model::*,
    tool_handler, prompt_handler,
    handler::server::router::tool::ToolRouter,
    handler::server::router::prompt::PromptRouter,
    schemars,
    service::{NotificationContext, RequestContext},
    Peer, RoleServer,
};
use std::collections::HashMap;
use std::future::Future;
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicUsize;

mod http;
mod tools;
mod prompts;
mod resources;
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
                peer.notify_resource_updated(ResourceUpdatedNotificationParam { uri }).await.ok();
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
    /// Command to execute
    pub command: String,
    /// Command arguments
    pub args: Option<Vec<String>>,
    /// Split stderr from stdout (default: false)
    pub split_stderr: Option<bool>,
    /// Spawn inside a pseudo-terminal (required for interactive programs like picocom, gdb TUI, etc.)
    pub use_pty: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionIdArgs {
    /// Session ID
    pub session_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendInputArgs {
    /// Session ID
    pub session_id: String,
    /// Text input to send as a UTF-8 string
    #[serde(default)]
    pub input: Option<String>,
    /// Raw bytes to send, as an array of integer values 0-255. Use this for control characters (e.g. [1, 24] for Ctrl-A Ctrl-X).
    #[serde(default)]
    pub bytes: Option<Vec<u8>>,
    /// If true, prompt the user directly for input via MCP elicitation (e.g. for passwords). The input never passes through the LLM. Provide 'elicit_message' to customize the prompt.
    #[serde(default)]
    pub elicit: Option<bool>,
    /// Custom message to show the user when eliciting input. Defaults to "Enter input for process".
    #[serde(default)]
    pub elicit_message: Option<String>,
    /// If set, block after sending input and collect output until no new data arrives for this many milliseconds, then return the output. Useful for request/response interactions (REPLs, serial consoles, debuggers).
    #[serde(default)]
    pub await_response_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendSignalArgs {
    /// Session ID
    pub session_id: String,
    /// Signal name (SIGINT, SIGTERM, SIGKILL, SIGSTOP, SIGCONT, SIGHUP, SIGQUIT)
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
