# Picocom Serial Terminal Guide

## Connecting
Use `start_command` with `use_pty: true` (required for picocom):
```json
{"command": "picocom", "args": ["-b", "<baud_rate>", "<device>"], "use_pty": true}
```
Common baud rates: 9600, 115200, 1000000. Common devices: /dev/ttyUSB0, /dev/ttyACM0.

## Reading output
Use `read_output` with the session_id. Picocom prints connection info first, then "Terminal ready" when connected. Device output follows.

## Sending input
Use `send_input` with `input` for text, or `bytes` for raw control characters.

Enter/newline is automatically appended when using `input` — just send the command text:
```json
{"session_id": "...", "input": "help"}
```

To send text without Enter (e.g. partial input), set `no_enter: true`:
```json
{"session_id": "...", "input": "partial", "no_enter": true}
```

## IMPORTANT: Exiting picocom
You MUST use raw bytes to exit picocom. The escape sequence is Ctrl-A then Ctrl-X:
```json
{"session_id": "...", "bytes": [1, 24]}
```
- Byte 1 = Ctrl-A (picocom escape prefix)
- Byte 24 = Ctrl-X (exit command)

A successful exit prints "Thanks for using picocom" with exit code 0.
Do NOT use `stop_command` to kill picocom — that produces "Picocom was killed" (exit code 1) and may leave the serial port in a bad state.

## Other picocom escape commands (all prefixed with Ctrl-A = byte 1)
- [1, 24] = Ctrl-A Ctrl-X = Exit
- [1, 17] = Ctrl-A Ctrl-Q = Quit without reset
- [1, 16] = Ctrl-A Ctrl-P = Pulse DTR
- [1, 20] = Ctrl-A Ctrl-T = Toggle DTR
- [1, 8]  = Ctrl-A Ctrl-H = Hangup
- [1, 2]  = Ctrl-A Ctrl-B = Send break
