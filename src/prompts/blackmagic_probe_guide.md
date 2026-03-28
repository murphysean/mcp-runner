# Black Magic Probe (BMP) Debugging Guide

## Overview
Black Magic Probe is a JTAG/SWD debugger that runs a GDB server directly on the probe — no OpenOCD needed. You connect GDB directly to the probe's serial port.

## Finding the probe
BMP exposes two serial ports. The first is GDB, the second is UART:
```json
{"command": "ls", "args": ["-la", "/dev/serial/by-id/"], "split_stderr": false}
```
Look for `usb-Black_Magic_Debug_*`. The one ending in `-if00` is GDB, `-if02` is UART.
Typically: `/dev/ttyACM0` (GDB) and `/dev/ttyACM1` (UART).

## Starting GDB
```json
{"command": "gdb-multiarch", "args": ["-q", "<path_to_elf>"], "use_pty": true}
```

## Connecting to the probe
Send these commands in sequence using `send_input` with `await_response_ms`:
```
target extended-remote /dev/ttyACM0
monitor swdp_scan
attach 1
```
- `target extended-remote` connects to the BMP GDB server
- `monitor swdp_scan` scans the SWD bus and lists found targets
- `attach 1` attaches to the first target found

## Flashing
```
load
```
This flashes the ELF file specified when starting GDB.

## Debugging
```
monitor reset halt
break main
continue
```
Then use standard GDB commands: `step`, `next`, `print`, `backtrace`, `info registers`, etc.

## Interrupting a running target
Use `send_signal` with SIGINT:
```json
{"session_id": "...", "signal": "SIGINT"}
```

## UART output
To also monitor UART output from the device, start a second session with picocom on the UART port (see picocom_guide prompt):
```json
{"command": "picocom", "args": ["-b", "115200", "/dev/ttyACM1"], "use_pty": true}
```

## Exiting
```json
{"session_id": "...", "input": "quit\n", "await_response_ms": 1000}
```

## Common issues
- "Remote communication error": probe disconnected or wrong port
- "No targets found": check wiring, try `monitor swdp_scan` again
- "Target voltage: ABSENT!": target board not powered
- If GDB hangs after `continue`, use SIGINT to interrupt
