use rmcp::{model::*, prompt, prompt_router, ErrorData as McpError};

use crate::Runner;

#[prompt_router(vis = "pub")]
impl Runner {
    #[prompt(
        description = "Guide for using picocom serial terminal. Covers connecting to a device, reading output, and exiting gracefully with raw byte control sequences."
    )]
    async fn picocom_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("picocom_guide.md"),
        )])
    }

    #[prompt(
        description = "Guide for using GDB (GNU Debugger) through the command wrapper. Covers starting GDB, common commands, and using Ctrl-C to interrupt execution."
    )]
    async fn gdb_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("gdb_guide.md"),
        )])
    }

    #[prompt(
        description = "Guide for on-device debugging with Black Magic Probe using GDB. Covers probe discovery, connecting, flashing, and debugging embedded targets."
    )]
    async fn blackmagic_probe_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("blackmagic_probe_guide.md"),
        )])
    }

    #[prompt(
        description = "Guide for running builds and tests. Covers using wait_for to detect completion, search_output for errors, timeout_seconds for bounded execution, and multi-step build workflows."
    )]
    async fn build_test_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("build_test_guide.md"),
        )])
    }

    #[prompt(
        description = "Guide for running and monitoring development servers. Covers waiting for ready state, log monitoring, restart patterns, and running multiple services."
    )]
    async fn dev_server_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("dev_server_guide.md"),
        )])
    }

    #[prompt(
        description = "Guide for SSH sessions, remote commands, secure password entry via elicitation, tunnels, and file transfers."
    )]
    async fn ssh_guide(&self) -> Result<Vec<PromptMessage>, McpError> {
        Ok(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            include_str!("ssh_guide.md"),
        )])
    }
}
