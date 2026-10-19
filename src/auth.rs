use crate::model_body::{DEFAULT_LIMIT, ModelBody};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{OriginalUri, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

struct Proxy {
    client: reqwest::Client,
    credentials: Value,
    secret: String,
    token: Mutex<(String, Instant)>,
    request_limit: u64,
    model_base_url: String,
}

// Record transport metadata only: never prompts, attachment data, tokens or upstream bodies.
fn diagnostic(mut event: Value) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    event["timestampMs"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
    if let Ok(root) =
        std::env::var("AGY_APP_SERVER_HOME").or_else(|_| std::env::var("AGY_HARNESS_HOME"))
    {
        let path = std::path::Path::new(&root).join("transport.jsonl");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
        {
            let _ = writeln!(file, "{event}");
        }
    }
    if event["error"].is_string() {
        eprintln!("transport: {event}");
    }
}

fn failure_kind(error: &anyhow::Error) -> &'static str {
    if let Some(error) = error
        .chain()
        .find_map(|e| e.downcast_ref::<reqwest::Error>())
    {
        if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connection failure"
        } else if error.is_body() {
            "request body failure"
        } else if error.is_decode() {
            "response decoding failure"
        } else {
            "transport failure"
        }
    } else {
        "proxy failure"
    }
}
impl Proxy {
    async fn token(&self) -> Result<String> {
        let mut cached = self.token.lock().await;
        if Instant::now() < cached.1 {
            return Ok(cached.0.clone());
        }
        let mut form = vec![("grant_type", "refresh_token")];
        for field in ["client_id", "client_secret", "refresh_token"] {
            form.push((
                field,
                self.credentials[field]
                    .as_str()
                    .context("incomplete personal OAuth credentials")?,
            ));
        }
        // Never forward credentials to an endpoint taken from a session or RPC request.
        let response = self
            .client
            .post("https://oauth2.googleapis.com/token")
            .timeout(Duration::from_secs(20))
            .form(&form)
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "OAuth token refresh rejected ({})",
            response.status()
        );
        let value: Value = response.json().await?;
        let token = value["access_token"]
            .as_str()
            .context("missing OAuth access token")?
            .to_owned();
        let ttl = value["expires_in"]
            .as_u64()
            .unwrap_or(3600)
            .saturating_sub(60);
        *cached = (token.clone(), Instant::now() + Duration::from_secs(ttl));
        Ok(token)
    }
}

pub async fn endpoint() -> Result<(Value, Option<tokio::task::JoinHandle<()>>)> {
    let idle_seconds = std::env::var("AGY_MODEL_IDLE_TIMEOUT_SECS")
        .map_or(Ok(600), |value| value.parse::<u64>())?;
    ensure!(
        idle_seconds > 0,
        "AGY_MODEL_IDLE_TIMEOUT_SECS must be positive"
    );
    endpoint_with_client(model_client_builder(Duration::from_secs(idle_seconds)).build()?).await
}

fn model_client_builder(idle_timeout: Duration) -> reqwest::ClientBuilder {
    // Tool-call arguments may arrive only after the complete file is generated.
    // Bound stalled reads, not the total duration of a healthy generation stream.
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(idle_timeout)
}

