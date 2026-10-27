//! `baryon-importer import`: write a raw disk image from an HTTP(S) URL into
//! the root disk volume.
//!
//! The image is streamed and its SHA-256 is verified when given.
//!
//! - Filesystem volume: the image becomes `disk.img`; all-zero 4 KiB blocks
//!   are skipped so the file is sparse, and the file is extended to the
//!   requested virtual disk size. A marker file next to it records the
//!   import.
//! - Block volume: the image is written to the start of the device. Runs of
//!   all-zero blocks, and the rest of the device after the image, are
//!   zeroed, because a new volume may hold old data: with write-zeroes that
//!   may deallocate (`FALLOC_FL_PUNCH_HOLE`), else with `FALLOC_FL_ZERO_RANGE`,
//!   else with written zeros. The first and last MiB of the device are
//!   zeroed before the download, and image data in them is written only
//!   after the image's size and SHA-256 are verified, so a failed import
//!   leaves no partition table, boot loader, or backup GPT behind. After a
//!   successful import every byte of the device is the image's or zero. There is no marker on the device; baryon-controller records the
//!   completed import on the claim's PersistentVolume.
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
/// Size of the region at each end of a device that is zeroed before an
/// import and receives image data only after verification.
const HEAD: u64 = 1 << 20;
const MAX_REDIRECTS: usize = 10;

/// Where the image goes.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Target<'a> {
    /// A directory on a filesystem volume.
    File(&'a Path),
    /// A raw block device.
    Device(&'a Path),
}

/// How long the importer waits for its approval before it fails and the
/// pod restarts it.
const APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Wait until `file` holds the approval, the claim UID `expected`. Nothing
/// is written to the disk before.
pub(crate) async fn wait_for_approval(file: &Path, expected: &str) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let mut logged = false;
    loop {
        match std::fs::read_to_string(file) {
            Ok(v) if !expected.is_empty() && v.trim() == expected => {
                tracing::info!("import approved");
                return Ok(());
            }
            Ok(_) | Err(_) if start.elapsed() >= APPROVAL_TIMEOUT => {
                bail!(
                    "the import was not approved within {} s",
                    APPROVAL_TIMEOUT.as_secs()
                );
            }
            _ => {}
        }
        if !logged {
            tracing::info!("waiting for baryon-controller to approve the import");
            logged = true;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
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
    use std::os::unix::fs::FileTypeExt;
    // O_EXCL on a block device is an exclusive open against other exclusive
    // openers (mounts, other importers); it does not exclude a VMM, which
    // opens without it. O_NOFOLLOW: the container runtime creates the node.
    let mut dev = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags((rustix::fs::OFlags::EXCL | rustix::fs::OFlags::NOFOLLOW).bits() as i32)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !dev.metadata()?.file_type().is_block_device() {
        bail!("{} is not a block device", path.display());
    }
    // Zeroing is aligned to BLOCK; larger logical blocks are not supported.
    let lbs = rustix::fs::ioctl_blksszget(&dev).context("reading the logical block size")?;
    if lbs == 0 || lbs as usize > BLOCK || !BLOCK.is_multiple_of(lbs as usize) {
        bail!(
            "{} has a logical block size of {lbs} bytes; at most {BLOCK} is supported",
            path.display()
        );
    }
    let dev_size = dev
        .seek(SeekFrom::End(0))
        .context("reading the device size")?;
    dev.seek(SeekFrom::Start(0))?;
    if size_bytes > dev_size {
        bail!("the requested disk size {size_bytes} is larger than the device ({dev_size} bytes)");
    }
    let mut sink = DeviceSink::new(dev, dev_size).context("clearing both ends of the device")?;
    let download = fetch(url).await?;
    let (written, digest) = stream(download.body, &mut sink, size_bytes).await?;
    check_length(written, download.length, sha256)?;
    if let Some(expected) = sha256
        && !expected.eq_ignore_ascii_case(&digest)
    {
        bail!("SHA-256 mismatch: expected {expected}, got {digest}");
    }
    sink.finish().context("completing the device")?;
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
                "{} exists but was not imported from {}; refusing to overwrite it",
                image.display(),
                url.parse::<Uri>()
                    .map_or_else(|_| "the source URL".to_owned(), |u| redacted(&u))
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

    let download = fetch(url).await?;
    let mut sink = FileSink { file: &mut file };
    let streamed = stream(download.body, &mut sink, size_bytes)
        .await
        .and_then(|(w, d)| check_length(w, download.length, sha256).map(|()| (w, d)));
    let (written, digest) = match streamed {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
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
/// A response body and its announced length.
struct Download {
    body: hyper::body::Incoming,
    length: Option<u64>,
}

async fn fetch(url: &str) -> anyhow::Result<Download> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client: Client<_, Empty<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);
    // The URL is not repeated in errors: it may carry a token in its query.
    let mut uri: Uri = url.parse().context("the source URL is not a valid URI")?;
    refuse_userinfo(&uri)?;
    for _ in 0..MAX_REDIRECTS {
        let resp = client
            .get(uri.clone())
            .await
            .with_context(|| format!("GET {}", redacted(&uri)))?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("redirect without Location")?;
            uri = resolve_location(&uri, location)?;
            tracing::info!(uri = %redacted(&uri), "following redirect");
            continue;
        }
        if resp.status() != StatusCode::OK {
            bail!("GET {} returned {}", redacted(&uri), resp.status());
        }
        let length = resp
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        return Ok(Download {
            body: resp.into_body(),
            length,
        });
    }
    bail!("too many redirects")
}

/// Refuse an image whose length differs from the announced one, or, without
/// a digest, an image of unknown length: a connection closed early would
/// otherwise look like a complete image.
fn check_length(written: u64, length: Option<u64>, sha256: Option<&str>) -> anyhow::Result<()> {
    match length {
        Some(l) if l != written => {
            bail!("the download ended after {written} of {l} bytes")
        }
        None if sha256.is_none() => {
            bail!(
                "the server sent no Content-Length and no SHA-256 was given; a truncated download could not be detected"
            )
        }
        _ => Ok(()),
    }
}

/// Stream `body` into `sink` in 4 KiB blocks, refusing to write past
/// `limit` bytes. Returns the image length and its SHA-256.
async fn stream(
    mut body: hyper::body::Incoming,
    sink: &mut dyn Sink,
    limit: u64,
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
        write_blocks(sink, &carry[..full], &mut written, limit)?;
        carry.drain(..full);
        if written - last_log >= 256 << 20 {
            tracing::info!(bytes = written, "importing");
            last_log = written;
        }
    }
    write_blocks(sink, &carry, &mut written, limit)?;
    Ok((written, to_hex(&hasher.finalize())))
}

