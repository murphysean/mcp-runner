# MCP Runner

In which I vibe code an Model Context Protocol (MCP) server that wraps long-running commands, allowing agents to start, interact with, and monitor processes like HTTP servers, debuggers, or complex compilation tasks.

## Features

- **Multiple concurrent sessions**: Run multiple commands simultaneously, each with a unique session ID
- **Persistent logging**: All output logged to `/tmp` files
- **Incremental output reading**: Only returns new output since last read
- **Flexible stderr handling**: Combine stderr with stdout or keep them separate
- **Auto-cleanup**: Sessions automatically cleaned up when process exits and all output is consumed
- **HTTP monitoring**: Web interface on port 8089 to view active sessions and their output
- **Raw byte input**: Send control characters (Ctrl-A, Ctrl-X, etc.) via byte arrays
- **Response awaiting**: Block after sending input and collect output until idle
- **MCP elicitation**: Prompt users directly for passwords/secrets without exposing them to the LLM
- **MCP prompts**: Built-in guides for picocom, GDB, and Black Magic Probe workflows

## Installation

### From GitHub (recommended)

```bash
cargo install --git https://github.com/murphysean/mcp-runner.git
```

### Build from source

```bash
git clone https://github.com/murphysean/mcp-runner.git
cd mcp-runner
cargo build --release
```

The binary will be at `target/release/mcp-runner`

## Usage

### Claude Code

Add to your Claude Code configuration in `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "runner": {
      "command": "/path/to/mcp-runner"
    }
  }
}
```

Or use a project-specific configuration in `.claude/settings.json` within your project directory.

### Kiro

Add to your Kiro MCP configuration. In Kiro, navigate to **Settings > MCP Servers** and add:

```json
{
  "runner": {
    "command": "/path/to/mcp-runner"
  }
}
```

Alternatively, create or edit the MCP configuration file at `~/.kiro/mcp.json`:

```json
{
  "mcpServers": {
    "runner": {
      "command": "/path/to/mcp-runner"
    }
  }
}
```

### Other MCP Clients

Add to your MCP client configuration:

```json
{
  "mcpServers": {
    "runner": {
      "command": "/path/to/mcp-runner"
    }
  }
}
```

### Available Tools

#### `start_command`
Start a new command session.

**Parameters:**
- `command` (string, required): Command to execute
- `args` (array of strings, optional): Command arguments
- `split_stderr` (boolean, optional): If true, keep stderr separate from stdout (default: false)
- `use_pty` (boolean, optional): Spawn inside a pseudo-terminal (required for interactive programs like picocom, gdb TUI, etc.)

**Returns:** Session ID

**Example:**
```json
{
  "command": "python3",
  "args": ["-m", "http.server", "8000"],
  "split_stderr": false
}
```

#### `send_input`
Send input to a running command's stdin. Supports text, raw bytes, and MCP elicitation for passwords.

**Parameters:**
- `session_id` (string, required): Session ID
- `input` (string, optional): Text input to send as UTF-8
- `bytes` (array of u8, optional): Raw bytes to send (e.g. `[1, 24]` for Ctrl-A Ctrl-X)
- `elicit` (boolean, optional): If true, prompt the user directly via MCP elicitation (password never touches the LLM)
- `elicit_message` (string, optional): Custom prompt message for elicitation
- `await_response_ms` (u64, optional): Block and collect output until no new data arrives for this many ms

At least one of `input`, `bytes`, or `elicit: true` must be provided. If `input` is present it takes priority over `bytes`. `await_response_ms` composes with any input mode.

**Important: Newlines**
Include `\n` in the JSON string to send a newline (Enter key). Do NOT double-escape:
- ✓ `"input": "ls\n"` — correct, sends `ls` followed by Enter
- ✗ `"input": "ls\\n"` — wrong, sends literal characters `ls\n` (no Enter)

When in doubt, use the `bytes` parameter with byte 10 (newline):
```json
{"session_id": "1", "bytes": [108, 115, 10]}
```
This sends `l`, `s`, then newline, eliminating escape confusion.

**Examples:**
```json
{"session_id": "1", "input": "print('hello')\n", "await_response_ms": 1000}
```
```json
{"session_id": "1", "bytes": [1, 24]}
```
```json
{"session_id": "1", "elicit": true, "elicit_message": "Enter the sudo password"}
```

#### `send_signal`
Send a Unix signal to a running command (e.g., for interrupting gdb).

**Parameters:**
- `session_id` (string, required): Session ID
- `signal` (string, required): Signal name (SIGINT, SIGTERM, SIGKILL, SIGSTOP, SIGCONT, SIGHUP, SIGQUIT)

**Note:** Only supported on Unix systems.

#### `read_output`
Read new stdout data since last read.

**Parameters:**
- `session_id` (string, required): Session ID
- `strip_ansi` (boolean, optional): Strip ANSI escape sequences from output (default: true)

**Returns:** New output text (with ANSI stripped by default)

#### `read_stderr`
Read new stderr data since last read (only if `split_stderr` was true).

**Parameters:**
- `session_id` (string, required): Session ID
- `strip_ansi` (boolean, optional): Strip ANSI escape sequences from output (default: true)

