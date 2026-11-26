//! ESL API 命令：`s3_upload`（全局）、`uuid_s3`（会话级）、
//! `s3_jobs`、`s3_cancel`。
//!
//! 响应约定：成功 `+OK ...`，失败 `-ERR ...`，参数不足 `-USAGE`。

use fswtch::sys;
use fswtch::{ApiStream, Status};
use s3_core::job::{JobStatus, Trigger};
use s3_core::uploader::{PresignMethod, PresignRequest};

use crate::enqueue::{self, EnqueueRequest, Shared};
use crate::sysutil;

/// `Option<String>` 参数：空串视为 None。
fn opt(v: Option<&[u8]>) -> Option<String> {
    v.and_then(|b| std::str::from_utf8(b).ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// 解析 `delete` 参数：`1/true/yes/on/delete` → Some(true)，
/// `0/false/no/off/keep` → Some(false)，缺失/其他 → None。
fn parse_delete(v: Option<&[u8]>) -> Option<bool> {
    let s = std::str::from_utf8(v?).ok()?.trim().to_ascii_lowercase();
    match s.as_str() {
        "1" | "true" | "yes" | "on" | "delete" => Some(true),
        "0" | "false" | "no" | "off" | "keep" => Some(false),
        _ => None,
    }
}

/// 解析 target_format：`orig/none/copy` → None，其余原样。
fn parse_format(v: Option<&[u8]>) -> Option<String> {
    let s = opt(v)?;
    match s.to_ascii_lowercase().as_str() {
        "orig" | "none" | "copy" => None,
        _ => Some(s),
    }
}

fn write(stream: ApiStream, text: &str) -> Status {
    stream.write(text);
    fswtch::SUCCESS
}

fn err(stream: ApiStream, msg: &str) -> Status {
    write(stream, &format!("-ERR {msg}\n"))
}

/// `s3_upload <path> [bucket] [key] [format] [delete] [metadata]`
fn dispatch_upload(args: &[Vec<u8>], stream: ApiStream, shared: &Shared) -> Status {
    let Some(path) = opt(args.first().map(Vec::as_slice)) else {
        return write(
            stream,
            "-USAGE s3_upload <path> [bucket] [key] [format] [delete] [metadata]\n",
        );
    };
    match enqueue::enqueue(
        shared,
        EnqueueRequest {
            path,
            bucket: opt(args.get(1).map(Vec::as_slice)),
            key: opt(args.get(2).map(Vec::as_slice)),
            target_format: parse_format(args.get(3).map(Vec::as_slice)),
            delete_override: parse_delete(args.get(4).map(Vec::as_slice)),
            metadata: opt(args.get(5).map(Vec::as_slice)),
            vars: Default::default(),
            trigger: Trigger::Api,
            uuid: None,
        },
    ) {
        Ok(id) => write(stream, &format!("+OK job={id}\n")),
        Err(e) => err(stream, &e),
    }
}

/// `uuid_s3 <uuid> upload|jobs|cancel ...`
fn dispatch_uuid(args: &[Vec<u8>], stream: ApiStream, shared: &Shared) -> Status {
    let uuid = match opt(args.first().map(Vec::as_slice)) {
        Some(u) => u,
        None => {
            return write(stream, "-USAGE uuid_s3 <uuid> upload|jobs|cancel ...\n");
        }
    };
    let action = opt(args.get(1).map(Vec::as_slice)).unwrap_or_default();

    match action.to_ascii_lowercase().as_str() {
        "upload" => {
            // args: uuid(0) upload(1) path(2) format(3) bucket(4) key(5) delete(6) metadata(7)
            let Some(path) = opt(args.get(2).map(Vec::as_slice)) else {
                return write(
                    stream,
                    "-USAGE uuid_s3 <uuid> upload <path> [format] [bucket] [key] [delete] [metadata]\n",
                );
            };
            // SAFETY: locate/unlock 在函数内部配对完成。
            let vars = unsafe { snapshot_vars(&uuid) };
            match enqueue::enqueue(
                shared,
                EnqueueRequest {
                    path,
                    bucket: opt(args.get(4).map(Vec::as_slice)),
                    key: opt(args.get(5).map(Vec::as_slice)),
                    target_format: parse_format(args.get(3).map(Vec::as_slice)),
                    delete_override: parse_delete(args.get(6).map(Vec::as_slice)),
                    metadata: opt(args.get(7).map(Vec::as_slice)),
                    vars,
                    trigger: Trigger::ApiSession { uuid: uuid.clone() },
                    uuid: Some(uuid),
                },
            ) {
                Ok(id) => write(stream, &format!("+OK job={id}\n")),
                Err(e) => err(stream, &e),
            }
        }
        "jobs" => dispatch_jobs(args.get(2).map(Vec::as_slice), stream, shared),
        "cancel" => {
            let id = opt(args.get(2).map(Vec::as_slice)).and_then(|s| s.parse::<u64>().ok());
            match id {
                Some(id) if shared.registry.cancel(id) => {
                    write(stream, &format!("+OK cancelled job={id}\n"))
                }
                Some(id) => err(stream, &format!("job {id} not cancellable")),
                None => write(stream, "-USAGE uuid_s3 <uuid> cancel <job_id>\n"),
            }
        }
        _ => write(stream, "-USAGE uuid_s3 <uuid> upload|jobs|cancel ...\n"),
    }
}

/// 快照 channel variables；会话不存在返回空 map。
/// SAFETY: 内部 locate/unlock 配对；channel 读取在读锁内完成。
unsafe fn snapshot_vars(uuid: &str) -> std::collections::BTreeMap<String, String> {
    let session = sysutil::locate_session(uuid.as_bytes());
    if session.is_null() {
        sysutil::log_info(&format!(
            "mod_s3: session {uuid} not found, channel vars unavailable"
        ));
        return Default::default();
    }
    // SAFETY: `session` 来自 locate_session，持有读锁；快照在锁内完成。
    let vars = unsafe { sysutil::channel_vars_snapshot(session) };
    // SAFETY: 释放 locate 读锁。
    unsafe { sysutil::rwunlock(session) };
    vars
}

/// `s3_jobs [status]`：每行一条 `job=.. status=.. path=.. key=..` 摘要。
fn dispatch_jobs(arg: Option<&[u8]>, stream: ApiStream, shared: &Shared) -> Status {
    let raw = opt(arg);
    let status = match raw.as_deref().map(str::to_ascii_lowercase) {
        None => None,
        Some(s) => match s.as_str() {
            "queued" => Some(JobStatus::Queued),
            "running" => Some(JobStatus::Running),
            "done" | "success" => Some(JobStatus::Done),
            "failed" | "error" => Some(JobStatus::Failed),
            "cancelled" | "canceled" => Some(JobStatus::Cancelled),
            _ => {
                return err(
                    stream,
                    "unknown status (queued|running|done|failed|cancelled)",
                );
            }
        },
    };
    let records = shared.registry.list(status);
    let mut out = String::new();
    for rec in &records {
        let o = &rec.outcome;
        out.push_str(&format!(
            "job={} status={} path={} key={} size={} error={}\n",
            rec.job.id,
            o.status.as_str(),
            rec.job.path.display(),
            o.key.as_deref().unwrap_or("-"),
            o.size_bytes,
            o.error.as_deref().unwrap_or("-"),
        ));
    }
    if records.is_empty() {
        out.push_str("no jobs\n");
    }
    write(stream, &out)
}

/// `s3_cancel <job_id>`。
fn dispatch_cancel(args: &[Vec<u8>], stream: ApiStream, shared: &Shared) -> Status {
    let id = opt(args.first().map(Vec::as_slice)).and_then(|s| s.parse::<u64>().ok());
    match id {
        Some(id) if shared.registry.cancel(id) => {
            write(stream, &format!("+OK cancelled job={id}\n"))
        }
        Some(id) => err(stream, &format!("job {id} not cancellable")),
        None => write(stream, "-USAGE s3_cancel <job_id>\n"),
    }
}

/// `s3_presign <get|put> <key> [bucket] [expires_secs]` → `+OK url=...`
///
/// 预签名是本地 SigV4 签名（无网络 IO），用临时 current_thread
/// runtime 执行，不占用 worker 的 multi_thread runtime。
fn dispatch_presign(args: &[Vec<u8>], stream: ApiStream, shared: &Shared) -> Status {
    let usage = "-USAGE s3_presign <get|put> <key> [bucket] [expires_secs]\n";
    let method = match opt(args.first().map(Vec::as_slice))
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "get" | "download" => PresignMethod::Get,
        "put" | "upload" => PresignMethod::Put,
        _ => return write(stream, usage),
    };
    let Some(key) = opt(args.get(1).map(Vec::as_slice)) else {
        return write(stream, usage);
    };
    let bucket = match opt(args.get(2).map(Vec::as_slice))
        .or_else(|| shared.config.s3.default_bucket.clone())
    {
        Some(b) => b,
        None => return err(stream, "no bucket (arg or default-bucket)"),
    };
    let expires_secs = opt(args.get(3).map(Vec::as_slice))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3600);

    let rt = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(rt) => rt,
        Err(e) => return err(stream, &format!("runtime: {e}")),
    };
    let result = rt.block_on(shared.store.presign(PresignRequest {
        bucket,
        key,
        method,
        expires_secs,
    }));
    match result {
        Ok(url) => write(stream, &format!("+OK url={url}\n")),
        Err(e) => err(stream, &e.to_string()),
    }
}

