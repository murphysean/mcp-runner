use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::{
    handler::server::wrapper::Parameters, model::*, service::ElicitationError, tool, tool_router,
    ErrorData as McpError, Peer, RoleServer,
};
use std::fs::File;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::util::{
    err, exit_code_from_status, pipe_to_file, pty_pipe_to_file, read_from_position,
    reap_session, remove_session, strip_ansi, text_result,
};
use crate::{
    ElicitedInput, ProcessHandle, ReadOutputArgs, Runner, SendInputArgs, SendSignalArgs, Session,
    SessionIdArgs, StartCommandArgs,
};

pub fn router() -> ToolRouter<Runner> {
    Runner::tool_router()
}

#[tool_router]
impl Runner {
    #[tool(description = "Start a new command session. Returns a session_id for use with other tools.\n\nIMPORTANT about use_pty:\n- Set use_pty: true ONLY for programs that need terminal features (picocom, gdb TUI, serial consoles, text editors).\n- For simple commands (python scripts, builds, tests), use_pty: false (default) gives cleaner output with proper newlines.\n- PTY output contains ANSI cursor positioning codes that look messy when stripped. For simple commands, avoid PTY.\n\nUse split_stderr: true to capture stderr separately.")]
    async fn start_command(
        &self,
        Parameters(args): Parameters<StartCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        let split_stderr = args.split_stderr.unwrap_or(false);
        let use_pty = args.use_pty.unwrap_or(false);
        let cmd_args = args.args.unwrap_or_default();

        let session_id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let stdout_path = format!("/tmp/mcp_cmd_{}_stdout.log", session_id);
        let stderr_path = if split_stderr && !use_pty {
            Some(format!("/tmp/mcp_cmd_{}_stderr.log", session_id))
        } else {
            None
        };

        File::create(&stdout_path).map_err(|e| err(e.to_string()))?;
        if let Some(ref path) = stderr_path {
            File::create(path).map_err(|e| err(e.to_string()))?;
        }

        let stdout_notify = self
            .peer
            .get()
            .map(|p| (p.clone(), format!("session://{session_id}/stdout")));
        let stderr_notify = self
            .peer
            .get()
            .map(|p| (p.clone(), format!("session://{session_id}/stderr")));

        let process = if use_pty {
            let (pty, pts) = pty_process::open().map_err(|e| err(e.to_string()))?;
            pty.resize(pty_process::Size::new(24, 80))
                .map_err(|e| err(e.to_string()))?;
            let cmd = pty_process::Command::new(&args.command);
            let child = cmd
                .args(&cmd_args)
                .spawn(pts)
                .map_err(|e| err(e.to_string()))?;

            let (read_pty, write_pty) = pty.into_split();
            let stdout_path_clone = stdout_path.clone();
            tokio::spawn(async move {
                pty_pipe_to_file(read_pty, stdout_path_clone, stdout_notify).await
            });

            let pty_writer = Arc::new(tokio::sync::Mutex::new(write_pty));
            ProcessHandle::Pty { child, pty_writer }
        } else {
            let mut cmd = Command::new(&args.command);
            cmd.args(&cmd_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());

            if split_stderr {
                cmd.stderr(Stdio::piped());
            } else {
                cmd.stderr(Stdio::inherit());
            }

            let mut child = cmd.spawn().map_err(|e| err(e.to_string()))?;

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take();

            let stdout_path_clone = stdout_path.clone();
            tokio::spawn(
                async move { pipe_to_file(stdout, stdout_path_clone, stdout_notify).await },
            );

            if let Some(stderr) = stderr {
                let p = stderr_path.clone().unwrap();
                tokio::spawn(async move { pipe_to_file(stderr, p, stderr_notify).await });
            }

            ProcessHandle::Pipe(child)
        };

        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            Session {
                process: Some(process),
                stdout_path,
                stderr_path,
                stdout_pos: 0,
                stderr_pos: 0,
                exit_code: None,
            },
        );

