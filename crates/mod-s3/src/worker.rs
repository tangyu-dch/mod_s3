//! 上传 worker：单消费线程 + 独占 tokio runtime。
//!
//! 每个任务流水线：转码（可选）→ sha256 → S3 put+head verify（带重试）
//! → 可选删除源文件 → 清理 temp → registry.finish → `mod_s3::upload` 事件。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use s3_core::config::ModuleConfig;
use s3_core::job::{success_outcome, JobOutcome, JobStatus, UploadJob};
use s3_core::sha256::file_sha256;
use s3_core::transcode::{
    default_args_for, run_ffmpeg, same_format, temp_output_path, TranscodeRequest,
};
use s3_core::uploader::{
    content_type_for, ProgressCallback, SharedStore, UploadRequest,
};

use crate::enqueue::Shared;
use crate::sysutil;

/// 启动 worker 线程；返回 JoinHandle（shutdown 时不主动 join，
/// 由 channel 关闭自然退出）。
pub fn spawn(shared: Arc<Shared>, rx: Receiver<UploadJob>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("mod_s3-worker".into())
        .spawn(move || run(shared, rx))
        .expect("spawn mod_s3-worker")
}

fn run(shared: Arc<Shared>, rx: Receiver<UploadJob>) {
    let config = shared.config.clone();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.workers.max(1))
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            sysutil::log_error(&format!("mod_s3: tokio runtime build failed: {e}"));
            return;
        }
    };
    let store = shared.store.clone();

    while !shared.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        // recv_timeout：shutdown 标志无法关闭 channel（Sender 随
        // OnceLock 常驻），靠周期性超时检查标志退出。
        let job = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(job) => job,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // 任务级 panic 隔离：单个任务崩溃不影响队列。
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_job(&rt, &store, &shared, &job);
        }));
    }
    sysutil::log_info("mod_s3: worker thread exiting (shutdown)");
}

/// 单个任务的完整流水线。
fn process_job(
    rt: &tokio::runtime::Runtime,
    store: &SharedStore,
    shared: &Shared,
    job: &UploadJob,
) {
    shared.registry.set_status(job.id, JobStatus::Running);

    // 已被取消的排队任务直接跳过。
    if let Some(rec) = shared.registry.get(job.id) {
        if rec.outcome.status == JobStatus::Cancelled {
            return;
        }
    }

    match execute(rt, store, shared, job) {
        Ok(outcome) => {
            shared.registry.finish(job.id, outcome.clone());
            emit_upload_event(shared, job, &outcome);
        }
        Err((mut outcome, err)) => {
            outcome.status = JobStatus::Failed;
            outcome.error = Some(err);
            let outcome = outcome;
            shared.registry.finish(job.id, outcome.clone());
            emit_upload_event(shared, job, &outcome);
        }
    }
}

