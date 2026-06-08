//! Unit tests for mcp-runner internal logic.
//!
//! These test discrete functions without spawning a full MCP server.
//! We test: ANSI stripping, PTY output normalization, file position reading,
//! HTML escaping, line reading, log level ordering, and search/pattern matching.

use std::fs::{self, File};
use std::io::Write;
use tempfile::tempdir;

// ─── ANSI Stripping ─────────────────────────────────────────────────────────

#[test]
fn strip_ansi_removes_color_codes() {
    let input = "\x1b[31mERROR\x1b[0m: something failed";
    let result = strip_ansi_escapes::strip(input.as_bytes());
    let result = String::from_utf8_lossy(&result).to_string();
    assert_eq!(result, "ERROR: something failed");
}

#[test]
fn strip_ansi_removes_cursor_movement() {
    // CSI sequences: cursor up, cursor position, etc.
    let input = "\x1b[2J\x1b[H\x1b[1;1HHello";
    let result = strip_ansi_escapes::strip(input.as_bytes());
    let result = String::from_utf8_lossy(&result).to_string();
    assert_eq!(result, "Hello");
}

#[test]
fn strip_ansi_preserves_plain_text() {
    let input = "Just a normal line with no escapes";
    let result = strip_ansi_escapes::strip(input.as_bytes());
    let result = String::from_utf8_lossy(&result).to_string();
    assert_eq!(result, input);
}

#[test]
fn strip_ansi_handles_bold_underline_combos() {
    let input = "\x1b[1m\x1b[4mBold Underline\x1b[0m normal";
    let result = strip_ansi_escapes::strip(input.as_bytes());
    let result = String::from_utf8_lossy(&result).to_string();
    assert_eq!(result, "Bold Underline normal");
}

#[test]
fn strip_ansi_handles_256_color() {
    let input = "\x1b[38;5;196mRed text\x1b[0m";
    let result = strip_ansi_escapes::strip(input.as_bytes());
    let result = String::from_utf8_lossy(&result).to_string();
    assert_eq!(result, "Red text");
}

#[test]
fn strip_ansi_handles_empty_string() {
    let input = "";
    let result = strip_ansi_escapes::strip(input.as_bytes());
    let result = String::from_utf8_lossy(&result).to_string();
    assert_eq!(result, "");
}

// ─── PTY Output Normalization ────────────────────────────────────────────────

/// Replicate normalize_pty_output logic for testing without needing to import
/// from the binary crate.
fn normalize_pty_output(s: &str) -> String {
    let s = s.replace("\r\n", "\n");
    if !s.contains('\r') {
        return s;
    }
    let mut result = String::with_capacity(s.len());
    for line in s.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }
        if line.contains('\r') {
            if let Some(last) = line.rsplit('\r').next() {
                result.push_str(last);
            }
        } else {
            result.push_str(line);
        }
    }
    result
}

#[test]
fn normalize_crlf_to_lf() {
    let input = "line1\r\nline2\r\nline3";
    let result = normalize_pty_output(input);
    assert_eq!(result, "line1\nline2\nline3");
}

#[test]
fn normalize_carriage_return_overwrites() {
    // Progress bar: "0%\r50%\r100%"
    let input = "0%\r50%\r100%";
    let result = normalize_pty_output(input);
    assert_eq!(result, "100%");
}

#[test]
fn normalize_mixed_cr_and_crlf() {
    // Line with progress (CR overwrite) followed by normal CRLF lines
    let input = "downloading...\r50%\r100%\r\ndone\r\n";
    let result = normalize_pty_output(input);
    assert_eq!(result, "100%\ndone\n");
}

#[test]
fn normalize_no_special_chars() {
    let input = "plain output\nno special chars\n";
    let result = normalize_pty_output(input);
    assert_eq!(result, input);
}

#[test]
fn normalize_spinner_overwrite() {
    // Simulated spinner: |\r/\r-\r\\\rDone
    let input = "|\r/\r-\r\\\rDone";
    let result = normalize_pty_output(input);
    assert_eq!(result, "Done");
}

#[test]
fn normalize_cr_at_start_of_line() {
    let input = "\rOverwritten start";
    let result = normalize_pty_output(input);
    assert_eq!(result, "Overwritten start");
}

