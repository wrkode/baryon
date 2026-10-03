//! Client for the Cloud Hypervisor REST API over its Unix socket.
//!
//! Cloud Hypervisor serves one request at a time on a single thread
//! (micro_http). The client opens one connection per request, applies a
//! timeout to the whole exchange, and closes the connection afterwards.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http::{Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

/// Errors from the Cloud Hypervisor API.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("connecting to {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("HTTP exchange with {endpoint}: {source}")]
    Http {
        endpoint: &'static str,
        #[source]
        source: hyper::Error,
    },
    #[error("building request for {endpoint}: {source}")]
    Request {
        endpoint: &'static str,
        #[source]
        source: http::Error,
    },
    #[error("{endpoint} timed out after {timeout:?}")]
    Timeout {
        endpoint: &'static str,
        timeout: Duration,
    },
    #[error("{endpoint} returned {status}: {body}")]
    Status {
        endpoint: &'static str,
        status: StatusCode,
        body: String,
    },
    #[error("decoding {endpoint} response: {source}")]
    Decode {
        endpoint: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl ApiError {
    /// The VMM reported that no VM has been created (HTTP 404, Cloud
    /// Hypervisor v53 and later).
    pub fn is_vm_not_created(&self) -> bool {
        matches!(self, Self::Status { status, .. } if *status == StatusCode::NOT_FOUND)
    }
}

/// Response of `vmm.ping`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct VmmPing {
    pub version: String,
    #[serde(default)]
    pub build_version: Option<String>,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub features: Vec<String>,
}

/// VM state reported by `vm.info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum VmState {
    Created,
    Running,
    Shutdown,
    Paused,
    BreakPoint,
}

/// Subset of `vm.info` used by Baryon. The full configuration is kept as
/// untyped JSON for diagnostics only.
#[derive(Debug, Clone, Deserialize)]
pub struct VmInfo {
    pub state: VmState,
    #[serde(default)]
    pub memory_actual_size: Option<u64>,
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Cloud Hypervisor API client bound to one socket path.
#[derive(Debug, Clone)]
pub struct ApiClient {
    socket: PathBuf,
    timeout: Duration,
}

impl ApiClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn ping(&self) -> Result<VmmPing, ApiError> {
        self.get_json("vmm.ping").await
    }

    pub async fn vm_info(&self) -> Result<VmInfo, ApiError> {
        self.get_json("vm.info").await
    }

    /// Press the virtual ACPI power button. The guest decides whether and
    /// when to shut down.
    pub async fn power_button(&self) -> Result<(), ApiError> {
        self.put_empty("vm.power-button").await
    }

    /// Stop the VMM process. The guest is stopped without a shutdown.
    pub async fn shutdown_vmm(&self) -> Result<(), ApiError> {
        self.put_empty("vmm.shutdown").await
    }

    async fn get_json<T: DeserializeOwned>(&self, endpoint: &'static str) -> Result<T, ApiError> {
        let body = self.call(Method::GET, endpoint, None).await?;
        serde_json::from_slice(&body).map_err(|source| ApiError::Decode { endpoint, source })
    }

    async fn put_empty(&self, endpoint: &'static str) -> Result<(), ApiError> {
        self.call(Method::PUT, endpoint, None).await.map(|_| ())
    }

    /// Perform one request and return the response body for 2xx statuses.
    pub async fn call(
        &self,
        method: Method,
        endpoint: &'static str,
        json_body: Option<Bytes>,
    ) -> Result<Bytes, ApiError> {
        let fut = self.call_inner(method, endpoint, json_body);
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(ApiError::Timeout {
                endpoint,
                timeout: self.timeout,
            }),
        }
    }

    async fn call_inner(
        &self,
        method: Method,
        endpoint: &'static str,
        json_body: Option<Bytes>,
    ) -> Result<Bytes, ApiError> {
        let stream =
            UnixStream::connect(&self.socket)
                .await
                .map_err(|source| ApiError::Connect {
                    path: self.socket.clone(),
                    source,
                })?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|source| ApiError::Http { endpoint, source })?;
        let conn = tokio::spawn(conn);

        let mut builder = Request::builder()
            .method(method)
            .uri(format!("/api/v1/{endpoint}"))
            .header(header::HOST, "localhost")
            .header(header::ACCEPT, "application/json");
        let body = match json_body {
            Some(b) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                b
            }
            None => Bytes::new(),
        };
        let request = builder
            .body(Full::new(body))
            .map_err(|source| ApiError::Request { endpoint, source })?;

        let result = async {
            let response = sender
                .send_request(request)
                .await
                .map_err(|source| ApiError::Http { endpoint, source })?;
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|source| ApiError::Http { endpoint, source })?
                .to_bytes();
            if status.is_success() {
                Ok(bytes)
            } else {
                Err(ApiError::Status {
                    endpoint,
                    status,
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                })
            }
        }
        .await;

        drop(sender);
        conn.abort();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Serve one canned HTTP response and return the raw request.
    async fn serve_once(
        listener: UnixListener,
        response: &'static str,
    ) -> tokio::task::JoinHandle<String> {
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let mut req = Vec::new();
            loop {
                let n = s.read(&mut buf).await.unwrap();
                req.extend_from_slice(&buf[..n]);
                if n == 0 || req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            s.write_all(response.as_bytes()).await.unwrap();
            s.shutdown().await.unwrap();
            String::from_utf8(req).unwrap()
        })
    }

    #[tokio::test]
    async fn ping_parses_response() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let body = r#"{"build_version":"v53.0","version":"53.0","pid":4242,"features":["kvm","io_uring"]}"#;
        let resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let server = serve_once(listener, resp).await;

        let ping = ApiClient::new(&sock).ping().await.unwrap();
        assert_eq!(ping.version, "53.0");
        assert_eq!(ping.pid, Some(4242));
        assert_eq!(ping.features, vec!["kvm", "io_uring"]);

        let req = server.await.unwrap();
        assert!(
            req.starts_with("GET /api/v1/vmm.ping HTTP/1.1\r\n"),
            "{req}"
        );
    }

    #[tokio::test]
    async fn error_status_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = serve_once(
            listener,
            "HTTP/1.1 404 Not Found\r\nContent-Length: 17\r\n\r\n[\"VM not created\"]",
        )
        .await;
        let err = ApiClient::new(&sock).vm_info().await.unwrap_err();
        assert!(err.is_vm_not_created(), "{err}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn put_without_body() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = serve_once(listener, "HTTP/1.1 204 No Content\r\n\r\n").await;
        ApiClient::new(&sock).power_button().await.unwrap();
        let req = server.await.unwrap();
        assert!(
            req.starts_with("PUT /api/v1/vm.power-button HTTP/1.1\r\n"),
            "{req}"
        );
    }

    #[tokio::test]
    async fn connect_failure() {
        let dir = tempfile::tempdir().unwrap();
        let err = ApiClient::new(dir.path().join("missing.sock"))
            .ping()
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Connect { .. }));
    }

    #[tokio::test]
    async fn timeout_applies_to_whole_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let _server = tokio::spawn(async move {
            let (_s, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        let err = ApiClient::new(&sock)
            .with_timeout(Duration::from_millis(100))
            .ping()
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::Timeout { .. }), "{err}");
    }

    #[test]
    fn decodes_vm_info() {
        let info: VmInfo = serde_json::from_str(
            r#"{"config":{"cpus":{"boot_vcpus":2}},"state":"Running","memory_actual_size":1073741824}"#,
        )
        .unwrap();
        assert_eq!(info.state, VmState::Running);
        assert_eq!(info.memory_actual_size, Some(1 << 30));
    }
}