/// 实际执行；返回 Ok(outcome) 或 Err((partial_outcome, error_string))。
fn execute(
    rt: &tokio::runtime::Runtime,
    store: &SharedStore,
    shared: &Shared,
    job: &UploadJob,
) -> Result<JobOutcome, (JobOutcome, String)> {
    let config = &shared.config;

    // ---------- 1. 转码决策 ----------
    let target = resolve_target_format(config, job);
    let need_transcode = should_transcode(config, &job.src_ext, target.as_deref());

    let (upload_path, dst_ext, transcode_ms) = if need_transcode {
        let Some(fmt) = target.as_deref() else {
            return Err((JobOutcome::default(), "transcode required but no target format".into()));
        };
        let dst_ext = s3_core::transcode::extension_for(fmt);
        let dst = temp_output_path(&config.transcode.temp_dir, job.id, &dst_ext);
        let _ = std::fs::create_dir_all(&config.transcode.temp_dir);
        let args = if config.transcode.ffmpeg_args.is_empty() {
            default_args_for(fmt)
        } else {
            config.transcode.ffmpeg_args.clone()
        };
        let started = Instant::now();
        let result = run_ffmpeg(
            std::ffi::OsStr::new(&config.transcode.ffmpeg_bin),
            &TranscodeRequest {
                src: &job.path,
                dst: &dst,
                args: &args,
                timeout: config.transcode.timeout,
            },
        );
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(outcome) => {
                sysutil::log_info(&format!(
                    "mod_s3: job {} transcoded {} -> {} in {}ms ({} bytes)",
                    job.id, job.src_ext, fmt, elapsed_ms, outcome.output_bytes
                ));
                (dst, dst_ext, elapsed_ms)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dst);
                return Err((
                    JobOutcome {
                        transcode_ms: elapsed_ms,
                        ..JobOutcome::default()
                    },
                    format!("transcode failed: {e}"),
                ));
            }
        }
    } else {
        (job.path.clone(), job.src_ext.trim_start_matches('.').to_string(), 0)
    };

    // 任何后续失败都必须清理 temp 产物。
    let cleanup = |p: &PathBuf| {
        if need_transcode {
            let _ = std::fs::remove_file(p);
        }
    };

    // ---------- 2. bucket / key / sha256 ----------
    let bucket = match job.bucket.clone().or_else(|| config.s3.default_bucket.clone()) {
        Some(b) => b,
        None => {
            cleanup(&upload_path);
            return Err((JobOutcome::default(), "no bucket configured".into()));
        }
    };

    let mut vars = job.vars.clone();
    vars.insert("dst_ext".into(), dst_ext.clone());
    let key = match &job.key {
        Some(k) => resolve_key(&config.key_template, &mut vars, k, &dst_ext),
        None => s3_core::keytemplate::render(
            &config.key_template,
            &vars,
            s3_core::keytemplate::DEFAULT_TEMPLATE,
        ),
    };
    if key.is_empty() {
        cleanup(&upload_path);
        return Err((JobOutcome::default(), "rendered empty object key".into()));
    }

    let meta = std::fs::metadata(&upload_path).map_err(|e| {
        cleanup(&upload_path);
        (JobOutcome::default(), format!("stat upload file: {e}"))
    })?;
    let size = meta.len();
    let sha256 = file_sha256(&upload_path).map_err(|e| {
        cleanup(&upload_path);
        (JobOutcome::default(), format!("sha256: {e}"))
    })?;

    // ---------- 3. 上传（任务级重试） ----------
    let req = UploadRequest {
        file: upload_path.clone(),
        bucket: bucket.clone(),
        key: key.clone(),
        content_type: content_type_for(&upload_path).to_string(),
        sha256: sha256.clone(),
        size,
        progress: Some(make_progress_callback(job, &key)),
    };

    let upload_started = Instant::now();
    let mut last_err = String::new();
    let mut receipt = None;
    for attempt in 0..=config.job_retries {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(500 * attempt as u64));
        }
        match rt.block_on(store.upload(req.clone())) {
            Ok(r) => {
                receipt = Some(r);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let upload_ms = upload_started.elapsed().as_millis() as u64;
    let receipt = match receipt {
        Some(r) => r,
        None => {
            cleanup(&upload_path);
            // 失败删除策略（默认 false：保留源文件便于重试）。
            let delete_on_failure = config.delete.on_failure;
            let deleted = delete_on_failure && delete_source(shared, job);
            return Err((
                JobOutcome {
                    status: JobStatus::Failed,
                    key: Some(key),
                    bucket: Some(bucket),
                    size_bytes: size,
                    sha256: Some(sha256),
                    src_format: Some(job.src_ext.trim_start_matches('.').to_string()),
                    dst_format: Some(dst_ext.clone()),
                    deleted,
                    transcode_ms,
                    error: Some(last_err.clone()),
                    ..JobOutcome::default()
                },
                last_err,
            ));
        }
    };
    // ---------- 4. 上传成功：可选删除源文件 ----------
    let delete_enabled = job
        .delete_override
        .unwrap_or(config.delete.after_upload);
    let deleted = delete_enabled && delete_source(shared, job);

    cleanup(&upload_path);

    let outcome = success_outcome(
        key,
        bucket,
        size,
        sha256,
        &receipt,
        Some(job.src_ext.trim_start_matches('.').to_string()),
        Some(dst_ext),
        deleted,
        transcode_ms,
        upload_ms,
    );
    Ok(outcome)
}

/// 解析显式 key：
/// - 普通文件名（不含 `/`、`\`、`{`）→ 自动套用 `key_template` 目录结构，
///   扩展名跟随目标格式（`{dst_ext}`），同时注入 `name`/`file`/`file_stem`/
///   `file_ext` 模板变量；
/// - 含路径分隔符或 `{` → 按完整 object key 处理（sanitize 收敛）。
fn resolve_key(
    template: &str,
    vars: &mut BTreeMap<String, String>,
    key: &str,
    dst_ext: &str,
) -> String {
    let is_simple_name = !key.contains('/') && !key.contains('\\') && !key.contains('{');
    if is_simple_name {
        // 文件名中的扩展名被目标格式扩展名替换（如 name=rec.wav + mp3 → rec.mp3）。
        let stem = key
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(key)
            .trim();
        if !stem.is_empty() {
            vars.insert("name".into(), stem.to_owned());
            vars.insert("file".into(), format!("{stem}.{dst_ext}"));
            vars.insert("file_stem".into(), stem.to_owned());
            vars.insert("file_ext".into(), dst_ext.to_owned());
            return s3_core::keytemplate::render(
                template,
                vars,
                s3_core::keytemplate::DEFAULT_TEMPLATE,
            );
        }
    }
    s3_core::keytemplate::sanitize_key(key)
}

/// 构造上传进度回调：multipart 每批分片、单次 PUT 首尾各触发一次，
/// 发出 `mod_s3::progress` 事件。
fn make_progress_callback(job: &UploadJob, key: &str) -> ProgressCallback {
    let job_id = job.id;
    let key = key.to_owned();
    let uuid = job_uuid(job);
    Arc::new(move |done, total| {
        let body = format!(
            "{{\"job_id\":{},\"key\":{},\"uploaded_bytes\":{},\"total_bytes\":{}}}",
            job_id,
            json_str(&key),
            done,
            total
        );
        sysutil::emit_event(uuid.as_deref(), "progress", &body);
    })
}

/// 任务关联的 uuid（事件挂接/回执定位）。
fn job_uuid(job: &UploadJob) -> Option<String> {
    match &job.trigger {
        s3_core::job::Trigger::ApiSession { uuid } | s3_core::job::Trigger::RecordStop { uuid } => {
            Some(uuid.clone())
        }
        s3_core::job::Trigger::Api => job.vars.get("uuid").cloned(),
    }
}

/// 解析目标格式：job 覆盖 > 配置默认；`orig`/空 → None。
fn resolve_target_format(config: &ModuleConfig, job: &UploadJob) -> Option<String> {
    let raw = job
        .target_format
        .clone()
        .or_else(|| config.transcode.target_format.clone())?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "orig" | "none" | "copy" => None,
        other => Some(other.to_string()),
    }
}

