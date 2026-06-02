# Development Server Guide

## Starting a dev server
```json
{"command": "npm", "args": ["run", "dev"], "label": "dev-server", "working_dir": "/path/to/project"}
```

Then wait for the ready message:
```json
{"session_id": "1", "wait_for": "ready", "timeout_ms": 30000}
```

Common ready patterns by framework:
- **Vite/Next.js**: "ready in", "Local:"
- **Django**: "Starting development server"
- **Flask**: "Running on"
- **Express**: "listening on port"
- **Rails**: "Listening on"
- **Go net/http**: "Serving" or custom log

## Monitoring server logs
After the server is running, periodically check for errors:
```json
{"session_id": "1", "pattern": "ERROR"}
{"session_id": "1", "pattern": "500"}
```

Or read recent output:
```json
{"session_id": "1", "timeout_ms": 1000}
```
This returns any output accumulated since last read, waiting up to 1s for new output.

## Restarting a server
Stop the old session and start fresh:
```json
// Send Ctrl-C first for graceful shutdown
{"session_id": "1", "signal": "SIGINT"}
```
Wait briefly, then check status:
```json
{"session_id": "1"}  // get_status
```
If still running after 3 seconds, use `stop_command`.

## Running multiple services
Common pattern: a frontend and backend running simultaneously:
```json
{"command": "npm", "args": ["run", "dev"], "label": "frontend", "working_dir": "/app/frontend"}
{"command": "python", "args": ["manage.py", "runserver"], "label": "backend", "working_dir": "/app/backend"}
```

Use `list_sessions` to see both at a glance. Use `search_output` on each to check for errors without reading full logs.

## Port conflicts
If a server fails to start due to a port conflict, find and kill the conflicting process:
```json
{"command": "lsof", "args": ["-ti", ":3000"]}
```
Read the output to get the PID, then start a new session to kill it or choose a different port.
