use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{sse::Sse, Html, IntoResponse, Response},
    routing::get,
    Form, Router,
};
use futures::stream::Stream;
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::util::{ansi_to_html, normalize_pty_output, reap_session, remove_session, strip_ansi};
use crate::{ProcessHandle, Sessions};

pub async fn serve(sessions: Sessions, port: u16, port_out: Arc<std::sync::atomic::AtomicU16>) {
    let app = Router::new()
        .route("/", get(http_index))
        .route(
            "/session/{id}",
            get(http_session).delete(http_delete_session),
        )
        .route("/session/{id}/stream", get(http_stream))
        .route(
            "/session/{id}/input",
            get(http_input_form).post(http_input_submit),
        )
        .with_state(sessions);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
        let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        port_out.store(actual_port, std::sync::atomic::Ordering::Relaxed);
        axum::serve(listener, app).await.ok();
    }
}

async fn http_index(State(sessions): State<Sessions>) -> Html<String> {
    let mut sessions = sessions.lock().await;
    let mut html = String::from(
        r#"<!DOCTYPE html><html><head><title>MCP Runner</title>
<style>
body { font-family: -apple-system, sans-serif; margin: 20px; background: #1a1a1a; color: #ddd; }
a { color: #6af; }
table { border-collapse: collapse; width: 100%; }
th, td { text-align: left; padding: 8px 12px; border-bottom: 1px solid #333; }
th { color: #999; font-size: 0.85em; text-transform: uppercase; }
.running { color: #5b5; }
.exited { color: #999; }
.failed { color: #e55; }
button { background: #433; color: #e88; border: 1px solid #644; padding: 4px 10px; border-radius: 3px; cursor: pointer; }
button:hover { background: #544; }
</style></head><body>
<h1>MCP Runner</h1>"#,
    );

    if sessions.is_empty() {
        html.push_str("<p>No active sessions</p>");
    } else {
        html.push_str(
            "<table><tr><th>ID</th><th>Label</th><th>Command</th><th>Status</th><th></th></tr>",
        );
        for (id, session) in sessions.iter_mut() {
            reap_session(session);
            let running = session.process.is_some();
            let (status, class) = if running {
                ("running".to_string(), "running")
            } else {
                match session.exit_code {
                    Some(0) => ("exited (0)".to_string(), "exited"),
                    Some(code) => (format!("exited ({code})"), "failed"),
                    None => ("unknown".to_string(), "exited"),
                }
            };
            let label = session.label.as_deref().unwrap_or("-");
            let cmd = html_escape(&session.command);
            html.push_str(&format!(
                r#"<tr>
<td><a href="/session/{id}">{id}</a></td>
<td>{label}</td>
<td><code>{cmd}</code></td>
<td class="{class}">{status}</td>
<td><button onclick="fetch('/session/{id}',{{method:'DELETE'}}).then(()=>location.reload())">delete</button></td>
</tr>"#
            ));
        }
        html.push_str("</table>");
    }
    html.push_str("</body></html>");
    Html(html)
}

async fn http_session(State(sessions): State<Sessions>, Path(id): Path<String>) -> Response {
    let sessions = sessions.lock().await;
    let Some(session) = sessions.get(&id) else {
        return Html("Session not found".to_string()).into_response();
    };
    let label = session.label.as_deref().unwrap_or(&id);
    let cmd = html_escape(&session.command);
    let running = session.process.is_some();

    Html(format!(r##"<!DOCTYPE html>
<html>
<head>
    <title>{label} - MCP Runner</title>
    <style>
        body {{ font-family: monospace; margin: 0; background: #1a1a1a; color: #ddd; display: flex; flex-direction: column; height: 100vh; }}
        #header {{ padding: 8px 12px; background: #222; border-bottom: 1px solid #333; display: flex; align-items: center; gap: 12px; flex-shrink: 0; }}
        #header a {{ color: #6af; text-decoration: none; }}
        #header .cmd {{ color: #999; font-size: 0.9em; }}
        #status {{ padding: 2px 8px; border-radius: 3px; font-size: 0.85em; }}
        .live {{ background: #253; color: #5b5; }}
        .done {{ background: #332; color: #a85; }}
        #output {{ flex: 1; overflow-y: auto; padding: 8px 12px; white-space: pre-wrap; word-wrap: break-word; }}
        #output .line {{ margin: 0; }}
        #input-bar {{ display: flex; padding: 8px; background: #222; border-top: 1px solid #333; flex-shrink: 0; }}
        #input-bar input {{ flex: 1; background: #111; color: #eee; border: 1px solid #444; padding: 6px 10px; font-family: monospace; font-size: 1em; }}
        #input-bar button {{ background: #335; color: #8bf; border: 1px solid #446; padding: 6px 14px; margin-left: 6px; cursor: pointer; }}
    </style>
</head>
<body>
    <div id="header">
        <a href="/">&larr; Sessions</a>
        <strong>{label}</strong>
        <span class="cmd">{cmd}</span>
        <span id="status" class="{status_class}">{status_text}</span>
    </div>
    <div id="output"></div>
    <form id="input-bar" onsubmit="sendInput(event)">
        <input type="text" id="cmd-input" placeholder="Type input and press Enter..." autocomplete="off" {disabled}>
        <button type="submit" {disabled}>Send</button>
    </form>
    <script>
        const output = document.getElementById('output');
        const statusEl = document.getElementById('status');
        const eventSource = new EventSource('/session/{id}/stream?from=0');

        eventSource.onmessage = function(e) {{
            const line = document.createElement('div');
            line.className = 'line';
            line.innerHTML = e.data;
            output.appendChild(line);
            output.scrollTop = output.scrollHeight;
        }};

        eventSource.addEventListener('done', function(e) {{
            statusEl.textContent = 'exited';
            statusEl.className = 'done';
            eventSource.close();
            document.getElementById('cmd-input').disabled = true;
        }});

        eventSource.onerror = function() {{
            statusEl.textContent = 'disconnected';
            statusEl.className = 'done';
        }};

        function sendInput(e) {{
            e.preventDefault();
            const input = document.getElementById('cmd-input');
            const text = input.value;
            if (!text && text !== '') return;
            fetch('/session/{id}/input', {{
                method: 'POST',
                headers: {{'Content-Type': 'application/x-www-form-urlencoded'}},
                body: 'input=' + encodeURIComponent(text)
            }});
            input.value = '';
        }}
    </script>
</body>
</html>"##,
        label = html_escape(label),
        cmd = cmd,
        id = id,
        status_class = if running { "live" } else { "done" },
        status_text = if running { "running" } else { "exited" },
        disabled = if running { "" } else { "disabled" },
    )).into_response()
}

async fn http_delete_session(State(sessions): State<Sessions>, Path(id): Path<String>) -> Response {
    let mut sessions = sessions.lock().await;
    if sessions.contains_key(&id) {
        remove_session(&id, &mut sessions);
        "Deleted".into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response()
    }
}

async fn http_stream(
    State(sessions): State<Sessions>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let stdout_path = {
        let sessions = sessions.lock().await;
        match sessions.get(&id) {
            Some(s) => s.stdout_path.clone(),
            None => return Html("Session not found".to_string()).into_response(),
        }
    };

    let initial_lines_seen = parse_last_event_id(&headers)
        .or_else(|| params.get("from").and_then(|v| v.parse().ok()))
        .unwrap_or(0);

    let mode = if params.contains_key("raw") {
        "raw"
    } else if params.contains_key("strip") {
        "strip"
    } else {
        "html"
    };
    let stream = create_log_stream(
        sessions,
        id,
        stdout_path,
        initial_lines_seen,
        mode.to_string(),
    );
    Sse::new(stream).into_response()
}

fn parse_last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

#[derive(serde::Deserialize)]
struct InputForm {
    input: String,
}

async fn http_input_form(Path(id): Path<String>) -> Html<String> {
    Html(format!(
        r#"<!DOCTYPE html><html><head><title>Input - {id}</title></head><body>
<h2>Input for session {id}</h2>
<form method="post" action="/session/{id}/input">
<textarea name="input" rows="6" style="width:100%;padding:8px;box-sizing:border-box;"></textarea>
<br><br><button type="submit" style="padding:8px 16px;">Send</button>
</form>
<p><a href="/session/{id}">Back to session</a> | <a href="/">Sessions</a></p>
</body></html>"#
    ))
}

async fn http_input_submit(
    State(sessions): State<Sessions>,
    Path(id): Path<String>,
    Form(form): Form<InputForm>,
) -> Response {
    let (input, pty_writer) = {
        let mut sessions = sessions.lock().await;
        let Some(session) = sessions.get_mut(&id) else {
            return (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response();
        };
        let line_ending = if session.is_pty { "\r\n" } else { "\n" };
        let input = format!("{}{}", form.input.trim_end(), line_ending);
        let writer = match session.process {
            Some(ProcessHandle::Pty { ref pty_writer, .. }) => Some(pty_writer.clone()),
            Some(ProcessHandle::Pipe(ref mut child)) => {
                if let Some(ref mut stdin) = child.stdin {
                    if stdin.write_all(input.as_bytes()).is_err() || stdin.flush().is_err() {
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "Write failed",
                        )
                            .into_response();
                    }
                }
                None
            }
            None => return (axum::http::StatusCode::GONE, "Process not running").into_response(),
        };
        (input, writer)
    };

    if let Some(writer) = pty_writer {
        use tokio::io::AsyncWriteExt;
        let mut w = writer.lock().await;
        if w.write_all(input.as_bytes()).await.is_err() || w.flush().await.is_err() {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "PTY write failed",
            )
                .into_response();
        }
    }

    (axum::http::StatusCode::OK, "OK").into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn create_log_stream(
    sessions: Sessions,
    session_id: String,
    log_path: String,
    initial_lines_seen: u64,
    mode: String,
) -> impl Stream<Item = Result<axum::response::sse::Event, axum::Error>> {
    let lines_seen = Arc::new(AtomicU64::new(initial_lines_seen));

    async_stream::stream! {
        loop {
            let running = {
                let sessions = sessions.lock().await;
                match sessions.get(&session_id) {
                    Some(s) => s.process.is_some(),
                    None => break,
                }
            };

            let current = lines_seen.load(Ordering::Relaxed);
            match read_complete_lines(&log_path, current) {
                Ok(lines) => {
                    for (line_num, line) in lines {
                        let line = match mode.as_str() {
                            "raw" => line,
                            "strip" => strip_ansi(&line),
                            _ => ansi_to_html(&line),
                        };
                        yield Ok(axum::response::sse::Event::default()
                            .id(line_num.to_string())
                            .data(&line));
                        lines_seen.store(line_num, Ordering::Relaxed);
                    }
                }
                Err(_) => break,
            }

            if !running {
                let current = lines_seen.load(Ordering::Relaxed);
                if let Ok(lines) = read_complete_lines(&log_path, current) {
                    for (line_num, line) in &lines {
                        let line = match mode.as_str() {
                            "raw" => line.clone(),
                            "strip" => strip_ansi(line),
                            _ => ansi_to_html(line),
                        };
                        yield Ok(axum::response::sse::Event::default()
                            .id(line_num.to_string())
                            .data(&line));
                        lines_seen.store(*line_num, Ordering::Relaxed);
                    }
                }
                yield Ok(axum::response::sse::Event::default()
                    .event("done")
                    .data("[process exited]"));
                break;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn read_complete_lines(path: &str, lines_seen: u64) -> Result<Vec<(u64, String)>, String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|e| e.to_string())?;

    let content = String::from_utf8_lossy(&data);

    let complete = match content.rfind('\n') {
        Some(pos) => &content[..pos + 1],
        None => return Ok(Vec::new()),
    };

    let mut result = Vec::new();
    for (i, line) in complete.split_terminator('\n').enumerate() {
        let line_num = (i as u64) + 1;
        if line_num > lines_seen {
            let line = normalize_pty_output(line.trim_end_matches('\r'));
            result.push((line_num, line));
        }
    }

    Ok(result)
}