/// 是否需要转码。
fn should_transcode(config: &ModuleConfig, src_ext: &str, target: Option<&str>) -> bool {
    if config.transcode.mode == s3_core::config::TranscodeMode::None {
        return false;
    }
    let Some(target) = target else {
        return false;
    };
    match config.transcode.mode {
        s3_core::config::TranscodeMode::Ffmpeg => true,
        s3_core::config::TranscodeMode::Auto => !same_format(src_ext, target),
        s3_core::config::TranscodeMode::None => false,
    }
}

/// 删除源文件：再次 pathguard 校验（防 TOCTOU）后 unlink。
fn delete_source(shared: &Shared, job: &UploadJob) -> bool {
    let path_str = job.path.to_string_lossy().into_owned();
    match shared.guard.check(&path_str) {
        Ok(_) => match std::fs::remove_file(&job.path) {
            Ok(()) => {
                sysutil::log_info(&format!("mod_s3: job {} deleted {}", job.id, path_str));
                true
            }
            Err(e) => {
                sysutil::log_error(&format!("mod_s3: delete {} failed: {e}", path_str));
                false
            }
        },
        Err(e) => {
            sysutil::log_error(&format!(
                "mod_s3: delete guard re-check failed for {}: {e}",
                path_str
            ));
            false
        }
    }
}

