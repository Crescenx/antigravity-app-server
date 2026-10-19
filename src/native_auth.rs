//! Native OAuth owns credential storage; this module never reads or returns tokens.
use crate::*;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::Command;

pub async fn status(endpoint: &Value, auth_home: &Path) -> Value {
    let mut result = json!({"runtime":"app-core-v1", "tokenFilePresent":auth_home.join(".gemini/jetski-standalone-oauth-token").is_file()});
    for (method, target) in [
        ("HasAuthToken", "tokenStatus"),
        ("GetAuthStatus", "authStatus"),
    ] {
        match tokio::time::timeout(
            Duration::from_secs(5),
            native::rpc(endpoint, method, json!({})),
        )
        .await
        {
            Ok(Ok(value)) => {
                let source = if method == "GetAuthStatus" {
                    &value["authResult"]
                } else {
                    &value
                };
                result[target] = redacted_status(source);
            }
            _ => result[target] = json!({"unavailable":true}),
        }
    }
    result
}

fn redacted_status(value: &Value) -> Value {
    let mut result = json!({});
    for key in [
        "hasToken",
        "hasValidAuth",
        "isGcpTos",
        "ineligible",
        "verificationRequired",
        "tosViolation",
        "generalError",
        "projectRequired",
        "licenseRequired",
    ] {
        if let Some(value) = value[key].as_bool() {
            result[key] = json!(value);
        }
    }
    result["headlessAuthRequired"] = json!(
        value
            .get("headlessAuthRequired")
            .is_some_and(|v| !v.is_null())
    );
    result
}

fn oauth_url(line: &str) -> Option<String> {
    let start = line.find("https://accounts.google.com/")?;
    let candidate = line[start..]
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '\u{1b}' | '<' | '>'))
        .next()?;
    let url = reqwest::Url::parse(candidate).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("accounts.google.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || !matches!(url.path(), "/o/oauth2/auth" | "/o/oauth2/v2/auth")
    {
        return None;
    }
    let params = url.query_pairs().collect::<BTreeMap<_, _>>();
    if !["client_id", "redirect_uri", "response_type"]
        .iter()
        .all(|key| params.contains_key(*key))
    {
        return None;
    }
    Some(url.into())
}

pub async fn login(binary: &Path, data: &Path, auth_home: &Path) -> Result<Value> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(data)?;
    std::fs::create_dir_all(auth_home.join(".gemini"))?;
    for path in [data, auth_home, &auth_home.join(".gemini")] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let csrf = uuid::Uuid::new_v4().to_string();
    let endpoint = json!({"baseUrl":format!("http://127.0.0.1:{port}"),"csrf":csrf});
    let mut command = Command::new(binary);
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("BASH_FUNC_")
            || [
                "BASH_ENV",
                "ENV",
                "SHELLOPTS",
                "BASHOPTS",
                "CDPATH",
                "GLOBIGNORE",
                "LD_PRELOAD",
                "LD_LIBRARY_PATH",
                "GOOGLE_APPLICATION_CREDENTIALS",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
            ]
            .contains(&name.as_ref())
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(data)
        .env("HOME", auth_home)
        .env("XDG_CONFIG_HOME", auth_home.join("config"))
        .env("XDG_DATA_HOME", auth_home.join("data"))
        .env("XDG_CACHE_HOME", auth_home.join("cache"))
        .args([
            "--headless",
            "--standalone",
            "--subclient_type=hub",
            "--override_ide_name=antigravity",
            "--override_ide_version=2.24.0",
            "--override_user_agent_name=antigravity",
            "--exit_on_stdin_close",
            "--disable_telemetry",
            "--use_ls_chrome_devtools_mcp=false",
            "--cloud_code_endpoint=https://daily-cloudcode-pa.googleapis.com",
            "--api_server_url=https://generativelanguage.googleapis.com",
        ])
        .arg(format!("--http_server_port={port}"))
        .arg(format!("--csrf_token={csrf}"))
        .arg(format!("--gemini_dir={}", data.join(".gemini").display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("starting native login")?;
    let mut stdin = child.stdin.take().context("native login stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    // A blocking OS thread avoids Tokio stdin keeping runtime shutdown stuck after Ctrl-C.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if input_tx.send(line).is_err() {
                break;
            }
        }
    });
    eprintln!(
        "Waiting for native Google sign-in. Open the URL below, then paste the authorization code if requested. Ctrl-C cancels."
    );
    let mut previous_url = None;
    let mut out_open = true;
    let mut err_open = true;
    let mut input_open = true;
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    let result = async {
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => bail!("native login cancelled"),
                exit = child.wait() => { let _ = exit?; bail!("native login process exited before authentication completed"); },
                line = stdout.next_line(), if out_open => { match line? { Some(line) => print_oauth_url(&line, &mut previous_url), None => out_open = false } },
                line = stderr.next_line(), if err_open => { match line? { Some(line) => print_oauth_url(&line, &mut previous_url), None => err_open = false } },
                line = input_rx.recv(), if input_open => {
                    match line { Some(line) => { stdin.write_all(line.as_bytes()).await?; stdin.write_all(b"\n").await?; stdin.flush().await?; }, None => input_open = false }
                },
                _ = poll.tick() => {
                    let token = tokio::select! {
                        _ = tokio::signal::ctrl_c() => bail!("native login cancelled"),
                        value = tokio::time::timeout(Duration::from_secs(2), native::rpc(&endpoint,"HasAuthToken",json!({}))) => value,
                    };
                    if token.is_ok_and(|v| v.is_ok_and(|v| v["hasToken"] == true)) {
                        return Ok(status(&endpoint, auth_home).await);
                    }
                }
            }
        }
    }.await;
    drop(stdin);
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    result
}

fn print_oauth_url(line: &str, previous: &mut Option<String>) {
    if let Some(url) = oauth_url(line)
        && previous.as_ref() != Some(&url)
    {
        eprintln!("\n{url}\n");
        *previous = Some(url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_native_google_oauth_urls_are_printable() {
        let url = "https://accounts.google.com/o/oauth2/auth?client_id=example&redirect_uri=http%3A%2F%2Flocalhost&response_type=code";
        assert_eq!(
            oauth_url(&format!("ANTIGRAVITY_OPEN_URL: {url}\x1b[0m")),
            Some(url.into())
        );
        assert!(oauth_url("https://accounts.google.com/secret?access_token=secret").is_none());
        assert!(
            oauth_url("https://accounts.google.com.evil.test/o/oauth2/auth?client_id=x").is_none()
        );
        assert!(oauth_url("access_token=secret").is_none());
    }
    #[test]
    fn account_status_excludes_credentials_and_arbitrary_messages() {
        let result = redacted_status(
            &json!({"hasValidAuth":false,"ineligible":true,"uiMessage":"secret","accessToken":"secret","headlessAuthRequired":{"authUrl":"secret"},"email":"secret"}),
        );
        assert_eq!(
            result,
            json!({"hasValidAuth":false,"ineligible":true,"headlessAuthRequired":true})
        );
    }
}
