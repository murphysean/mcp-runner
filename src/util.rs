use rmcp::{
    model::CallToolResult, model::Content, model::LoggingLevel,
    model::LoggingMessageNotificationParam, model::ResourceUpdatedNotificationParam,
    ErrorData as McpError, Peer, RoleServer,
};
use serde_json;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::process::ExitStatus;

use crate::{ProcessHandle, Session};

/// Strip ANSI escape sequences from a string
pub fn strip_ansi(s: &str) -> String {
    let bytes = strip_ansi_escapes::strip(s.as_bytes());
    String::from_utf8_lossy(&bytes).to_string()
}

/// Convert ANSI escape sequences to HTML with inline styles
pub fn ansi_to_html(s: &str) -> String {
    ansi_to_html::convert(s).unwrap_or_else(|_| strip_ansi(s))
}

/// Normalize PTY output for clean line-based consumption.
///
/// 1. Normalize `\r\n` to `\n`
/// 2. Handle standalone `\r` (carriage return without newline) by discarding
///    everything before the `\r` on that line, keeping only what follows.
///    This collapses progress bar / spinner updates to their final state.
pub fn normalize_pty_output(s: &str) -> String {
    // Step 1: \r\n → \n
    let s = s.replace("\r\n", "\n");

    // Step 2: handle standalone \r (carriage return = overwrite from start of line)
    if !s.contains('\r') {
        return s;
    }

    let mut result = String::with_capacity(s.len());
    for line in s.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }
        // For each line, split on \r and keep only the last segment
        // (each \r returns the cursor to column 0 and overwrites)
        if line.contains('\r') {
            if let Some(last) = line.rsplit('\r').next() {
                result.push_str(last);
            }
        } else {
            result.push_str(line);
        }
    }
    result
}

pub fn text_result(msg: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![Content::text(msg.into())]))
}

pub fn err(msg: impl Into<String>) -> McpError {
    McpError::internal_error(msg.into(), None)
}

pub fn exit_code_from_status(status: ExitStatus) -> Option<i32> {
    status.code().or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|s| 128 + s)
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
}

pub type LogNotify = (
    Peer<RoleServer>,
    String,
    LoggingLevel,
    std::sync::Arc<std::sync::Mutex<LoggingLevel>>,
);

fn flush_lines(
    line_buf: &mut Vec<u8>,
    log: &Option<LogNotify>,
    handle: &Option<tokio::runtime::Handle>,
    flush_all: bool,
) {
    let (Some((ref peer, ref logger, level, ref min_level)), Some(ref handle)) = (log, handle)
    else {
        if flush_all {
            line_buf.clear();
        }
        return;
    };
    loop {
        if let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
            let line_bytes = line_buf.drain(..=pos).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line_bytes).trim_end().to_string();
            if line.is_empty() {
                continue;
            }
            let line = normalize_pty_output(&line);
            let line = strip_ansi(&line);
            let min = *min_level.lock().unwrap();
            if crate::level_value(*level) >= crate::level_value(min) {
                let peer = peer.clone();
                let param = LoggingMessageNotificationParam {
                    level: *level,
                    logger: Some(logger.clone()),
                    data: serde_json::Value::String(line),
                };
                handle.spawn(async move {
                    peer.notify_logging_message(param).await.ok();
                });
            }
        } else if flush_all && !line_buf.is_empty() {
            let line = String::from_utf8_lossy(line_buf).trim_end().to_string();
            line_buf.clear();
            if line.is_empty() {
                return;
            }
            let line = normalize_pty_output(&line);
            let line = strip_ansi(&line);
            let min = *min_level.lock().unwrap();
            if crate::level_value(*level) >= crate::level_value(min) {
                let peer = peer.clone();
                let param = LoggingMessageNotificationParam {
                    level: *level,
                    logger: Some(logger.clone()),
                    data: serde_json::Value::String(line),
                };
                handle.spawn(async move {
                    peer.notify_logging_message(param).await.ok();
                });
            }
            return;
        } else {
            return;
        }
    }
}

