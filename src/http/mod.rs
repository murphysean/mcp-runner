use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response, sse::Sse},
    routing::{delete, get},
    Form, Router,
};
use futures::stream::Stream;
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::util::{ansi_to_html, reap_session, remove_session, strip_ansi};
use crate::{ProcessHandle, Sessions};

pub async fn serve(sessions: Sessions) {
    let app = Router::new()
        .route("/", get(http_index))
        .route("/session/{id}", delete(http_delete_session))
        .route("/session/{id}/stdout", get(http_stdout))
        .route("/session/{id}/stderr", get(http_stderr))
        .route("/session/{id}/stdout/stream", get(http_stdout_stream))
        .route("/session/{id}/stderr/stream", get(http_stderr_stream))
        .route("/session/{id}/stdout/follow", get(http_stdout_follow))
        .route("/session/{id}/stderr/follow", get(http_stderr_follow))
        .route(
            "/session/{id}/input",
            get(http_input_form).post(http_input_submit),
        )
        .route(
            "/session/{id}/password",
            get(http_password_form).post(http_input_submit),
        )
        .with_state(sessions);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8089));
    if let Ok(listener) = tokio::net::TcpListener::bind(addr).await {
        axum::serve(listener, app).await.ok();
    }
}

async fn http_index(State(sessions): State<Sessions>) -> Html<String> {
    let mut sessions = sessions.lock().unwrap();
    let mut html = String::from(
        "<!DOCTYPE html><html><head><title>MCP Runner</title></head><body>\
         <h1>Sessions</h1>",
    );
    if sessions.is_empty() {
        html.push_str("<p>No sessions</p>");
    } else {
        html.push_str("<ul>");
        for (id, session) in sessions.iter_mut() {
            let running = reap_session(session).is_none();
            let status = if running {
                "running".to_string()
            } else {
                match session.exit_code {
                    Some(code) => format!("exited ({code})"),
                    None => "unknown".to_string(),
                }
            };
            html.push_str(&format!(
                "<li>{id} ({status}) <a href=\"/session/{id}/stdout\">stdout</a> | <a href=\"/session/{id}/stdout/follow\">follow</a>"
            ));
            if session.stderr_path.is_some() {
                html.push_str(&format!(" | <a href=\"/session/{id}/stderr\">stderr</a> | <a href=\"/session/{id}/stderr/follow\">follow</a>"));
            }
            html.push_str(&format!(
                " | <a href=\"/session/{id}/input\">input</a>\
                 | <a href=\"/session/{id}/password\">password</a>\
                 | <button onclick=\"fetch('/session/{id}',{{method:'DELETE'}}).then(()=>location.reload())\">delete</button></li>"
            ));
        }
        html.push_str("</ul>");
    }
    html.push_str("</body></html>");
    Html(html)
}

async fn http_delete_session(State(sessions): State<Sessions>, Path(id): Path<String>) -> Response {
    let mut sessions = sessions.lock().unwrap();
    if sessions.contains_key(&id) {
        remove_session(&id, &mut sessions);
        "Deleted".into_response()
    } else {
        (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response()
    }
}

async fn http_stdout(
    State(sessions): State<Sessions>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let sessions = sessions.lock().unwrap();
    match sessions.get(&id) {
        Some(s) => match std::fs::read_to_string(&s.stdout_path) {
            Ok(c) => {
                let (content, mode_links) = format_output(&c, &id, "stdout", &params);
                Html(format!("<pre>{}</pre>{} | <a href=\"/session/{}/stdout/follow\">Follow Live</a>",
                    content, mode_links, id)).into_response()
            }
            Err(_) => Html("Error reading stdout".to_string()).into_response(),
        },
        None => Html("Session not found".to_string()).into_response(),
    }
}

async fn http_stderr(
    State(sessions): State<Sessions>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let sessions = sessions.lock().unwrap();
    match sessions.get(&id) {
        Some(s) => match &s.stderr_path {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(c) => {
                    let (content, mode_links) = format_output(&c, &id, "stderr", &params);
                    Html(format!("<pre>{}</pre>{} | <a href=\"/session/{}/stderr/follow\">Follow Live</a>",
                        content, mode_links, id)).into_response()
                }
                Err(_) => Html("Error reading stderr".to_string()).into_response(),
            },
            None => Html("stderr not split".to_string()).into_response(),
        },
        None => Html("Session not found".to_string()).into_response(),
    }
}