/// 构造并发出 `mod_s3::upload` 完成事件（body 为 JSON）。
fn emit_upload_event(shared: &Shared, job: &UploadJob, outcome: &JobOutcome) {
    let uuid = job_uuid(job);

    let status = outcome.status.as_str();
    let mut body = format!(
        "{{\"job_id\":{},\"status\":\"{}\",\"path\":{},\"key\":{},\"bucket\":{},\"size_bytes\":{},\"sha256\":{},\"etag\":{},\"src_format\":{},\"dst_format\":{},\"deleted\":{},\"transcode_ms\":{},\"upload_ms\":{},\"trigger\":\"{}\"",
        job.id,
        status,
        json_str(&job.path.to_string_lossy()),
        opt_json_str(outcome.key.as_deref()),
        opt_json_str(outcome.bucket.as_deref()),
        outcome.size_bytes,
        opt_json_str(outcome.sha256.as_deref()),
        opt_json_str(outcome.etag.as_deref()),
        opt_json_str(outcome.src_format.as_deref()),
        opt_json_str(outcome.dst_format.as_deref()),
        outcome.deleted,
        outcome.transcode_ms,
        outcome.upload_ms,
        trigger_name(&job.trigger),
    );
    if let Some(err) = &outcome.error {
        body.push_str(&format!(",\"error\":{}", json_str(err)));
    }
    if let Some(md) = &job.metadata {
        // metadata 约定为 JSON 片段原文内嵌；用 serde_json 严格校验，
        // 非合法 JSON 降级为字符串字段，避免破坏事件 body 的 JSON 结构。
        if serde_json::from_str::<serde_json::Value>(md).is_ok() {
            body.push_str(&format!(",\"metadata\":{}", md.trim()));
        } else {
            body.push_str(&format!(",\"metadata\":{}", json_str(md)));
        }
    }
    if let Some(u) = &uuid {
        body.push_str(&format!(",\"uuid\":{}", json_str(u)));
    }
    body.push('}');

    let _ = shared;
    sysutil::emit_event(uuid.as_deref(), "upload", &body);
}

fn trigger_name(t: &s3_core::job::Trigger) -> &'static str {
    match t {
        s3_core::job::Trigger::Api => "api",
        s3_core::job::Trigger::ApiSession { .. } => "api_session",
        s3_core::job::Trigger::RecordStop { .. } => "record_stop",
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn opt_json_str(v: Option<&str>) -> String {
    match v {
        Some(s) => json_str(s),
        None => "null".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_name_renders_template_with_dst_ext() {
        let mut vars = BTreeMap::new();
        vars.insert("prefix".into(), "callcenter/recordings".into());
        vars.insert("uuid".into(), "u-1".into());
        let key = resolve_key("", &mut vars, "myrec.wav", "mp3");
        assert!(key.starts_with("callcenter/recordings/"));
        assert!(key.ends_with("/myrec.mp3"));
        assert_eq!(vars.get("name").map(String::as_str), Some("myrec"));
        assert_eq!(vars.get("file").map(String::as_str), Some("myrec.mp3"));
        assert_eq!(vars.get("file_ext").map(String::as_str), Some("mp3"));
    }

    #[test]
    fn simple_name_strips_only_last_extension() {
        let mut vars = BTreeMap::new();
        vars.insert("prefix".into(), "p".into());
        let key = resolve_key("", &mut vars, "call.2024.wav", "wav");
        assert!(key.ends_with("/call.2024.wav"));
    }

    #[test]
    fn key_with_slash_is_full_object_key() {
        let mut vars = BTreeMap::new();
        let key = resolve_key("{prefix}/{file}", &mut vars, "custom/dir/rec.mp3", "mp3");
        assert_eq!(key, "custom/dir/rec.mp3");
        assert!(!vars.contains_key("name"));
    }

    #[test]
    fn key_with_braces_is_full_object_key() {
        let mut vars = BTreeMap::new();
        let key = resolve_key("", &mut vars, "{uuid}/rec", "mp3");
        // 含 `{` 走显式 key 路径：sanitize 收敛，`{`/`}` → `-`。
        assert_eq!(key, "-uuid-/rec");
    }

    #[test]
    fn empty_stem_falls_back_to_explicit_key() {
        let mut vars = BTreeMap::new();
        let key = resolve_key("", &mut vars, ".mp3", "mp3");
        assert_eq!(key, ".mp3");
    }
}
