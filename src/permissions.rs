//! External tools execute under an explicit Codex sandbox state, never user config defaults.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionPolicy {
    pub mode: String,
    pub writable_roots: Vec<PathBuf>,
    pub network_access: bool,
    pub exclude_tmpdir_env_var: bool,
    pub exclude_slash_tmp: bool,
}

impl PermissionPolicy {
    pub fn from_rpc(params: &Value, cwd: &Path) -> Result<Self> {
        let explicit = params.get("sandboxPolicy").filter(|v| !v.is_null());
        let legacy = params.get("sandbox").filter(|v| !v.is_null());
        ensure!(
            explicit.is_none() || legacy.is_none(),
            "specify sandboxPolicy or sandbox, not both"
        );
        let policy = explicit.or(legacy);
        let raw = match policy {
            None => "workspaceWrite",
            Some(v) => v
                .as_str()
                .or_else(|| v.get("type").and_then(Value::as_str))
                .context("sandbox policy requires a valid type")?,
        };
        let mode = match raw {
            "workspaceWrite" | "workspace-write" => "workspaceWrite",
            "readOnly" | "read-only" => "readOnly",
            "dangerFullAccess" | "danger-full-access" => "dangerFullAccess",
            _ => bail!("unsupported sandbox policy {raw}"),
        }
        .to_owned();
        if let Some(object) = policy.and_then(Value::as_object) {
            for field in object.keys() {
                let supported = match mode.as_str() {
                    "workspaceWrite" => matches!(
                        field.as_str(),
                        "type"
                            | "writableRoots"
                            | "networkAccess"
                            | "excludeTmpdirEnvVar"
                            | "excludeSlashTmp"
                    ),
                    "readOnly" => matches!(field.as_str(), "type" | "networkAccess"),
                    _ => field == "type",
                };
                ensure!(supported, "unsupported {mode} sandbox field {field}");
            }
        }
        if mode != "workspaceWrite"
            && let Some(v) = params.get("additionalDirectories").filter(|v| !v.is_null())
        {
            ensure!(
                v.as_array().is_some_and(Vec::is_empty),
                "additionalDirectories requires workspaceWrite"
            );
        }
        let mut roots = vec![];
        if mode == "workspaceWrite" {
            roots.push(cwd.canonicalize().context("workspace directory")?);
            for key in ["writableRoots", "additionalDirectories"] {
                let values = if key == "writableRoots" {
                    policy.and_then(|p| p.get(key))
                } else {
                    params.get(key)
                };
                if let Some(v) = values {
                    for root in v.as_array().context("writable roots must be an array")? {
                        let p =
                            PathBuf::from(root.as_str().context("writable root must be a string")?);
                        ensure!(p.is_absolute(), "writable root must be absolute");
                        let p = p.canonicalize().context("writable root must exist")?;
                        ensure!(p.is_dir(), "writable root must be a directory");
                        if !roots.contains(&p) {
                            roots.push(p);
                        }
                    }
                }
            }
        }
        let flag = |key: &str, default| -> Result<bool> {
            match policy.and_then(|p| p.get(key)) {
                None => Ok(default),
                Some(v) => v.as_bool().context("sandbox flags must be booleans"),
            }
        };
        Ok(Self {
            network_access: if mode == "dangerFullAccess" {
                true
            } else {
                flag("networkAccess", false)?
            },
            mode,
            writable_roots: roots,
            exclude_tmpdir_env_var: flag("excludeTmpdirEnvVar", true)?,
            exclude_slash_tmp: flag("excludeSlashTmp", true)?,
        })
    }
    pub fn effective(&self) -> Value {
        match self.mode.as_str() {
            "dangerFullAccess" => json!({"type":"dangerFullAccess"}),
            "readOnly" => json!({"type":"readOnly","networkAccess":self.network_access}),
            _ => {
                json!({"type":"workspaceWrite","writableRoots":self.writable_roots,"networkAccess":self.network_access,"excludeTmpdirEnvVar":self.exclude_tmpdir_env_var,"excludeSlashTmp":self.exclude_slash_tmp})
            }
        }
    }
    pub fn sandbox_state(&self, cwd: &Path) -> Result<Value> {
        let profile = if self.mode == "dangerFullAccess" {
            json!({"type":"disabled"})
        } else {
            let mut entries =
                vec![json!({"path":{"type":"special","value":{"kind":"root"}},"access":"read"})];
            if self.mode == "workspaceWrite" {
                for root in &self.writable_roots {
                    entries.push(json!({"path":{"type":"path","path":root},"access":"write"}));
                    for protected in [".git", ".agents", ".codex"] {
                        entries.push(json!({"path":{"type":"path","path":root.join(protected)},"access":"read"}));
                    }
                }
                if !self.exclude_slash_tmp {
                    entries.push(json!({"path":{"type":"special","value":{"kind":"slash_tmp"}},"access":"write"}));
                }
                if !self.exclude_tmpdir_env_var {
                    entries.push(json!({"path":{"type":"special","value":{"kind":"tmpdir"}},"access":"write"}));
                }
            }
            json!({"type":"managed","file_system":{"type":"restricted","entries":entries},"network":if self.network_access {"enabled"}else{"restricted"}})
        };
        let uri = reqwest::Url::from_file_path(cwd)
            .map_err(|_| anyhow::anyhow!("invalid sandbox cwd"))?;
        Ok(
            json!({"permissionProfile":profile,"sandboxCwd":uri.to_string(),"useLegacyLandlock":false,"useMxc":false}),
        )
    }
}