/// Format output based on query params: default (HTML), ?raw=1 (keep ANSI), ?strip=1 (plain text)
fn format_output(content: &str, id: &str, stream: &str, _params: &HashMap<String, String>) -> (String, String) {
    let mode_links = format!(
        "<a href=\"/\">Back</a> | <a href=\"/session/{id}/{stream}?raw=1\">Raw</a> | <a href=\"/session/{id}/{stream}?strip=1\">Strip</a>"
    );

    let content = if _params.contains_key("raw") {
        // Keep ANSI codes as-is, but HTML-escape
        html_escape(content)
    } else if _params.contains_key("strip") {
        // Strip ANSI codes, plain text
        strip_ansi(content)
    } else {
        // Default: convert ANSI to HTML
        ansi_to_html(content)
    };

    (content, mode_links)
}

/// HTML-escape special characters
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(serde::Deserialize)]
struct InputForm {
    input: String,
}

fn render_input_form(id: &str, password: bool) -> Html<String> {
    let (field, action, kind) = if password {
        (
            r#"<input type="password" name="input" style="width:100%;padding:8px;box-sizing:border-box;" autocomplete="off">"#,
            format!("/session/{id}/password"),
            "Password",
        )
    } else {
        (
            r#"<textarea name="input" rows="6" style="width:100%;padding:8px;box-sizing:border-box;"></textarea>"#,
            format!("/session/{id}/input"),
            "Text",
        )
    };
    Html(format!(
        r#"<!DOCTYPE html><html><head><title>Input - {id}</title></head><body>
<h2>{kind} Input for {id}</h2>
<form method="post" action="{action}">
{field}
<br><br><button type="submit" style="padding:8px 16px;">Send</button>
</form>
<p><a href="/">Back</a></p>
</body></html>"#
    ))
}

async fn http_input_form(Path(id): Path<String>) -> Html<String> {
    render_input_form(&id, false)
}

async fn http_password_form(Path(id): Path<String>) -> Html<String> {
    render_input_form(&id, true)
}

async fn http_input_submit(
    State(sessions): State<Sessions>,
    Path(id): Path<String>,
    Form(form): Form<InputForm>,
) -> Response {
    let input = format!("{}\r\n", form.input);

    let pty_writer = {
        let mut sessions = sessions.lock().unwrap();
        let Some(session) = sessions.get_mut(&id) else {
            return Html("Session not found".to_string()).into_response();
        };
        match session.process {
            Some(ProcessHandle::Pty { ref pty_writer, .. }) => Some(pty_writer.clone()),
            Some(ProcessHandle::Pipe(ref mut child)) => {
                if let Some(ref mut stdin) = child.stdin {
                    if stdin.write_all(input.as_bytes()).is_err() || stdin.flush().is_err() {
                        return Html("Failed to write to stdin".to_string()).into_response();
                    }
                }
                None
            }
            None => return Html("Process not running".to_string()).into_response(),
        }
    };

    if let Some(writer) = pty_writer {
        use tokio::io::AsyncWriteExt;
        let mut w = writer.lock().await;
        if w.write_all(input.as_bytes()).await.is_err() || w.flush().await.is_err() {
            return Html("Failed to write to PTY".to_string()).into_response();
        }
    }

    Redirect::to(&format!("/session/{id}/input")).into_response()
}

// SSE streaming for stdout
async fn http_stdout_stream(
    State(sessions): State<Sessions>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (stdout_path, session_pos) = {
        let sessions = sessions.lock().unwrap();
        match sessions.get(&id) {
            Some(s) => (s.stdout_path.clone(), s.stdout_pos),
            None => return Html("Session not found".to_string()).into_response(),
        }
    };

    // Use Last-Event-ID header if present, then ?from= param, then session position
    let initial_pos = parse_last_event_id(&headers)
        .or_else(|| params.get("from").and_then(|v| v.parse().ok()))
        .unwrap_or(session_pos);

    // Mode: default (html), ?raw=1 (keep ansi), ?strip=1 (plain text)
    let mode = if params.contains_key("raw") {
        "raw".to_string()
    } else if params.contains_key("strip") {
        "strip".to_string()
    } else {
        "html".to_string()
    };
    let stream = create_log_stream(sessions, id, stdout_path, initial_pos, mode);
    Sse::new(stream).into_response()
}

// SSE streaming for stderr
async fn http_stderr_stream(
    State(sessions): State<Sessions>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (stderr_path, session_pos) = {
        let sessions = sessions.lock().unwrap();
        match sessions.get(&id) {
            Some(s) => match &s.stderr_path {
                Some(p) => (p.clone(), s.stderr_pos),
                None => return Html("stderr not split".to_string()).into_response(),
            },
            None => return Html("Session not found".to_string()).into_response(),
        }
    };

    // Use Last-Event-ID header if present, then ?from= param, then session position
    let initial_pos = parse_last_event_id(&headers)
        .or_else(|| params.get("from").and_then(|v| v.parse().ok()))
        .unwrap_or(session_pos);

    // Mode: default (html), ?raw=1 (keep ansi), ?strip=1 (plain text)
    let mode = if params.contains_key("raw") {
        "raw".to_string()
    } else if params.contains_key("strip") {
        "strip".to_string()
    } else {
        "html".to_string()
    };
    let stream = create_log_stream(sessions, id, stderr_path, initial_pos, mode);
    Sse::new(stream).into_response()
}

