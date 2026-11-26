//! `SWITCH_EVENT_RECORD_START` / `SWITCH_EVENT_RECORD_STOP` 事件绑定。
//!
//! RECORD_START 只发 `mod_s3::record_start` 通知事件；RECORD_STOP
//! 提取路径 + 快照变量 + 入队上传。回调在 FreeSWITCH 事件线程执行，
//! 绝不做网络 IO；队列满时丢弃并补发 failed 事件。

use std::sync::Arc;

use fswtch::sys;
use s3_core::job::Trigger;

use crate::enqueue::{self, EnqueueRequest, Shared};
use crate::sysutil;

/// 绑定 RECORD_START / RECORD_STOP 事件；返回是否全部成功。
pub fn bind(shared: &Arc<Shared>) -> bool {
    let _ = shared;
    let mut ok = true;
    for (event_type, callback, label) in [
        (
            sys::switch_event_types_t::SWITCH_EVENT_RECORD_START,
            on_record_start as unsafe extern "C" fn(*mut sys::switch_event_t),
            "RECORD_START",
        ),
        (
            sys::switch_event_types_t::SWITCH_EVENT_RECORD_STOP,
            on_record_stop as unsafe extern "C" fn(*mut sys::switch_event_t),
            "RECORD_STOP",
        ),
    ] {
        // SAFETY: 回调是 'static extern "C" fn；FreeSWITCH 持有绑定直到 unbind。
        let status = unsafe {
            sys::switch_event_bind(
                c"mod_s3".as_ptr(),
                event_type,
                std::ptr::null(),
                Some(callback),
                std::ptr::null_mut(),
            )
        };
        if status == fswtch::SUCCESS {
            sysutil::log_info(&format!("mod_s3: bound SWITCH_EVENT_{label}"));
        } else {
            sysutil::log_error(&format!(
                "mod_s3: switch_event_bind({label}) failed"
            ));
            ok = false;
        }
    }
    ok
}

/// 解绑（模块卸载时调用；best-effort）。
pub fn unbind() {
    // SAFETY: 与 bind 使用同一回调指针；重复 unbind 无害。
    unsafe {
        sys::switch_event_unbind_callback(Some(on_record_start));
        sys::switch_event_unbind_callback(Some(on_record_stop));
    }
}

/// RECORD_START 回调入口：仅发 `mod_s3::record_start` 通知事件，
/// 供上层关联"录音开始→上传完成"全链路；不做任何上传动作。
unsafe extern "C" fn on_record_start(event: *mut sys::switch_event_t) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `event` 由 FreeSWITCH 在回调期间持有。
        unsafe { handle_record_start(event) };
    }));
    if result.is_err() {
        sysutil::log_error("mod_s3: RECORD_START callback panicked");
    }
}

/// SAFETY: `event` 在回调期间存活。
unsafe fn handle_record_start(event: *mut sys::switch_event_t) {
    // SAFETY: `event` 存活；RECORD_START 与 RECORD_STOP 使用同一组
    // Record-* 头（FS `send_record_*_event` 对称实现）。
    let (path, uuid) = match unsafe { sysutil::record_stop_path(event) } {
        Some(v) => v,
        None => (String::new(), None),
    };
    let body = format!(
        "{{\"path\":{},\"uuid\":{}}}",
        json_escape(&path),
        uuid.as_deref().map(json_escape).unwrap_or_else(|| "null".into())
    );
    sysutil::emit_event(uuid.as_deref(), "record_start", &body);
}

/// RECORD_STOP 回调入口。
unsafe extern "C" fn on_record_stop(event: *mut sys::switch_event_t) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `event` 由 FreeSWITCH 在回调期间持有。
        unsafe { handle_record_stop(event) };
    }));
    if result.is_err() {
        sysutil::log_error("mod_s3: RECORD_STOP callback panicked");
    }
}

/// 处理 RECORD_STOP：提取路径 → 校验入队。
/// SAFETY: `event` 在回调期间存活。
unsafe fn handle_record_stop(event: *mut sys::switch_event_t) {
    let Some(shared) = crate::shared_arc() else {
        return;
    };
    if !shared.config.auto_upload {
        return;
    }

    // SAFETY: `event` 存活。
    let Some((path, uuid)) = (unsafe { sysutil::record_stop_path(event) }) else {
        sysutil::log_info("mod_s3: RECORD_STOP without file path header, skipped");
        // SAFETY: `event` 存活；输出候选头命中情况供真机核对字段名。
        unsafe { sysutil::dump_event_headers(event) };
        return;
    };

    // 尝试 locate 会话补充 channel vars（通话可能已拆，失败则仅用事件头）。
    let mut vars = std::collections::BTreeMap::new();
    if let Some(u) = &uuid {
        vars.insert("uuid".into(), u.clone());
        // SAFETY: locate/unlock 配对。
        let session = sysutil::locate_session(u.as_bytes());
        if !session.is_null() {
            // SAFETY: session 持有读锁。
            let snap = unsafe { sysutil::channel_vars_snapshot(session) };
            // SAFETY: 释放读锁。
            unsafe { sysutil::rwunlock(session) };
            for (k, v) in snap {
                vars.entry(k).or_insert(v);
            }
        }
    }
    // 事件头里的常见字段也并入模板变量。
    for (var, header) in [
        ("caller_id_number", "Caller-Caller-ID-Number"),
        ("destination_number", "Caller-Destination-Number"),
        ("call_direction", "Call-Direction"),
        ("sip_call_id", "variable_sip_call_id"),
        ("record_completion_cause", "Record-Completion-Cause"),
    ] {
        // SAFETY: `event` 存活。
        if let Some(v) = unsafe { sysutil::event_header(event, header) } {
            vars.entry(var.to_owned()).or_insert(v);
        }
    }

    match enqueue::enqueue(
        shared.as_ref(),
        EnqueueRequest {
            path,
            bucket: None,
            key: None,
            target_format: None,
            delete_override: None,
            metadata: None,
            vars,
            trigger: Trigger::RecordStop {
                uuid: uuid.clone().unwrap_or_default(),
            },
            uuid,
        },
    ) {
        Ok(id) => sysutil::log_info(&format!("mod_s3: RECORD_STOP enqueued job {id}")),
        Err(e) => {
            sysutil::log_error(&format!("mod_s3: RECORD_STOP enqueue failed: {e}"));
            // 入队失败也发 failed 事件，便于 Java 侧感知。
            let body = format!(
                "{{\"status\":\"failed\",\"error\":{},\"trigger\":\"record_stop\"}}",
                json_escape(&e)
            );
            sysutil::emit_event(None, "upload", &body);
        }
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