/// Destination of image blocks.
trait Sink {
    fn data(&mut self, chunk: &[u8]) -> std::io::Result<()>;
    fn zeros(&mut self, len: u64) -> std::io::Result<()>;
}

fn write_blocks(
    sink: &mut dyn Sink,
    data: &[u8],
    written: &mut u64,
    limit: u64,
) -> anyhow::Result<()> {
    for chunk in data.chunks(BLOCK) {
        if written.saturating_add(chunk.len() as u64) > limit {
            bail!("the image is larger than the requested disk size {limit}");
        }
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

/// How a device range is zeroed: [`zero_range`] in production; tests
/// substitute a method to exercise each fallback.
type ZeroFn = fn(&std::fs::File, u64, u64) -> std::io::Result<()>;

/// A block device. The import contract: after [`DeviceSink::finish`], every
/// byte of the device is either the image's byte at that offset or zero.
///
/// Zero runs of the image and everything after it are zeroed explicitly,
/// coalesced into one request per run; nothing relies on the device having
/// been zero or on sparse writes leaving zeros. The first [`HEAD`] bytes of
/// the image, and image bytes in the device's last [`HEAD`] bytes, are kept
/// in memory and written only by [`DeviceSink::finish`], after the image is
/// verified; both regions are zeroed before the download, so a failed
/// import leaves neither a partition table, a boot loader, nor a backup
/// GPT header from the image or from earlier use of the device.
struct DeviceSink {
    dev: std::fs::File,
    size: u64,
    pos: u64,
    zero_from: Option<u64>,
    head: Vec<u8>,
    /// Start of the protected region at the end of the device.
    tail_start: u64,
    tail: Vec<u8>,
    zero: ZeroFn,
}

impl DeviceSink {
    fn new(dev: std::fs::File, size: u64) -> std::io::Result<Self> {
        Self::with_zero(dev, size, zero_range)
    }

    /// Zero both protected regions, so a failed import leaves nothing
    /// bootable at either end of the device.
    fn with_zero(dev: std::fs::File, size: u64, zero: ZeroFn) -> std::io::Result<Self> {
        let block = BLOCK as u64;
        // BLOCK-aligned and never inside the head region.
        let tail_start = (size.saturating_sub(HEAD) / block * block).max(HEAD.min(size));
        zero(&dev, 0, size.min(HEAD))?;
        zero(&dev, tail_start, size - tail_start)?;
        // Durable before the download: an interrupted import must not leave
        // the old ends on the device after a power loss.
        dev.sync_all()?;
        Ok(Self {
            dev,
            size,
            pos: 0,
            zero_from: None,
            head: Vec::new(),
            tail_start,
            tail: Vec::new(),
            zero,
        })
    }

    /// Whether the current position is in a region held until `finish`.
    fn held(&self) -> bool {
        self.pos < HEAD || self.pos >= self.tail_start
    }

    fn hold(&mut self, chunk: Option<&[u8]>, len: u64) {
        let buf = if self.pos < HEAD {
            &mut self.head
        } else {
            &mut self.tail
        };
        match chunk {
            Some(c) => buf.extend_from_slice(c),
            None => buf.resize(buf.len() + len as usize, 0),
        }
        self.pos += len;
    }

    fn flush_zeros(&mut self) -> std::io::Result<()> {
        if let Some(from) = self.zero_from.take() {
            (self.zero)(&self.dev, from, self.pos - from)?;
        }
        Ok(())
    }

    /// Zero everything after the image, write the verified head and tail,
    /// and make the writes durable.
    fn finish(mut self) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.flush_zeros()?;
        if self.pos < self.size {
            (self.zero)(&self.dev, self.pos, self.size - self.pos)?;
        }
        self.dev.write_all_at(&self.tail, self.tail_start)?;
        self.dev.write_all_at(&self.head, 0)?;
        self.dev.sync_all()
    }
}

impl Sink for DeviceSink {
    fn data(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        // Chunks start BLOCK-aligned, and HEAD and tail_start are multiples
        // of BLOCK, so a chunk never straddles a region boundary.
        if self.held() {
            self.flush_zeros()?;
            self.hold(Some(chunk), chunk.len() as u64);
            return Ok(());
        }
        self.flush_zeros()?;
        self.dev.seek(SeekFrom::Start(self.pos))?;
        self.dev.write_all(chunk)?;
        self.pos += chunk.len() as u64;
        Ok(())
    }
    fn zeros(&mut self, len: u64) -> std::io::Result<()> {
        if self.held() {
            // Already zero on the device; keep the buffer contiguous.
            self.flush_zeros()?;
            self.hold(None, len);
            return Ok(());
        }
        if self.zero_from.is_none() {
            self.zero_from = Some(self.pos);
        }
        self.pos += len;
        Ok(())
    }
}

/// Zero `len` bytes at `offset` of a block device. Parts not aligned to
/// [`BLOCK`] are written as zeros; the aligned middle is zeroed by
/// [`zero_aligned`]. Devices whose logical block size does not divide
/// [`BLOCK`] are refused before the import.
fn zero_range(dev: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let end = offset.saturating_add(len);
    let block = BLOCK as u64;
    let start_aligned = offset.div_ceil(block) * block;
    let end_aligned = end / block * block;
    if start_aligned >= end_aligned {
        return write_zeros(dev, offset, len);
    }
    dev.write_all_at(&vec![0u8; (start_aligned - offset) as usize], offset)?;
    zero_aligned(dev, start_aligned, end_aligned - start_aligned)?;
    dev.write_all_at(&vec![0u8; (end - end_aligned) as usize], end_aligned)
}

/// Zero an aligned range: write-zeroes that may deallocate
/// (`FALLOC_FL_PUNCH_HOLE`, so thin-provisioned storage is not filled),
/// else `FALLOC_FL_ZERO_RANGE` (which the block layer completes with zero
/// writes), else written zeros. Only "not supported" falls through; other
/// errors are reported.
fn zero_aligned(dev: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
    use rustix::fs::FallocateFlags;
    for flags in [FallocateFlags::PUNCH_HOLE, FallocateFlags::ZERO_RANGE] {
        match rustix::fs::fallocate(dev, flags | FallocateFlags::KEEP_SIZE, offset, len) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::OPNOTSUPP) => {}
            Err(e) => return Err(e.into()),
        }
    }
    write_zeros(dev, offset, len)
}

