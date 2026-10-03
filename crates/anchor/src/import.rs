//! `baryon-anchor import`: write a raw disk image from an HTTP(S) URL into
//! the root disk volume.
//!
//! The image is streamed, all-zero 4 KiB blocks are skipped so the result is
//! sparse, the SHA-256 is verified when given, and the file is extended to
//! the requested virtual disk size. The import is recorded in a marker file
//! next to the image so that a restarted pod does not import again. This is
//! a convenience for filesystem-mode volumes, not an image management
//! system.

use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, bail};
use bytes::Bytes;
use http::{StatusCode, Uri, header};
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};

use baryon_runtime_api::anchor_spec::DISK_IMAGE_NAME;

const MARKER: &str = ".baryon-import.json";
const BLOCK: usize = 4096;
const MAX_REDIRECTS: usize = 10;

pub(crate) async fn import(
    url: &str,
    sha256: Option<&str>,
    size_bytes: u64,
    dir: &Path,
) -> anyhow::Result<()> {
    if url.starts_with("http://") && sha256.is_none() {
        bail!("a plain HTTP source needs a SHA-256 digest");
    }
    let image = dir.join(DISK_IMAGE_NAME);
    let marker = dir.join(MARKER);
    if image.exists() {
        match std::fs::read_to_string(&marker) {
            Ok(m) if marker_matches(&m, url) => {
                tracing::info!(image = %image.display(), "disk already imported");
                return Ok(());
            }
            _ => bail!(
                "{} exists but was not imported from {url}; refusing to overwrite it",
                image.display()
            ),
        }
    }

    let partial = dir.join(format!("{DISK_IMAGE_NAME}.partial"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&partial)
        .with_context(|| format!("creating {}", partial.display()))?;

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);

    let mut uri: Uri = url.parse().with_context(|| format!("invalid URL {url}"))?;
    let mut response = None;
    for _ in 0..MAX_REDIRECTS {
        let resp = client
            .get(uri.clone())
            .await
            .with_context(|| format!("GET {uri}"))?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("redirect without Location")?;
            uri = resolve_location(&uri, location)?;
            tracing::info!(%uri, "following redirect");
            continue;
        }
        if resp.status() != StatusCode::OK {
            bail!("GET {uri} returned {}", resp.status());
        }
        response = Some(resp);
        break;
    }
    let mut body = response.context("too many redirects")?.into_body();

    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut carry: Vec<u8> = Vec::with_capacity(BLOCK);
    let mut last_log = 0u64;
    while let Some(frame) = body.frame().await {
        let frame = frame.context("reading response body")?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        hasher.update(data);
        carry.extend_from_slice(data);
        let full = carry.len() / BLOCK * BLOCK;
        write_sparse(&mut file, &carry[..full], &mut written)?;
        carry.drain(..full);
        if written - last_log >= 256 << 20 {
            tracing::info!(bytes = written, "importing");
            last_log = written;
        }
    }
    write_sparse(&mut file, &carry, &mut written)?;

    let digest = to_hex(&hasher.finalize());
    if let Some(expected) = sha256
        && !expected.eq_ignore_ascii_case(&digest)
    {
        let _ = std::fs::remove_file(&partial);
        bail!("SHA-256 mismatch: expected {expected}, got {digest}");
    }
    if written > size_bytes {
        let _ = std::fs::remove_file(&partial);
        bail!("image is {written} bytes, larger than the requested disk size {size_bytes}");
    }
    file.set_len(size_bytes).context("extending disk image")?;
    file.sync_all().context("syncing disk image")?;
    drop(file);
    std::fs::rename(&partial, &image).context("renaming disk image")?;
    let marker_json = serde_json::json!({ "url": url, "sha256": digest, "imageBytes": written });
    std::fs::write(&marker, marker_json.to_string()).context("writing import marker")?;
    tracing::info!(image = %image.display(), bytes = written, size = size_bytes, sha256 = %digest, "import complete");
    Ok(())
}

/// Write `data` at the current length, seeking over all-zero blocks.
fn write_sparse(file: &mut std::fs::File, data: &[u8], written: &mut u64) -> anyhow::Result<()> {
    for chunk in data.chunks(BLOCK) {
        if chunk.iter().all(|b| *b == 0) {
            file.seek(SeekFrom::Current(chunk.len() as i64))?;
        } else {
            file.write_all(chunk)?;
        }
        *written += chunk.len() as u64;
    }
    // A trailing hole must still count toward the length.
    file.set_len((*written).max(file.metadata()?.len()))?;
    Ok(())
}

