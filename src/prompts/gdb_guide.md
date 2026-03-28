# GDB Guide

## Starting GDB
Use `start_command` with `use_pty: true` for interactive use:
```json
{"command": "gdb", "args": ["<binary>"], "use_pty": true}
```
Or attach to a remote target:
```json
{"command": "gdb-multiarch", "args": ["-q", "<elf_file>"], "use_pty": true}
```

## Sending commands
Use `send_input` with `await_response_ms` to send a command and get the response in one call:
```json
{"session_id": "...", "input": "target remote :3333\n", "await_response_ms": 3000}
```
Always include `\n` at the end of commands.

## Interrupting execution
When the target is running (e.g. after `continue`), send SIGINT to break:
```json
{"session_id": "...", "signal": "SIGINT"}
```
Then `read_output` to see where it stopped.

## Common GDB commands
- `target remote <host>:<port>` — connect to remote target
- `load` — flash the program
- `break <location>` — set breakpoint (e.g. `break main`)
- `continue` / `c` — resume execution
- `step` / `s` — step into
- `next` / `n` — step over
- `print <expr>` — evaluate expression
- `info registers` — show registers
- `backtrace` / `bt` — show call stack
- `monitor reset halt` — reset target (OpenOCD/BMP)
- `quit` — exit GDB

## Exiting GDB
```json
{"session_id": "...", "input": "quit\n", "await_response_ms": 1000}
```
If GDB asks for confirmation, send `y\n`.
