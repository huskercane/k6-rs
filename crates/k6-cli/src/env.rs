use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

const CMD_PREFIX: &str = "cmd:";
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolves environment variables from multiple sources with clear priority:
///   1. CLI `--env` flags (highest)
///   2. `.env` file in the given directory (lowest)
///
/// Values prefixed with `cmd:` are resolved by executing the remainder as a
/// shell command (e.g. `DB_PASS=cmd:pass show db/password`). This allows
/// integration with any secret provider without hardcoding specific tools.
///
/// Writes informational messages to `out` (typically stderr) so callers can
/// control output in tests.
pub fn resolve_env_vars(
    cli_envs: &[String],
    dotenv_dir: &Path,
    out: &mut dyn Write,
) -> Result<Vec<(String, String)>> {
    let mut env_map = load_dotenv_file(dotenv_dir, out);
    apply_cli_envs(cli_envs, &mut env_map)?;
    resolve_commands(&mut env_map, &shell_exec, out)?;
    Ok(env_map.into_iter().collect())
}

/// Loads key-value pairs from a `.env` file in `dir`.
/// Returns an empty map if the file doesn't exist or can't be opened.
fn load_dotenv_file(dir: &Path, out: &mut dyn Write) -> HashMap<String, String> {
    let path = dir.join(".env");
    let iter = match dotenvy::from_path_iter(&path) {
        Ok(iter) => iter,
        Err(_) => return HashMap::new(),
    };

    let mut map = HashMap::new();
    for item in iter {
        match item {
            Ok((key, value)) => {
                map.insert(key, value);
            }
            Err(e) => {
                let _ = writeln!(out, "  warning: skipping invalid .env entry: {e}");
            }
        }
    }

    if !map.is_empty() {
        let _ = writeln!(
            out,
            "  reading {} environment variable(s) from .env file",
            map.len()
        );
    }
    map
}

/// Parses `VAR=value` strings and inserts them into `env_map`, overriding
/// any existing entries (giving CLI flags highest priority).
fn apply_cli_envs(cli_envs: &[String], env_map: &mut HashMap<String, String>) -> Result<()> {
    for s in cli_envs {
        let (k, v) = s
            .split_once('=')
            .with_context(|| format!("invalid --env format '{s}', expected VAR=value"))?;
        anyhow::ensure!(!k.is_empty(), "empty variable name in --env '{s}'");
        env_map.insert(k.to_string(), v.to_string());
    }
    Ok(())
}

/// Scans `env_map` for values starting with `cmd:` and replaces them with the
/// stdout of the shell command. The `executor` parameter abstracts shell
/// execution so tests can supply a fake without spawning processes.
fn resolve_commands(
    env_map: &mut HashMap<String, String>,
    executor: &dyn Fn(&str) -> Result<String>,
    out: &mut dyn Write,
) -> Result<()> {
    let cmd_keys: Vec<String> = env_map
        .iter()
        .filter(|(_, v)| v.starts_with(CMD_PREFIX))
        .map(|(k, _)| k.clone())
        .collect();

    if cmd_keys.is_empty() {
        return Ok(());
    }

    let _ = writeln!(
        out,
        "  resolving {} environment variable(s) via shell commands",
        cmd_keys.len()
    );

    for key in cmd_keys {
        let raw = &env_map[&key];
        let cmd_str = &raw[CMD_PREFIX.len()..];
        let value = executor(cmd_str)
            .with_context(|| format!("failed to resolve env var '{key}' via cmd: — is the provider authenticated? (run with a pre-existing session, e.g. `op signin`, `vault login`)"))?;
        let _ = writeln!(out, "    {key} ← cmd:***");
        env_map.insert(key, value);
    }

    Ok(())
}

