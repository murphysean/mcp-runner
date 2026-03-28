use rmcp::{
    ErrorData as McpError,
    model::*,
    service::RequestContext,
    RoleServer,
};

use crate::{Runner, util};

impl Runner {
    pub async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult {
            resource_templates: vec![
                Annotated::new(RawResourceTemplate {
                    uri_template: "session://{session_id}/stdout".into(),
                    name: "Session stdout".into(),
                    title: Some("Session stdout output".into()),
                    description: Some("Full stdout output for a command session".into()),
                    mime_type: Some("text/plain".into()),
                    icons: None,
                }, None),
                Annotated::new(RawResourceTemplate {
                    uri_template: "session://{session_id}/stderr".into(),
                    name: "Session stderr".into(),
                    title: Some("Session stderr output".into()),
                    description: Some("Full stderr output for a command session (only if split_stderr was true)".into()),
                    mime_type: Some("text/plain".into()),
                    icons: None,
                }, None),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    pub async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let sessions = self.sessions.lock().unwrap();
        let mut resources = Vec::new();

        for (id, session) in sessions.iter() {
            resources.push(Annotated::new(RawResource::new(
                format!("session://{id}/stdout"),
                format!("Session {id} stdout"),
            ), None));
            if session.stderr_path.is_some() {
                resources.push(Annotated::new(RawResource::new(
                    format!("session://{id}/stderr"),
                    format!("Session {id} stderr"),
                ), None));
            }
        }

        Ok(ListResourcesResult {
            resources,
            next_cursor: None,
            meta: None,
        })
    }

    pub async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let uri = &request.uri;

        let rest = uri.strip_prefix("session://")
            .ok_or_else(|| McpError::resource_not_found(format!("Unknown resource URI: {uri}"), None))?;
        let (id, stream) = rest.rsplit_once('/')
            .ok_or_else(|| McpError::resource_not_found(format!("Invalid resource URI: {uri}"), None))?;

        let path = {
            let sessions = self.sessions.lock().unwrap();
            let session = sessions.get(id)
                .ok_or_else(|| McpError::resource_not_found(format!("Session not found: {id}"), None))?;
            match stream {
                "stdout" => session.stdout_path.clone(),
                "stderr" => session.stderr_path.clone()
                    .ok_or_else(|| McpError::resource_not_found("stderr not split for this session", None))?,
                _ => return Err(McpError::resource_not_found(format!("Unknown stream: {stream}"), None)),
            }
        };

        let text = util::read_file_full(&path)
            .map_err(|e| McpError::internal_error(e, None))?;

        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri.clone())]))
    }
}