#[test]
fn normalize_multiline_with_cr_on_some_lines() {
    let input = "line1\npartial\rfull\nline3";
    let result = normalize_pty_output(input);
    assert_eq!(result, "line1\nfull\nline3");
}

#[test]
fn normalize_empty_string() {
    assert_eq!(normalize_pty_output(""), "");
}

#[test]
fn normalize_only_cr() {
    let input = "\r";
    let result = normalize_pty_output(input);
    assert_eq!(result, "");
}

#[test]
fn normalize_crlf_only() {
    let input = "\r\n\r\n";
    let result = normalize_pty_output(input);
    assert_eq!(result, "\n\n");
}

// ─── File Position Reading ───────────────────────────────────────────────────

fn read_from_position(path: &str, pos: u64) -> Result<(String, u64), String> {
    use std::io::{Read, Seek};
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let file_size = file.metadata().map_err(|e| e.to_string())?.len();
    if pos >= file_size {
        return Ok((String::new(), pos));
    }
    file.seek(std::io::SeekFrom::Start(pos))
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|e| e.to_string())?;
    Ok((String::from_utf8_lossy(&data).to_string(), file_size))
}

#[test]
fn read_from_position_start() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "hello world").unwrap();

    let (data, new_pos) = read_from_position(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(data, "hello world");
    assert_eq!(new_pos, 11);
}

#[test]
fn read_from_position_middle() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "hello world").unwrap();

    let (data, new_pos) = read_from_position(path.to_str().unwrap(), 6).unwrap();
    assert_eq!(data, "world");
    assert_eq!(new_pos, 11);
}

#[test]
fn read_from_position_at_end() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "hello").unwrap();

    let (data, new_pos) = read_from_position(path.to_str().unwrap(), 5).unwrap();
    assert_eq!(data, "");
    assert_eq!(new_pos, 5);
}

#[test]
fn read_from_position_past_end() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "hi").unwrap();

    let (data, new_pos) = read_from_position(path.to_str().unwrap(), 100).unwrap();
    assert_eq!(data, "");
    assert_eq!(new_pos, 100);
}

#[test]
fn read_from_position_empty_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "").unwrap();

    let (data, new_pos) = read_from_position(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(data, "");
    assert_eq!(new_pos, 0);
}

#[test]
fn read_from_position_incremental() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    let path_str = path.to_str().unwrap();

    // Write initial content
    fs::write(&path, "line1\n").unwrap();
    let (data, pos) = read_from_position(path_str, 0).unwrap();
    assert_eq!(data, "line1\n");
    assert_eq!(pos, 6);

    // Append more
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"line2\n").unwrap();

    let (data, pos) = read_from_position(path_str, pos).unwrap();
    assert_eq!(data, "line2\n");
    assert_eq!(pos, 12);
}

#[test]
fn read_from_position_nonexistent_file() {
    let result = read_from_position("/tmp/nonexistent_mcp_test_file_xyz.log", 0);
    assert!(result.is_err());
}

// ─── HTML Escaping ───────────────────────────────────────────────────────────

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[test]
fn html_escape_basic() {
    assert_eq!(
        html_escape("<script>alert('xss')</script>"),
        "&lt;script&gt;alert('xss')&lt;/script&gt;"
    );
}

#[test]
fn html_escape_ampersand() {
    assert_eq!(html_escape("foo & bar"), "foo &amp; bar");
}

#[test]
fn html_escape_mixed() {
    assert_eq!(
        html_escape("a < b && c > d"),
        "a &lt; b &amp;&amp; c &gt; d"
    );
}

#[test]
fn html_escape_no_special() {
    let input = "nothing to escape here";
    assert_eq!(html_escape(input), input);
}

#[test]
fn html_escape_empty() {
    assert_eq!(html_escape(""), "");
}

// ─── Line-Based File Reading (SSE stream logic) ──────────────────────────────

fn read_complete_lines(path: &str, lines_seen: u64) -> Result<Vec<(u64, String)>, String> {
    use std::io::Read;

    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    file.read_to_end(&mut data).map_err(|e| e.to_string())?;

    let content = String::from_utf8_lossy(&data);

    let complete = match content.rfind('\n') {
        Some(pos) => &content[..pos + 1],
        None => return Ok(Vec::new()),
    };

    let mut result = Vec::new();
    for (i, line) in complete.split_terminator('\n').enumerate() {
        let line_num = (i as u64) + 1;
        if line_num > lines_seen {
            let line = normalize_pty_output(line.trim_end_matches('\r'));
            result.push((line_num, line));
        }
    }

    Ok(result)
}

