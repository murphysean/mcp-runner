use rmcp::{
    ErrorData as McpError,
    model::*,
    prompt, prompt_router,
};
use rmcp::handler::server::router::prompt::PromptRouter;

use crate::Runner;

pub fn router() -> PromptRouter<Runner> {
    Runner::prompt_router()
}

#[prompt_router]
impl Runner {
    #[prompt(description = "Guide for using picocom serial terminal. Covers connecting to a device, reading output, and exiting gracefully with raw byte control sequences.")]
    async fn picocom_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("picocom_guide.md"),
        )])
    }

    #[prompt(description = "Guide for using GDB (GNU Debugger) through the command wrapper. Covers starting GDB, common commands, and using Ctrl-C to interrupt execution.")]
    async fn gdb_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("gdb_guide.md"),
        )])
    }

    #[prompt(description = "Guide for on-device debugging with Black Magic Probe using GDB. Covers probe discovery, connecting, flashing, and debugging embedded targets.")]
    async fn blackmagic_probe_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("blackmagic_probe_guide.md"),
        )])
    }
}