**Returns:** New stderr text (with ANSI stripped by default)

#### `stop_command`
Stop a running command.

**Parameters:**
- `session_id` (string, required): Session ID

#### `delete_session`
Delete a session and clean up its log files. Stops the process first if still running.

**Parameters:**
- `session_id` (string, required): Session ID

#### `get_status`
Get status of a command session.

**Parameters:**
- `session_id` (string, required): Session ID

**Returns:** Running status and exit code (if finished)

### HTTP Endpoints

The MCP runner provides an HTTP interface on port 8089 for monitoring sessions:

- `GET /` — List all sessions with status and links
- `GET /session/{id}/stdout` — View current stdout content
- `GET /session/{id}/stderr` — View current stderr content
- `GET /session/{id}/stdout/stream` — SSE stream of stdout (like `tail -f`)
- `GET /session/{id}/stderr/stream` — SSE stream of stderr (like `tail -f`)
- `POST /session/{id}/input` — Send input to the process
- `DELETE /session/{id}` — Delete a session

#### ANSI Escape Code Handling

HTTP endpoints handle ANSI escape codes (colors, bold, etc.) in three modes:

| Query Param | Behavior |
|-------------|----------|
| *(default)* | Convert ANSI to styled HTML (colors, bold, etc.) |
| `?raw=1` | Keep ANSI codes as-is |
| `?strip=1` | Strip ANSI codes, plain text |

Examples:
- `GET /session/1/stdout` — HTML with styled output
- `GET /session/1/stdout?raw=1` — Raw output with ANSI codes
- `GET /session/1/stdout?strip=1` — Plain text without ANSI

For SSE streams, the default is stripped (clean data). Use `?raw=1` to keep ANSI codes:
- `GET /session/1/stdout/stream` — Stripped output
- `GET /session/1/stdout/stream?raw=1` — Raw output with ANSI codes

#### SSE Streaming

The `/session/{id}/stdout/stream` and `/session/{id}/stderr/stream` endpoints use Server-Sent Events (SSE) to stream output in real-time:

```
id: 42
data: Hello, world!

id: 58
data: Another line

event: done
data: [process exited]
```

- Each event has an `id` field containing the byte offset (useful for resume)
- The `data` field contains one line of output
- When the process exits, a final `event: done` message is sent

**Resume from last position:** Include `Last-Event-ID` header with the byte offset to resume from:
```bash
curl -H "Last-Event-ID: 42" http://localhost:8089/session/1/stdout/stream
```

**JavaScript example:**
```javascript
const source = new EventSource('/session/1/stdout/stream');
source.onmessage = (e) => console.log(e.data);
source.addEventListener('done', () => source.close());
```

### Available Prompts

#### `picocom_guide`
Guide for using picocom serial terminal. Covers connecting to a device, reading output, and exiting gracefully with raw byte control sequences (Ctrl-A Ctrl-X = `[1, 24]`).

#### `gdb_guide`
Guide for using GDB through the command wrapper. Covers starting GDB, sending commands with `await_response_ms`, interrupting execution with SIGINT, and common debugging commands.

#### `blackmagic_probe_guide`
Guide for on-device debugging with Black Magic Probe. Covers probe discovery, connecting via `target extended-remote`, SWD scanning, flashing, debugging, and monitoring UART output in a parallel picocom session.

## Example Workflow

1. **Start a web server:**
   ```
   Tool: start_command
   Args: {"command": "python3", "args": ["-m", "http.server", "8000"]}
   Returns: session_id: "1"
   ```

2. **Check output:**
   ```
   Tool: read_output
   Args: {"session_id": "1"}
   Returns: "Serving HTTP on 0.0.0.0 port 8000..."
   ```

3. **Check status:**
   ```
   Tool: get_status
   Args: {"session_id": "1"}
   Returns: {"running": true, "exit_code": null}
   ```

4. **Stop the server:**
   ```
   Tool: stop_command
   Args: {"session_id": "1"}
   ```

## Implementation Details

- Log files are created at `/tmp/mcp_cmd_<session_id>_stdout.log` and `_stderr.log`
- Read positions are tracked in-memory per session
- Sessions persist after process exit so output can be reviewed; use `delete_session` to clean up
- All sessions are terminated when the MCP server shuts down

## TODO

- [x] **HTTP Streaming**: SSE endpoints for real-time stdout/stderr streaming (`/session/{id}/stdout/stream`, `/session/{id}/stderr/stream`)
- [x] **HTTP Follow Pages**: Live HTML pages that display SSE streams (`/session/{id}/stdout/follow`, `/session/{id}/stderr/follow`)
- [x] **MCP Resources**: Expose session stdout/stderr as subscribable MCP resources (`session://1/stdout`). Push updates via `notify_resource_updated`.
- [ ] **MCP Logging Notifications**: Stream process output to the client log in real-time via `notify_logging_message`, so agents can passively watch long-running builds or servers without calling `read_output`.
- [ ] **Progress Notifications**: Send `notify_progress` updates while `await_response_ms` is blocking, so clients can show elapsed time or parsed build progress.

## License

MIT