#[test]
fn read_complete_lines_from_start() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\n").unwrap();

    let lines = read_complete_lines(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], (1, "line1".to_string()));
    assert_eq!(lines[1], (2, "line2".to_string()));
    assert_eq!(lines[2], (3, "line3".to_string()));
}

#[test]
fn read_complete_lines_skips_seen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\n").unwrap();

    let lines = read_complete_lines(path.to_str().unwrap(), 2).unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], (3, "line3".to_string()));
}

#[test]
fn read_complete_lines_ignores_incomplete_last_line() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    // "partial" has no trailing newline, should be ignored
    fs::write(&path, "line1\nline2\npartial").unwrap();

    let lines = read_complete_lines(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], (1, "line1".to_string()));
    assert_eq!(lines[1], (2, "line2".to_string()));
}

#[test]
fn read_complete_lines_no_newline_at_all() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "incomplete data").unwrap();

    let lines = read_complete_lines(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(lines.len(), 0);
}

#[test]
fn read_complete_lines_strips_cr() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\r\nline2\r\n").unwrap();

    let lines = read_complete_lines(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], (1, "line1".to_string()));
    assert_eq!(lines[1], (2, "line2".to_string()));
}

#[test]
fn read_complete_lines_empty_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "").unwrap();

    let lines = read_complete_lines(path.to_str().unwrap(), 0).unwrap();
    assert_eq!(lines.len(), 0);
}

#[test]
fn read_complete_lines_all_seen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    fs::write(&path, "a\nb\nc\n").unwrap();

    let lines = read_complete_lines(path.to_str().unwrap(), 3).unwrap();
    assert_eq!(lines.len(), 0);
}

#[test]
fn read_complete_lines_incremental_append() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("test.log");
    let path_str = path.to_str().unwrap();

    // Initial write
    fs::write(&path, "first\n").unwrap();
    let lines = read_complete_lines(path_str, 0).unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], (1, "first".to_string()));

    // Append more
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"second\nthird\n").unwrap();

    let lines = read_complete_lines(path_str, 1).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], (2, "second".to_string()));
    assert_eq!(lines[1], (3, "third".to_string()));
}

// ─── Log Level Ordering ──────────────────────────────────────────────────────

fn level_value(level: &str) -> u8 {
    match level {
        "debug" => 0,
        "info" => 1,
        "notice" => 2,
        "warning" => 3,
        "error" => 4,
        "critical" => 5,
        "alert" => 6,
        "emergency" => 7,
        _ => 255,
    }
}

#[test]
fn level_ordering_debug_lowest() {
    assert!(level_value("debug") < level_value("info"));
    assert!(level_value("debug") < level_value("emergency"));
}

#[test]
fn level_ordering_emergency_highest() {
    assert!(level_value("emergency") > level_value("alert"));
    assert!(level_value("emergency") > level_value("debug"));
}

#[test]
fn level_ordering_monotonic() {
    let levels = [
        "debug",
        "info",
        "notice",
        "warning",
        "error",
        "critical",
        "alert",
        "emergency",
    ];
    for window in levels.windows(2) {
        assert!(
            level_value(window[0]) < level_value(window[1]),
            "{} should be less than {}",
            window[0],
            window[1]
        );
    }
}

#[test]
fn level_filtering_respects_minimum() {
    // Simulate: min_level = warning, check which messages get through
    let min = level_value("warning");
    assert!(level_value("debug") < min, "debug should be filtered out");
    assert!(level_value("info") < min, "info should be filtered out");
    assert!(level_value("notice") < min, "notice should be filtered out");
    assert!(level_value("warning") >= min, "warning should pass");
    assert!(level_value("error") >= min, "error should pass");
    assert!(level_value("critical") >= min, "critical should pass");
}

// ─── Search/Pattern Matching Logic ───────────────────────────────────────────

