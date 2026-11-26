//! FreeSWITCH `mod_s3` 模块的 Rust 实现：FS 进程内 S3 直传。
//!
//! 能力：
//! - `s3_upload` / `uuid_s3` / `s3_jobs` / `s3_cancel` / `s3_presign`
//!   ESL API；
//! - `SWITCH_EVENT_RECORD_START`/`RECORD_STOP` 事件绑定：开始发
//!   `mod_s3::record_start` 通知，结束自动上传（`auto-upload` 配置）；
//! - 可选 ffmpeg 转码（wav→mp3/opus/ogg 等）后上传；
//! - 上传+HEAD 校验成功后可选删除本地源文件；
//! - `mod_s3::queued` / `mod_s3::upload` / `mod_s3::progress` /
//!   `mod_s3::record_start` CUSTOM 事件回执；
//! - `SHARED` 为 RwLock，支持 `reload mod_s3` 热重载配置。
//!
//! 编译产物 `libmod_s3.so`；FreeSWITCH 按 `<name>.so` 加载，
//! 需安装为 `mod_s3.so`（或建立符号链接）。

mod api;
mod config_xml;
mod enqueue;
mod recorder;
mod sysutil;
mod worker;

use std::sync::{Arc, RwLock};

use fswtch::{Status, SUCCESS};

use enqueue::Shared;

/// 共享状态：RwLock 使 `reload mod_s3` / shutdown→load 循环可替换实例。
/// 旧实例在 shutdown 中置位 shutdown 标志并移除引用；仍被 worker/API
/// 持有 Arc 的旧实例随引用归零自然析构。
static SHARED: RwLock<Option<Arc<Shared>>> = RwLock::new(None);

fswtch::module_exports! {
    module = mod_s3,
    load = switch_module_load,
    shutdown = Some(switch_module_shutdown),
    runtime = None,
}

const EVENT_SUBCLASSES: [&str; 4] = [
    "mod_s3::queued",
    "mod_s3::upload",
    "mod_s3::progress",
    "mod_s3::record_start",
];

/// API/事件回调访问共享状态；未初始化返回 None（Arc 克隆开销极小，
/// 调用方按需持有到回调结束）。
pub(crate) fn shared_arc() -> Option<Arc<Shared>> {
    SHARED
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

fswtch::module_load! {
    fn switch_module_load(module) for "mod_s3" {
        fswtch::log_info(sysutil::MODULE_NAME, "mod_s3 API loading..");

        for subclass in EVENT_SUBCLASSES {
            let name = std::ffi::CString::new(subclass).unwrap_or_default();
            // SAFETY: `name` 在调用期间是存活的 C 字符串；
            // FreeSWITCH 跟踪该预留直到卸载时释放。
            let status = unsafe {
                fswtch::sys::switch_event_reserve_subclass_detailed(
                    c"mod_s3".as_ptr(),
                    name.as_ptr(),
                )
            };
            if let Err(error) = fswtch::status_to_result(status) {
                fswtch::log_error(
                    sysutil::MODULE_NAME,
                    format!("Couldn't register event subclass {subclass}: {error}"),
                );
                return error.0;
            }
        }

        let config = config_xml::load();
        sysutil::log_info(&format!(
            "mod_s3: config loaded endpoint={:?} bucket={:?} auto_upload={} transcode={:?} target={:?} delete_after={}",
            config.s3.endpoint,
            config.s3.default_bucket,
            config.auto_upload,
            config.transcode.mode,
            config.transcode.target_format,
            config.delete.after_upload,
        ));

        let (shared, rx) = Shared::new(config);
        let auto_upload = shared.config.auto_upload;
        {
            // 热重载路径：若旧实例仍在（异常 load 未走 shutdown），
            // 先置其 shutdown 标志再替换，保证旧 worker 退出。
            let mut slot = SHARED.write().unwrap_or_else(|p| p.into_inner());
            if let Some(old) = slot.take() {
                old.shutdown
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            *slot = Some(shared.clone());
        }

        // 启动 worker 线程（独占 tokio runtime）。
        let _worker = worker::spawn(shared.clone(), rx);
        // worker handle 不 join：shutdown 时 channel 关闭自然退出。
        std::mem::forget(_worker);

        if auto_upload {
            let bound_shared = shared.clone();
            if !recorder::bind(&bound_shared) {
                sysutil::log_error("mod_s3: auto-upload enabled but event bind failed");
            }
        }

        module
            .api(
                c"s3_upload",
                c"upload file to S3",
                c"s3_upload <path> [bucket] [key] [format] [delete] [metadata]",
                api::s3_upload,
            )
            .and_then(|m| {
                m.api(
                    c"uuid_s3",
                    c"session-aware S3 upload",
                    c"uuid_s3 <uuid> upload <path> [format] [bucket] [key] [delete] [metadata] | uuid_s3 <uuid> jobs|cancel ...",
                    api::uuid_s3,
                )
            })
            .and_then(|m| {
                m.api(
                    c"s3_jobs",
                    c"list S3 upload jobs",
                    c"s3_jobs [status]",
                    api::s3_jobs,
                )
            })
            .and_then(|m| {
                m.api(
                    c"s3_cancel",
                    c"cancel queued S3 upload job",
                    c"s3_cancel <job_id>",
                    api::s3_cancel,
                )
            })
            .and_then(|m| {
                m.api(
                    c"s3_presign",
                    c"generate S3 presigned URL",
                    c"s3_presign <get|put> <key> [bucket] [expires_secs]",
                    api::s3_presign,
                )
            })
    }
}

// SAFETY: FreeSWITCH 以模块 ABI 调用卸载回调；只解绑事件、置 worker
// 退出标志并释放子类。
unsafe extern "C" fn switch_module_shutdown() -> Status {
    recorder::unbind();
    let old = SHARED.write().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(shared) = old {
        shared.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    for subclass in EVENT_SUBCLASSES {
        let name = std::ffi::CString::new(subclass).unwrap_or_default();
        // SAFETY: `name` 是存活的 C 字符串，且该子类是本模块预留的。
        unsafe {
            fswtch::sys::switch_event_free_subclass_detailed(
                c"mod_s3".as_ptr(),
                name.as_ptr(),
            );
        }
    }
    SUCCESS
}
