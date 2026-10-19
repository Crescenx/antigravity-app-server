use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{self, client::IntoClientRequest},
};

pub const EXTERNAL_SHA256: &str =
    "30c80540147b7f6ce9157f8fbfce1ce5308e96b10602d4236f9108463a839373";

pub fn verify_binary(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    anyhow::ensure!(
        format!("{:x}", hash.finalize()) == EXTERNAL_SHA256,
        "unsupported external binary: expected the verified Linux 1.4.0 build"
    );
    Ok(())
}

#[derive(Clone, PartialEq, Message)]
struct InputConfig {
    #[prost(string, tag = "1")]
    storage_directory: String,
    #[prost(string, tag = "3")]
    bind_address: String,
}
#[derive(Clone, PartialEq, Message)]
struct OutputConfig {
    #[prost(int32, tag = "1")]
    port: i32,
    #[prost(string, tag = "2")]
    api_key: String,
}

pub struct Native {
    child: Child,
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Native {
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }
    pub async fn open(binary: &Path, root: &Path, config: Value) -> Result<(Self, Value)> {
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions)?;
        let mut command = Command::new(binary);
        // The native shell starts before our per-command sandbox wrapper.
        // Do not let inherited startup hooks execute outside that wrapper.
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
                ]
                .contains(&name.as_ref())
            {
                command.env_remove(key);
            }
        }
        let mut child = command
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting localharness_external")?;
        let connect = async {
            let bytes = InputConfig {
                storage_directory: sessions.to_string_lossy().into(),
                bind_address: "127.0.0.1".into(),
            }
            .encode_to_vec();
            let stdin = child.stdin.as_mut().context("native stdin")?;
            stdin.write_u32_le(bytes.len() as u32).await?;
            stdin.write_all(&bytes).await?;
            stdin.flush().await?;
            let stdout = child.stdout.as_mut().context("native stdout")?;
            let len = stdout.read_u32_le().await? as usize;
            anyhow::ensure!(len <= 65536, "invalid native handshake length");
            let mut bytes = vec![0; len];
            stdout.read_exact(&mut bytes).await?;
            let reply = OutputConfig::decode(bytes.as_slice())?;
            anyhow::ensure!((1..=65535).contains(&reply.port), "invalid native port");
            let mut request = format!("ws://127.0.0.1:{}/", reply.port).into_client_request()?;
            request
                .headers_mut()
                .insert("x-goog-api-key", reply.api_key.parse()?);
            let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;
            ws.send(tungstenite::Message::Text(
                json!({"config":config}).to_string().into(),
            ))
            .await?;
            let event = ws
                .next()
                .await
                .context("native closed during initialization")??;
            let event: Value = serde_json::from_str(event.to_text()?)?;
            anyhow::ensure!(
                event.get("initializeConversationResponse").is_some(),
                "native initialization rejected"
            );
            Ok::<_, anyhow::Error>((ws, event))
        };
        let (ws, event) = tokio::time::timeout(Duration::from_secs(30), connect)
            .await
            .context("native initialization timed out")??;
        Ok((Self { child, ws }, event))
    }

    pub async fn send(&mut self, event: Value) -> Result<()> {
        self.ws
            .send(tungstenite::Message::Text(event.to_string().into()))
            .await?;
        Ok(())
    }

    pub async fn next(&mut self) -> Result<Value> {
        loop {
            match self.ws.next().await.context("native disconnected")?? {
                tungstenite::Message::Text(text) => return Ok(serde_json::from_str(&text)?),
                tungstenite::Message::Close(_) => bail!("native closed connection"),
                _ => {}
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.send(json!({"sessionEndRequest":true})).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            while self.next().await?["sessionEndResponse"] != true {}
            Ok::<_, anyhow::Error>(())
        })
        .await;
        let _ = self.ws.close(None).await;
        drop(self.child.stdin.take());
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}
