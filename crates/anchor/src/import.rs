//! `baryon-anchor import`: write a raw disk image from an HTTP(S) URL into
//! the root disk volume.
//!
//! The image is streamed and its SHA-256 is verified when given.
//!
//! - Filesystem volume: the image becomes `disk.img`; all-zero 4 KiB blocks
//!   are skipped so the file is sparse, and the file is extended to the
//!   requested virtual disk size. A marker file next to it records the
//!   import.
//! - Block volume: the image is written to the start of the device. Runs of
//!   all-zero blocks, and the rest of the device after the image, are zeroed
//!   with zero-range requests (written as zeros where the device does not
//!   support them), because a new volume may hold old data. There is no
//!   marker on the device; baryon-controller records the completed import on
//!   the claim.
//!
//! This is a convenience for importing a boot image, not an image management
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

/// Where the image goes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Target<'a> {
    /// A directory on a filesystem volume.
    File(&'a Path),
    /// A raw block device.
    Device(&'a Path),
}

pub(crate) async fn import(
    url: &str,
    sha256: Option<&str>,
    size_bytes: u64,
    target: Target<'_>,
) -> anyhow::Result<()> {
    if url.starts_with("http://") && sha256.is_none() {
        bail!("a plain HTTP source needs a SHA-256 digest");
    }
    match target {
        Target::File(dir) => import_file(url, sha256, size_bytes, dir).await,
        Target::Device(dev) => import_device(url, sha256, size_bytes, dev).await,
    }
}

async fn import_device(
    url: &str,
    sha256: Option<&str>,
    size_bytes: u64,
    path: &Path,
) -> anyhow::Result<()> {
    // O_EXCL on a block device is an exclusive open: it fails while the
    // device is mounted or opened exclusively elsewhere.
    let mut dev = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(rustix::fs::OFlags::EXCL.bits() as i32)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let dev_size = dev
        .seek(SeekFrom::End(0))
        .context("reading the device size")?;
    dev.seek(SeekFrom::Start(0))?;
    if size_bytes > dev_size {
        bail!("the requested disk size {size_bytes} is larger than the device ({dev_size} bytes)");
    }
    let mut sink = DeviceSink::new(dev, dev_size);
    let body = fetch(url).await?;
    let (written, digest) = stream(body, &mut sink).await?;
    if let Some(expected) = sha256
        && !expected.eq_ignore_ascii_case(&digest)
    {
        bail!("SHA-256 mismatch: expected {expected}, got {digest}");
    }
    if written > size_bytes {
        bail!("image is {written} bytes, larger than the requested disk size {size_bytes}");
    }
    sink.finish().context("zeroing the rest of the device")?;
    tracing::info!(device = %path.display(), bytes = written, device_bytes = dev_size, sha256 = %digest, "import complete");
    Ok(())
}

async fn import_file(
    url: &str,
    sha256: Option<&str>,
    size_bytes: u64,
    dir: &Path,
) -> anyhow::Result<()> {
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

    let body = fetch(url).await?;
    let mut sink = FileSink { file: &mut file };
    let (written, digest) = stream(body, &mut sink).await?;
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

/// GET `url`, following redirects, and return the response body.
async fn fetch(url: &str) -> anyhow::Result<hyper::body::Incoming> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);
    let mut uri: Uri = url.parse().with_context(|| format!("invalid URL {url}"))?;
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
        return Ok(resp.into_body());
    }
    bail!("too many redirects")
}

/// Stream `body` into `sink` in 4 KiB blocks. Returns the image length and
/// its SHA-256.
async fn stream(
    mut body: hyper::body::Incoming,
    sink: &mut dyn Sink,
) -> anyhow::Result<(u64, String)> {
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
        write_blocks(sink, &carry[..full], &mut written)?;
        carry.drain(..full);
        if written - last_log >= 256 << 20 {
            tracing::info!(bytes = written, "importing");
            last_log = written;
        }
    }
    write_blocks(sink, &carry, &mut written)?;
    Ok((written, to_hex(&hasher.finalize())))
}

/// Destination of image blocks.
trait Sink {
    fn data(&mut self, chunk: &[u8]) -> std::io::Result<()>;
    fn zeros(&mut self, len: u64) -> std::io::Result<()>;
}

fn write_blocks(sink: &mut dyn Sink, data: &[u8], written: &mut u64) -> anyhow::Result<()> {
    for chunk in data.chunks(BLOCK) {
        if chunk.iter().all(|b| *b == 0) {
            sink.zeros(chunk.len() as u64)?;
        } else {
            sink.data(chunk)?;
        }
        *written += chunk.len() as u64;
    }
    Ok(())
}

