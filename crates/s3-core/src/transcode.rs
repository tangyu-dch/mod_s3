//! ffmpeg 子进程转码封装：命令构造、超时强杀进程组、stderr 截断、产物校验。
//!
//! 设计要点：
//! - `process_group(0)` 让 ffmpeg 及其子进程独立成组，超时按组 SIGKILL；
//! - stderr 在独立线程读取（`-loglevel error` 输出量小但防管道死锁），
//!   只保留尾部 4KB 用于失败事件；
//! - 产物必须存在、size>0 才算成功。

use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 转码失败原因。
#[derive(Debug)]
pub enum TranscodeError {
    /// ffmpeg 二进制不存在或无法启动。
    Spawn(std::io::Error),
    /// ffmpeg 非零退出。
    Failed { code: i32, stderr_tail: String },
    /// 超过 `timeout` 被强杀。
    Timeout(Duration),
    /// ffmpeg 报告成功但产物缺失/为空。
    EmptyOutput,
    /// IO 错误（读文件/写 temp 等）。
    Io(std::io::Error),
}

impl std::fmt::Display for TranscodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "ffmpeg spawn failed: {e}"),
            Self::Failed { code, stderr_tail } => {
                write!(f, "ffmpeg exited {code}: {}", tail(stderr_tail, 200))
            }
            Self::Timeout(d) => write!(f, "ffmpeg timed out after {d:?}"),
            Self::EmptyOutput => write!(f, "ffmpeg produced empty output"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for TranscodeError {}

impl From<std::io::Error> for TranscodeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

fn tail(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    s[s.len() - max..].to_string()
}

/// 转码请求。
pub struct TranscodeRequest<'a> {
    pub src: &'a Path,
    pub dst: &'a Path,
    /// 编码参数（`-i` 与输出文件之间的部分），由配置/格式档位提供。
    pub args: &'a [String],
    pub timeout: Duration,
}

/// 转码结果统计。
#[derive(Clone, Debug)]
pub struct TranscodeOutcome {
    pub elapsed: Duration,
    pub output_bytes: u64,
}

/// 目标格式 → 默认编码参数。
/// 用户可通过配置 `ffmpeg-args` 整体覆盖。
pub fn default_args_for(format: &str) -> Vec<String> {
    let split = |s: &str| s.split_whitespace().map(str::to_owned).collect();
    match format.to_ascii_lowercase().as_str() {
        "mp3" => split("-ac 1 -ar 16000 -b:a 32k -f mp3"),
        "opus" | "ogg" => split("-ac 1 -ar 16000 -b:a 16k -f ogg -c:a libopus"),
        "wav" => split("-ac 1 -ar 16000 -f wav"),
        _ => Vec::new(),
    }
}

/// 格式名 → 输出扩展名（无点）。未知格式原样小写返回。
pub fn extension_for(format: &str) -> String {
    match format.to_ascii_lowercase().as_str() {
        "mp3" => "mp3".to_string(),
        "opus" => "opus".to_string(),
        "ogg" => "ogg".to_string(),
        "wav" => "wav".to_string(),
        other => other.to_string(),
    }
}

/// 源扩展名是否等于目标格式（决定 Auto 模式是否跳过转码）。
pub fn same_format(src_ext: &str, target: &str) -> bool {
    let src = src_ext.trim_start_matches('.').to_ascii_lowercase();
    let dst = target.trim_start_matches('.').to_ascii_lowercase();
    src == dst || (src == "oga" && dst == "ogg") || (src == "oga" && dst == "opus")
}

/// 执行一次 ffmpeg 转码。`ffmpeg_bin` 为可执行文件路径。
pub fn run_ffmpeg(
    ffmpeg_bin: &OsStr,
    req: &TranscodeRequest<'_>,
) -> Result<TranscodeOutcome, TranscodeError> {
    let started = Instant::now();

    let mut cmd = Command::new(ffmpeg_bin);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(req.src)
        .args(req.args)
        .arg(req.dst)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // 独立进程组：超时按组强杀，避免 ffmpeg 拉起的子进程泄漏。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(TranscodeError::Spawn)?;

    // stderr 收集线程：写共享 buffer，截断到 64KB 防内存膨胀。
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let buf = Arc::clone(&stderr_buf);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut guard = buf.lock().unwrap_or_else(|p| p.into_inner());
                        if guard.len() < 64 * 1024 {
                            let room = 64 * 1024 - guard.len();
                            guard.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                }
            }
        });
    }

    let status = wait_with_timeout(&mut child, req.timeout)?;
    let stderr_tail = String::from_utf8_lossy(
        &stderr_buf
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone(),
    )
    .into_owned();

    if !status.success() {
        return Err(TranscodeError::Failed {
            code: status.code().unwrap_or(-1),
            stderr_tail,
        });
    }

    let output_bytes = fs::metadata(req.dst)?.len();
    if output_bytes == 0 {
        return Err(TranscodeError::EmptyOutput);
    }

    Ok(TranscodeOutcome {
        elapsed: started.elapsed(),
        output_bytes,
    })
}