fn dispatch(name: &str, args: &[Vec<u8>], stream: ApiStream, shared: &Shared) -> Status {
    match name {
        "s3_upload" => dispatch_upload(args, stream, shared),
        "uuid_s3" => dispatch_uuid(args, stream, shared),
        "s3_jobs" => dispatch_jobs(args.first().map(Vec::as_slice), stream, shared),
        "s3_cancel" => dispatch_cancel(args, stream, shared),
        "s3_presign" => dispatch_presign(args, stream, shared),
        _ => write(stream, "-ERR unknown api\n"),
    }
}

/// 统一入口包装：panic 隔离 + shared 状态获取 + 参数拆分。
fn run(name: &'static str, cmd: Option<String>, stream: ApiStream) -> Status {
    let Some(shared) = crate::shared_arc() else {
        return write(stream, "-ERR mod_s3 not initialized\n");
    };
    let args = cmd
        .as_deref()
        .map(|text| sysutil::separate_args(text.as_bytes(), 16))
        .unwrap_or_default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(name, &args, stream, shared.as_ref())
    }));
    match result {
        Ok(status) => status,
        Err(_) => {
            fswtch::log_error(sysutil::MODULE_NAME, "api panicked");
            let _ = write(stream, "-ERR mod_s3 internal error\n");
            fswtch::SUCCESS
        }
    }
}

