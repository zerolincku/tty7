//! Rsync's remote shell runs over the already authenticated native SSH session.
//! No credentials are passed to child processes, argv, or a second SSH client.
use super::{SshConnection, sftp::Job};
use crate::daemon::protocol::{SftpTransferKind, SftpTransferSpec};
use russh::ChannelMsg;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn bridge_entry() {
    if std::env::args().nth(1).as_deref() != Some("--tty7-rsync-bridge") {
        return;
    }
    let result = bridge();
    if let Err(ref e) = result {
        eprintln!("rsync SSH bridge: {e}");
    }
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

fn bridge() -> std::io::Result<()> {
    use std::io::{Read, Write};
    // Some remote-shell clients (including macOS openrsync) hand the helper
    // nonblocking pipe ends. This synchronous bridge owns those ends.
    #[cfg(unix)]
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    let mut args = std::env::args().skip(2);
    let address = args.next().ok_or(std::io::ErrorKind::InvalidInput)?;
    let token =
        std::env::var("TTY7_RSYNC_TOKEN").map_err(|_| std::io::ErrorKind::PermissionDenied)?;
    let mut socket = std::net::TcpStream::connect(address)?;
    socket.set_nodelay(true)?;
    let request = serde_json::to_vec(&(token, args.collect::<Vec<_>>()))?;
    socket.write_all(&(request.len() as u32).to_be_bytes())?;
    socket.write_all(&request)?;
    let mut input = socket.try_clone()?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut input);
        let _ = input.shutdown(std::net::Shutdown::Write);
    });
    // Frames keep remote stderr separate from the binary rsync protocol.
    loop {
        let mut kind = [0];
        socket.read_exact(&mut kind)?;
        let kind = kind[0];
        let mut len = [0; 4];
        socket.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        if len > 1024 * 1024 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut data = vec![0; len];
        socket.read_exact(&mut data)?;
        match kind {
            0 => {
                let mut out = std::io::stdout().lock();
                out.write_all(&data)?;
                out.flush()?;
            }
            1 => {
                std::io::stderr().lock().write_all(&data)?;
            }
            2 => {
                return if data == [0] {
                    Ok(())
                } else {
                    Err(std::io::ErrorKind::Other.into())
                };
            }
            _ => return Err(std::io::ErrorKind::InvalidData.into()),
        }
    }
}
async fn local_binary() -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|p| p.join("rsync")).collect())
        .unwrap_or_default();
    paths.extend(
        [
            "/opt/homebrew/bin/rsync",
            "/usr/local/bin/rsync",
            "/usr/bin/rsync",
        ]
        .map(PathBuf::from),
    );
    for path in paths {
        let output = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::process::Command::new(&path)
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await;
        if matches!(output, Ok(Ok(ref out)) if out.status.success()) {
            return Some(path);
        }
    }
    None
}

/// Missing executables are a fallback; a broken SSH connection is an error.
pub(super) async fn detect(conn: &SshConnection) -> Result<Option<PathBuf>, String> {
    if cfg!(windows) {
        return Ok(None);
    }
    let Some(binary) = local_binary().await else {
        return Ok(None);
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut channel = conn.open_command_channel().await.map_err(|e| e.to_string())?;
        channel.exec(true, "if command -v rsync >/dev/null 2>&1; then rsync --version >/dev/null 2>&1; else exit 127; fi").await.map_err(|e| e.to_string())?;
        while let Some(msg) = channel.wait().await {
            if let ChannelMsg::ExitStatus { exit_status } = msg {
                return match exit_status { 0 => Ok(Some(binary)), 127 => Ok(None), _ => Err("远程 rsync 无法运行".into()) };
            }
        }
        Err("SSH 通道未返回 rsync 检测结果".into())
    }).await.map_err(|_| "rsync 检测超时".to_string())?
}

async fn frame(
    socket: &mut tokio::net::tcp::OwnedWriteHalf,
    kind: u8,
    bytes: &[u8],
) -> Result<(), String> {
    socket.write_u8(kind).await.map_err(|e| e.to_string())?;
    socket
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|e| e.to_string())?;
    socket.write_all(bytes).await.map_err(|e| e.to_string())
}

