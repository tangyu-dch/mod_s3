//! 路径白名单守卫：防止符号链接逃逸与越权路径访问。
//!
//! 校验分两层：
//! 1. 词法归一化——解析 `.`/`..`，确认候选路径位于某个 allowed root 之下；
//! 2. `openat` 逐段 `O_NOFOLLOW` 打开——即使路径中插入符号链接也无法逃逸，
//!    最终 `fstat` 确认是普通文件并读取 size/mtime。

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::FromRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::SecurityLimits;

/// pathguard 校验失败原因。
#[derive(Debug)]
pub enum GuardError {
    /// 输入为空或无法解析为候选路径。
    InvalidPath(String),
    /// 不在任何 allowed root 之下。
    OutsideRoots(String),
    /// 路径中存在符号链接或非常规文件。
    NotRegularFile(String),
    /// 文件大小为 0 或超过上限。
    SizeOutOfRange(u64, u64),
    /// 文件 mtime 距今不足 `min_age`。
    TooFresh,
    /// 扩展名不在白名单。
    BadExtension(String),
    /// 底层 IO 错误。
    Io(std::io::Error),
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPath(p) => write!(f, "invalid path: {p}"),
            Self::OutsideRoots(p) => write!(f, "path outside allowed roots: {p}"),
            Self::NotRegularFile(p) => write!(f, "not a regular file: {p}"),
            Self::SizeOutOfRange(size, max) => {
                write!(f, "file size {size} out of range (max {max})")
            }
            Self::TooFresh => write!(f, "file mtime too recent"),
            Self::BadExtension(e) => write!(f, "extension not allowed: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for GuardError {}

impl From<std::io::Error> for GuardError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// 通过 pathguard 校验的文件信息。
#[derive(Clone, Debug)]
pub struct GuardedFile {
    /// 词法归一化后的绝对路径（用于后续打开/删除/展示）。
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
    /// 小写扩展名（含点），无扩展名为空串。
    pub ext: String,
    /// 文件名（不含目录）。
    pub filename: String,
}

/// 路径守卫：持有 allowed roots 与限制参数。
pub struct PathGuard {
    limits: SecurityLimits,
    /// `canonicalize` 后的 allowed roots。root 前缀自身可能含符号链接
    /// （如 macOS `/var` → `/private/var`），fd walk 需从 canonical 路径
    /// 开始，O_NOFOLLOW 只负责 root 之下的逃逸防护。
    canonical_roots: Vec<PathBuf>,
}

impl PathGuard {
    pub fn new(limits: SecurityLimits) -> Self {
        let canonical_roots = limits
            .allowed_roots
            .iter()
            .map(|root| {
                fs::canonicalize(root).unwrap_or_else(|_| normalize_lexical(root))
            })
            .collect();
        Self {
            limits,
            canonical_roots,
        }
    }

    /// 校验 `input` 并返回文件信息；`input` 为绝对路径时直接校验，
    /// 相对路径依次尝试拼接到每个 allowed root 下。
    pub fn check(&self, input: &str) -> Result<GuardedFile, GuardError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(GuardError::InvalidPath(input.to_owned()));
        }

        let absolute = Path::new(trimmed).is_absolute();
        let candidates: Vec<PathBuf> = if absolute {
            vec![PathBuf::from(trimmed)]
        } else {
            self.limits
                .allowed_roots
                .iter()
                .map(|root| root.join(trimmed))
                .collect()
        };

        let mut last_err = GuardError::InvalidPath(input.to_owned());
        for candidate in candidates {
            match self.check_one(&candidate) {
                Ok(file) => return Ok(file),
                // 绝对路径只有一个候选：Io/OutsideRoots 原样上报，
                // 否则 ENOENT/EACCES 会被误报成 OutsideRoots。
                Err(e) if absolute => return Err(e),
                Err(GuardError::Io(_)) | Err(GuardError::OutsideRoots(_)) => {
                    last_err = GuardError::OutsideRoots(candidate.display().to_string());
                }
                Err(other) => return Err(other),
            }
        }
        Err(last_err)
    }