async fn endpoint_with_client(
    client: reqwest::Client,
) -> Result<(Value, Option<tokio::task::JoinHandle<()>>)> {
    if let Ok(path) = std::env::var("AGY_PERSONAL_CREDENTIALS") {
        let credentials: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            credentials["project_id"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "personal credentials need an onboarded project_id"
        );
        let proxy = Arc::new(Proxy {
            client,
            credentials,
            secret: uuid::Uuid::new_v4().to_string(),
            token: Mutex::new((String::new(), Instant::now())),
            request_limit: std::env::var("AGY_MODEL_REQUEST_MAX_BYTES")
                .map_or(Ok(DEFAULT_LIMIT), |v| v.parse::<u64>())?,
            model_base_url: "https://daily-cloudcode-pa.googleapis.com/v1internal".into(),
        });
        ensure!(
            proxy.request_limit > 0,
            "AGY_MODEL_REQUEST_MAX_BYTES must be positive"
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = json!({"baseUrl":format!("http://{}",listener.local_addr()?),"apiKey":"local-proxy", "httpHeaders":{"X-ACP-Proxy-Token":proxy.secret}});
        let app = Router::new()
            .route("/v1beta/models", get(metadata))
            .route("/v1beta/account/rateLimits", get(metadata))
            .route("/{*path}", post(generate))
            .with_state(proxy);
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("personal proxy stopped: {error}");
            }
        });
        Ok((endpoint, Some(task)))
    } else {
        Ok((
            json!({"baseUrl":std::env::var("AGY_API_BASE_URL").unwrap_or("https://generativelanguage.googleapis.com".into()),
            "apiKey":std::env::var("GEMINI_API_KEY").context("set GEMINI_API_KEY or AGY_PERSONAL_CREDENTIALS")?}),
            None,
        ))
    }
}

async fn metadata(
    State(proxy): State<Arc<Proxy>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if headers
        .get("x-acp-proxy-token")
        .and_then(|v| v.to_str().ok())
        != Some(&proxy.secret)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let method = if uri.path() == "/v1beta/models" {
        "fetchAvailableModels"
    } else {
        "retrieveUserQuotaSummary"
    };
    let started = Instant::now();
    let result = async {
        let response = proxy
            .client
            .post(format!(
                "https://daily-cloudcode-pa.googleapis.com/v1internal:{method}"
            ))
            .timeout(Duration::from_secs(20))
            .bearer_auth(proxy.token().await?)
            .header("User-Agent", user_agent())
            .json(&json!({"project":proxy.credentials["project_id"]}))
            .send()
            .await?;
        ensure!(
            response.status().is_success(),
            "account metadata rejected: HTTP {}",
            response.status()
        );
        Ok::<Value, anyhow::Error>(response.json().await?)
    }
    .await;
    match result {
        Ok(value) => axum::Json(value).into_response(),
        Err(error) => {
            diagnostic(
                json!({"phase":"metadata","method":method,"elapsedMs":started.elapsed().as_millis(),"kind":failure_kind(&error),"error":format!("{error:#}")}),
            );
            (StatusCode::BAD_GATEWAY, "account metadata unavailable").into_response()
        }
    }
}