/// A sparse image file: zero blocks become holes.
struct FileSink<'a> {
    file: &'a mut std::fs::File,
}

impl Sink for FileSink<'_> {
    fn data(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.file.write_all(chunk)
    }
    fn zeros(&mut self, len: u64) -> std::io::Result<()> {
        self.file
            .seek(SeekFrom::Current(i64::try_from(len).unwrap_or(i64::MAX)))?;
        // A trailing hole must still count toward the length.
        let pos = self.file.stream_position()?;
        if self.file.metadata()?.len() < pos {
            self.file.set_len(pos)?;
        }
        Ok(())
    }
}

/// A block device: zero runs are zeroed explicitly, coalesced into one
/// request per run.
struct DeviceSink {
    dev: std::fs::File,
    size: u64,
    pos: u64,
    zero_from: Option<u64>,
}

impl DeviceSink {
    fn new(dev: std::fs::File, size: u64) -> Self {
        Self {
            dev,
            size,
            pos: 0,
            zero_from: None,
        }
    }

    fn flush_zeros(&mut self) -> std::io::Result<()> {
        if let Some(from) = self.zero_from.take() {
            zero_range(&self.dev, from, self.pos - from)?;
        }
        Ok(())
    }

    /// Zero everything after the image and make the writes durable.
    fn finish(mut self) -> std::io::Result<()> {
        let end = self.size;
        if self.zero_from.is_none() && self.pos < end {
            self.zero_from = Some(self.pos);
        }
        self.pos = end;
        self.flush_zeros()?;
        self.dev.sync_all()
    }
}

impl Sink for DeviceSink {
    fn data(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        self.flush_zeros()?;
        self.dev.seek(SeekFrom::Start(self.pos))?;
        self.dev.write_all(chunk)?;
        self.pos += chunk.len() as u64;
        Ok(())
    }
    fn zeros(&mut self, len: u64) -> std::io::Result<()> {
        if self.zero_from.is_none() {
            self.zero_from = Some(self.pos);
        }
        self.pos += len;
        Ok(())
    }
}

/// Zero `len` bytes at `offset` of a block device: a zero-range request
/// (`FALLOC_FL_ZERO_RANGE`, which the block layer turns into write-zeroes or
/// zero writes), or plain zero writes where the request is not supported.
fn zero_range(dev: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use rustix::fs::FallocateFlags;
    if len == 0 {
        return Ok(());
    }
    match rustix::fs::fallocate(
        dev,
        FallocateFlags::ZERO_RANGE | FallocateFlags::KEEP_SIZE,
        offset,
        len,
    ) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::INVAL) => {
            use std::os::unix::fs::FileExt;
            let zeros = vec![0u8; 1 << 20];
            let mut done = 0u64;
            while done < len {
                let n = (len - done).min(zeros.len() as u64) as usize;
                dev.write_all_at(&zeros[..n], offset + done)?;
                done += n as u64;
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
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
        write_blocks(&mut FileSink { file: &mut f }, &data, &mut written).unwrap();
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
        write_blocks(&mut FileSink { file: &mut f }, &[0u8; 100], &mut written).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 100);
    }

    #[test]
    fn device_sink_zeroes_old_data_in_holes_and_tail() {
        // A regular file stands in for a used block device: fill it with
        // garbage, then import an image with a hole into the first part.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev");
        std::fs::write(&path, vec![0xee_u8; 8 * BLOCK]).unwrap();
        let dev = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let mut sink = DeviceSink::new(dev, 8 * BLOCK as u64);
        let mut image = vec![0u8; 3 * BLOCK];
        image[0] = 1; // block 0 data, block 1 zero, block 2 data
        image[2 * BLOCK] = 2;
        let mut written = 0;
        write_blocks(&mut sink, &image, &mut written).unwrap();
        sink.finish().unwrap();
        let out = std::fs::read(&path).unwrap();
        assert_eq!(out.len(), 8 * BLOCK, "device size unchanged");
        assert_eq!(&out[..3 * BLOCK], &image[..], "image written, hole zeroed");
        assert!(out[3 * BLOCK..].iter().all(|b| *b == 0), "tail zeroed");
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
        import(&url, Some(&digest), 1 << 20, Target::File(dir.path()))
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
        import(&url, Some(&digest), 1 << 20, Target::File(dir.path()))
            .await
            .unwrap();
        // A different URL refuses to overwrite.
        assert!(
            import(
                "http://127.0.0.1:1/x",
                None,
                1 << 20,
                Target::File(dir.path())
            )
            .await
            .is_err()
        );
    }
}