    /// 对单个候选路径执行完整校验。
    fn check_one(&self, candidate: &Path) -> Result<GuardedFile, GuardError> {
        let normalized = normalize_lexical(candidate);
        if !self.under_allowed_root(&normalized) {
            return Err(GuardError::OutsideRoots(
                normalized.display().to_string(),
            ));
        }

        // fd walk 从 canonical root + 相对部分开始：root 前缀的符号链接
        // 由 canonicalize 预先解析，root 之下的每一段仍走 O_NOFOLLOW。
        let walk_path = self.walk_path(&normalized);
        let (size, modified) = self.stat_no_follow(&walk_path)?;
        if size == 0 || size > self.limits.max_bytes {
            return Err(GuardError::SizeOutOfRange(size, self.limits.max_bytes));
        }
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        if age < self.limits.min_age {
            return Err(GuardError::TooFresh);
        }

        let ext = normalized
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_default();
        if !self.limits.allowed_ext.iter().any(|a| a == &ext) {
            return Err(GuardError::BadExtension(ext));
        }

        let filename = normalized
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        Ok(GuardedFile {
            path: normalized,
            size,
            modified,
            ext,
            filename,
        })
    }

    /// 把 normalized 候选路径映射为 fd walk 路径：命中的 allowed root
    /// 替换为其 canonical 形式，相对部分原样保留。
    fn walk_path(&self, normalized: &Path) -> PathBuf {
        for (i, root) in self.limits.allowed_roots.iter().enumerate() {
            let root_norm = normalize_lexical(root);
            if let Ok(rel) = normalized.strip_prefix(&root_norm) {
                return self.canonical_roots[i].join(rel);
            }
        }
        normalized.to_path_buf()
    }

    /// 词法层面确认路径位于某个 allowed root 之下。
    fn under_allowed_root(&self, path: &Path) -> bool {
        self.limits
            .allowed_roots
            .iter()
            .any(|root| {
                let root_norm = normalize_lexical(root);
                path.starts_with(&root_norm)
            })
    }

    /// `openat` 逐段 `O_NOFOLLOW` 打开路径，`fstat` 取元数据。
    /// 任一目录/文件是符号链接都会以 `ELOOP`/`NotRegularFile` 失败。
    fn stat_no_follow(&self, path: &Path) -> Result<(u64, SystemTime), GuardError> {
        let components: Vec<&std::ffi::OsStr> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(os) => Some(os),
                Component::RootDir => None,
                _ => None, // normalize_lexical 已消除 CurDir/ParentDir
            })
            .collect();
        if components.is_empty() {
            return Err(GuardError::InvalidPath(path.display().to_string()));
        }

        // SAFETY: 全程只用 libc openat/fstat/close，fd 生命周期由
        // RAII File 包装与手动 close 保证。
        unsafe {
            let mut dirfd = libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if dirfd < 0 {
                return Err(GuardError::Io(std::io::Error::last_os_error()));
            }

            let result = (|| -> Result<(u64, SystemTime), GuardError> {
                for (i, comp) in components.iter().enumerate() {
                    let c_path = CString::new(comp.as_bytes())
                        .map_err(|_| GuardError::InvalidPath(path.display().to_string()))?;
                    let is_last = i + 1 == components.len();
                    let flags = if is_last {
                        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK
                    } else {
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
                    };
                    let next = libc::openat(dirfd, c_path.as_ptr(), flags);
                    if next < 0 {
                        let err = std::io::Error::last_os_error();
                        // ELOOP = 符号链接；ENOTDIR = 中间组件非常规文件。
                        return Err(match err.raw_os_error() {
                            Some(libc::ELOOP) => {
                                GuardError::NotRegularFile(path.display().to_string())
                            }
                            _ => GuardError::Io(err),
                        });
                    }
                    libc::close(dirfd);
                    dirfd = next;
                }

                // dirfd 现在是目标文件的 fd（最后一段以 O_NOFOLLOW 打开）。
                // SAFETY: dirfd 是 openat 返回的有效 fd，所有权移交给 File。
                let file = fs::File::from_raw_fd(dirfd);
                dirfd = -1; // 所有权已移交，外层 close 需跳过
                let meta = file.metadata()?;
                if !meta.is_file() {
                    return Err(GuardError::NotRegularFile(
                        path.display().to_string(),
                    ));
                }
                Ok((meta.len(), meta.modified()?))
            })();

            if dirfd >= 0 {
                libc::close(dirfd);
            }
            result
        }
    }
}