pub fn requests_bypass(args: &Value) -> bool {
    args.get("BypassSandbox").and_then(Value::as_bool) == Some(true)
        || args.get("sandbox_permissions").and_then(Value::as_str) == Some("require_escalated")
}

pub(crate) fn shell_quote(value: &str) -> Result<String> {
    ensure!(!value.contains('\0'), "shell arguments cannot contain NUL");
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

/// Preserve the native command tool while replacing its execution sandbox.
/// The caller must approve original bypass requests before executing this line,
/// retain the original Cwd, and reject uncorrelated policy callbacks.
pub fn native_command_line(policy: &PermissionPolicy, cwd: &Path, args: &Value) -> Result<String> {
    let line = args
        .get("CommandLine")
        .and_then(Value::as_str)
        .context("CommandLine is required")?;
    ensure!(!line.contains('\0'), "CommandLine cannot contain NUL");
    let directory = args
        .get("Cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .unwrap_or(cwd);
    ensure!(
        directory.is_absolute() && directory.is_dir(),
        "command cwd must be an existing absolute directory"
    );
    if policy.mode == "dangerFullAccess" {
        return Ok(line.to_owned());
    }
    ensure!(
        args.get("RunPersistent").and_then(Value::as_bool) != Some(true)
            && args
                .get("RequestedTerminalID")
                .is_none_or(|v| v.as_str() == Some("") || v.is_null()),
        "persistent terminals are unsupported under a restricted policy"
    );
    let mut program = vec![];
    let mut helper_path = None;
    if !requests_bypass(args) {
        let binary = crate::codex::binary("AGY_CODEX_SANDBOX_BINARY")?;
        let helper = crate::codex::command(&binary)?;
        helper_path = helper
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "PATH")
            .and_then(|(_, value)| value.map(std::ffi::OsStr::to_owned));
        program.extend([
            binary
                .to_str()
                .context("Codex executable path must be UTF-8")?
                .to_owned(),
            "sandbox".into(),
            "--sandbox-state-json".into(),
            policy.sandbox_state(cwd)?.to_string(),
            "--".into(),
        ]);
    }
    program.extend([
        "/bin/bash".into(),
        "--noprofile".into(),
        "--norc".into(),
        "-c".into(),
        line.to_owned(),
    ]);
    // Clear the child environment just as execute() does. The native parent
    // shell must separately have startup hooks removed before it is launched.
    let mut words = vec!["/usr/bin/env".into(), "-i".into()];
    for key in [
        "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR",
    ] {
        let value = if key == "PATH" {
            helper_path.clone().or_else(|| std::env::var_os(key))
        } else {
            std::env::var_os(key)
        };
        if let Some(value) = value {
            words.push(format!(
                "{key}={}",
                value
                    .to_str()
                    .context("command environment must be UTF-8")?
            ));
        }
    }
    words.push("PAGER=cat".into());
    words.extend(program);
    words
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Result<Vec<_>>>()
        .map(|words| words.join(" "))
}

/// Confirm sandbox helper compatibility before accepting a scoped thread.
pub async fn verify(policy: &PermissionPolicy, cwd: &Path) -> Result<()> {
    if policy.mode == "dangerFullAccess" {
        return Ok(());
    }
    let result = execute(
        policy,
        cwd,
        "run_command",
        &json!({"CommandLine":"/bin/true","TimeoutSeconds":10}),
        false,
    )
    .await?;
    ensure!(
        result["success"] == true,
        "sandbox verification failed: {}",
        result["error"]
    );
    Ok(())
}

pub fn tool_schemas() -> Vec<Value> {
    let bypass = json!({"type":"boolean","description":"Request explicit approval to execute outside the sandbox. Never implies approval."});
    let mut out = vec![];
    for (name, description, properties, required) in [
        (
            "run_command",
            "Run a shell command under the active filesystem/network policy. Commands complete synchronously; persistent terminals and daemon mode are unsupported.",
            json!({"CommandLine":{"type":"string"},"Cwd":{"type":"string"},"TimeoutSeconds":{"type":"integer"},"BypassSandbox":bypass.clone()}),
            vec!["CommandLine"],
        ),
        (
            "write_to_file",
            "Write a UTF-8 file under the active filesystem policy. Overwrite defaults to false.",
            json!({"TargetFile":{"type":"string"},"CodeContent":{"type":"string"},"Overwrite":{"type":"boolean"},"BypassSandbox":bypass.clone()}),
            vec!["TargetFile", "CodeContent"],
        ),
        (
            "replace_file_content",
            "Replace one exact occurrence in a UTF-8 file under the active filesystem policy. Ambiguous or missing matches fail.",
            json!({"TargetFile":{"type":"string"},"TargetContent":{"type":"string"},"ReplacementContent":{"type":"string"},"BypassSandbox":bypass}),
            vec!["TargetFile", "TargetContent", "ReplacementContent"],
        ),
    ] {
        out.push(json!({"name":name,"description":description,"parametersJsonSchema":json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}).to_string()}));
    }
    out
}