fn parse_last_event_id(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
}

// HTML page that follows stdout stream using EventSource
async fn http_stdout_follow(Path(id): Path<String>) -> Html<String> {
    render_follow_page(&id, "stdout")
}

// HTML page that follows stderr stream using EventSource
async fn http_stderr_follow(Path(id): Path<String>) -> Html<String> {
    render_follow_page(&id, "stderr")
}

fn render_follow_page(id: &str, stream: &str) -> Html<String> {
    Html(format!(r##"<!DOCTYPE html>
<html>
<head>
    <title>Follow {stream} - Session {id}</title>
    <style>
        body {{ font-family: monospace; margin: 10px; background: #1a1a1a; color: #ddd; }}
        #output {{ white-space: pre-wrap; word-wrap: break-word; }}
        .line {{ margin: 0; }}
        #status {{ position: fixed; bottom: 10px; right: 10px; padding: 5px 10px; border-radius: 3px; }}
        .connected {{ background: #2a2; }}
        .disconnected {{ background: #a22; }}
        a {{ color: #6af; }}
    </style>
</head>
<body>
    <h3>Session {id} - {stream}</h3>
    <div id="status" class="connected">Live</div>
    <div id="output"></div>
    <script>
        const output = document.getElementById('output');
        const status = document.getElementById('status');
        // Start from beginning (from=0) to get all existing + new content
        const eventSource = new EventSource('/session/{id}/{stream}/stream?from=0');

        eventSource.onmessage = function(e) {{
            const line = document.createElement('div');
            line.className = 'line';
            line.innerHTML = e.data;
            output.appendChild(line);
            window.scrollTo(0, document.body.scrollHeight);
        }};

        eventSource.addEventListener('done', function(e) {{
            status.textContent = 'Exited';
            status.className = 'disconnected';
            eventSource.close();
        }});

        eventSource.onerror = function() {{
            status.textContent = 'Disconnected';
            status.className = 'disconnected';
        }};
    </script>
</body>
</html>"##))
}

fn create_log_stream(
    sessions: Sessions,
    session_id: String,
    log_path: String,
    initial_pos: u64,
    mode: String,
) -> impl Stream<Item = Result<axum::response::sse::Event, axum::Error>> {
    let pos = Arc::new(AtomicU64::new(initial_pos));

    async_stream::stream! {
        loop {
            // Check if process is still running
            let running = {
                let sessions = sessions.lock().unwrap();
                match sessions.get(&session_id) {
                    Some(s) => s.process.is_some(),
                    None => break,
                }
            };

            // Read new content from current position
            let current_pos = pos.load(Ordering::Relaxed);
            match read_lines_from_position(&log_path, current_pos) {
                Ok((lines, new_pos)) => {
                    for line in lines {
                        let line = match mode.as_str() {
                            "raw" => line,
                            "strip" => strip_ansi(&line),
                            _ => ansi_to_html(&line),
                        };
                        let id = pos.load(Ordering::Relaxed);
                        yield Ok(axum::response::sse::Event::default()
                            .id(id.to_string())
                            .data(&line));
                    }
                    pos.store(new_pos, Ordering::Relaxed);
                }
                Err(_) => break,
            }

            // If process exited and no new data, we're done
            if !running {
                let current_pos = pos.load(Ordering::Relaxed);
                match read_lines_from_position(&log_path, current_pos) {
                    Ok((lines, new_pos)) => {
                        if lines.is_empty() {
                            yield Ok(axum::response::sse::Event::default()
                                .event("done")
                                .data("[process exited]"));
                            break;
                        }
                        for line in lines {
                            let line = match mode.as_str() {
                                "raw" => line,
                                "strip" => strip_ansi(&line),
                                _ => ansi_to_html(&line),
                            };
                            let id = pos.load(Ordering::Relaxed);
                            yield Ok(axum::response::sse::Event::default()
                                .id(id.to_string())
                                .data(&line));
                        }
                        pos.store(new_pos, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
                break;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn read_lines_from_position(path: &str, pos: u64) -> Result<(Vec<String>, u64), String> {
    use std::io::{Read, Seek};

    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let file_size = file.metadata().map_err(|e| e.to_string())?.len();

    if pos >= file_size {
        return Ok((Vec::new(), pos));
    }

    file.seek(std::io::SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|e| e.to_string())?;

    let content = String::from_utf8_lossy(&data);
    let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

    // Calculate new position: each line includes its content + newline
    let new_pos = pos + content.len() as u64;

    Ok((lines, new_pos))
}