        self.notify_resource_list_changed();
        text_result(format!("Started command with session_id: {}", session_id))
    }

    #[tool(description = "Stop a running command by session_id. Sends SIGKILL to the process. Use send_signal with SIGINT for a graceful interrupt instead.")]
    async fn stop_command(
        &self,
        Parameters(args): Parameters<SessionIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let process = {
            let mut sessions = self.sessions.lock().unwrap();
            let session = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            session.process.take()
        };

        match process {
            Some(ProcessHandle::Pipe(mut child)) => {
                child.kill().map_err(|e| err(e.to_string()))?;
                let status = child.wait().map_err(|e| err(e.to_string()))?;
                let mut sessions = self.sessions.lock().unwrap();
                if let Some(session) = sessions.get_mut(&args.session_id) {
                    session.exit_code = exit_code_from_status(status);
                }
            }
            Some(ProcessHandle::Pty { mut child, .. }) => {
                child.start_kill().map_err(|e| err(e.to_string()))?;
                let status = child.wait().await.map_err(|e| err(e.to_string()))?;
                let mut sessions = self.sessions.lock().unwrap();
                if let Some(session) = sessions.get_mut(&args.session_id) {
                    session.exit_code = status.code().or_else(|| {
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            status.signal().map(|s| 128 + s)
                        }
                        #[cfg(not(unix))]
                        {
                            None
                        }
                    });
                }
            }
            None => {}
        }

        self.notify_resource_list_changed();
        text_result("Command stopped")
    }

    #[tool(
        description = "Delete a session and clean up its log files. Stops the process first if still running."
    )]
    async fn delete_session(
        &self,
        Parameters(args): Parameters<SessionIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        {
            let mut sessions = self.sessions.lock().unwrap();
            if !sessions.contains_key(&args.session_id) {
                return Err(err("Session not found"));
            }
            remove_session(&args.session_id, &mut sessions);
        }
        self.notify_resource_list_changed();
        text_result("Session deleted")
    }

    #[tool(
        description = "Send input to a running command's stdin.\n\nRECOMMENDED: Use await_response_ms to send input AND get output in one call:\n{\"session_id\": \"1\", \"input\": \"command\\r\\n\", \"await_response_ms\": 500}\nThis waits up to 500ms for output after sending, then returns both confirmation and response.\n\nHOW TO SEND ENTER/NEWLINE:\n- In JSON, use a SINGLE backslash: {\"input\": \"command\\n\"}\n- WRONG: {\"input\": \"command\\\\n\"} sends literal characters 'command\\n' (no Enter)\n- For PTY sessions (gdb, picocom), you may need \\r\\n: {\"input\": \"command\\r\\n\"}\n- When in doubt, use bytes: {\"bytes\": [..., 13, 10]} where 13=CR, 10=newline\n\nFor control characters: [1,24]=Ctrl-A Ctrl-X, [10]=newline, [13]=carriage return."
    )]
    async fn send_input(
        &self,
        Parameters(args): Parameters<SendInputArgs>,
        peer: Peer<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let data = if args.elicit.unwrap_or(false) {
            let msg = args
                .elicit_message
                .as_deref()
                .unwrap_or("Enter input for process");
            match peer.elicit::<ElicitedInput>(msg).await {
                Ok(Some(elicited)) => {
                    let mut bytes = elicited.input.into_bytes();
                    bytes.push(b'\n');
                    bytes
                }
                Ok(None) => return Err(err("User provided no input")),
                Err(ElicitationError::UserDeclined) => return text_result("User declined input"),
                Err(ElicitationError::UserCancelled) => return text_result("User cancelled input"),
                Err(ElicitationError::CapabilityNotSupported) => {
                    return Err(err("Client does not support elicitation"))
                }
                Err(e) => return Err(err(format!("Elicitation failed: {e}"))),
            }
        } else {
            match (args.input, args.bytes) {
                (Some(text), _) => text.into_bytes(),
                (None, Some(bytes)) => bytes,
                (None, None) => return Err(err("Provide 'input', 'bytes', or set 'elicit: true'")),
            }
        };

        let pty_writer = {
            let mut sessions = self.sessions.lock().unwrap();
            let session = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;

            match session.process {
                Some(ProcessHandle::Pty { ref pty_writer, .. }) => Some(pty_writer.clone()),
                Some(ProcessHandle::Pipe(ref mut child)) => {
                    if let Some(ref mut stdin) = child.stdin {
                        stdin.write_all(&data).map_err(|e| err(e.to_string()))?;
                        stdin.flush().map_err(|e| err(e.to_string()))?;
                    } else {
                        return Err(err("Process stdin not available"));
                    }
                    None
                }
                None => return Err(err("Process not running")),
            }
        };

        if let Some(writer) = pty_writer {
            use tokio::io::AsyncWriteExt;
            let mut w = writer.lock().await;
            w.write_all(&data).await.map_err(|e| err(e.to_string()))?;
            w.flush().await.map_err(|e| err(e.to_string()))?;
        }

        if let Some(timeout_ms) = args.await_response_ms {
            let timeout = std::time::Duration::from_millis(timeout_ms);
            let mut collected = String::new();

            loop {
                tokio::time::sleep(timeout).await;

                let (data, new_pos) = {
                    let sessions = self.sessions.lock().unwrap();
                    let s = sessions
                        .get(&args.session_id)
                        .ok_or_else(|| err("Session not found"))?;
                    read_from_position(&s.stdout_path, s.stdout_pos).map_err(err)?
                };

                if data.is_empty() {
                    break;
                }

                collected.push_str(&data);
                {
                    let mut sessions = self.sessions.lock().unwrap();
                    if let Some(s) = sessions.get_mut(&args.session_id) {
                        s.stdout_pos = new_pos;
                    }
                }
            }

            if collected.is_empty() {
                return text_result("Input sent (no response)");
            }
            return text_result(collected);
        }

        text_result("Input sent")
    }

    #[tool(description = "Send a Unix signal to a running command. Most common: SIGINT (like Ctrl-C, interrupts the program), SIGTERM (request graceful termination), SIGKILL (force kill). Use this to interrupt a running program like gdb or a REPL without killing the session.")]
    async fn send_signal(
        &self,
        Parameters(args): Parameters<SendSignalArgs>,
    ) -> Result<CallToolResult, McpError> {
        #[cfg(unix)]
        {
            use nix::sys::signal::{self, Signal};
            use nix::unistd::Pid;

            let signal_type = match args.signal.to_uppercase().as_str() {
                "SIGINT" => Signal::SIGINT,
                "SIGTERM" => Signal::SIGTERM,
                "SIGKILL" => Signal::SIGKILL,
                "SIGSTOP" => Signal::SIGSTOP,
                "SIGCONT" => Signal::SIGCONT,
                "SIGHUP" => Signal::SIGHUP,
                "SIGQUIT" => Signal::SIGQUIT,
                _ => return Err(err(format!("Unsupported signal: {}", args.signal))),
            };

            let mut sessions = self.sessions.lock().unwrap();
            let session = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;

            let pid = match session.process {
                Some(ProcessHandle::Pipe(ref child)) => child.id() as i32,
                Some(ProcessHandle::Pty { ref child, .. }) => {
                    child.id().ok_or_else(|| err("Process already exited"))? as i32
                }
                None => return Err(err("Process not running")),
            };

            signal::kill(Pid::from_raw(pid), signal_type).map_err(|e| err(e.to_string()))?;

            drop(sessions);
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(session) = sessions.get_mut(&args.session_id) {
                match session.process {
                    Some(ProcessHandle::Pipe(ref mut child)) => {
                        if let Ok(Some(status)) = child.try_wait() {
                            session.exit_code = exit_code_from_status(status);
                            session.process = None;
                        }
                    }
                    Some(ProcessHandle::Pty { ref mut child, .. }) => {
                        if let Ok(Some(status)) = child.try_wait() {
                            session.exit_code = status.code().or_else(|| {
                                #[cfg(unix)]
                                {
                                    use std::os::unix::process::ExitStatusExt;
                                    status.signal().map(|s| 128 + s)
                                }
                                #[cfg(not(unix))]
                                {
                                    None
                                }
                            });
                            session.process = None;
                        }
                    }
                    None => {}
                }
            }

            text_result(format!("Signal {} sent", args.signal))
        }

        #[cfg(not(unix))]
        {
            Err(err("Signal sending is only supported on Unix systems"))
        }
    }

    #[tool(description = "Read new stdout data since last read. Each call returns only new output (tracked per session).\n\nANSI escape codes are stripped by default (set strip_ansi: false to keep them).\n\nNOTE: If use_pty: true was set, output may contain cursor positioning codes that make it look messy when stripped. For clean output from simple commands, use use_pty: false.")]
    async fn read_output(
        &self,
        Parameters(args): Parameters<ReadOutputArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (path, pos) = {
            let sessions = self.sessions.lock().unwrap();
            let s = sessions
                .get(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            (s.stdout_path.clone(), s.stdout_pos)
        };

        let (data, new_pos) = read_from_position(&path, pos).map_err(err)?;

        let exited = {
            let mut sessions = self.sessions.lock().unwrap();
            let s = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            s.stdout_pos = new_pos;
            reap_session(s)
        };

        let mut result = if args.strip_ansi {
            strip_ansi(&data)
        } else {
            data
        };
        if let Some(msg) = exited {
            result.push_str(&format!("\n{msg}\n"));
        }
        text_result(result)
    }

    #[tool(description = "Read new stderr data since last read (only if split_stderr: true was set when starting). Each call returns only new output. ANSI escape codes are stripped by default (set strip_ansi: false to keep them).")]
    async fn read_stderr(
        &self,
        Parameters(args): Parameters<ReadOutputArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (path, pos) = {
            let sessions = self.sessions.lock().unwrap();
            let s = sessions
                .get(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            let p = s
                .stderr_path
                .as_ref()
                .ok_or_else(|| err("stderr not split for this session"))?;
            (p.clone(), s.stderr_pos)
        };

        let (data, new_pos) = read_from_position(&path, pos).map_err(err)?;

        let exited = {
            let mut sessions = self.sessions.lock().unwrap();
            let s = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            s.stderr_pos = new_pos;
            reap_session(s)
        };

        let mut result = if args.strip_ansi {
            strip_ansi(&data)
        } else {
            data
        };
        if let Some(msg) = exited {
            result.push_str(&format!("\n{msg}\n"));
        }
        text_result(result)
    }

    #[tool(description = "Get status of a command session. Returns whether the process is still running and its exit code (if finished). Use this to check if a long-running command has completed.")]
    async fn get_status(
        &self,
        Parameters(args): Parameters<SessionIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&args.session_id)
            .ok_or_else(|| err("Session not found"))?;
        let running = reap_session(session).is_none();
        text_result(format!(
            "Running: {}, Exit code: {:?}",
            running, session.exit_code
        ))
    }
}