async fn generate(
    State(proxy): State<Arc<Proxy>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if headers
        .get("x-acp-proxy-token")
        .and_then(|v| v.to_str().ok())
        != Some(&proxy.secret)
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let started = Instant::now();
    let body = match ModelBody::receive(request.into_body(), proxy.request_limit).await {
        Ok(body) => body,
        Err((status, message)) => {
            return (
                status,
                axum::Json(json!({"error":{"code":status.as_u16(),"message":message}})),
            )
                .into_response();
        }
    };
    diagnostic(
        json!({"requestId":request_id,"phase":"local_body_received","requestBytes":body.bytes(),"elapsedMs":started.elapsed().as_millis()}),
    );
    match forward(proxy, uri.path(), body, request_id.clone(), started).await {
        Ok(response) => response,
        Err(error) => {
            let kind = failure_kind(&error);
            diagnostic(
                json!({"requestId":request_id,"phase":"request_failed","elapsedMs":started.elapsed().as_millis(),"kind":kind,"error":format!("{error:#}")}),
            );
            (
                if kind == "timeout" {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                format!(
                    "model proxy failed: {kind}; request_id={request_id}; elapsed_ms={}",
                    started.elapsed().as_millis()
                ),
            )
                .into_response()
        }
    }
}

async fn forward(
    proxy: Arc<Proxy>,
    path: &str,
    body: ModelBody,
    request_id: String,
    started: Instant,
) -> Result<Response> {
    let suffix = path
        .split("/models/")
        .nth(1)
        .context("unsupported model path")?;
    let (model, method) = suffix.split_once(':').context("unsupported model method")?;
    ensure!(
        ["streamGenerateContent", "generateContent"].contains(&method),
        "unsupported model method"
    );
    let token = proxy.token().await.context("OAuth token acquisition")?;
    let (request, length, yielded) = body.envelope(&proxy.credentials["project_id"], model);
    let query = if method == "streamGenerateContent" {
        "?alt=sse"
    } else {
        ""
    };
    diagnostic(
        json!({"requestId":request_id,"phase":"upstream_send","model":model,"requestBytes":length,"elapsedMs":started.elapsed().as_millis()}),
    );
    let response = proxy
        .client
        .post(format!("{}:{method}{query}", proxy.model_base_url))
        .bearer_auth(token)
        .header("User-Agent", user_agent())
        .header("content-type", "application/json")
        .header("content-length", length)
        .body(request)
        .send()
        .await;
    // Yielded bytes have reached reqwest, not necessarily the remote peer.
    diagnostic(
        json!({"requestId":request_id,"phase":"upstream_headers","elapsedMs":started.elapsed().as_millis(),"bodyBytesYielded":yielded.load(std::sync::atomic::Ordering::Relaxed),"requestBytes":length,"status":response.as_ref().ok().map(|r|r.status().as_u16()),"httpVersion":response.as_ref().ok().map(|r|format!("{:?}",r.version()))}),
    );
    let response = response.context("Awaiting upstream response headers")?;
    if !response.status().is_success() {
        // Upstream error bodies can contain account metadata, so return only the status.
        return Ok((response.status(), "upstream model request rejected").into_response());
    }
    if method == "generateContent" {
        let value: Value = response.json().await?;
        return Ok(axum::Json(value.get("response").cloned().unwrap_or(value)).into_response());
    }
    let stream = async_stream::try_stream! {
        let mut chunks = response.bytes_stream();
        let mut pending = Vec::new();
        let mut response_bytes = 0_u64;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|error| {
                diagnostic(json!({"requestId":request_id,"phase":"response_stream_failed","elapsedMs":started.elapsed().as_millis(),"responseBytes":response_bytes,"error":format!("{error:?}")}));
                std::io::Error::other(error)
            })?;
            if response_bytes == 0 { diagnostic(json!({"requestId":request_id,"phase":"first_response_bytes","elapsedMs":started.elapsed().as_millis()})); }
            response_bytes += chunk.len() as u64;
            pending.extend_from_slice(&chunk);
            while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=pos).collect();
                yield Bytes::from(unwrap_sse(&line)?);
            }
            if pending.len() > 16*1024*1024 { Err(std::io::Error::other("SSE line too large"))?; }
        }
        if !pending.is_empty() { yield Bytes::from(unwrap_sse(&pending)?); }
        diagnostic(json!({"requestId":request_id,"phase":"response_complete","elapsedMs":started.elapsed().as_millis(),"responseBytes":response_bytes}));
    };
    let stream = stream.map(|item: std::io::Result<Bytes>| item);
    Ok(Response::builder()
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))?)
}

fn unwrap_sse(line: &[u8]) -> std::io::Result<Vec<u8>> {
    if line == b"\r\n" {
        return Ok(b"\n".to_vec());
    }
    if let Some(data) = line.strip_prefix(b"data:") {
        let text = std::str::from_utf8(data)
            .map_err(std::io::Error::other)?
            .trim();
        if text == "[DONE]" {
            return Ok(line.to_vec());
        }
        let value: Value = serde_json::from_str(text).map_err(std::io::Error::other)?;
        return Ok(format!("data: {}\n", value.get("response").unwrap_or(&value)).into_bytes());
    }
    Ok(line.to_vec())
}