/// 轮询 `try_wait` 实现超时；超时按进程组 SIGKILL。
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Result<std::process::ExitStatus, TranscodeError> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_group(child);
                    let _ = child.wait();
                    return Err(TranscodeError::Timeout(timeout));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(TranscodeError::Io(e)),
        }
    }
}

/// SIGKILL 整个进程组（`process_group(0)` 已让子进程自成组长）。
#[cfg(unix)]
fn kill_process_group(child: &Child) {
    // SAFETY: kill(-pgid) 只影响本子进程组；pgid 即 child pid。
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut Child) {
    let _ = child.kill();
}

/// 为任务生成唯一临时产物路径。
pub fn temp_output_path(temp_dir: &Path, job_id: u64, ext: &str) -> PathBuf {
    temp_dir.join(format!(
        "mod_s3_{job_id}.{}",
        extension_for(ext.trim_start_matches('.'))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args_cover_formats() {
        assert!(default_args_for("mp3").iter().any(|a| a == "32k"));
        assert!(default_args_for("ogg").iter().any(|a| a == "libopus"));
        assert!(default_args_for("wav").iter().any(|a| a == "16000"));
    }

    #[test]
    fn same_format_detection() {
        assert!(same_format(".wav", "wav"));
        assert!(same_format(".mp3", "MP3"));
        assert!(!same_format(".wav", "mp3"));
        assert!(same_format(".oga", "ogg"));
    }

    #[test]
    fn ffmpeg_missing_binary_fails_spawn() {
        let dir = std::env::temp_dir();
        let req = TranscodeRequest {
            src: Path::new("/nonexistent/src.wav"),
            dst: &dir.join("out.mp3"),
            args: &[],
            timeout: Duration::from_secs(5),
        };
        let err = run_ffmpeg(OsStr::new("/nonexistent/ffmpeg-xyz"), &req).unwrap_err();
        assert!(matches!(err, TranscodeError::Spawn(_)));
    }

    #[test]
    fn fake_ffmpeg_success() {
        // 用 /bin/sh -c 模拟 ffmpeg：把 src 复制成 dst
        let dir = std::env::temp_dir().join(format!("mod_s3_tc_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.wav");
        let dst = dir.join("out.mp3");
        fs::write(&src, b"fake-wav").unwrap();

        // 写一个假 ffmpeg 脚本：解析 -i src ... dst，cp src dst
        let fake = dir.join("ffmpeg.sh");
        fs::write(
            &fake,
            "#!/bin/sh\n\
             src=\"\"; prev=\"\"; last=\"\"\n\
             for a in \"$@\"; do\n\
               [ \"$prev\" = \"-i\" ] && src=\"$a\"\n\
               prev=\"$a\"; last=\"$a\"\n\
             done\n\
             cp \"$src\" \"$last\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let args: Vec<String> = vec![];
        let req = TranscodeRequest {
            src: &src,
            dst: &dst,
            args: &args,
            timeout: Duration::from_secs(10),
        };
        let out = run_ffmpeg(fake.as_os_str(), &req).unwrap();
        assert_eq!(out.output_bytes, 8);
        assert!(dst.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_kills() {
        let dir = std::env::temp_dir().join(format!("mod_s3_tmo_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("ffmpeg-slow.sh");
        fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dst = dir.join("out.mp3");
        let req = TranscodeRequest {
            src: Path::new("/dev/null"),
            dst: &dst,
            args: &[],
            timeout: Duration::from_millis(500),
        };
        let err = run_ffmpeg(fake.as_os_str(), &req).unwrap_err();
        assert!(matches!(err, TranscodeError::Timeout(_)));
        let _ = fs::remove_dir_all(&dir);
    }
}