/// Replicate the search_output pattern matching logic
fn search_output(content: &str, pattern: &str, max_results: usize) -> Vec<String> {
    let content = normalize_pty_output(content);
    let bytes = strip_ansi_escapes::strip(content.as_bytes());
    let content = String::from_utf8_lossy(&bytes).to_string();

    let mut matches: Vec<String> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.contains(pattern) {
            matches.push(format!("{}: {}", i + 1, line));
            if matches.len() >= max_results {
                break;
            }
        }
    }
    matches
}

#[test]
fn search_finds_matching_lines() {
    let content = "line1 OK\nline2 ERROR fail\nline3 OK\nline4 ERROR again\n";
    let matches = search_output(content, "ERROR", 20);
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0], "2: line2 ERROR fail");
    assert_eq!(matches[1], "4: line4 ERROR again");
}

#[test]
fn search_respects_max_results() {
    let content = "error1\nerror2\nerror3\nerror4\nerror5\n";
    let matches = search_output(content, "error", 3);
    assert_eq!(matches.len(), 3);
    assert_eq!(matches[2], "3: error3");
}

#[test]
fn search_no_matches() {
    let content = "everything is fine\nall good\n";
    let matches = search_output(content, "ERROR", 20);
    assert!(matches.is_empty());
}

#[test]
fn search_case_sensitive() {
    let content = "Error on line 1\nERROR on line 2\nerror on line 3\n";
    let matches = search_output(content, "ERROR", 20);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0], "2: ERROR on line 2");
}

#[test]
fn search_strips_ansi_before_matching() {
    let content = "\x1b[31mERROR\x1b[0m: something failed\nnormal line\n";
    let matches = search_output(content, "ERROR", 20);
    assert_eq!(matches.len(), 1);
    assert!(matches[0].contains("ERROR: something failed"));
}

#[test]
fn search_normalizes_pty_output() {
    // Progress line overwritten, final value contains the match
    let content = "building\r50%\r100% DONE\nnext line\n";
    let matches = search_output(content, "DONE", 20);
    assert_eq!(matches.len(), 1);
    assert!(matches[0].contains("DONE"));
}

#[test]
fn search_empty_content() {
    let matches = search_output("", "anything", 20);
    assert!(matches.is_empty());
}

#[test]
fn search_empty_pattern_matches_all() {
    let content = "line1\nline2\nline3\n";
    let matches = search_output(content, "", 20);
    assert_eq!(matches.len(), 3);
}

#[test]
fn search_line_numbers_are_1_indexed() {
    let content = "first\nsecond\nthird\n";
    let matches = search_output(content, "second", 20);
    assert_eq!(matches[0], "2: second");
}

// ─── Resource URI Parsing ────────────────────────────────────────────────────

/// Replicate the URI parsing from read_resource
fn parse_resource_uri(uri: &str) -> Result<(&str, &str), String> {
    let rest = uri
        .strip_prefix("session://")
        .ok_or_else(|| format!("Unknown resource URI: {uri}"))?;
    let (id, stream) = rest
        .rsplit_once('/')
        .ok_or_else(|| format!("Invalid resource URI: {uri}"))?;
    Ok((id, stream))
}

#[test]
fn parse_uri_stdout() {
    let (id, stream) = parse_resource_uri("session://1/stdout").unwrap();
    assert_eq!(id, "1");
    assert_eq!(stream, "stdout");
}

#[test]
fn parse_uri_stderr() {
    let (id, stream) = parse_resource_uri("session://42/stderr").unwrap();
    assert_eq!(id, "42");
    assert_eq!(stream, "stderr");
}

#[test]
fn parse_uri_invalid_prefix() {
    let result = parse_resource_uri("file:///tmp/foo");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Unknown resource URI"));
}

#[test]
fn parse_uri_no_slash_in_rest() {
    let result = parse_resource_uri("session://nostreampart");
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Invalid resource URI"));
}

#[test]
fn parse_uri_empty_session_id() {
    // "session:///stdout" → id="" stream="stdout"
    let (id, stream) = parse_resource_uri("session:///stdout").unwrap();
    assert_eq!(id, "");
    assert_eq!(stream, "stdout");
}

#[test]
fn parse_uri_multi_segment_id() {
    // Unlikely but tests the rsplit_once behavior: "session://a/b/stdout"
    // rsplit_once('/') gives ("a/b", "stdout")
    let (id, stream) = parse_resource_uri("session://a/b/stdout").unwrap();
    assert_eq!(id, "a/b");
    assert_eq!(stream, "stdout");
}

