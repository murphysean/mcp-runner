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
    err, exit_code_from_status, normalize_pty_output, pipe_to_file, pty_pipe_to_file,
    read_from_position, reap_session, remove_session, strip_ansi, text_result,
};
use crate::{
    ElicitedInput, ProcessHandle, ReadOutputArgs, Runner, SendInputArgs, SendSignalArgs, Session,
    SessionIdArgs, StartCommandArgs,
};

#[tool_router(vis = "pub")]
impl Runner {
    #[tool(
        description = "Start a new command session. Returns a session_id for use with other tools.\n\nIMPORTANT about use_pty:\n- Set use_pty: true ONLY for programs that need terminal features (picocom, gdb TUI, serial consoles, text editors).\n- For simple commands (python scripts, builds, tests), use_pty: false (default) gives cleaner output with proper newlines.\n- PTY output contains ANSI cursor positioning codes that look messy when stripped. For simple commands, avoid PTY.\n\nUse split_stderr: true to capture stderr separately."
    )]
    async fn start_command(
        &self,
        Parameters(args): Parameters<StartCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        let split_stderr = args.split_stderr.unwrap_or(false);
        let use_pty = args.use_pty.unwrap_or(false);
        let stream_log = args.stream_log.unwrap_or(false);
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

        let stdout_log = if stream_log {
            self.peer.get().map(|p| {
                (
                    p.clone(),
                    format!("session/{session_id}/stdout"),
                    LoggingLevel::Info,
                    self.log_level.clone(),
                )
            })
        } else {
            None
        };
        let stderr_log = if stream_log {
            self.peer.get().map(|p| {
                (
                    p.clone(),
                    format!("session/{session_id}/stderr"),
                    LoggingLevel::Warning,
                    self.log_level.clone(),
                )
            })
        } else {
            None
        };

        let process = if use_pty {
            let (pty, pts) = pty_process::open().map_err(|e| err(e.to_string()))?;
            pty.resize(pty_process::Size::new(24, 80))
                .map_err(|e| err(e.to_string()))?;
            let mut cmd = pty_process::Command::new(&args.command);
            cmd = cmd.args(&cmd_args);
            if let Some(ref dir) = args.working_dir {
                cmd = cmd.current_dir(dir);
            }
            if let Some(ref env) = args.env {
                for (k, v) in env {
                    cmd = cmd.env(k, v);
                }
            }
            let child = cmd.spawn(pts).map_err(|e| err(e.to_string()))?;

            let (read_pty, write_pty) = pty.into_split();
            let stdout_path_clone = stdout_path.clone();
            tokio::spawn(async move {
                pty_pipe_to_file(read_pty, stdout_path_clone, stdout_notify, stdout_log).await
            });

            let pty_writer = Arc::new(tokio::sync::Mutex::new(write_pty));
            ProcessHandle::Pty { child, pty_writer }
        } else {
            let mut cmd = Command::new(&args.command);
            cmd.args(&cmd_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(ref dir) = args.working_dir {
                cmd.current_dir(dir);
            }
            if let Some(ref env) = args.env {
                for (k, v) in env {
                    cmd.env(k, v);
                }
            }

            let mut child = cmd.spawn().map_err(|e| err(e.to_string()))?;

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();

            let stdout_path_clone = stdout_path.clone();
            tokio::spawn(async move {
                pipe_to_file(stdout, stdout_path_clone, stdout_notify, stdout_log).await
            });

            if split_stderr {
                let p = stderr_path.clone().unwrap();
                tokio::spawn(
                    async move { pipe_to_file(stderr, p, stderr_notify, stderr_log).await },
                );
            } else {
                // Merge stderr into stdout log file
                let stdout_path_for_stderr = stdout_path.clone();
                tokio::spawn(async move {
                    pipe_to_file(stderr, stdout_path_for_stderr, stderr_notify, stderr_log).await
                });
            }

            ProcessHandle::Pipe(child)
        };

        let cmd_display = if cmd_args.is_empty() {
            args.command.clone()
        } else {
            format!("{} {}", args.command, cmd_args.join(" "))
        };

        self.sessions.lock().await.insert(
            session_id.clone(),
            Session {
                process: Some(process),
                is_pty: use_pty,
                label: args.label,
                command: cmd_display,
                stdout_path,
                stderr_path,
                stdout_pos: 0,
                stderr_pos: 0,
                exit_code: None,
                stream_log,
            },
        );

        if let Some(timeout_secs) = args.timeout_seconds {
            let sessions = self.sessions.clone();
            let sid = session_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
                let process = {
                    let mut sessions = sessions.lock().await;
                    let Some(session) = sessions.get_mut(&sid) else {
                        return;
                    };
                    session.process.take()
                };
                match process {
                    Some(ProcessHandle::Pipe(mut child)) => {
                        #[cfg(unix)]
                        {
                            use nix::sys::signal::{self, Signal};
                            use nix::unistd::Pid;
                            signal::kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).ok();
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        child.kill().ok();
                        if let Ok(status) = child.wait() {
                            let mut sessions = sessions.lock().await;
                            if let Some(s) = sessions.get_mut(&sid) {
                                s.exit_code = exit_code_from_status(status);
                            }
                        }
                    }
                    Some(ProcessHandle::Pty { mut child, .. }) => {
                        #[cfg(unix)]
                        {
                            if let Some(pid) = child.id() {
                                use nix::sys::signal::{self, Signal};
                                use nix::unistd::Pid;
                                signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM).ok();
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        child.start_kill().ok();
                        if let Ok(status) = child.wait().await {
                            let mut sessions = sessions.lock().await;
                            if let Some(s) = sessions.get_mut(&sid) {
                                s.exit_code = exit_code_from_status(status);
                            }
                        }
                    }
                    None => {}
                }
            });
        }

        self.notify_resource_list_changed();
        let base = self.http_base_url();
        text_result(format!(
            "Started command with session_id: {}\nFollow: {}/session/{}",
            session_id, base, session_id
        ))
    }

    #[tool(
        description = "Run a command to completion and return its full output + exit code. Blocks until the process exits (or timeout). Use this for commands that produce a result and exit: builds, tests, scripts, one-shot CLI tools. For long-running processes (servers, REPLs, debuggers), use start_command instead.\n\nDefault timeout: 300s (5 minutes). Set timeout_seconds for longer builds."
    )]
    async fn run_command(
        &self,
        Parameters(args): Parameters<crate::RunCommandArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cmd_args = args.args.unwrap_or_default();
        let timeout_secs = args.timeout_seconds.unwrap_or(300);

        let mut cmd = Command::new(&args.command);
        cmd.args(&cmd_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(ref dir) = args.working_dir {
            cmd.current_dir(dir);
        }
        if let Some(ref env) = args.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        let mut child = cmd.spawn().map_err(|e| err(e.to_string()))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stdout_handle = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::BufReader::new(stdout), &mut buf).ok();
            buf
        });
        let stderr_handle = tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut std::io::BufReader::new(stderr), &mut buf).ok();
            buf
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
            let stdout_bytes = stdout_handle.await.unwrap_or_default();
            let stderr_bytes = stderr_handle.await.unwrap_or_default();
            let status = tokio::task::spawn_blocking(move || child.wait())
                .await
                .map_err(|e| err(e.to_string()))?
                .map_err(|e| err(e.to_string()))?;
            Ok::<_, McpError>((stdout_bytes, stderr_bytes, status))
        })
        .await;

        match result {
            Ok(Ok((stdout_bytes, stderr_bytes, status))) => {
                let exit_code = exit_code_from_status(status);
                let stdout_str = String::from_utf8_lossy(&stdout_bytes);
                let stderr_str = String::from_utf8_lossy(&stderr_bytes);
                let stdout_clean = strip_ansi(&stdout_str);
                let stderr_clean = strip_ansi(&stderr_str);

                let mut output = stdout_clean;
                if !stderr_clean.is_empty() {
                    if !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str("[stderr]\n");
                    output.push_str(&stderr_clean);
                }
                output.push_str(&format!("\n[exit code: {}]\n", exit_code.unwrap_or(-1)));
                text_result(output)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => text_result(format!(
                "(command timed out after {}s, process killed)\n",
                timeout_secs
            )),
        }
    }

    #[tool(
        description = "Stop a running command by session_id. Sends SIGKILL to the process. Use send_signal with SIGINT for a graceful interrupt instead."
    )]
    async fn stop_command(
        &self,
        Parameters(args): Parameters<SessionIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let process = {
            let mut sessions = self.sessions.lock().await;
            let session = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            session.process.take()
        };

        match process {
            Some(ProcessHandle::Pipe(mut child)) => {
                child.kill().map_err(|e| err(e.to_string()))?;
                let status = child.wait().map_err(|e| err(e.to_string()))?;
                let mut sessions = self.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&args.session_id) {
                    session.exit_code = exit_code_from_status(status);
                }
            }
            Some(ProcessHandle::Pty { mut child, .. }) => {
                child.start_kill().map_err(|e| err(e.to_string()))?;
                let status = child.wait().await.map_err(|e| err(e.to_string()))?;
                let mut sessions = self.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&args.session_id) {
                    session.exit_code = exit_code_from_status(status);
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
            let mut sessions = self.sessions.lock().await;
            if !sessions.contains_key(&args.session_id) {
                return Err(err("Session not found"));
            }
            remove_session(&args.session_id, &mut sessions);
        }
        self.notify_resource_list_changed();
        text_result("Session deleted")
    }

    #[tool(
        description = "Send input to a running command's stdin. Enter is auto-appended.\n\nGETTING OUTPUT BACK:\n- wait_for: \"$\" — BEST for interactive sessions. Waits until the prompt pattern appears, then returns all output. Use the shell/REPL prompt (\"$\", \">>>\", \"(gdb)\").\n- wait: true — Waits for output, returns when it stops arriving (1s idle). Use when you don't know the prompt.\n- Neither — Fire and forget. Use read_output later to check.\n\nExamples:\n  {\"session_id\": \"1\", \"input\": \"ls\", \"wait_for\": \"$\"}\n  {\"session_id\": \"1\", \"input\": \"make\", \"wait\": true}\n  {\"session_id\": \"1\", \"bytes\": [1, 24]} — raw Ctrl-A Ctrl-X, no Enter\n\nAll waits are capped at 30s. If nothing arrives for 5s with wait_for, returns what it has."
    )]
    async fn send_input(
        &self,
        Parameters(args): Parameters<SendInputArgs>,
        peer: Peer<RoleServer>,
        meta: Meta,
    ) -> Result<CallToolResult, McpError> {
        // Determine if session is PTY (needed for correct line ending)
        let is_pty = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            session.is_pty
        };
        let no_enter = args.no_enter.unwrap_or(false);
        let line_ending: &[u8] = if is_pty { b"\r\n" } else { b"\n" };

        let data = if args.elicit.unwrap_or(false) {
            let msg = args
                .elicit_message
                .as_deref()
                .unwrap_or("Enter input for process");
            match peer.elicit::<ElicitedInput>(msg).await {
                Ok(Some(elicited)) => {
                    let mut bytes = elicited.input.trim_end().as_bytes().to_vec();
                    bytes.extend_from_slice(line_ending);
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
                (Some(text), _) => {
                    let mut bytes = text.trim_end().as_bytes().to_vec();
                    if !no_enter {
                        bytes.extend_from_slice(line_ending);
                    }
                    bytes
                }
                (None, Some(bytes)) => bytes,
                (None, None) => return Err(err("Provide 'input', 'bytes', or set 'elicit: true'")),
            }
        };

        let has_wait = args.wait.unwrap_or(false)
            || args.await_response_ms.is_some()
            || args.wait_for.is_some();

        let pty_writer = {
            let mut sessions = self.sessions.lock().await;
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

        if has_wait {
            let idle_ms = args.await_response_ms.unwrap_or(1000);
            let idle_timeout = std::time::Duration::from_millis(idle_ms);
            let poll_interval = std::time::Duration::from_millis(50);
            let wall_cap = std::time::Duration::from_secs(30);
            let mut collected = String::new();
            let mut idle_since = tokio::time::Instant::now();
            let start_time = tokio::time::Instant::now();
            let progress_token = meta.get_progress_token();
            let mut last_progress = tokio::time::Instant::now();

            loop {
                tokio::time::sleep(poll_interval).await;

                let (data, exited) = {
                    let mut sessions = self.sessions.lock().await;
                    let s = sessions
                        .get_mut(&args.session_id)
                        .ok_or_else(|| err("Session not found"))?;
                    let (data, new_pos) =
                        read_from_position(&s.stdout_path, s.stdout_pos).map_err(err)?;
                    s.stdout_pos = new_pos;
                    let exited = reap_session(s);
                    (data, exited)
                };

                if !data.is_empty() {
                    collected.push_str(&data);
                    idle_since = tokio::time::Instant::now();
                }

                // Check wait_for pattern
                if let Some(ref pattern) = args.wait_for {
                    let check = strip_ansi(&normalize_pty_output(&collected));
                    if check.contains(pattern.as_str()) {
                        let result = strip_ansi(&normalize_pty_output(&collected));
                        return text_result(result);
                    }
                }

                // Process exited — return what we have
                if exited.is_some() {
                    let mut result = strip_ansi(&normalize_pty_output(&collected));
                    if let Some(msg) = exited {
                        result.push_str(&format!("\n{msg}\n"));
                    }
                    return text_result(result);
                }

                // Wall-time cap (30s) — prevents unbounded blocking on noisy output
                if start_time.elapsed() >= wall_cap {
                    let mut result = strip_ansi(&normalize_pty_output(&collected));
                    result.push_str("\n(30s wall-time cap reached)\n");
                    return text_result(result);
                }

                // Idle timeout: return when output stops arriving
                if data.is_empty() && idle_since.elapsed() >= idle_timeout {
                    // For wait_for: if nothing has arrived at all for 5s, give up
                    // If some output arrived but pattern not found, idle means "done"
                    if args.wait_for.is_some() {
                        let no_output_cap = std::time::Duration::from_secs(5);
                        if idle_since.elapsed() >= no_output_cap {
                            break;
                        }
                    } else {
                        break;
                    }
                }

                if let Some(ref token) = progress_token {
                    if last_progress.elapsed() >= std::time::Duration::from_secs(1) {
                        let elapsed = start_time.elapsed().as_secs_f64();
                        peer.notify_progress(ProgressNotificationParam {
                            progress_token: token.clone(),
                            progress: elapsed,
                            total: None,
                            message: Some(format!(
                                "Awaiting response... {:.1}s elapsed, {} bytes collected",
                                elapsed,
                                collected.len()
                            )),
                        })
                        .await
                        .ok();
                        last_progress = tokio::time::Instant::now();
                    }
                }
            }

            if collected.is_empty() {
                return text_result("Input sent (no response)");
            }
            let collected = normalize_pty_output(&collected);
            return text_result(strip_ansi(&collected));
        }

        text_result("Input sent")
    }

    #[tool(
        description = "Send a Unix signal to a running command. Most common: SIGINT (like Ctrl-C, interrupts the program), SIGTERM (request graceful termination), SIGKILL (force kill). Use this to interrupt a running program like gdb or a REPL without killing the session."
    )]
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

            let pid = {
                let mut sessions = self.sessions.lock().await;
                let session = sessions
                    .get_mut(&args.session_id)
                    .ok_or_else(|| err("Session not found"))?;

                match session.process {
                    Some(ProcessHandle::Pipe(ref child)) => child.id() as i32,
                    Some(ProcessHandle::Pty { ref child, .. }) => {
                        child.id().ok_or_else(|| err("Process already exited"))? as i32
                    }
                    None => return Err(err("Process not running")),
                }
            };

            signal::kill(Pid::from_raw(pid), signal_type).map_err(|e| err(e.to_string()))?;

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mut sessions = self.sessions.lock().await;
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
                            session.exit_code = exit_code_from_status(status);
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

    #[tool(
        description = "Read new stdout data since last read. Each call returns only new output (tracked per session).\n\nANSI escape codes are stripped by default (set strip_ansi: false to keep them).\n\nWAITING FOR OUTPUT: Use timeout_ms to wait for output instead of polling. Use wait_for to wait until a specific pattern appears (e.g., \"BUILD SUCCESS\", \"error\", \"listening on\").\n\nNOTE: If use_pty: true was set, output may contain cursor positioning codes that make it look messy when stripped. For clean output from simple commands, use use_pty: false."
    )]
    async fn read_output(
        &self,
        Parameters(args): Parameters<ReadOutputArgs>,
    ) -> Result<CallToolResult, McpError> {
        let has_wait = args.timeout_ms.is_some() || args.wait_for.is_some();

        if has_wait {
            let max_wait = std::time::Duration::from_millis(args.timeout_ms.unwrap_or(30_000));
            let idle_timeout = std::time::Duration::from_millis(300);
            let poll_interval = std::time::Duration::from_millis(50);
            let deadline = tokio::time::Instant::now() + max_wait;
            let mut collected = String::new();
            let mut idle_since = tokio::time::Instant::now();

            loop {
                tokio::time::sleep(poll_interval).await;

                let (data, _new_pos, exited) = {
                    let mut sessions = self.sessions.lock().await;
                    let s = sessions
                        .get_mut(&args.session_id)
                        .ok_or_else(|| err("Session not found"))?;
                    let (data, new_pos) =
                        read_from_position(&s.stdout_path, s.stdout_pos).map_err(err)?;
                    s.stdout_pos = new_pos;
                    let exited = reap_session(s);
                    (data, new_pos, exited)
                };

                if !data.is_empty() {
                    let data = normalize_pty_output(&data);
                    let data = if args.strip_ansi {
                        strip_ansi(&data)
                    } else {
                        data
                    };
                    collected.push_str(&data);
                    idle_since = tokio::time::Instant::now();
                }

                if let Some(ref pattern) = args.wait_for {
                    if collected.contains(pattern.as_str()) {
                        if let Some(msg) = exited {
                            collected.push_str(&format!("\n{msg}\n"));
                        }
                        return text_result(collected);
                    }
                }

                if exited.is_some() {
                    if let Some(msg) = exited {
                        collected.push_str(&format!("\n{msg}\n"));
                    }
                    return text_result(collected);
                }

                if tokio::time::Instant::now() >= deadline {
                    if collected.is_empty() {
                        return text_result("(timeout, no output)");
                    }
                    collected.push_str("\n(timeout)\n");
                    return text_result(collected);
                }

                // For idle-based return (no wait_for pattern): return when idle long enough
                if args.wait_for.is_none()
                    && !collected.is_empty()
                    && idle_since.elapsed() >= idle_timeout
                {
                    return text_result(collected);
                }
            }
        }

        // Immediate read (no waiting)
        let (path, pos) = {
            let sessions = self.sessions.lock().await;
            let s = sessions
                .get(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            (s.stdout_path.clone(), s.stdout_pos)
        };

        let (data, new_pos) = read_from_position(&path, pos).map_err(err)?;

        let exited = {
            let mut sessions = self.sessions.lock().await;
            let s = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            s.stdout_pos = new_pos;
            reap_session(s)
        };

        let data = normalize_pty_output(&data);
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

    #[tool(
        description = "Read new stderr data since last read (only if split_stderr: true was set when starting). Each call returns only new output. ANSI escape codes are stripped by default (set strip_ansi: false to keep them)."
    )]
    async fn read_stderr(
        &self,
        Parameters(args): Parameters<ReadOutputArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (path, pos) = {
            let sessions = self.sessions.lock().await;
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
            let mut sessions = self.sessions.lock().await;
            let s = sessions
                .get_mut(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            s.stderr_pos = new_pos;
            reap_session(s)
        };

        let data = normalize_pty_output(&data);
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

    #[tool(
        description = "Get status of a command session. Returns whether the process is still running and its exit code (if finished). Use this to check if a long-running command has completed."
    )]
    async fn get_status(
        &self,
        Parameters(args): Parameters<SessionIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&args.session_id)
            .ok_or_else(|| err("Session not found"))?;
        reap_session(session);
        let running = session.process.is_some();
        text_result(format!(
            "Running: {}, Exit code: {:?}",
            running, session.exit_code
        ))
    }

    #[tool(
        description = "List all sessions with their IDs, labels, commands, and status. Use this to discover active sessions or find a session by label."
    )]
    async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.is_empty() {
            return text_result("No sessions");
        }
        let base = self.http_base_url();
        let mut lines = Vec::new();
        for (id, session) in sessions.iter_mut() {
            reap_session(session);
            let status = if session.process.is_some() {
                "running".to_string()
            } else {
                match session.exit_code {
                    Some(code) => format!("exited ({})", code),
                    None => "unknown".to_string(),
                }
            };
            let label_str = session
                .label
                .as_deref()
                .map(|l| format!(" [{}]", l))
                .unwrap_or_default();
            lines.push(format!(
                "  {}{}: {} ({}) — {}/session/{}",
                id, label_str, session.command, status, base, id
            ));
        }
        text_result(format!("Sessions:\n{}", lines.join("\n")))
    }

    #[tool(
        description = "Search session output for a pattern. Returns matching lines with line numbers. Searches the full output history (not just unread). Use this to find errors, specific log messages, or confirm expected output without reading the entire buffer."
    )]
    async fn search_output(
        &self,
        Parameters(args): Parameters<crate::SearchOutputArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&args.session_id)
                .ok_or_else(|| err("Session not found"))?;
            if args.stderr {
                session
                    .stderr_path
                    .clone()
                    .ok_or_else(|| err("stderr not split for this session"))?
            } else {
                session.stdout_path.clone()
            }
        };

        let content = std::fs::read_to_string(&path).map_err(|e| err(e.to_string()))?;
        let content = normalize_pty_output(&content);
        let content = strip_ansi(&content);
        let max = args.max_results.unwrap_or(20);

        let mut matches: Vec<String> = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.contains(&args.pattern) {
                matches.push(format!("{}: {}", i + 1, line));
                if matches.len() >= max {
                    break;
                }
            }
        }

        if matches.is_empty() {
            text_result(format!("No matches for \"{}\"", args.pattern))
        } else {
            let total_note = if matches.len() >= max {
                format!("\n(showing first {} matches)", max)
            } else {
                String::new()
            };
            text_result(format!(
                "{} match(es) for \"{}\":\n{}{}",
                matches.len(),
                args.pattern,
                matches.join("\n"),
                total_note
            ))
        }
    }
}
