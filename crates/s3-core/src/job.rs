//! 上传任务模型、有界队列与任务注册表（供 `s3_jobs`/`s3_cancel` 查询）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::uploader::UploadReceipt;

/// 任务来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// `s3_upload` 全局 API。
    Api,
    /// `uuid_s3` 会话级 API（vars 含 channel variables）。
    ApiSession { uuid: String },
    /// RECORD_STOP 自动上传。
    RecordStop { uuid: String },
}

/// 上传任务：worker 消费的最小输入。
#[derive(Clone, Debug)]
pub struct UploadJob {
    pub id: u64,
    /// 已通过 pathguard 校验的源文件绝对路径。
    pub path: PathBuf,
    /// 源文件扩展名（含点，小写）。
    pub src_ext: String,
    /// 源文件名。
    pub filename: String,
    /// 显式 bucket；`None` 用配置默认。
    pub bucket: Option<String>,
    /// 显式 object key；`None` 时 worker 按模板渲染。
    pub key: Option<String>,
    /// 目标格式覆盖（`mp3`/`opus`/`ogg`/`wav`/`orig`）。
    pub target_format: Option<String>,
    /// 本任务覆盖的删除策略：`Some(true/false)` 优先于全局配置。
    pub delete_override: Option<bool>,
    /// metadata JSON 原文（透传到事件，不进 S3 metadata）。
    pub metadata: Option<String>,
    /// key 模板变量（channel vars / 事件头 / uuid 等）。
    pub vars: BTreeMap<String, String>,
    pub trigger: Trigger,
    pub created: SystemTime,
}

/// 任务状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl Default for JobStatus {
    fn default() -> Self {
        Self::Queued
    }
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// 任务终态详情（成功/失败共用）。
#[derive(Clone, Debug, Default)]
pub struct JobOutcome {
    pub status: JobStatus,
    pub key: Option<String>,
    pub bucket: Option<String>,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub etag: Option<String>,
    pub src_format: Option<String>,
    pub dst_format: Option<String>,
    pub deleted: bool,
    pub transcode_ms: u64,
    pub upload_ms: u64,
    pub error: Option<String>,
}

/// 注册表中的一条任务记录。
#[derive(Clone, Debug)]
pub struct JobRecord {
    pub job: UploadJob,
    pub outcome: JobOutcome,
}

/// 任务注册表：API 查询与 worker 更新共享。
#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    next_id: u64,
    jobs: BTreeMap<u64, JobRecord>,
}

/// 注册表容量上限：超出后淘汰最旧的终态记录（保留进行中任务）。
const REGISTRY_CAP: usize = 1024;

impl JobRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 分配新 job id 并登记 Queued 记录。
    pub fn register(&self, job: UploadJob) -> u64 {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.next_id += 1;
        let id = inner.next_id;
        let mut job = job;
        job.id = id;
        inner.jobs.insert(
            id,
            JobRecord {
                job,
                outcome: JobOutcome {
                    status: JobStatus::Queued,
                    ..JobOutcome::default()
                },
            },
        );
        self.evict_locked(&mut inner);
        id
    }

    fn evict_locked(&self, inner: &mut RegistryInner) {
        if inner.jobs.len() <= REGISTRY_CAP {
            return;
        }
        // 淘汰最旧的终态记录；进行中任务永不淘汰。
        let mut candidates: Vec<u64> = inner
            .jobs
            .iter()
            .filter(|(_, r)| {
                matches!(
                    r.outcome.status,
                    JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled
                )
            })
            .map(|(id, _)| *id)
            .collect();
        candidates.sort_by_key(|id| *id);
        for id in candidates {
            if inner.jobs.len() <= REGISTRY_CAP {
                break;
            }
            inner.jobs.remove(&id);
        }
    }

    /// 更新任务状态（worker 侧）。
    pub fn set_status(&self, id: u64, status: JobStatus) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(rec) = inner.jobs.get_mut(&id) {
            rec.outcome.status = status;
        }
    }

    /// 写入终态结果。
    pub fn finish(&self, id: u64, outcome: JobOutcome) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(rec) = inner.jobs.get_mut(&id) {
            rec.outcome = outcome;
        }
    }

    /// 取消排队中的任务；运行中/终态返回 false。
    pub fn cancel(&self, id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match inner.jobs.get_mut(&id) {
            Some(rec) if rec.outcome.status == JobStatus::Queued => {
                rec.outcome.status = JobStatus::Cancelled;
                true
            }
            _ => false,
        }
    }

    /// 查询单个任务。
    pub fn get(&self, id: u64) -> Option<JobRecord> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .jobs
            .get(&id)
            .cloned()
    }

    /// 列出任务（可按状态过滤），按 id 升序。
    pub fn list(&self, status: Option<JobStatus>) -> Vec<JobRecord> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .jobs
            .values()
            .filter(|r| status.map_or(true, |s| r.outcome.status == s))
            .cloned()
            .collect()
    }
}

