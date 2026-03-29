use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get},
    Form, Router,
};
use std::io::Write;
use std::net::SocketAddr;

use crate::util::{reap_session, remove_session};
use crate::{ProcessHandle, Sessions};

pub async fn serve(sessions: Sessions) {
    let app = Router::new()
        .route("/", get(http_index))
        .route("/session/:id", delete(http_delete_session))
        .route("/session/:id/stdout", get(http_stdout))
        .route("/session/:id/stderr", get(http_stderr))
        .route(
            "/session/:id/input",
            get(http_input_form).post(http_input_submit),
        )
        .route(
            "/session/:id/password",
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
                "<li>{id} ({status}) <a href=\"/session/{id}/stdout\">stdout</a>"
            ));
            if session.stderr_path.is_some() {
                html.push_str(&format!(" | <a href=\"/session/{id}/stderr\">stderr</a>"));
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

async fn http_stdout(State(sessions): State<Sessions>, Path(id): Path<String>) -> Response {
    let sessions = sessions.lock().unwrap();
    match sessions.get(&id) {
        Some(s) => match std::fs::read_to_string(&s.stdout_path) {
            Ok(c) => Html(format!("<pre>{c}</pre><a href=\"/\">Back</a>")).into_response(),
            Err(_) => Html("Error reading stdout".to_string()).into_response(),
        },
        None => Html("Session not found".to_string()).into_response(),
    }
}

async fn http_stderr(State(sessions): State<Sessions>, Path(id): Path<String>) -> Response {
    let sessions = sessions.lock().unwrap();
    match sessions.get(&id) {
        Some(s) => match &s.stderr_path {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(c) => Html(format!("<pre>{c}</pre><a href=\"/\">Back</a>")).into_response(),
                Err(_) => Html("Error reading stderr".to_string()).into_response(),
            },
            None => Html("stderr not split".to_string()).into_response(),
        },
        None => Html("Session not found".to_string()).into_response(),
    }
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
