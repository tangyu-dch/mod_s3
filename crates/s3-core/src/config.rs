//! 模块配置模型：由胶水层的 XML/环境变量解析填充，本 crate 只做消费。

use std::path::PathBuf;
use std::time::Duration;

/// S3 端点与凭证。凭证优先环境变量，conf 值仅兜底。
#[derive(Clone, Debug, Default)]
pub struct S3Settings {
    /// S3 endpoint，例如 `http://rustfs:9000`；`None` 表示 AWS 默认端点。
    pub endpoint: Option<String>,
    /// 区域；MinIO/RustFS 场景任意值均可，默认 `us-east-1`。
    pub region: String,
    /// 默认桶；API 未显式指定 bucket 时使用。
    pub default_bucket: Option<String>,
    /// 是否强制 path-style 寻址（MinIO/RustFS 必需）。
    pub force_path_style: bool,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub session_token: Option<String>,
    /// 超过该字节数走 multipart 分片上传；默认 256 MiB。
    /// 0 表示始终单次 put_object（上限 5 GB）。
    pub multipart_threshold: u64,
}

impl S3Settings {
    /// 凭证是否齐全（endpoint 可选——缺省走 AWS 公有云）。
    pub fn credentials_present(&self) -> bool {
        self.access_key.is_some() && self.secret_key.is_some()
    }
}

/// 安全边界：允许上传的文件范围与大小限制。
#[derive(Clone, Debug)]
pub struct SecurityLimits {
    /// 允许的根目录；相对路径按顺序尝试拼接，绝对路径必须位于其中之一之下。
    pub allowed_roots: Vec<PathBuf>,
    /// 允许的源文件扩展名（小写、含点，如 `.wav`）。
    pub allowed_ext: Vec<String>,
    /// 源文件大小上限（字节）。
    pub max_bytes: u64,
    /// 文件 mtime 距今的最小年龄，防止读取仍在写入的录音。
    pub min_age: Duration,
}

impl Default for SecurityLimits {
    fn default() -> Self {
        Self {
            allowed_roots: Vec::new(),
            allowed_ext: vec![
                ".wav".into(),
                ".mp3".into(),
                ".ogg".into(),
                ".opus".into(),
            ],
            max_bytes: 100 * 1024 * 1024,
            min_age: Duration::from_secs(2),
        }
    }
}

/// 转码模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TranscodeMode {
    /// 原样上传。
    #[default]
    None,
    /// 源格式 != 目标格式时转码；相同则跳过。
    Auto,
    /// 强制走 ffmpeg（即使格式相同也重新编码）。
    Ffmpeg,
}

impl TranscodeMode {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "ffmpeg" | "force" => Self::Ffmpeg,
            _ => Self::None,
        }
    }
}

/// ffmpeg 转码参数。
#[derive(Clone, Debug)]
pub struct TranscodeSettings {
    pub mode: TranscodeMode,
    /// 目标格式：`mp3`/`opus`/`ogg`/`wav`；`None` 或 `orig` 表示不转码。
    pub target_format: Option<String>,
    /// ffmpeg 可执行文件路径。
    pub ffmpeg_bin: String,
    /// 追加到 `-i <src>` 之后的编码参数（按空白拆分）。
    pub ffmpeg_args: Vec<String>,
    /// 单次转码超时；超时杀进程组。
    pub timeout: Duration,
    /// 转码中间产物目录。
    pub temp_dir: PathBuf,
}

impl Default for TranscodeSettings {
    fn default() -> Self {
        Self {
            mode: TranscodeMode::None,
            target_format: None,
            ffmpeg_bin: "ffmpeg".into(),
            ffmpeg_args: Vec::new(),
            timeout: Duration::from_secs(120),
            temp_dir: PathBuf::from("/tmp/mod_s3"),
        }
    }
}

/// 上传后本地文件处理策略。
#[derive(Clone, Copy, Debug, Default)]
pub struct DeletePolicy {
    /// 上传+HEAD 校验成功后删除源文件。
    pub after_upload: bool,
    /// 上传失败后也删除源文件（默认 false：失败保留便于排查/重试）。
    pub on_failure: bool,
}

/// 模块完整配置。
#[derive(Clone, Debug)]
pub struct ModuleConfig {
    pub s3: S3Settings,
    /// RECORD_STOP 自动上传开关。
    pub auto_upload: bool,
    /// recordings 根目录（事件回调的相对路径基准）。
    pub recordings_root: PathBuf,
    /// object key 前缀（模板 `{prefix}` 变量来源）。
    pub key_prefix: String,
    /// object key 模板；空串使用默认 `{prefix}/{yyyy}/{MM}/{dd}/{file}`。
    pub key_template: String,
    pub limits: SecurityLimits,
    pub transcode: TranscodeSettings,
    pub delete: DeletePolicy,
    /// 并发 worker 数（当前实现为单消费线程 + tokio worker 线程数）。
    pub workers: usize,
    /// 有界任务队列容量。
    pub queue_size: usize,
    /// 任务级重试次数（不含首次）。
    pub job_retries: u32,
}

impl Default for ModuleConfig {
    fn default() -> Self {
        Self {
            s3: S3Settings {
                region: "us-east-1".into(),
                force_path_style: true,
                multipart_threshold: 256 * 1024 * 1024,
                ..S3Settings::default()
            },
            auto_upload: true,
            recordings_root: PathBuf::from("/var/lib/freeswitch/recordings"),
            key_prefix: "callcenter/recordings".into(),
            key_template: String::new(),
            limits: SecurityLimits::default(),
            transcode: TranscodeSettings::default(),
            delete: DeletePolicy::default(),
            workers: 2,
            queue_size: 64,
            job_retries: 3,
        }
    }
}

impl ModuleConfig {
    /// 环境变量凭证覆盖 conf 值（环境变量优先）。
    pub fn apply_env_credentials(&mut self) {
        if let Ok(v) = std::env::var("MOD_S3_ACCESS_KEY_ID") {
            if !v.is_empty() {
                self.s3.access_key = Some(v);
            }
        }
        if let Ok(v) = std::env::var("MOD_S3_SECRET_ACCESS_KEY") {
            if !v.is_empty() {
                self.s3.secret_key = Some(v);
            }
        }
        if let Ok(v) = std::env::var("MOD_S3_ENDPOINT") {
            if !v.is_empty() {
                self.s3.endpoint = Some(v);
            }
        }
        if let Ok(v) = std::env::var("MOD_S3_BUCKET") {
            if !v.is_empty() {
                self.s3.default_bucket = Some(v);
            }
        }
        if let Ok(v) = std::env::var("MOD_S3_REGION") {
            if !v.is_empty() {
                self.s3.region = v;
            }
        }
    }
}