// ─── Stderr/Stdout Merging Behavior ─────────────────────────────────────────

/// Test the run_command output formatting logic (stderr merged with label)
fn format_run_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
    let stdout_clean = {
        let bytes = strip_ansi_escapes::strip(stdout.as_bytes());
        String::from_utf8_lossy(&bytes).to_string()
    };
    let stderr_clean = {
        let bytes = strip_ansi_escapes::strip(stderr.as_bytes());
        String::from_utf8_lossy(&bytes).to_string()
    };

    let mut output = stdout_clean;
    if !stderr_clean.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[stderr]\n");
        output.push_str(&stderr_clean);
    }
    output.push_str(&format!("\n[exit code: {}]\n", exit_code));
    output
}

#[test]
fn format_output_stdout_only() {
    let result = format_run_output("hello world\n", "", 0);
    assert_eq!(result, "hello world\n\n[exit code: 0]\n");
    assert!(!result.contains("[stderr]"));
}

#[test]
fn format_output_stderr_only() {
    let result = format_run_output("", "error msg\n", 1);
    assert!(result.contains("[stderr]"));
    assert!(result.contains("error msg"));
    assert!(result.contains("[exit code: 1]"));
}

#[test]
fn format_output_both_streams() {
    let result = format_run_output("output line\n", "warning\n", 0);
    assert!(result.contains("output line"));
    assert!(result.contains("[stderr]"));
    assert!(result.contains("warning"));
    assert!(result.contains("[exit code: 0]"));
    // stderr should come after stdout
    let stderr_pos = result.find("[stderr]").unwrap();
    let stdout_pos = result.find("output line").unwrap();
    assert!(stderr_pos > stdout_pos);
}

#[test]
fn format_output_strips_ansi_from_both() {
    let result = format_run_output(
        "\x1b[32mSUCCESS\x1b[0m\n",
        "\x1b[31mWARN\x1b[0m: something\n",
        0,
    );
    assert!(result.contains("SUCCESS"));
    assert!(!result.contains("\x1b["));
    assert!(result.contains("WARN: something"));
}

#[test]
fn format_output_negative_exit_code() {
    let result = format_run_output("", "", -1);
    assert!(result.contains("[exit code: -1]"));
}

#[test]
fn format_output_signal_exit_code() {
    // 128 + signal number (e.g., SIGTERM=15 → 143)
    let result = format_run_output("", "", 143);
    assert!(result.contains("[exit code: 143]"));
}

// ─── ANSI to HTML Conversion ─────────────────────────────────────────────────

#[test]
fn ansi_to_html_basic_color() {
    let input = "\x1b[31mred text\x1b[0m";
    let result = ansi_to_html::convert(input).unwrap();
    // Should contain some HTML span/style for red
    assert!(result.contains("red text"));
    assert!(result.contains("<") || result.contains("style"));
}

#[test]
fn ansi_to_html_plain_text_passthrough() {
    let input = "no escapes here";
    let result = ansi_to_html::convert(input).unwrap();
    assert_eq!(result, "no escapes here");
}

#[test]
fn ansi_to_html_empty_string() {
    let result = ansi_to_html::convert("").unwrap();
    assert_eq!(result, "");
}

// ─── Exit Code From Status (Unix signals) ────────────────────────────────────

#[test]
fn exit_code_normal_exit() {
    use std::process::Command;
    let status = Command::new("true").status().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn exit_code_nonzero_exit() {
    use std::process::Command;
    let status = Command::new("false").status().unwrap();
    assert_eq!(status.code(), Some(1));
}

#[test]
fn exit_code_custom_exit() {
    use std::process::Command;
    let status = Command::new("sh").args(["-c", "exit 42"]).status().unwrap();
    assert_eq!(status.code(), Some(42));
}

#[cfg(unix)]
#[test]
fn exit_code_signal_kill() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    // Start a sleep and kill it
    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = child.id();

    // Send SIGKILL
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    let status = child.wait().unwrap();

    // Should have no exit code but signal = 9
    assert!(status.code().is_none());
    assert_eq!(status.signal(), Some(9));

    // Our logic: 128 + signal
    let exit_code = status.code().or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|s| 128 + s)
    });
    assert_eq!(exit_code, Some(137)); // 128 + 9 = 137
}