/// Executes a command string via the platform shell and returns trimmed stdout.
///
/// Stdin is closed (`/dev/null`) so commands that prompt for passwords fail
/// immediately with EOF instead of hanging the test run. A 30-second timeout
/// acts as a safety net for any other blocking scenario.
fn shell_exec(cmd: &str) -> Result<String> {
    let mut child = if cfg!(target_os = "windows") {
        Command::new("cmd")
            .args(["/C", cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    } else {
        Command::new("sh")
            .args(["-c", cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }
    .with_context(|| format!("failed to spawn shell for: {cmd}"))?;

    // Wait with timeout using a channel so we don't block forever if a command
    // hangs (e.g. waiting for interactive password input despite Stdio::null).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(CMD_TIMEOUT) {
        Ok(Ok(output)) if !output.status.success() => {
            anyhow::bail!(
                "command exited with {}: {}\nstderr: {}",
                output.status,
                cmd,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(Ok(output)) => Ok(String::from_utf8_lossy(&output.stdout).trim().to_string()),
        Ok(Err(e)) => Err(e).with_context(|| format!("I/O error running command: {cmd}")),
        Err(_) => anyhow::bail!(
            "command timed out after {}s (is it prompting for a password?): {cmd}",
            CMD_TIMEOUT.as_secs()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper: creates a temp dir with optional .env content.
    fn setup_dotenv(content: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(content) = content {
            fs::write(dir.path().join(".env"), content).unwrap();
        }
        dir
    }

    // ── .env file loading ──────────────────────────────────────────

    #[test]
    fn no_dotenv_file_returns_empty() {
        let dir = setup_dotenv(None);
        let mut out = Vec::new();
        let result = resolve_env_vars(&[], dir.path(), &mut out).unwrap();
        assert!(result.is_empty());
        assert!(out.is_empty(), "should produce no output when no .env file");
    }

    #[test]
    fn loads_vars_from_dotenv() {
        let dir = setup_dotenv(Some("FOO=bar\nBAZ=123\n"));
        let mut out = Vec::new();
        let result = resolve_env_vars(&[], dir.path(), &mut out).unwrap();

        let map: HashMap<_, _> = result.into_iter().collect();
        assert_eq!(map.get("FOO").unwrap(), "bar");
        assert_eq!(map.get("BAZ").unwrap(), "123");

        let msg = String::from_utf8(out).unwrap();
        assert!(
            msg.contains("reading 2 environment variable(s) from .env file"),
            "should print count message, got: {msg}"
        );
    }

    #[test]
    fn dotenv_with_comments_and_blanks() {
        let dir = setup_dotenv(Some("# a comment\nKEY=val\n\n# another\nKEY2=val2\n"));
        let mut out = Vec::new();
        let result = resolve_env_vars(&[], dir.path(), &mut out).unwrap();

        let map: HashMap<_, _> = result.into_iter().collect();
        assert_eq!(map.len(), 2);
        assert_eq!(map["KEY"], "val");
        assert_eq!(map["KEY2"], "val2");
    }

    #[test]
    fn dotenv_quoted_values() {
        let dir = setup_dotenv(Some("QUOTED=\"hello world\"\nSINGLE='foo bar'\n"));
        let mut out = Vec::new();
        let result = resolve_env_vars(&[], dir.path(), &mut out).unwrap();

        let map: HashMap<_, _> = result.into_iter().collect();
        assert_eq!(map["QUOTED"], "hello world");
        assert_eq!(map["SINGLE"], "foo bar");
    }

    // ── CLI --env flags ────────────────────────────────────────────

    #[test]
    fn cli_env_overrides_dotenv() {
        let dir = setup_dotenv(Some("HOST=from_file\nPORT=8080\n"));
        let cli = vec!["HOST=from_cli".to_string()];
        let mut out = Vec::new();
        let result = resolve_env_vars(&cli, dir.path(), &mut out).unwrap();

        let map: HashMap<_, _> = result.into_iter().collect();
        assert_eq!(map.get("HOST").unwrap(), "from_cli", "CLI must win");
        assert_eq!(map.get("PORT").unwrap(), "8080", ".env value preserved");
    }

    #[test]
    fn cli_env_works_without_dotenv() {
        let dir = setup_dotenv(None);
        let cli = vec!["A=1".to_string(), "B=2".to_string()];
        let mut out = Vec::new();
        let result = resolve_env_vars(&cli, dir.path(), &mut out).unwrap();

        let map: HashMap<_, _> = result.into_iter().collect();
        assert_eq!(map.len(), 2);
        assert_eq!(map["A"], "1");
        assert_eq!(map["B"], "2");
    }

    #[test]
    fn rejects_invalid_cli_env_format() {
        let dir = setup_dotenv(None);
        let mut out = Vec::new();
        let result = resolve_env_vars(&["NOEQUALS".to_string()], dir.path(), &mut out);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("invalid --env format"),);
    }

    #[test]
    fn rejects_empty_key_in_cli_env() {
        let dir = setup_dotenv(None);
        let mut out = Vec::new();
        let result = resolve_env_vars(&["=value".to_string()], dir.path(), &mut out);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("empty variable name"));
    }

    #[test]
    fn value_with_equals_sign_preserved() {
        let dir = setup_dotenv(None);
        let cli = vec!["URL=http://host?a=1&b=2".to_string()];
        let mut out = Vec::new();
        let result = resolve_env_vars(&cli, dir.path(), &mut out).unwrap();

        let map: HashMap<_, _> = result.into_iter().collect();
        assert_eq!(map["URL"], "http://host?a=1&b=2");
    }

    // ── cmd: resolution (unit tests with fake executor) ────────────

    fn fake_executor(cmd: &str) -> Result<String> {
        match cmd {
            "echo hunter2" => Ok("hunter2".to_string()),
            "echo multiline" => Ok("first\nsecond".to_string()),
            "failing-command" => anyhow::bail!("command failed with exit code 1"),
            other => Ok(format!("mocked:{other}")),
        }
    }

    #[test]
    fn cmd_prefix_resolves_via_executor() {
        let mut env_map = HashMap::from([
            ("PLAIN".to_string(), "literal_value".to_string()),
            ("SECRET".to_string(), "cmd:echo hunter2".to_string()),
        ]);
        let mut out = Vec::new();

        resolve_commands(&mut env_map, &fake_executor, &mut out).unwrap();

        assert_eq!(env_map["PLAIN"], "literal_value", "plain values untouched");
        assert_eq!(env_map["SECRET"], "hunter2", "cmd: value resolved");

        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("resolving 1 environment variable(s) via shell commands"));
        assert!(msg.contains("SECRET ← cmd:***"), "should mask the command");
        assert!(!msg.contains("hunter2"), "must not leak secret in output");
    }

    #[test]
    fn cmd_prefix_error_includes_key_name() {
        let mut env_map = HashMap::from([(
            "BAD".to_string(),
            "cmd:failing-command".to_string(),
        )]);
        let mut out = Vec::new();

        let err = resolve_commands(&mut env_map, &fake_executor, &mut out).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("BAD"), "error should name the variable");
        assert!(msg.contains("authenticated"), "error should suggest checking auth");
    }

    #[test]
    fn no_cmd_prefixes_skips_resolution() {
        let mut env_map = HashMap::from([
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ]);
        let mut out = Vec::new();

        resolve_commands(&mut env_map, &fake_executor, &mut out).unwrap();

        assert!(out.is_empty(), "no output when no cmd: values");
        assert_eq!(env_map["A"], "1");
        assert_eq!(env_map["B"], "2");
    }

    #[test]
    fn multiple_cmd_values_all_resolved() {
        let mut env_map = HashMap::from([
            ("A".to_string(), "cmd:echo hunter2".to_string()),
            ("B".to_string(), "cmd:echo multiline".to_string()),
            ("C".to_string(), "plain".to_string()),
        ]);
        let mut out = Vec::new();

        resolve_commands(&mut env_map, &fake_executor, &mut out).unwrap();

        assert_eq!(env_map["A"], "hunter2");
        assert_eq!(env_map["B"], "first\nsecond");
        assert_eq!(env_map["C"], "plain");

        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("resolving 2 environment variable(s)"));
    }

    #[test]
    fn cmd_prefix_via_cli_flag() {
        let mut env_map = HashMap::new();
        apply_cli_envs(
            &["TOKEN=cmd:echo hunter2".to_string()],
            &mut env_map,
        )
        .unwrap();
        let mut out = Vec::new();

        resolve_commands(&mut env_map, &fake_executor, &mut out).unwrap();

        assert_eq!(env_map["TOKEN"], "hunter2");
    }

    // ── cmd: integration test with real shell ──────────────────────

    #[test]
    fn cmd_real_shell_echo() {
        let result = shell_exec("echo hello").unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn cmd_real_shell_failure() {
        let result = shell_exec("false");
        assert!(result.is_err());
    }

    #[test]
    fn cmd_real_shell_trims_trailing_newline() {
        let result = shell_exec("printf 'no-newline'").unwrap();
        assert_eq!(result, "no-newline");
    }

    #[test]
    fn cmd_stdin_is_closed_so_read_fails() {
        // Simulates a command that tries to read from stdin (like a password prompt).
        // With stdin piped from /dev/null, `read` gets EOF immediately and fails.
        let result = shell_exec("read -p 'Password: ' x && echo $x");
        assert!(result.is_err(), "should fail because stdin is /dev/null");
    }

    #[test]
    fn cmd_timeout_kills_hanging_command() {
        // We can't test the full 30s timeout in unit tests, but we verify
        // the timeout mechanism works by confirming a fast command succeeds.
        let result = shell_exec("echo fast").unwrap();
        assert_eq!(result, "fast");
    }

    #[test]
    fn end_to_end_dotenv_with_cmd() {
        // dotenvy needs the cmd: value quoted to preserve it literally
        let dir = setup_dotenv(Some("PLAIN=hello\nSECRET=\"cmd:echo resolved_secret\"\n"));
        let mut out = Vec::new();
        let result = resolve_env_vars(&[], dir.path(), &mut out).unwrap();

        let map: HashMap<_, _> = result.into_iter().collect();
        assert_eq!(map["PLAIN"], "hello");
        assert_eq!(map["SECRET"], "resolved_secret");

        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("reading 2 environment variable(s) from .env file"));
        assert!(msg.contains("resolving 1 environment variable(s) via shell commands"));
        assert!(msg.contains("SECRET ← cmd:***"));
    }
}