fn user_agent() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    format!(
        "antigravity/acp/1.4.0 (aidev_client; os_type={}; arch={arch}; host_path=zed/unknown; proxy_client=antigravity/sdk)",
        std::env::consts::OS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn active_stream_can_outlive_the_idle_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/stream",
                    get(|| async {
                        Body::from_stream(async_stream::stream! {
                            for _ in 0..8 {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                yield Ok::<_,std::io::Error>(Bytes::from_static(b"progress\n"));
                            }
                        })
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let client = model_client_builder(Duration::from_millis(200))
            .no_proxy()
            .build()
            .unwrap();
        let started = Instant::now();
        let response = client.get(url).send().await.unwrap().text().await.unwrap();
        assert_eq!(response, "progress\n".repeat(8));
        assert!(started.elapsed() > Duration::from_millis(200));
        server.abort();
    }
    #[tokio::test]
    async fn upstream_header_timeout_is_not_reported_as_an_opaque_bad_gateway() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}/v1internal", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/{*path}",
                    post(|| async {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        "data: {}\n\n"
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let proxy = Arc::new(Proxy {
            client: model_client_builder(Duration::from_millis(40))
                .no_proxy()
                .build()
                .unwrap(),
            credentials: json!({"project_id":"fixture"}),
            secret: "fixture".into(),
            token: Mutex::new((
                "fixture-token".into(),
                Instant::now() + Duration::from_secs(60),
            )),
            request_limit: 1024,
            model_base_url: upstream,
        });
        let mut headers = HeaderMap::new();
        headers.insert("x-acp-proxy-token", "fixture".parse().unwrap());
        let response = generate(
            State(proxy),
            OriginalUri(
                "/v1beta/models/fixture:streamGenerateContent"
                    .parse()
                    .unwrap(),
            ),
            headers,
            Request::new(Body::from("{}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), 2048)
            .await
            .unwrap();
        let message = std::str::from_utf8(&body).unwrap();
        assert!(message.contains("timeout; request_id="));
        assert!(!message.contains("fixture-token"));
        server.abort();
    }
    #[tokio::test]
    #[ignore = "real account transport; explicit request file and credentials required"]
    async fn live_transport_probe() {
        let request = std::env::var("AGY_TRANSPORT_PROBE_REQUEST").expect("explicit probe body");
        let model = std::env::var("AGY_TRANSPORT_PROBE_MODEL")
            .unwrap_or_else(|_| "gemini-4-argon-medium".into());
        let timeout = std::env::var("AGY_TRANSPORT_PROBE_TIMEOUT_SECS")
            .ok()
            .map(|s| s.parse::<u64>().unwrap())
            .unwrap_or(360);
        let mut builder = if std::env::var("AGY_TRANSPORT_PROBE_TOTAL_TIMEOUT").is_ok() {
            reqwest::Client::builder().timeout(Duration::from_secs(timeout))
        } else {
            model_client_builder(Duration::from_secs(timeout))
        };
        if std::env::var("AGY_TRANSPORT_PROBE_HTTP1").is_ok() {
            builder = builder.http1_only();
        }
        let (endpoint, task) = endpoint_with_client(builder.build().unwrap())
            .await
            .unwrap();
        let started = Instant::now();
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(timeout + 20))
            .build()
            .unwrap()
            .post(format!(
                "{}/v1beta/models/{model}:streamGenerateContent?alt=sse",
                endpoint["baseUrl"].as_str().unwrap()
            ))
            .header(
                "X-ACP-Proxy-Token",
                endpoint["httpHeaders"]["X-ACP-Proxy-Token"]
                    .as_str()
                    .unwrap(),
            )
            .body(std::fs::read(&request).unwrap())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        let output = format!("{request}.response.txt");
        std::fs::write(&output, &body).unwrap();
        println!(
            "probe status={status} elapsed_ms={} response_bytes={} output={output}",
            started.elapsed().as_millis(),
            body.len()
        );
        if let Some(task) = task {
            task.abort();
        }
        assert!(status.is_success(), "{body}");
    }
    #[tokio::test]
    async fn proxy_authentication_and_configured_limit_are_enforced() {
        let proxy = Arc::new(Proxy {
            client: reqwest::Client::new(),
            credentials: json!({}),
            secret: "test".into(),
            token: Mutex::new((String::new(), Instant::now())),
            request_limit: 4 * 1024 * 1024,
            model_base_url: "http://127.0.0.1:1".into(),
        });
        let app = Router::new()
            .route("/{*path}", post(generate))
            .with_state(proxy.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        // Unsupported path fails before OAuth or model requests, after the body is read.
        let url = format!("http://{address}/unsupported");
        let request = Request::new(Body::from(vec![b' '; 3 * 1024 * 1024]));
        let response = generate(
            State(proxy.clone()),
            OriginalUri("/unsupported".parse().unwrap()),
            HeaderMap::new(),
            request,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = client
            .post(&url)
            .header("x-acp-proxy-token", "test")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(
            response
                .text()
                .await
                .unwrap()
                .starts_with("model proxy failed: proxy failure; request_id=")
        );
        let mut headers = HeaderMap::new();
        headers.insert("x-acp-proxy-token", "test".parse().unwrap());
        let request = Request::new(Body::from(vec![b' '; 4 * 1024 * 1024 + 1]));
        let response = generate(
            State(proxy),
            OriginalUri("/unsupported".parse().unwrap()),
            headers,
            request,
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        task.abort();
    }

    #[tokio::test]
    async fn large_native_request_is_forwarded_without_changing_any_attachment_bytes() {
        use sha2::{Digest, Sha256};
        async fn upstream(request: Request) -> axum::Json<Value> {
            let length = request.headers()["content-length"]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            let mut stream = request.into_body().into_data_stream();
            let mut hash = Sha256::new();
            let mut bytes = 0_u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap();
                bytes += chunk.len() as u64;
                hash.update(&chunk);
            }
            assert_eq!(bytes, length);
            axum::Json(json!({"response":{"bytes":bytes,"sha256":format!("{:x}",hash.finalize())}}))
        }
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!(
            "http://{}/v1internal",
            upstream_listener.local_addr().unwrap()
        );
        let upstream_task = tokio::spawn(async move {
            axum::serve(
                upstream_listener,
                Router::new().route("/{*path}", post(upstream)),
            )
            .await
            .unwrap()
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let proxy = Arc::new(Proxy {
            client: client.clone(),
            credentials: json!({"project_id":"test-project"}),
            secret: "test".into(),
            token: Mutex::new((
                "test-token".into(),
                Instant::now() + Duration::from_secs(300),
            )),
            request_limit: DEFAULT_LIMIT,
            model_base_url: upstream_url,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1beta/models/test-model:generateContent",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/{*path}", post(generate))
                    .with_state(proxy),
            )
            .await
            .unwrap()
        });
        // Larger than the former 128 MiB proxy ceiling; generate and hash in bounded chunks.
        let chunk = Bytes::from(vec![b'A'; 64 * 1024]);
        let repeats = 140 * 1024 * 1024 / chunk.len();
        let prefix =
            br#"{"contents":[{"parts":[{"inlineData":{"mimeType":"application/pdf","data":""#;
        let suffix = br#""}}]}]}"#;
        let mut expected = Sha256::new();
        expected.update(br#"{"model":"test-model","project":"test-project","request":"#);
        expected.update(prefix);
        for _ in 0..repeats {
            expected.update(&chunk);
        }
        expected.update(suffix);
        expected.update(b"}");
        let stream = async_stream::try_stream! {
            yield Bytes::from_static(prefix);
            for _ in 0..repeats {yield chunk.clone();}
            yield Bytes::from_static(suffix);
        };
        let stream = stream.map(|chunk: std::io::Result<Bytes>| chunk);
        let response = client
            .post(url)
            .header("x-acp-proxy-token", "test")
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value = response.json().await.unwrap();
        assert_eq!(result["sha256"], format!("{:x}", expected.finalize()));
        assert!(result["bytes"].as_u64().unwrap() > 128 * 1024 * 1024);
        task.abort();
        upstream_task.abort();
    }
    #[test]
    fn sse_unwrap_preserves_framing_and_unicode() {
        assert_eq!(
            unwrap_sse("data: {\"response\":{\"text\":\"你好\"}}\r\n".as_bytes()).unwrap(),
            "data: {\"text\":\"你好\"}\n".as_bytes()
        );
        assert_eq!(unwrap_sse(b"\n").unwrap(), b"\n");
        assert_eq!(unwrap_sse(b"\r\n").unwrap(), b"\n");
        assert!(unwrap_sse(b"data: broken\n").is_err());
    }
}