async fn relay(
    listener: tokio::net::TcpListener,
    token: String,
    conn: Arc<SshConnection>,
) -> Result<(), String> {
    let (mut socket, _) = listener.accept().await.map_err(|e| e.to_string())?;
    socket.set_nodelay(true).map_err(|e| e.to_string())?;
    let args = tokio::time::timeout(Duration::from_secs(5), async {
        let len = socket.read_u32().await.map_err(|e| e.to_string())?;
        if len > 65536 {
            return Err("invalid bridge request".into());
        }
        let mut data = vec![0; len as usize];
        socket
            .read_exact(&mut data)
            .await
            .map_err(|e| e.to_string())?;
        let (supplied, args): (String, Vec<String>) =
            serde_json::from_slice(&data).map_err(|e| e.to_string())?;
        if supplied != token
            || args.first().map(String::as_str) != Some("tty7")
            || args.get(1).map(String::as_str) != Some("rsync")
            || args.get(2).map(String::as_str) != Some("--server")
        {
            return Err("invalid bridge credentials or command".into());
        }
        Ok::<_, String>(args)
    })
    .await
    .map_err(|_| "bridge handshake timed out".to_string())??;
    let command = args[1..]
        .iter()
        .map(|s| quote(s))
        .collect::<Vec<_>>()
        .join(" ");
    let mut channel = conn
        .open_command_channel()
        .await
        .map_err(|e| e.to_string())?;
    channel
        .exec(true, command)
        .await
        .map_err(|e| e.to_string())?;
    let (mut read, mut write) = socket.into_split();
    let mut buf = vec![0; 65536];
    let mut eof = false;
    let mut status = 1;
    loop {
        tokio::select! {
            data = read.read(&mut buf), if !eof => {
                let n = data.map_err(|e| e.to_string())?;
                if n == 0 { eof = true; channel.eof().await.map_err(|e| e.to_string())?; }
                else { channel.data(&buf[..n]).await.map_err(|e| e.to_string())?; }
            }
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) => frame(&mut write, 0, &data).await?,
                Some(ChannelMsg::ExtendedData { data, .. }) => frame(&mut write, 1, &data).await?,
                Some(ChannelMsg::ExitStatus { exit_status }) => status = exit_status,
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
    }
    frame(&mut write, 2, &[u8::from(status != 0)]).await
}

fn arguments(spec: &SftpTransferSpec, directory: bool) -> Vec<String> {
    let mut local = spec.local.to_string_lossy().into_owned();
    // A local relative path containing ':' must not be interpreted as a remote.
    if !spec.local.is_absolute() {
        local = format!("./{local}");
    }
    let mut remote = spec.remote.clone();
    if directory {
        if spec.kind == SftpTransferKind::Upload {
            local.push('/');
        } else {
            remote.push('/');
        }
    }
    match spec.kind {
        SftpTransferKind::Upload => vec![local, format!("tty7:{remote}")],
        SftpTransferKind::Download => vec![format!("tty7:{remote}"), local],
    }
}

#[derive(Default)]
struct Progress {
    completed: u64,
    current: u64,
}
impl Progress {
    fn line(&mut self, line: &str, job: &Job) {
        if let Some(name) = line.strip_prefix("TTY7:") {
            self.completed += self.current;
            self.current = 0;
            job.set_current(name);
        } else {
            let mut parts = line.split_whitespace();
            if let (Some(bytes), Some(percent)) = (parts.next(), parts.next()) {
                if percent.ends_with('%') {
                    if let Ok(bytes) = bytes.replace(',', "").parse::<u64>() {
                        self.current = self.current.max(bytes);
                        let mut progress = job.progress.lock().unwrap();
                        progress.bytes_done =
                            (self.completed + self.current).min(progress.bytes_total);
                    }
                }
            }
        }
    }
}

pub(super) async fn transfer(
    binary: PathBuf,
    conn: Arc<SshConnection>,
    spec: &SftpTransferSpec,
    directory: bool,
    job: &Job,
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    transfer_with_exe(binary, conn, spec, directory, job, exe).await
}

