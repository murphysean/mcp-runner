# SSH & Remote Command Guide

## Starting an SSH session
Use PTY mode for interactive SSH (required for shell prompts and password entry):
```json
{"command": "ssh", "args": ["user@host"], "use_pty": true, "label": "remote-host"}
```

Wait for the shell prompt:
```json
{"session_id": "1", "wait_for": "$", "timeout_ms": 15000}
```

## Running remote commands
Once connected, send commands and await their output:
```json
{"session_id": "1", "input": "ls -la /var/log", "await_response_ms": 3000}
```

For long-running remote commands, use `read_output` with `wait_for`:
```json
{"session_id": "1", "input": "sudo apt update"}
```
```json
{"session_id": "1", "wait_for": "Reading package lists", "timeout_ms": 60000}
```

## Password entry
Use elicitation to securely enter passwords without exposing them:
```json
{"session_id": "1", "elicit": true, "elicit_message": "Enter SSH password for user@host"}
```

Or for sudo:
```json
{"session_id": "1", "input": "sudo systemctl restart nginx"}
```
If prompted for password:
```json
{"session_id": "1", "elicit": true, "elicit_message": "Enter sudo password"}
```

## SSH tunnel
Start a tunnel as a background session:
```json
{"command": "ssh", "args": ["-N", "-L", "5432:localhost:5432", "user@bastion"], "label": "db-tunnel"}
```

Check if it's still running with `get_status`. Kill with `stop_command` when done.

## SCP / file transfer
```json
{"command": "scp", "args": ["user@host:/remote/path", "/local/path"], "label": "download"}
```

Wait for completion:
```json
{"session_id": "1", "wait_for": "100%", "timeout_ms": 60000}
```

## Exiting SSH
```json
{"session_id": "1", "input": "exit"}
```

Or disconnect forcefully:
```json
{"session_id": "1", "signal": "SIGTERM"}
```

## Common issues
- Timeout on connect: host unreachable or SSH key not set up
- "Host key verification failed": need to accept the host key first
- Hung session: send `~.` (SSH escape to disconnect): `{"session_id": "1", "input": "~.", "no_enter": true}`
