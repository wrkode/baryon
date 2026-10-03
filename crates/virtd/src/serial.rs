//! Serial console capture.
//!
//! Cloud Hypervisor serves the guest UART on a Unix socket and buffers up to
//! 1 MiB of output while no client is connected (v53). baryon-virtd keeps one
//! client connected for the VM's lifetime, appends the output to a log file
//! with a size cap, and reports a login prompt as boot evidence. When
//! baryon-virtd restarts it reconnects and receives what was buffered.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Log files rotate once at this size; one previous file is kept.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// Events reported to the VM supervisor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialEvent {
    LoginPrompt,
}

/// Detects `<hostname> login: ` at the end of the output seen so far. The
/// prompt has no trailing newline, so a sliding window over the most recent
/// bytes is checked after every read.
#[derive(Debug, Default)]
pub struct LoginPromptDetector {
    window: Vec<u8>,
    seen: bool,
}

impl LoginPromptDetector {
    const WINDOW: usize = 256;

    /// Feed output. Returns true the first time a prompt is seen.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        if self.seen {
            return false;
        }
        self.window.extend_from_slice(bytes);
        if self.window.len() > Self::WINDOW {
            let excess = self.window.len() - Self::WINDOW;
            self.window.drain(..excess);
        }
        let text = String::from_utf8_lossy(&self.window);
        let found = text
            .lines()
            .any(|l| l.trim_end().ends_with(" login:") && !l.contains("Last login"));
        if found {
            self.seen = true;
        }
        found
    }
}

/// Capture the serial socket into `log_path` until `cancel` fires. `connect`
/// opens a new connection to the socket; it is called again whenever the
/// connection ends, for example when the guest reboots and Cloud Hypervisor
/// re-creates the socket.
pub async fn capture<F, Fut>(
    mut connect: F,
    log_path: PathBuf,
    events: mpsc::Sender<SerialEvent>,
    cancel: CancellationToken,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<UnixStream>>,
{
    let mut detector = LoginPromptDetector::default();
    let mut buf = vec![0u8; 16 * 1024];
    while !cancel.is_cancelled() {
        let stream = tokio::select! {
            () = cancel.cancelled() => return,
            r = connect() => r,
        };
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => {
                // The VMM has not created the socket yet, or is restarting.
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(Duration::from_millis(250)) => continue,
                }
            }
        };
        tracing::debug!("serial console connected");
        let mut log = match open_log(&log_path).await {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!(path = %log_path.display(), error = %e, "cannot open console log");
                None
            }
        };
        loop {
            let n = tokio::select! {
                () = cancel.cancelled() => return,
                r = stream.read(&mut buf) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                },
            };
            if detector.feed(&buf[..n]) {
                let _ = events.send(SerialEvent::LoginPrompt).await;
            }
            if let Some(f) = log.as_mut() {
                if f.write_all(&buf[..n]).await.is_err() {
                    log = None;
                    continue;
                }
                if let Ok(meta) = f.metadata().await
                    && meta.len() > MAX_LOG_BYTES
                {
                    let _ = f.flush().await;
                    log = rotate(&log_path).await.ok();
                }
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }
}

/// Console output can contain anything the guest prints, including
/// credentials, so the log and the directories created for it are root-only.
async fn open_log(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    if let Some(dir) = path.parent() {
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .await?;
    }
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .await
}

async fn rotate(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    let mut old = path.as_os_str().to_owned();
    old.push(".1");
    tokio::fs::rename(path, &old).await?;
    open_log(path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_prompt_across_reads() {
        let mut d = LoginPromptDetector::default();
        assert!(!d.feed(b"[  OK  ] Reached target multi-user.target\r\n\r\nDebian GNU/Linux 13 vm1 ttyS0\r\n\r\nvm1 lo"));
        assert!(d.feed(b"gin: "));
        // Reported once.
        assert!(!d.feed(b"\r\nvm1 login: "));
    }

    #[test]
    fn ignores_last_login_and_unrelated_text() {
        let mut d = LoginPromptDetector::default();
        assert!(!d.feed(b"Last login: Mon Oct  5 on ttyS0\r\n"));
        assert!(!d.feed(b"systemd-logind: New seat seat0.\r\n"));
    }

    #[tokio::test]
    async fn captures_to_log_and_reports_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("serial.sock");
        let log = dir.path().join("logs/console.log");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let path = sock.clone();
        let task = tokio::spawn(capture(
            move || UnixStream::connect(path.clone()),
            log.clone(),
            tx,
            cancel.clone(),
        ));

        let (mut s, _) = listener.accept().await.unwrap();
        s.write_all(b"booting\r\nvm1 login: ").await.unwrap();
        assert_eq!(rx.recv().await, Some(SerialEvent::LoginPrompt));
        // Wait for the write to reach the file.
        for _ in 0..50 {
            if std::fs::read_to_string(&log)
                .map(|c| c.contains("login:"))
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(std::fs::read_to_string(&log).unwrap().contains("booting"));
        cancel.cancel();
        task.await.unwrap();
    }
}
