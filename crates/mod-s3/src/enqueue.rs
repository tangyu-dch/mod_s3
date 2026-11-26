//! 任务入队共享逻辑：API 与 RECORD_STOP 回调都走这里。
//! 顺序：组装 UploadJob → pathguard 校验 → registry.register →
//! queue.try_send → `mod_s3::queued` 事件。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use s3_core::config::ModuleConfig;
use s3_core::job::{JobQueue, JobRegistry, Trigger, UploadJob};
use s3_core::pathguard::PathGuard;
use s3_core::uploader::{build_store, SharedStore};

use crate::sysutil;

/// 模块共享状态（lib.rs 持有，支持热重载替换）。
pub struct Shared {
    pub config: ModuleConfig,
    pub guard: PathGuard,
    pub registry: Arc<JobRegistry>,
    pub queue: JobQueue,
    /// S3 对象存储：worker 上传与 API 侧 presign 共用同一 client。
    pub store: SharedStore,
    /// 模块卸载标志：worker 轮询队列超时后检查，置位则退出。
    /// （Sender 寄生在 Shared 中无法随卸载关闭 channel，
    /// 用标志位实现等价语义。）
    pub shutdown: Arc<AtomicBool>,
}

impl Shared {
    /// 创建共享状态并返回（state, receiver）。
    /// receiver 交给 worker 线程。
    pub fn new(config: ModuleConfig) -> (Arc<Self>, std::sync::mpsc::Receiver<UploadJob>) {
        let store = build_store(&config.s3);
        let guard = PathGuard::new(config.limits.clone());
        let registry = JobRegistry::new();
        let (queue, rx) = JobQueue::new(config.queue_size);
        let state = Arc::new(Self {
            config,
            guard,
            registry,
            queue,
            store,
            shutdown: Arc::new(AtomicBool::new(false)),
        });
        (state, rx)
    }
}

/// 入队参数（API/事件回调的公共子集）。
pub struct EnqueueRequest {
    /// 用户输入路径（绝对或相对 recordings_root）。
    pub path: String,
    pub bucket: Option<String>,
    pub key: Option<String>,
    pub target_format: Option<String>,
    pub delete_override: Option<bool>,
    pub metadata: Option<String>,
    pub vars: BTreeMap<String, String>,
    pub trigger: Trigger,
    /// 关联 uuid（事件挂接与回执定位）。
    pub uuid: Option<String>,
}

/// 校验并入队；返回 job id 或错误描述（调用方转 `-ERR`）。
pub fn enqueue(shared: &Shared, req: EnqueueRequest) -> Result<u64, String> {
    // 相对路径先拼 recordings_root，再交 pathguard 判定。
    let candidate = if std::path::Path::new(&req.path).is_absolute() {
        req.path.clone()
    } else {
        shared
            .config
            .recordings_root
            .join(&req.path)
            .to_string_lossy()
            .into_owned()
    };

    let file = shared
        .guard
        .check(&candidate)
        .map_err(|e| e.to_string())?;

    let mut vars = req.vars;
    // 文件派生变量：worker 渲染 key 模板时使用。
    vars.insert("file".into(), file.filename.clone());
    vars.insert(
        "file_stem".into(),
        file.filename
            .rsplit_once('.')
            .map(|(stem, _)| stem.to_owned())
            .unwrap_or_else(|| file.filename.clone()),
    );
    vars.insert("file_ext".into(), file.ext.clone());
    vars.insert("prefix".into(), shared.config.key_prefix.clone());
    if let Some(uuid) = &req.uuid {
        vars.entry("uuid".to_owned()).or_insert_with(|| uuid.clone());
    }

    let job = UploadJob {
        id: 0,
        path: PathBuf::from(&file.path),
        src_ext: file.ext.clone(),
        filename: file.filename,
        bucket: req.bucket,
        key: req.key,
        target_format: req.target_format,
        delete_override: req.delete_override,
        metadata: req.metadata,
        vars,
        trigger: req.trigger,
        created: std::time::SystemTime::now(),
    };

    let id = shared.registry.register(job.clone());
    let mut job = job;
    job.id = id;

    if !shared.queue.try_send(job) {
        shared.registry.set_status(
            id,
            s3_core::job::JobStatus::Cancelled,
        );
        return Err("queue full".to_owned());
    }

    emit_queued(id, req.uuid.as_ref());
    Ok(id)
}

/// `mod_s3::queued` 事件。
pub fn emit_queued(job_id: u64, uuid: Option<&String>) {
    let body = format!("{{\"job_id\":{},\"status\":\"queued\"}}", job_id);
    sysutil::emit_event(uuid.map(String::as_str), "queued", &body);
}