/// Resolve a redirect target. A redirect from HTTPS to plain HTTP is
/// refused: it would silently drop transport integrity for an image that may
/// have no digest.
fn resolve_location(base: &Uri, location: &str) -> anyhow::Result<Uri> {
    let target: Uri = if location.starts_with("http://") || location.starts_with("https://") {
        location.parse().context("invalid redirect location")?
    } else {
        let scheme = base.scheme_str().unwrap_or("https");
        let authority = base.authority().context("redirect base has no authority")?;
        let path = if location.starts_with('/') {
            location.to_owned()
        } else {
            // Relative to the directory of the current path.
            let dir = base.path().rsplit_once('/').map_or("", |(d, _)| d);
            format!("{dir}/{location}")
        };
        format!("{scheme}://{authority}{path}")
            .parse()
            .context("invalid redirect location")?
    };
    if base.scheme_str() == Some("https") && target.scheme_str() != Some("https") {
        bail!("refusing redirect from {base} to non-HTTPS {target}");
    }
    Ok(target)
}

fn marker_matches(marker: &str, url: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(marker)
        .ok()
        .and_then(|v| v.get("url").and_then(|u| u.as_str()).map(|u| u == url))
        .unwrap_or(false)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn sparse_write_skips_zero_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img");
        let mut f = std::fs::File::create(&path).unwrap();
        let mut data = vec![0u8; 1 << 20];
        data[0] = 1;
        data[(1 << 20) - 1] = 2;
        let mut written = 0;
        write_sparse(&mut f, &data, &mut written).unwrap();
        drop(f);
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1 << 20);
        assert_eq!(written, 1 << 20);
        // Only two blocks are allocated (when the filesystem supports holes).
        assert!(meta.blocks() * 512 < 1 << 20);
        let back = std::fs::read(&path).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn trailing_zero_block_keeps_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img");
        let mut f = std::fs::File::create(&path).unwrap();
        let mut written = 0;
        write_sparse(&mut f, &[0u8; 100], &mut written).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 100);
    }

    #[test]
    fn resolves_redirects() {
        let base: Uri = "https://cloud.example.org/images/x.raw".parse().unwrap();
        assert_eq!(
            resolve_location(&base, "https://mirror.example.net/a.raw")
                .unwrap()
                .to_string(),
            "https://mirror.example.net/a.raw"
        );
        assert_eq!(
            resolve_location(&base, "/other/a.raw").unwrap().to_string(),
            "https://cloud.example.org/other/a.raw"
        );
        assert_eq!(
            resolve_location(&base, "y.raw").unwrap().to_string(),
            "https://cloud.example.org/images/y.raw"
        );
    }

    #[test]
    fn refuses_https_to_http_redirect() {
        let base: Uri = "https://cloud.example.org/images/x.raw".parse().unwrap();
        assert!(resolve_location(&base, "http://mirror.example.net/a.raw").is_err());
        let plain: Uri = "http://cloud.example.org/x.raw".parse().unwrap();
        assert!(resolve_location(&plain, "https://mirror.example.net/a.raw").is_ok());
    }

    #[test]
    fn marker() {
        assert!(marker_matches(r#"{"url":"https://a/b"}"#, "https://a/b"));
        assert!(!marker_matches(r#"{"url":"https://a/c"}"#, "https://a/b"));
        assert!(!marker_matches("garbage", "https://a/b"));
    }

    #[tokio::test]
    async fn imports_from_local_http_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut image = vec![0u8; 3 * BLOCK];
        image[BLOCK] = 0xab;
        let served = image.clone();
        tokio::spawn(async move {
            // First request: redirect. Second: the image.
            for i in 0..2 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 2048];
                let _ = s.read(&mut buf).await.unwrap();
                if i == 0 {
                    s.write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /real.raw\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await
                    .unwrap();
                } else {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                        served.len()
                    );
                    s.write_all(head.as_bytes()).await.unwrap();
                    s.write_all(&served).await.unwrap();
                }
                s.shutdown().await.unwrap();
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let url = format!("http://{addr}/disk.raw");
        let digest = to_hex(&Sha256::digest(&image));
        import(&url, Some(&digest), 1 << 20, dir.path())
            .await
            .unwrap();
        let out = std::fs::read(dir.path().join(DISK_IMAGE_NAME)).unwrap();
        assert_eq!(out.len(), 1 << 20);
        // The image belongs to the importing identity (the VM's) and is not
        // readable by anyone else.
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(dir.path().join(DISK_IMAGE_NAME)).unwrap();
            assert_eq!(meta.mode() & 0o777, 0o600);
            let me = std::fs::metadata(dir.path()).unwrap().uid();
            assert_eq!(meta.uid(), me);
        }
        assert_eq!(&out[..image.len()], &image[..]);
        // A second import with the same URL is a no-op.
        import(&url, Some(&digest), 1 << 20, dir.path())
            .await
            .unwrap();
        // A different URL refuses to overwrite.
        assert!(
            import("http://127.0.0.1:1/x", None, 1 << 20, dir.path())
                .await
                .is_err()
        );
    }
}
