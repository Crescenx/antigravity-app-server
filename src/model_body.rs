use axum::{
    body::{Body, Bytes},
    http::StatusCode,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufReader, Seek};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const DEFAULT_LIMIT: u64 = 512 * 1024 * 1024;
pub struct ModelBody {
    file: tokio::fs::File,
    bytes: u64,
}

impl ModelBody {
    pub async fn receive(body: Body, limit: u64) -> Result<Self, (StatusCode, String)> {
        let io_error = |e| {
            (
                StatusCode::INSUFFICIENT_STORAGE,
                format!("Cannot buffer model request: {e}"),
            )
        };
        // Anonymous files are removed by the OS on close, including cancellation/crash.
        let mut file = tokio::fs::File::from_std(tempfile::tempfile().map_err(io_error)?);
        let mut stream = body.into_data_stream();
        let mut bytes = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Cannot read model request: {e}"),
                )
            })?;
            bytes = bytes.saturating_add(chunk.len() as u64);
            if bytes <= limit {
                file.write_all(&chunk).await.map_err(io_error)?;
            }
            // Drain an oversized local upload so the runtime receives 413 rather than broken pipe.
        }
        if bytes > limit {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "Model request is {bytes} bytes; local limit is {limit} bytes (AGY_MODEL_REQUEST_MAX_BYTES). Reduce attached files/history or raise the configured limit."
                ),
            ));
        }
        file.flush().await.map_err(io_error)?;
        let mut file = file.into_std().await;
        let file = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            file.rewind()?;
            // Validate the complete JSON without allocating its Base64 strings or Value tree.
            let mut parser = serde_json::Deserializer::from_reader(BufReader::new(&mut file));
            serde::de::IgnoredAny::deserialize(&mut parser)?;
            parser.end()?;
            file.rewind()?;
            Ok(file)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Invalid model request JSON: {e}"),
            )
        })?;
        Ok(Self {
            file: tokio::fs::File::from_std(file),
            bytes,
        })
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn envelope(self, project: &Value, model: &str) -> (reqwest::Body, u64, Arc<AtomicU64>) {
        let mut prefix =
            serde_json::to_vec(&serde_json::json!({"project":project,"model":model})).unwrap();
        prefix.pop();
        prefix.extend_from_slice(b",\"request\":");
        let length = prefix.len() as u64 + self.bytes + 1;
        let yielded = Arc::new(AtomicU64::new(0));
        let progress = yielded.clone();
        let stream = async_stream::try_stream! {
            progress.fetch_add(prefix.len() as u64, Ordering::Relaxed);
            yield Bytes::from(prefix);
            let mut file = self.file;
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let n = file.read(&mut buffer).await?;
                if n == 0 { break; }
                progress.fetch_add(n as u64, Ordering::Relaxed);
                yield Bytes::copy_from_slice(&buffer[..n]);
            }
            progress.fetch_add(1, Ordering::Relaxed);
            yield Bytes::from_static(b"}");
        };
        let stream = stream.map(|chunk: std::io::Result<Bytes>| chunk);
        (reqwest::Body::wrap_stream(stream), length, yielded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rejects_trailing_json_and_reports_size_after_draining() {
        for input in ["{\"data\":\"broken", "{} , \"project\":\"other\"", "{} {}"] {
            assert_eq!(
                ModelBody::receive(Body::from(input), 1024)
                    .await
                    .err()
                    .unwrap()
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let consumed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = consumed.clone();
        let stream = futures_util::stream::iter((0..8).map(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(b"0123456789"))
        }));
        let error = ModelBody::receive(Body::from_stream(stream), 20)
            .await
            .err()
            .unwrap();
        assert_eq!(error.0, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(error.1.contains("80 bytes"));
        assert_eq!(consumed.load(std::sync::atomic::Ordering::SeqCst), 8);
    }
}