fn write_zeros(dev: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
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
        bail!(
            "refusing redirect from {} to non-HTTPS {}",
            redacted(base),
            redacted(&target)
        );
    }
    refuse_userinfo(&target)?;
    Ok(target)
}

/// Refuse a URI with userinfo (`user:password@`): credentials do not belong
/// in a VM's source URL or in a redirect target. The API server refuses them
/// in new VirtualMachines; this also covers older objects and redirects.
fn refuse_userinfo(uri: &Uri) -> anyhow::Result<()> {
    if uri.authority().is_some_and(|a| a.as_str().contains('@')) {
        bail!(
            "refusing a URL with credentials (user:password@) for {}",
            redacted(uri)
        );
    }
    Ok(())
}

/// `uri` for logs and messages: scheme, host, port, and path, without
/// userinfo or query, which may carry credentials or a signed token.
fn redacted(uri: &Uri) -> String {
    format!(
        "{}://{}{}{}",
        uri.scheme_str().unwrap_or("?"),
        uri.host().unwrap_or("?"),
        uri.port().map(|p| format!(":{p}")).unwrap_or_default(),
        uri.path()
    )
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
        write_blocks(
            &mut FileSink { file: &mut f },
            &data,
            &mut written,
            u64::MAX,
        )
        .unwrap();
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
        write_blocks(
            &mut FileSink { file: &mut f },
            &[0u8; 100],
            &mut written,
            u64::MAX,
        )
        .unwrap();
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
        let mut sink = DeviceSink::new(dev, 8 * BLOCK as u64).unwrap();
        let mut image = vec![0u8; 3 * BLOCK];
        image[0] = 1; // block 0 data, block 1 zero, block 2 data
        image[2 * BLOCK] = 2;
        let mut written = 0;
        write_blocks(&mut sink, &image, &mut written, u64::MAX).unwrap();
        sink.finish().unwrap();
        let out = std::fs::read(&path).unwrap();
        assert_eq!(out.len(), 8 * BLOCK, "device size unchanged");
        assert_eq!(&out[..3 * BLOCK], &image[..], "image written, hole zeroed");
        assert!(out[3 * BLOCK..].iter().all(|b| *b == 0), "tail zeroed");
    }

    /// A used device larger than HEAD, with an image that has data past
    /// HEAD, a zero run, and an unaligned length.
    fn used_device(blocks: usize) -> (tempfile::TempDir, std::path::PathBuf, std::fs::File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev");
        // 512 bytes past a BLOCK boundary, like a device whose size is a
        // multiple of 512 only.
        std::fs::write(&path, vec![0xee_u8; blocks * BLOCK + 512]).unwrap();
        let dev = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        (dir, path, dev)
    }

    #[test]
    fn device_import_beyond_head_with_unaligned_end() {
        let blocks = (HEAD as usize) / BLOCK + 16;
        let (_dir, path, dev) = used_device(blocks);
        let size = (blocks * BLOCK + 512) as u64;
        let mut sink = DeviceSink::new(dev, size).unwrap();
        let mut image = vec![0u8; HEAD as usize + 5 * BLOCK + 100];
        image[7] = 7;
        image[HEAD as usize + 1] = 9; // data after HEAD
        // blocks HEAD+1..HEAD+4 zero, then a partial final block with data
        image[HEAD as usize + 5 * BLOCK + 50] = 5;
        let mut written = 0;
        write_blocks(&mut sink, &image, &mut written, size).unwrap();
        sink.finish().unwrap();
        let out = std::fs::read(&path).unwrap();
        assert_eq!(out.len() as u64, size);
        assert_eq!(&out[..image.len()], &image[..]);
        assert!(out[image.len()..].iter().all(|b| *b == 0), "tail zeroed");
    }

    #[test]
    fn a_failed_import_leaves_no_head() {
        let blocks = (HEAD as usize) / BLOCK + 4;
        let (_dir, path, dev) = used_device(blocks);
        let mut sink = DeviceSink::new(dev, (blocks * BLOCK + 512) as u64).unwrap();
        let image = vec![1u8; HEAD as usize + BLOCK];
        let mut written = 0;
        write_blocks(&mut sink, &image, &mut written, u64::MAX).unwrap();
        // Verification fails: finish() is never called.
        drop(sink);
        let out = std::fs::read(&path).unwrap();
        assert!(out[..HEAD as usize].iter().all(|b| *b == 0), "head cleared");
    }

    #[test]
    fn the_size_limit_holds_before_writing() {
        let (_dir, path, dev) = used_device(4);
        let mut sink = DeviceSink::new(dev, (4 * BLOCK + 512) as u64).unwrap();
        let mut written = 0;
        let image = vec![3u8; 3 * BLOCK];
        assert!(write_blocks(&mut sink, &image, &mut written, 2 * BLOCK as u64).is_err());
        assert_eq!(written, 2 * BLOCK as u64);
        drop(sink);
        let out = std::fs::read(&path).unwrap();
        assert!(!out.contains(&3u8), "nothing past the head was written");
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
        // Credentials in a redirect target are refused; logs and messages
        // never show userinfo or the query.
        assert!(resolve_location(&base, "https://u:p@mirror.example.net/a.raw").is_err());
        let signed: Uri = "https://h.example.org:8443/d/a.raw?X-Signature=secret"
            .parse()
            .unwrap();
        assert_eq!(redacted(&signed), "https://h.example.org:8443/d/a.raw");
        let creds: Uri = "https://user:pw@h.example.org/a.raw".parse().unwrap();
        let err = refuse_userinfo(&creds).unwrap_err().to_string();
        assert!(!err.contains("pw") && !err.contains("user:pw"), "{err}");
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

    /// A recognizable pattern standing in for an earlier user's data.
    const STALE: &[u8] = b"BARYON-STALE-DATA-";

    fn stale_device(bytes: usize) -> (tempfile::TempDir, std::path::PathBuf, std::fs::File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev");
        let data: Vec<u8> = STALE.iter().copied().cycle().take(bytes).collect();
        std::fs::write(&path, data).unwrap();
        let dev = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        (dir, path, dev)
    }

    fn contains_stale(buf: &[u8]) -> bool {
        buf.windows(STALE.len()).any(|w| w == STALE)
    }

    /// FALLOC_FL_ZERO_RANGE, then zero writes: a device on which
    /// write-zeroes that may deallocate is not supported.
    fn zero_range_only(dev: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
        use rustix::fs::FallocateFlags;
        match rustix::fs::fallocate(
            dev,
            FallocateFlags::ZERO_RANGE | FallocateFlags::KEEP_SIZE,
            offset,
            len,
        ) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::OPNOTSUPP) => write_zeros(dev, offset, len),
            Err(e) => Err(e.into()),
        }
    }

    /// Only plain zero writes, as on a device without write-zeroes support.
    fn zeros_only(dev: &std::fs::File, offset: u64, len: u64) -> std::io::Result<()> {
        write_zeros(dev, offset, len)
    }

    /// A sparse image (data, a long zero run, data in the last MiB of the
    /// device) imported onto a device full of stale data, with every
    /// zeroing method: each device byte is the image's or zero.
    #[test]
    fn import_contract_holds_for_every_zeroing_method() {
        let methods: [(&str, ZeroFn); 3] = [
            ("default", zero_range),
            ("zero writes only", zeros_only),
            ("without write-zeroes with unmap", zero_range_only),
        ];
        for (name, zero) in methods {
            // 6 MiB plus 512 bytes: unaligned end, like a device whose size
            // is only a multiple of 512.
            let size = 6 * HEAD as usize + 512;
            let (_dir, path, dev) = stale_device(size);
            let mut sink = DeviceSink::with_zero(dev, size as u64, zero).unwrap();
            let mut image = vec![0u8; size - 1000];
            image[..BLOCK].fill(1); // head data
            image[2 * HEAD as usize..2 * HEAD as usize + BLOCK].fill(2); // middle
            // zero run from there to the last MiB; data in the last MiB
            let last = size - 2 * BLOCK;
            image[last..last + 100].fill(3);
            let mut written = 0;
            write_blocks(&mut sink, &image, &mut written, size as u64).unwrap();
            sink.finish().unwrap();
            let out = std::fs::read(&path).unwrap();
            assert_eq!(out.len(), size, "{name}");
            assert_eq!(&out[..image.len()], &image[..], "{name}: image bytes");
            assert!(
                out[image.len()..].iter().all(|b| *b == 0),
                "{name}: tail zero"
            );
            assert!(!contains_stale(&out), "{name}: no stale data");
        }
    }

    /// A device much larger than the image: everything after the image is
    /// zero.
    #[test]
    fn a_small_image_on_a_large_device() {
        let size = 8 * HEAD as usize;
        let (_dir, path, dev) = stale_device(size);
        let mut sink = DeviceSink::new(dev, size as u64).unwrap();
        let image = vec![7u8; 3 * BLOCK + 17];
        let mut written = 0;
        write_blocks(&mut sink, &image, &mut written, size as u64).unwrap();
        sink.finish().unwrap();
        let out = std::fs::read(&path).unwrap();
        assert_eq!(&out[..image.len()], &image[..]);
        assert!(out[image.len()..].iter().all(|b| *b == 0));
    }

    /// A failed import (verification failed, finish not called) leaves
    /// zeros at both ends of the device, even where the image reached the
    /// last MiB.
    #[test]
    fn a_failed_import_clears_both_ends() {
        let size = 4 * HEAD as usize;
        let (_dir, path, dev) = stale_device(size);
        let mut sink = DeviceSink::new(dev, size as u64).unwrap();
        let image = vec![9u8; size];
        let mut written = 0;
        write_blocks(&mut sink, &image, &mut written, size as u64).unwrap();
        drop(sink);
        let out = std::fs::read(&path).unwrap();
        let tail_start = size - HEAD as usize;
        assert!(out[..HEAD as usize].iter().all(|b| *b == 0), "head clear");
        assert!(out[tail_start..].iter().all(|b| *b == 0), "tail clear");
    }

    /// Devices smaller than two protected regions.
    #[test]
    fn small_devices() {
        for size in [BLOCK, HEAD as usize, HEAD as usize + 3 * BLOCK + 512] {
            let (_dir, path, dev) = stale_device(size);
            let mut sink = DeviceSink::new(dev, size as u64).unwrap();
            let mut image = vec![0u8; size / 2];
            image[0] = 5;
            if let Some(b) = image.last_mut() {
                *b = 6;
            }
            let mut written = 0;
            write_blocks(&mut sink, &image, &mut written, size as u64).unwrap();
            sink.finish().unwrap();
            let out = std::fs::read(&path).unwrap();
            assert_eq!(&out[..image.len()], &image[..], "size {size}");
            assert!(out[image.len()..].iter().all(|b| *b == 0), "size {size}");
        }
    }

    #[tokio::test]
    async fn waits_for_a_non_empty_approval() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("approved");
        std::fs::write(&f, "").unwrap();
        let waiter = tokio::spawn({
            let f = f.clone();
            async move { wait_for_approval(&f, "claim-uid").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!waiter.is_finished(), "an empty approval must not proceed");
        std::fs::write(&f, "another-claim").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            !waiter.is_finished(),
            "an approval for another claim must not proceed"
        );
        std::fs::write(&f, "claim-uid").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[test]
    fn truncated_or_unannounced_downloads_are_refused() {
        assert!(check_length(10, Some(10), None).is_ok());
        assert!(check_length(9, Some(10), Some("abc")).is_err());
        assert!(check_length(10, None, Some("abc")).is_ok());
        assert!(check_length(10, None, None).is_err());
    }
}
