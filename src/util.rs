use rmcp::{
    model::CallToolResult, model::Content, model::ResourceUpdatedNotificationParam,
    ErrorData as McpError, Peer, RoleServer,
};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::process::ExitStatus;

use crate::{ProcessHandle, Session};

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

pub async fn pipe_to_file<R: Read + Send + 'static>(
    reader: R,
    path: String,
    notify: Option<(Peer<RoleServer>, String)>,
) {
    let handle = notify.as_ref().map(|_| tokio::runtime::Handle::current());
    tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut file = OpenOptions::new().append(true).open(&path).ok();
        let mut buf = vec![0u8; 4096];
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
        }
    })
    .await
    .ok();
}

pub async fn pty_pipe_to_file(
    mut reader: pty_process::OwnedReadPty,
    path: String,
    notify: Option<(Peer<RoleServer>, String)>,
) {
    use tokio::io::AsyncReadExt;
    let mut file = OpenOptions::new().append(true).open(&path).ok();
    let mut buf = vec![0u8; 4096];
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
            }
            Err(_) => break,
        }
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
    if session.process.is_none() {
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