/// 词法归一化：消除 `.`/`..`，不访问文件系统。
/// `..` 超出根时钳制到根（词法层）；真正的逃逸由 fd walk 兜底。
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(os) => out.push(os),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mod_s3_pg_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn limits(root: &Path) -> SecurityLimits {
        SecurityLimits {
            allowed_roots: vec![root.to_path_buf()],
            allowed_ext: vec![".wav".into(), ".mp3".into()],
            max_bytes: 1024 * 1024,
            min_age: Duration::ZERO,
        }
    }

    #[test]
    fn accepts_file_under_root() {
        let root = temp_root("ok");
        let file = root.join("a.wav");
        fs::write(&file, b"RIFF").unwrap();
        let guard = PathGuard::new(limits(&root));
        let got = guard.check(file.to_str().unwrap()).unwrap();
        assert_eq!(got.filename, "a.wav");
        assert_eq!(got.ext, ".wav");
        assert_eq!(got.size, 4);
    }

    #[test]
    fn rejects_outside_root() {
        let root = temp_root("out");
        let other = temp_root("other");
        let file = other.join("a.wav");
        fs::write(&file, b"x").unwrap();
        let guard = PathGuard::new(limits(&root));
        assert!(matches!(
            guard.check(file.to_str().unwrap()),
            Err(GuardError::OutsideRoots(_))
        ));
    }

    #[test]
    fn rejects_dotdot_escape() {
        let root = temp_root("dotdot");
        let file = root.join("a.wav");
        fs::write(&file, b"x").unwrap();
        // root/../other/a.wav 词法归一化后落在 root 的兄弟目录
        let sibling = root.parent().unwrap().join("mod_s3_pg_sibling");
        fs::create_dir_all(&sibling).unwrap();
        let evil = sibling.join("evil.wav");
        fs::write(&evil, b"x").unwrap();
        let guard = PathGuard::new(limits(&root));
        let rel = format!(
            "../{}",
            sibling.file_name().unwrap().to_string_lossy()
        );
        assert!(guard.check(&rel).is_err());
    }

    #[test]
    fn rejects_symlink_escape() {
        let root = temp_root("sym");
        let outside = temp_root("sym_out");
        let target = outside.join("real.wav");
        fs::write(&target, b"data").unwrap();
        let link = root.join("link.wav");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let guard = PathGuard::new(limits(&root));
        assert!(matches!(
            guard.check(link.to_str().unwrap()),
            Err(GuardError::NotRegularFile(_)) | Err(GuardError::Io(_))
        ));
    }

    #[test]
    fn rejects_bad_ext_and_size() {
        let root = temp_root("ext");
        fs::write(root.join("a.txt"), b"x").unwrap();
        fs::write(root.join("big.wav"), vec![0u8; 2 * 1024 * 1024]).unwrap();
        let guard = PathGuard::new(limits(&root));
        assert!(matches!(
            guard.check(root.join("a.txt").to_str().unwrap()),
            Err(GuardError::BadExtension(_))
        ));
        assert!(matches!(
            guard.check(root.join("big.wav").to_str().unwrap()),
            Err(GuardError::SizeOutOfRange(_, _))
        ));
    }

    #[test]
    fn absolute_missing_file_reports_io_not_outside_roots() {
        let root = temp_root("missing");
        let guard = PathGuard::new(limits(&root));
        // root 下不存在的文件：绝对路径应返回 Io(ENOENT)，
        // 而不是被吞成 OutsideRoots。
        let missing = root.join("nope.wav");
        assert!(matches!(
            guard.check(missing.to_str().unwrap()),
            Err(GuardError::Io(_))
        ));
    }

    #[test]
    fn relative_path_joins_root() {
        let root = temp_root("rel");
        fs::write(root.join("sub").join("b.mp3"), b"mp3").unwrap_err();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("b.mp3"), b"mp3").unwrap();
        let guard = PathGuard::new(limits(&root));
        let got = guard.check("sub/b.mp3").unwrap();
        assert!(got.path.ends_with("sub/b.mp3"));
    }
}
