# Build & Test Workflow Guide

## Strategy: Use wait_for instead of fixed timeouts
When running builds or tests, use `read_output` with `wait_for` to wait for completion patterns instead of guessing how long the build will take.

## Running a build
```json
{"command": "cargo", "args": ["build", "--release"], "label": "build", "timeout_seconds": 300}
```

Then wait for completion:
```json
{"session_id": "1", "wait_for": "Finished", "timeout_ms": 60000}
```

Common completion patterns by tool:
- **cargo**: "Finished", "error[E" (failure)
- **npm/yarn**: "Done in", "ERR!" (failure)
- **make**: "make: Nothing to be done", "Error" (failure)
- **go build**: returns empty on success, error text on failure
- **gradle**: "BUILD SUCCESSFUL", "BUILD FAILED"
- **cmake --build**: "Built target", "Error" (failure)

## Running tests
```json
{"command": "cargo", "args": ["test"], "label": "tests", "timeout_seconds": 300}
```

Wait for the summary line:
```json
{"session_id": "1", "wait_for": "test result:", "timeout_ms": 120000}
```

Then use `search_output` to find failures:
```json
{"session_id": "1", "pattern": "FAILED"}
```

## Watching for errors in long output
Instead of reading all output, search for what matters:
```json
{"session_id": "1", "pattern": "error"}
{"session_id": "1", "pattern": "warning"}
```

## Multiple build steps
Label each session to keep track:
```json
{"command": "npm", "args": ["install"], "label": "install"}
{"command": "npm", "args": ["run", "build"], "label": "build", "working_dir": "/path/to/project"}
{"command": "npm", "args": ["test"], "label": "test"}
```

Use `list_sessions` to see status of all steps at once.

## Setting environment for builds
```json
{
  "command": "cargo",
  "args": ["build"],
  "env": {"RUSTFLAGS": "-C target-cpu=native", "CARGO_INCREMENTAL": "0"},
  "working_dir": "/path/to/project"
}
```