pub async fn pipe_to_file<R: Read + Send + 'static>(
    reader: R,
    path: String,
    notify: Option<(Peer<RoleServer>, String)>,
    log: Option<LogNotify>,
) {
    let needs_handle = notify.is_some() || log.is_some();
    let handle = if needs_handle {
        Some(tokio::runtime::Handle::current())
    } else {
        None
    };
    tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut file = OpenOptions::new().append(true).open(&path).ok();
        let mut buf = vec![0u8; 4096];
        let mut line_buf = Vec::new();
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            if let Some(ref mut f) = file {
                f.write_all(&buf[..n]).ok();
                f.flush().ok();
            }
            if let (Some((ref peer, ref uri)), Some(ref handle)) = (&notify, &handle) {
                let peer = peer.clone();
                let uri = uri.clone();
                handle.spawn(async move {
                    peer.notify_resource_updated(ResourceUpdatedNotificationParam { uri })
                        .await
                        .ok();
                });
            }
            if log.is_some() {
                line_buf.extend_from_slice(&buf[..n]);
                flush_lines(&mut line_buf, &log, &handle, false);
            }
        }
        // Flush remaining partial line on EOF
        if !line_buf.is_empty() {
            flush_lines(&mut line_buf, &log, &handle, true);
        }
    })
    .await
    .ok();
}

pub async fn pty_pipe_to_file(
    mut reader: pty_process::OwnedReadPty,
    path: String,
    notify: Option<(Peer<RoleServer>, String)>,
    log: Option<LogNotify>,
) {
    use tokio::io::AsyncReadExt;
    let mut file = OpenOptions::new().append(true).open(&path).ok();
    let mut buf = vec![0u8; 4096];
    let mut line_buf = Vec::new();
    let handle = log.as_ref().map(|_| tokio::runtime::Handle::current());
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if let Some(ref mut f) = file {
                    f.write_all(&buf[..n]).ok();
                    f.flush().ok();
                }
                if let Some((ref peer, ref uri)) = notify {
                    peer.notify_resource_updated(ResourceUpdatedNotificationParam {
                        uri: uri.clone(),
                    })
                    .await
                    .ok();
                }
                if log.is_some() {
                    line_buf.extend_from_slice(&buf[..n]);
                    flush_lines(&mut line_buf, &log, &handle, false);
                }
            }
            Err(_) => break,
        }
    }
    // Flush remaining partial line on EOF
    if !line_buf.is_empty() {
        flush_lines(&mut line_buf, &log, &handle, true);
    }
}

pub fn read_from_position(path: &str, pos: u64) -> Result<(String, u64), String> {
    use std::io::Seek;
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let file_size = file.metadata().map_err(|e| e.to_string())?.len();
    if pos >= file_size {
        return Ok((String::new(), pos));
    }
    file.seek(std::io::SeekFrom::Start(pos))
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|e| e.to_string())?;
    Ok((String::from_utf8_lossy(&data).to_string(), file_size))
}

pub fn read_file_full(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

pub fn reap_session(session: &mut Session) -> Option<String> {
    let was_running = session.process.is_some();
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
    if was_running && session.process.is_none() {
        Some(format!(
            "[process exited with code {:?}]",
            session.exit_code.unwrap_or(-1)
        ))
    } else {
        None
    }
}

pub fn remove_session(session_id: &str, sessions: &mut HashMap<String, Session>) {
    if let Some(mut s) = sessions.remove(session_id) {
        match s.process.take() {
            Some(ProcessHandle::Pipe(mut child)) => {
                child.kill().ok();
            }
            Some(ProcessHandle::Pty { mut child, .. }) => {
                child.start_kill().ok();
            }
            None => {}
        }
        std::fs::remove_file(&s.stdout_path).ok();
        if let Some(ref p) = s.stderr_path {
            std::fs::remove_file(p).ok();
        }
    }
}

pub fn cleanup_all_sessions(sessions: &crate::Sessions) {
    for (_, mut session) in sessions.lock().unwrap().drain() {
        match session.process.take() {
            Some(ProcessHandle::Pipe(mut child)) => {
                child.kill().ok();
            }
            Some(ProcessHandle::Pty { mut child, .. }) => {
                child.start_kill().ok();
            }
            None => {}
        }
        std::fs::remove_file(&session.stdout_path).ok();
        if let Some(ref p) = session.stderr_path {
            std::fs::remove_file(p).ok();
        }
    }
}