fswtch::api_callback!(fn s3_upload_impl(cmd, _session, stream) {
    run("s3_upload", cmd, stream)
});

/// SAFETY: FreeSWITCH ABI 实参原样转发。
pub unsafe extern "C" fn s3_upload(
    cmd: *const std::ffi::c_char,
    session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status {
    // SAFETY: 调用方契约与 fswtch 生成回调一致。
    unsafe { s3_upload_impl(cmd, session, stream) }
}

fswtch::api_callback!(fn uuid_s3_impl(cmd, _session, stream) {
    run("uuid_s3", cmd, stream)
});

/// SAFETY: FreeSWITCH ABI 实参原样转发。
pub unsafe extern "C" fn uuid_s3(
    cmd: *const std::ffi::c_char,
    session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status {
    // SAFETY: 调用方契约与 fswtch 生成回调一致。
    unsafe { uuid_s3_impl(cmd, session, stream) }
}

fswtch::api_callback!(fn s3_jobs_impl(cmd, _session, stream) {
    run("s3_jobs", cmd, stream)
});

/// SAFETY: FreeSWITCH ABI 实参原样转发。
pub unsafe extern "C" fn s3_jobs(
    cmd: *const std::ffi::c_char,
    session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status {
    // SAFETY: 调用方契约与 fswtch 生成回调一致。
    unsafe { s3_jobs_impl(cmd, session, stream) }
}

fswtch::api_callback!(fn s3_cancel_impl(cmd, _session, stream) {
    run("s3_cancel", cmd, stream)
});

/// SAFETY: FreeSWITCH ABI 实参原样转发。
pub unsafe extern "C" fn s3_cancel(
    cmd: *const std::ffi::c_char,
    session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status {
    // SAFETY: 调用方契约与 fswtch 生成回调一致。
    unsafe { s3_cancel_impl(cmd, session, stream) }
}

fswtch::api_callback!(fn s3_presign_impl(cmd, _session, stream) {
    run("s3_presign", cmd, stream)
});

/// SAFETY: FreeSWITCH ABI 实参原样转发。
pub unsafe extern "C" fn s3_presign(
    cmd: *const std::ffi::c_char,
    session: *mut sys::switch_core_session_t,
    stream: *mut sys::switch_stream_handle_t,
) -> Status {
    // SAFETY: 调用方契约与 fswtch 生成回调一致。
    unsafe { s3_presign_impl(cmd, session, stream) }
}