/// 有界任务队列：`try_send` 满时返回 `Err` 由调用方决定背压响应。
pub struct JobQueue {
    tx: SyncSender<UploadJob>,
}

impl JobQueue {
    /// 创建容量为 `capacity` 的队列。
    pub fn new(capacity: usize) -> (Self, Receiver<UploadJob>) {
        let (tx, rx) = sync_channel(capacity.max(1));
        (Self { tx }, rx)
    }

    /// 非阻塞入队；队列满返回 false。
    pub fn try_send(&self, job: UploadJob) -> bool {
        self.tx.try_send(job).is_ok()
    }
}

impl Clone for JobQueue {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

/// 由 `UploadReceipt` 构造成功 outcome 的便捷方法。
pub fn success_outcome(
    key: String,
    bucket: String,
    size: u64,
    sha256: String,
    receipt: &UploadReceipt,
    src_format: Option<String>,
    dst_format: Option<String>,
    deleted: bool,
    transcode_ms: u64,
    upload_ms: u64,
) -> JobOutcome {
    JobOutcome {
        status: JobStatus::Done,
        key: Some(key),
        bucket: Some(bucket),
        size_bytes: size,
        sha256: Some(sha256),
        etag: receipt.etag.clone(),
        src_format,
        dst_format,
        deleted,
        transcode_ms,
        upload_ms,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample_job(id_path: &str) -> UploadJob {
        UploadJob {
            id: 0,
            path: PathBuf::from(id_path),
            src_ext: ".wav".into(),
            filename: "a.wav".into(),
            bucket: None,
            key: None,
            target_format: None,
            delete_override: None,
            metadata: None,
            vars: BTreeMap::new(),
            trigger: Trigger::Api,
            created: SystemTime::now(),
        }
    }

    #[test]
    fn registry_lifecycle() {
        let reg = JobRegistry::new();
        let id = reg.register(sample_job("/tmp/a.wav"));
        assert_eq!(id, 1);
        assert_eq!(reg.get(id).unwrap().outcome.status, JobStatus::Queued);
        reg.set_status(id, JobStatus::Running);
        assert!(reg.cancel(id) == false);
        reg.set_status(id, JobStatus::Queued);
        assert!(reg.cancel(id));
        assert_eq!(reg.get(id).unwrap().outcome.status, JobStatus::Cancelled);
    }

    #[test]
    fn queue_backpressure() {
        let (q, _rx) = JobQueue::new(2);
        assert!(q.try_send(sample_job("/a")));
        assert!(q.try_send(sample_job("/b")));
        assert!(!q.try_send(sample_job("/c")));
    }

    #[test]
    fn list_filters_by_status() {
        let reg = JobRegistry::new();
        let a = reg.register(sample_job("/a"));
        let b = reg.register(sample_job("/b"));
        reg.finish(
            a,
            JobOutcome {
                status: JobStatus::Done,
                ..JobOutcome::default()
            },
        );
        reg.finish(
            b,
            JobOutcome {
                status: JobStatus::Failed,
                error: Some("x".into()),
                ..JobOutcome::default()
            },
        );
        assert_eq!(reg.list(Some(JobStatus::Done)).len(), 1);
        assert_eq!(reg.list(Some(JobStatus::Failed)).len(), 1);
        assert_eq!(reg.list(None).len(), 2);
        let _ = Duration::from_secs(0);
    }
}