/// File tools still require the external executor; commands use the native tool.
pub fn file_tool_schemas() -> Vec<Value> {
    tool_schemas()
        .into_iter()
        .filter(|tool| tool["name"] != "run_command")
        .collect()
}

async fn capture<R: AsyncRead + Unpin>(mut reader: R) -> std::io::Result<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut b).await?;
        if n == 0 {
            break;
        }
        let keep = n.min(1_048_576usize.saturating_sub(out.len()));
        out.extend_from_slice(&b[..keep]);
        truncated |= keep < n;
    }
    let mut text = String::from_utf8_lossy(&out).into_owned();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    Ok(text)
}

struct OutputTask(tokio::task::JoinHandle<std::io::Result<String>>);
impl OutputTask {
    async fn finish(&mut self) -> Result<String> {
        match tokio::time::timeout(std::time::Duration::from_secs(2), &mut self.0).await {
            Ok(result) => Ok(result??),
            Err(_) => {
                self.0.abort();
                Ok("[output pipe remained open after process termination]".into())
            }
        }
    }
}
impl Drop for OutputTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The child owns a fresh process group. Dropping an interrupted execute future kills it.
struct ProcessGroup(Option<u32>);
impl ProcessGroup {
    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.take() {
            unsafe extern "C" {
                fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
            }
            // SAFETY: positive child PID is allocated by spawn with process_group(0).
            unsafe {
                kill(-(pid as std::ffi::c_int), 9);
            }
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub async fn execute(
    policy: &PermissionPolicy,
    cwd: &Path,
    tool: &str,
    args: &Value,
    approved_bypass: bool,
) -> Result<Value> {
    ensure!(
        !requests_bypass(args) || approved_bypass,
        "sandbox bypass requires explicit approval"
    );
    ensure!(
        matches!(
            tool,
            "run_command" | "write_to_file" | "replace_file_content"
        ),
        "unsupported external tool"
    );
    let directory = if tool == "run_command" {
        args.get("Cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.to_owned())
    } else {
        cwd.to_owned()
    };
    ensure!(
        directory.is_absolute() && directory.is_dir(),
        "command cwd must be an existing absolute directory"
    );
    for flag in ["IsDaemon", "RunPersistent"] {
        ensure!(
            args.get(flag).and_then(Value::as_bool) != Some(true),
            "daemon and persistent execution are unsupported"
        );
    }
    let mut program = vec![];
    if tool == "run_command" {
        program.extend([
            "/bin/bash".to_owned(),
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            args.get("CommandLine")
                .and_then(Value::as_str)
                .context("CommandLine is required")?
                .to_owned(),
        ]);
    } else {
        program.extend([
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "--permission-tool-helper".into(),
            tool.to_owned(),
        ]);
    }
    let bypass = approved_bypass && requests_bypass(args);
    let mut cmd = if bypass || policy.mode == "dangerFullAccess" {
        let mut c = Command::new(&program[0]);
        c.args(&program[1..]);
        c
    } else {
        let helper = crate::codex::binary("AGY_CODEX_SANDBOX_BINARY")?;
        let mut c = crate::codex::command(&helper)?;
        c.args([
            "sandbox",
            "--sandbox-state-json",
            &policy.sandbox_state(cwd)?.to_string(),
            "--",
        ]);
        c.args(&program);
        c
    };
    let helper_path = cmd
        .as_std()
        .get_envs()
        .find(|(k, _)| *k == "PATH")
        .and_then(|(_, v)| v.map(std::ffi::OsStr::to_owned));
    cmd.current_dir(&directory).env_clear();
    for key in [
        "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "TMPDIR",
    ] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    if let Some(path) = helper_path {
        cmd.env("PATH", path);
    }
    cmd.env("PAGER", "cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .context("sandbox/tool helper unavailable; refusing unsandboxed fallback")?;
    let mut process_group = ProcessGroup(child.id());
    let mut stdin = child.stdin.take().context("tool stdin")?;
    if tool != "run_command" {
        stdin.write_all(&serde_json::to_vec(args)?).await?;
    }
    drop(stdin);
    let mut stdout = OutputTask(tokio::spawn(capture(
        child.stdout.take().context("stdout")?,
    )));
    let mut stderr = OutputTask(tokio::spawn(capture(
        child.stderr.take().context("stderr")?,
    )));
    let seconds = args
        .get("TimeoutSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(120)
        .clamp(1, 600);
    let status = tokio::time::timeout(std::time::Duration::from_secs(seconds), child.wait()).await;
    // Bwrap also owns the sandbox PID namespace. Explicit full access cannot
    // guarantee cleanup of programs that deliberately detach into a new session.
    process_group.terminate();
    let timed_out = status.is_err();
    if timed_out {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let output = stdout.finish().await?;
    let error = stderr.finish().await?;
    let code = match status {
        Ok(s) => s?.code().unwrap_or(-1),
        Err(_) => -1,
    };
    let mut result = json!({"success":code==0&&!timed_out,"exitCode":code,"output":output,"error":error,"timedOut":timed_out,"sandboxBypassed":bypass||policy.mode=="dangerFullAccess"});
    if code == 0 && !timed_out && tool != "run_command" {
        let report: Value =
            serde_json::from_str(&output).context("invalid successful file-helper report")?;
        result["fileChange"] = report["fileChange"].clone();
        result["output"] = report["output"].clone();
    }
    Ok(result)
}

fn whole_file_diff(path: &Path, before: Option<&[u8]>, after: &str) -> Option<String> {
    let before = std::str::from_utf8(before.unwrap_or_default()).ok()?;
    if before.len() + after.len() > 65536 {
        return None;
    }
    let old_count = before.split_terminator('\n').count();
    let new_count = after.split_terminator('\n').count();
    let label = std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok().map(Path::to_owned))
        .unwrap_or_else(|| path.to_owned());
    let mut diff = format!(
        "--- a/{}\n+++ b/{}\n@@ -{},{} +{},{} @@\n",
        label.display(),
        label.display(),
        usize::from(old_count > 0),
        old_count,
        usize::from(new_count > 0),
        new_count
    );
    for (prefix, content) in [('-', before), ('+', after)] {
        for line in content.split_terminator('\n') {
            diff.push(prefix);
            diff.push_str(line);
            diff.push('\n');
        }
        if !content.is_empty() && !content.ends_with('\n') {
            diff.push_str("\\ No newline at end of file\n");
        }
    }
    Some(diff)
}

/// Called before authentication/runtime initialization by the executable entrypoint.
pub fn helper_main() -> Option<i32> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--permission-tool-helper") {
        return None;
    }
    let result = (|| -> Result<()> {
        let tool = args.get(2).context("missing helper tool")?;
        let mut input = String::new();
        std::io::stdin()
            .take(16 * 1024 * 1024 + 1)
            .read_to_string(&mut input)?;
        ensure!(input.len() <= 16 * 1024 * 1024, "file input too large");
        let value: Value = serde_json::from_str(&input)?;
        let target = PathBuf::from(
            value
                .get("TargetFile")
                .and_then(Value::as_str)
                .context("TargetFile required")?,
        );
        let target = if target.is_absolute() {
            target
        } else {
            std::env::current_dir()?.join(target)
        };
        let before = match std::fs::read(&target) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let content = match tool.as_str() {
            "write_to_file" => value
                .get("CodeContent")
                .and_then(Value::as_str)
                .context("CodeContent required")?
                .to_owned(),
            "replace_file_content" => {
                let old = std::str::from_utf8(
                    before
                        .as_deref()
                        .context("replacement target does not exist")?,
                )?;
                let needle = value
                    .get("TargetContent")
                    .and_then(Value::as_str)
                    .context("TargetContent required")?;
                ensure!(
                    !needle.is_empty() && old.matches(needle).count() == 1,
                    "replacement requires exactly one match"
                );
                old.replacen(
                    needle,
                    value
                        .get("ReplacementContent")
                        .and_then(Value::as_str)
                        .context("ReplacementContent required")?,
                    1,
                )
            }
            _ => bail!("unsupported helper tool"),
        };
        let overwrite = tool == "replace_file_content"
            || value.get("Overwrite").and_then(Value::as_bool) == Some(true);
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if overwrite {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        options.open(&target)?.write_all(content.as_bytes())?;
        println!(
            "{}",
            json!({"output":format!("wrote {} bytes",content.len()),"fileChange":{"path":target,"kind":{"type":if before.is_some(){"update"}else{"add"}},"diff":whole_file_diff(&target,before.as_deref(),&content)}})
        );
        Ok(())
    })();
    Some(match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_command_preserves_shell_text_and_clears_untrusted_environment() {
        let cwd = std::env::current_dir().unwrap();
        let policy = PermissionPolicy::from_rpc(&json!({}), &cwd).unwrap();
        let literal = "quotes: '\"; $(printf injected) `printf injected`\nsecond line";
        let original = format!(
            "printf '%s\\n' {}; printf '%s' \"${{UNTRUSTED_NATIVE_ENV-clean}}\"",
            shell_quote(literal).unwrap()
        );
        let wrapped = native_command_line(
            &policy,
            &cwd,
            &json!({"CommandLine":original,"BypassSandbox":true}),
        )
        .unwrap();
        let output = std::process::Command::new("/bin/bash")
            .args(["--noprofile", "--norc", "-c", &wrapped])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env("UNTRUSTED_NATIVE_ENV", "must-not-reach-child")
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{literal}\nclean")
        );
    }

    #[test]
    fn native_command_does_not_weaken_restricted_persistent_calls() {
        let cwd = std::env::current_dir().unwrap();
        let restricted = PermissionPolicy::from_rpc(&json!({}), &cwd).unwrap();
        for extra in [
            json!({"RunPersistent":true}),
            json!({"RequestedTerminalID":"already-running"}),
            json!({"RequestedTerminalID":42}),
        ] {
            let mut args = json!({"CommandLine":"export X=1", "BypassSandbox":true});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(native_command_line(&restricted, &cwd, &args).is_err());
        }
        let full =
            PermissionPolicy::from_rpc(&json!({"sandbox":"danger-full-access"}), &cwd).unwrap();
        assert_eq!(
            native_command_line(
                &full,
                &cwd,
                &json!({"CommandLine":"export X=1", "RunPersistent":true})
            )
            .unwrap(),
            "export X=1"
        );
        assert!(shell_quote("nul\0suffix").is_err());
        assert!(
            native_command_line(
                &full,
                &cwd,
                &json!({"CommandLine":"true", "Cwd":"relative"})
            )
            .is_err()
        );
        assert!(native_command_line(&full, &cwd, &json!({"CommandLine":"nul\0suffix"})).is_err());
    }

    #[test]
    fn native_commands_have_no_duplicate_custom_schema() {
        let schemas = file_tool_schemas();
        let names = schemas
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["write_to_file", "replace_file_content"]);
    }

    #[test]
    fn policies_are_explicit() {
        let cwd = std::env::current_dir().unwrap();
        let p = PermissionPolicy::from_rpc(&json!({}), &cwd).unwrap();
        assert!(!p.network_access);
        assert!(p.exclude_slash_tmp);
        assert_eq!(p.effective()["type"], "workspaceWrite");
        let s = p.sandbox_state(&cwd).unwrap();
        assert!(s.to_string().contains(".git"));
        assert!(PermissionPolicy::from_rpc(&json!({"sandbox":"unsupported"}), &cwd).is_err());
    }
    #[test]
    fn readonly_has_no_writes() {
        let p =
            PermissionPolicy::from_rpc(&json!({"sandbox":"read-only"}), Path::new("/")).unwrap();
        assert!(p.writable_roots.is_empty());
        assert!(
            !p.sandbox_state(Path::new("/"))
                .unwrap()
                .to_string()
                .contains("\"write\"")
        );
    }
    #[test]
    fn invalid_policy_is_not_silently_weakened() {
        for v in [
            json!({"sandbox":"read-only","sandboxPolicy":{"type":"workspaceWrite"}}),
            json!({"sandboxPolicy":{"networkAccess":true}}),
            json!({"sandboxPolicy":{"type":"readOnly","writableRoots":["/"]}}),
            json!({"sandboxPolicy":{"type":"dangerFullAccess","networkAccess":false}}),
            json!({"sandboxPolicy":{"type":"workspaceWrite","unknown":true}}),
        ] {
            assert!(
                PermissionPolicy::from_rpc(&v, Path::new("/")).is_err(),
                "{v}"
            );
        }
    }
}