async fn transfer_with_exe(
    binary: PathBuf,
    conn: Arc<SshConnection>,
    spec: &SftpTransferSpec,
    directory: bool,
    job: &Job,
    exe: PathBuf,
) -> Result<(), String> {
    if spec.kind == SftpTransferKind::Download {
        if let Some(parent) = spec.local.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
        if directory {
            tokio::fs::create_dir_all(&spec.local)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let token = uuid::Uuid::new_v4().to_string();
    let shell = format!(
        "{} --tty7-rsync-bridge {address}",
        quote(&exe.to_string_lossy())
    );
    let mut command = tokio::process::Command::new(binary);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .args([
            "-rpt",
            "--blocking-io",
            "--partial-dir=.tty7-rsync-partial",
            "--progress",
            "--out-format=TTY7:%n",
            "-e",
            &shell,
            "--",
        ])
        .args(arguments(spec, directory))
        // Ask rsync to pass literal argv; the bridge quotes every remote argument.
        .env("RSYNC_OLD_ARGS", "1")
        .env("TTY7_RSYNC_TOKEN", &token)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("启动 rsync: {e}"))?;
    let relay = relay(listener, token, conn);
    let output = child.stdout.take().unwrap();
    let errors = child.stderr.take().unwrap();
    let read_progress = async {
        let mut output = BufReader::new(output);
        let mut state = Progress::default();
        let mut record = Vec::new();
        let mut buf = [0; 4096];
        loop {
            let n = output.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            for &b in &buf[..n] {
                if b == b'\r' || b == b'\n' {
                    state.line(&String::from_utf8_lossy(&record), job);
                    record.clear();
                } else if record.len() < 8192 {
                    record.push(b);
                }
            }
        }
        Ok::<_, String>(())
    };
    let read_errors = async {
        let mut reader = BufReader::new(errors);
        let mut tail = String::new();
        let mut buf = [0; 4096];
        loop {
            let n = reader.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            tail.push_str(&String::from_utf8_lossy(&buf[..n]));
            if tail.len() > 16384 {
                tail = tail
                    .chars()
                    .rev()
                    .take(8192)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
            }
        }
        Ok::<_, String>(tail)
    };
    let wait = async {
        loop {
            if job.is_cancelled() {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGINT);
                    }
                }
                return match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                    Ok(status) => status.map_err(|e| e.to_string()),
                    Err(_) => {
                        let _ = child.kill().await;
                        child.wait().await.map_err(|e| e.to_string())
                    }
                };
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    // Dropping the relay closes its command channel if the local process exits early.
    tokio::pin!(relay, read_progress, read_errors, wait);
    let mut relay_done = false;
    let mut progress_done = false;
    let mut errors_done = false;
    let mut error = String::new();
    let mut relay_error = None;
    loop {
        tokio::select! {
            result = &mut relay, if !relay_done => { relay_done = true; relay_error = result.err(); }
            result = &mut read_progress, if !progress_done => { progress_done = true; result?; }
            result = &mut read_errors, if !errors_done => { errors_done = true; error = result?; }
            result = &mut wait => {
                let status = result?;
                if !errors_done { error = read_errors.await?; }
                if !progress_done { read_progress.await?; }
                if job.is_cancelled() { return Err("cancelled".into()); }
                return if status.success() && relay_error.is_none() { Ok(()) } else { Err(format!("rsync 传输失败（{status}），重试可续传：{error} {}", relay_error.unwrap_or_default())) };
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::test_support::{Exec, FakeSshd};
    use super::*;

    #[test]
    fn literal_remote_arguments_and_exact_directory_target() {
        let name = "中文 a';$(touch nope)";
        let quoted = quote(name);
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("printf %s {quoted}")])
            .output()
            .unwrap();
        assert_eq!(output.stdout, name.as_bytes());
        let mut spec = SftpTransferSpec {
            pane_id: 1,
            kind: SftpTransferKind::Upload,
            local: PathBuf::from("a:b"),
            remote: "/tmp/a b".into(),
            recursive: true,
        };
        assert_eq!(arguments(&spec, true), ["./a:b/", "tty7:/tmp/a b"]);
        spec.kind = SftpTransferKind::Download;
        assert_eq!(arguments(&spec, true), ["tty7:/tmp/a b/", "./a:b"]);
    }

    #[tokio::test]
    async fn missing_rsync_falls_back_but_probe_timeout_is_an_error() {
        if local_binary().await.is_none() {
            return;
        }
        let missing = FakeSshd::connect(Exec::MissingRsync, None).await;
        assert!(detect(&missing.conn).await.unwrap().is_none());
        let broken = FakeSshd::connect(Exec::Hangs, None).await;
        assert!(detect(&broken.conn).await.is_err());
    }

    #[test]
    fn file_progress_accumulates_without_counting_repeated_updates() {
        let job = Job::test_job(3000);
        let mut p = Progress::default();
        p.line("TTY7:a", &job);
        p.line("1,000 100% 1MB/s", &job);
        p.line("1,000 100% 1MB/s", &job);
        p.line("TTY7:b", &job);
        p.line("500 25% 1MB/s", &job);
        assert_eq!(job.progress.lock().unwrap().bytes_done, 1500);
    }

    /// Build tty7-server first, then set TTY7_TEST_BRIDGE to its absolute path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a built tty7-server and real rsync on PATH"]
    async fn rsync_wire_upload_download_cancel_resume() {
        let exe = PathBuf::from(std::env::var_os("TTY7_TEST_BRIDGE").expect("TTY7_TEST_BRIDGE"));
        let sshd = FakeSshd::connect(Exec::Shell, None).await;
        let binary = std::env::var_os("TTY7_TEST_RSYNC")
            .map(PathBuf::from)
            .unwrap_or(detect(&sshd.conn).await.unwrap().expect("rsync installed"));
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("本地 ' ;$ 文件");
        let remote = tmp.path().join("远程 ' ;$ 文件");
        let bytes: Vec<u8> = (0..8 * 1024 * 1024)
            .map(|i| ((i * 31 + i / 4096) % 251) as u8)
            .collect();
        std::fs::write(&local, &bytes).unwrap();
        let spec = SftpTransferSpec {
            pane_id: 1,
            kind: SftpTransferKind::Upload,
            local: local.clone(),
            remote: remote.to_str().unwrap().into(),
            recursive: false,
        };
        let job = Job::test_job(bytes.len() as u64);
        tokio::time::timeout(
            Duration::from_secs(30),
            transfer_with_exe(
                binary.clone(),
                sshd.conn.clone(),
                &spec,
                false,
                &job,
                exe.clone(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read(&remote).unwrap(), bytes);
        // Interrupt a deliberately throttled real rsync process, then reuse its
        // partial output. The wrapper is only a test speed limiter.
        let wrapper = tmp.path().join("slow-rsync");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec {} --bwlimit=512 \"$@\"\n",
                quote(binary.to_str().unwrap())
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let download = SftpTransferSpec {
            kind: SftpTransferKind::Download,
            local: tmp.path().join("下载目录/下载文件"),
            ..spec.clone()
        };
        let job = Arc::new(Job::test_job(bytes.len() as u64));
        let cancel = job.clone();
        let trigger = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            cancel.cancel_for_test();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            transfer_with_exe(
                wrapper.clone(),
                sshd.conn.clone(),
                &download,
                false,
                &job,
                exe.clone(),
            ),
        )
        .await
        .unwrap();
        trigger.await.unwrap();
        assert!(result.is_err(), "{result:?}");

        let partial = download
            .local
            .parent()
            .unwrap()
            .join(".tty7-rsync-partial")
            .join(download.local.file_name().unwrap());
        let partial_size = std::fs::metadata(&partial)
            .expect("retained partial file")
            .len();
        assert!(
            partial_size > 0 && partial_size < bytes.len() as u64,
            "partial={partial_size}"
        );
        assert!(
            !download.local.exists(),
            "incomplete file is never published"
        );
        let job = Job::test_job(bytes.len() as u64);
        tokio::time::timeout(
            Duration::from_secs(30),
            transfer_with_exe(
                binary.clone(),
                sshd.conn.clone(),
                &download,
                false,
                &job,
                exe.clone(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read(&download.local).unwrap(), bytes);
        assert!(!partial.exists());
        let upload = SftpTransferSpec {
            remote: tmp.path().join("可续传上传").to_str().unwrap().into(),
            ..spec.clone()
        };
        let job = Arc::new(Job::test_job(bytes.len() as u64));
        let cancel = job.clone();
        let trigger = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            cancel.cancel_for_test();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            transfer_with_exe(
                wrapper,
                sshd.conn.clone(),
                &upload,
                false,
                &job,
                exe.clone(),
            ),
        )
        .await
        .unwrap();
        trigger.await.unwrap();
        assert!(result.is_err());
        let partial = tmp.path().join(".tty7-rsync-partial/可续传上传");
        tokio::time::timeout(Duration::from_secs(3), async {
            while !partial.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("remote partial saved");
        assert!(std::fs::metadata(&partial).unwrap().len() > 0);
        let job = Job::test_job(bytes.len() as u64);
        transfer_with_exe(
            binary.clone(),
            sshd.conn.clone(),
            &upload,
            false,
            &job,
            exe.clone(),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&upload.remote).unwrap(), bytes);
        // Recursive transfers copy directory contents into the exact selected
        // target, rather than nesting an extra source directory on a retry.
        let source = tmp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("child"), b"child").unwrap();
        let spec = SftpTransferSpec {
            local: source,
            remote: tmp.path().join("destination").to_str().unwrap().into(),
            recursive: true,
            ..spec
        };
        let job = Job::test_job(5);
        for _ in 0..2 {
            transfer_with_exe(
                binary.clone(),
                sshd.conn.clone(),
                &spec,
                true,
                &job,
                exe.clone(),
            )
            .await
            .unwrap();
        }
        assert_eq!(
            std::fs::read(PathBuf::from(&spec.remote).join("child")).unwrap(),
            b"child"
        );
        assert!(!PathBuf::from(&spec.remote).join("source").exists());
    }
}
