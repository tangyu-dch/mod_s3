//! mod_s3 胶水层共用的 FreeSWITCH ABI 辅助函数。
//! 与 mod_audio_relay 的 sysutil 同源裁剪：只保留本模块需要的部分。

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::ptr;

use fswtch::sys;

pub const MODULE_NAME: &str = "mod_s3";

/// FreeSWITCH conf 目录下的模块配置文件名（`XmlConfig::open` 按名查找）。
pub const CONFIG_NAME: &str = "mod_s3.conf";

/// key 模板可用的 channel variables（uuid_s3 / RECORD_STOP 场景快照）。
const CHANNEL_VARS: [&str; 9] = [
    "uuid",
    "caller_id_number",
    "destination_number",
    "call_direction",
    "sip_call_id",
    "record_file",
    "call_uuid",
    "uuid_direction",
    "Hunt-Destination-Number",
];

pub fn cstring(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

pub fn bytes_cstring(bytes: &[u8]) -> CString {
    CString::new(bytes).unwrap_or_default()
}

/// 对存活 session 指针调用 `switch_core_session_get_channel`。
// SAFETY: 调用方保证 session 指针在回调或 locate 期间由 FreeSWITCH 保持存活。
pub unsafe fn channel_of(session: *mut sys::switch_core_session_t) -> *mut sys::switch_channel_t {
    // SAFETY: 调用方保证 `session` 在调用期间存活。
    unsafe { sys::switch_core_session_get_channel(session) }
}

/// `switch_core_session_get_uuid`，复制为自有字符串。
pub unsafe fn session_uuid(session: *mut sys::switch_core_session_t) -> Option<String> {
    // SAFETY: 调用方保证 `session` 在调用期间存活。
    let raw = unsafe { sys::switch_core_session_get_uuid(session) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: FreeSWITCH 返回存活的 NUL 结尾字符串。
    let value = unsafe { CStr::from_ptr(raw) }
        .to_str()
        .ok()
        .map(str::to_owned);
    value
}

/// `switch_core_session_perform_locate`，调用方必须用
/// `switch_core_session_rwunlock` 释放。
pub fn locate_session(uuid: &[u8]) -> *mut sys::switch_core_session_t {
    let uuid = bytes_cstring(uuid);
    // SAFETY: `uuid` 是存活的 C 字符串；非 NULL 结果带有读锁，
    // 调用方须用 switch_core_session_rwunlock 释放。
    unsafe {
        sys::switch_core_session_perform_locate(
            uuid.as_ptr(),
            c"mod_s3".as_ptr(),
            c"locate_session".as_ptr(),
            line!() as i32,
        )
    }
}

// SAFETY: `session` 必须来自 `locate_session`（或存活的回调），
// 且仍持有其读锁。
pub unsafe fn rwunlock(session: *mut sys::switch_core_session_t) {
    // SAFETY: 调用方保证 `session` 持有其读锁。
    unsafe { sys::switch_core_session_rwunlock(session) };
}

/// 把一个通道变量读为自有字符串（`switch_channel_get_variable_strdup`
/// + libc free，与 mod_audio_relay 相同的处理）。
pub unsafe fn channel_variable(
    channel: *mut sys::switch_channel_t,
    name: &str,
) -> Option<String> {
    let name = cstring(name);
    // SAFETY: 调用方保证 `channel` 在调用期间存活。
    let raw = unsafe { sys::switch_channel_get_variable_strdup(channel, name.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: FreeSWITCH 返回调用方持有的 NUL 结尾 `strdup` 分配。
    let value = unsafe { CStr::from_ptr(raw) }
        .to_str()
        .ok()
        .map(str::to_owned);
    // SAFETY: `raw` 是上面调用返回的 `strdup` 结果，复制后恰好释放一次。
    unsafe {
        libc_free(raw as *mut std::ffi::c_void);
    }
    value
}

// SAFETY: 与 mod_audio_relay sysutil 一致——FreeSWITCH strdup 结果
// 由 libc free 释放。
unsafe fn libc_free(ptr: *mut std::ffi::c_void) {
    // SAFETY: 调用方保证指针来自 FreeSWITCH strdup。
    unsafe { libc::free(ptr) };
}

/// 快照 CHANNEL_VARS 为 BTreeMap；另注入 `uuid`（会话 UUID）。
pub unsafe fn channel_vars_snapshot(
    session: *mut sys::switch_core_session_t,
) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    // SAFETY: `session` 在调用期间存活（locate 读锁或 API 回调）。
    let channel = unsafe { channel_of(session) };
    if channel.is_null() {
        return vars;
    }
    // SAFETY: `channel` 属于存活会话。
    if let Some(uuid) = unsafe { session_uuid(session) } {
        vars.insert("uuid".into(), uuid);
    }
    for name in CHANNEL_VARS {
        // SAFETY: `channel` 在快照期间存活。
        if let Some(value) = unsafe { channel_variable(channel, name) } {
            vars.entry(name.to_owned()).or_insert(value);
        }
    }
    vars
}

/// 模块级日志行，等价于 `switch_log_printf(SWITCH_CHANNEL_LOG, ...)`。
pub fn log(level: sys::switch_log_level_t, message: &str) {
    let text = cstring(message);
    // SAFETY: 所有指针在调用期间都是有效的 C 字符串。
    unsafe {
        sys::switch_log_printf(
            sys::switch_text_channel_t_SWITCH_CHANNEL_ID_LOG,
            c"mod_s3".as_ptr(),
            c"log".as_ptr(),
            line!() as i32,
            ptr::null(),
            level,
            c"%s".as_ptr(),
            text.as_ptr(),
        );
    }
}

pub fn log_info(message: &str) {
    log(sys::switch_log_level_t_SWITCH_LOG_INFO, message);
}

pub fn log_error(message: &str) {
    log(sys::switch_log_level_t_SWITCH_LOG_ERROR, message);
}

/// 以 JSON body 触发 `mod_s3::<suffix>` CUSTOM 事件。
/// `uuid` 为 `Some` 时挂接 channel event data；`None` 时发全局事件。
pub fn emit_event(uuid: Option<&str>, suffix: &str, body_json: &str) {
    let subclass = format!("mod_s3::{suffix}");
    let subclass_c = cstring(&subclass);
    let mut event: *mut sys::switch_event_t = ptr::null_mut();

    // SAFETY: 输出指针有效；所有字符串是存活的 C 字符串。
    let status = unsafe {
        sys::switch_event_create_subclass_detailed(
            c"mod_s3".as_ptr(),
            c"emit_event".as_ptr(),
            line!() as i32,
            &mut event,
            sys::switch_event_types_t::SWITCH_EVENT_CUSTOM,
            subclass_c.as_ptr(),
        )
    };
    if status != fswtch::SUCCESS || event.is_null() {
        if !event.is_null() {
            // SAFETY: 事件已创建但无法投递；销毁。
            unsafe { sys::switch_event_destroy(&mut event) };
        }
        return;
    }

    let mut session: *mut sys::switch_core_session_t = ptr::null_mut();
    if let Some(uuid) = uuid {
        session = locate_session(uuid.as_bytes());
    }

    // SAFETY: `event` 存活；session 若非空则持有 locate 读锁。
    unsafe {
        if !session.is_null() {
            let channel = channel_of(session);
            if !channel.is_null() {
                sys::switch_channel_event_set_data(channel, event);
            }
        }
        let body = cstring(body_json);
        sys::switch_event_add_body(event, c"%s".as_ptr(), body.as_ptr());
        sys::switch_event_fire_detailed(
            c"mod_s3".as_ptr(),
            c"emit_event".as_ptr(),
            line!() as i32,
            &mut event,
            ptr::null_mut(),
        );
        if !session.is_null() {
            rwunlock(session);
        }
    }
}

/// 读取事件头字符串（`switch_event_get_header_ptr` → `value`）。
pub unsafe fn event_header(event: *mut sys::switch_event_t, name: &str) -> Option<String> {
    let name_c = cstring(name);
    // SAFETY: `event` 在回调期间存活；返回的 header 指针借用事件。
    let hdr = unsafe { sys::switch_event_get_header_ptr(event, name_c.as_ptr()) };
    if hdr.is_null() {
        return None;
    }
    // SAFETY: `hdr` 是事件持有的头结构，`value` 为 NUL 结尾字符串。
    let value = unsafe { (*hdr).value };
    if value.is_null() {
        return None;
    }
    // SAFETY: `value` 由事件持有且在本函数返回前有效。
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// RECORD_STOP 诊断：path 提取失败时把已知候选头的命中情况打进日志，
/// 便于在新 FS 版本上确认真实字段名（fswtch 未暴露 headers 链表迭代器，
/// 只能按名探测）。
pub unsafe fn dump_event_headers(event: *mut sys::switch_event_t) {
    // SAFETY: `event` 在回调期间存活；event_header 只读借用。
    unsafe {
        const PROBE: [&str; 12] = [
            "Record-File-Path",
            "Record-Path",
            "RecordFile",
            "record-path",
            "File-Path",
            "filename",
            "Record-File",
            "Path",
            "Record-Completion-Cause",
            "Unique-ID",
            "Caller-Caller-ID-Number",
            "Caller-Destination-Number",
        ];
        let mut found = Vec::new();
        for name in PROBE {
            // SAFETY: `event` 存活。
            if let Some(v) = event_header(event, name) {
                found.push(format!("{name}={v}"));
            }
        }
        log_info(&format!(
            "mod_s3: RECORD_STOP headers present: {}",
            if found.is_empty() {
                "(none of the probed names)".to_string()
            } else {
                found.join(" ")
            }
        ));
    }
}

/// 从 RECORD_STOP 事件提取录音文件路径：依次尝试已知头名。
/// 返回 (path, uuid)。找不到路径返回 None。
///
/// FreeSWITCH `send_record_stop_event`（src/switch_ivr_async.c）固定写入
/// `Record-File-Path`；其余候选仅为历史/自定义事件的兜底。
pub unsafe fn record_stop_path(event: *mut sys::switch_event_t) -> Option<(String, Option<String>)> {
    const PATH_HEADERS: [&str; 8] = [
        "Record-File-Path",
        "Record-Path",
        "RecordFile",
        "record-path",
        "File-Path",
        "filename",
        "Record-File",
        "Path",
    ];
    let mut path = None;
    for name in PATH_HEADERS {
        // SAFETY: `event` 在回调期间存活。
        if let Some(v) = unsafe { event_header(event, name) } {
            if !v.trim().is_empty() {
                path = Some(v);
                break;
            }
        }
    }
    // SAFETY: 同上。
    let uuid = unsafe { event_header(event, "Unique-ID") };
    path.map(|p| (p, uuid))
}

/// switch_utils.c 中的 `ESCAPE_META`。
const ESCAPE_META: u8 = b'\\';

/// switch_utils.c 中的 `unescape_char()`。
fn unescape_char(escaped: u8) -> u8 {
    match escaped {
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b's' => b' ',
        other => other,
    }
}

/// switch_utils.c 中的 `cleanup_separated_string()`：去除引号字符、
/// 解析反斜杠转义并裁剪尾部空白。
fn cleanup_separated_string(input: &[u8], delim: u8) -> Vec<u8> {
    let mut index = 0;
    while index < input.len() && input[index] == b' ' {
        index += 1;
    }

    let mut output: Vec<u8> = Vec::with_capacity(input.len() - index);
    let mut inside_quotes = false;
    // `end` 对应 C 中的 `end` 指针：输出会截断到该长度。
    let mut end = 0usize;

    while index < input.len() {
        let byte = input[index];
        let mut handled = false;

        if byte == ESCAPE_META {
            let escaped = if index + 1 < input.len() {
                input[index + 1]
            } else {
                0
            };
            let unescaped = unescape_char(escaped);
            if escaped == b'\''
                || escaped == b'"'
                || (delim != 0 && escaped == delim)
                || escaped == ESCAPE_META
                || unescaped != escaped
            {
                output.push(unescaped);
                end = output.len();
                index += 2;
                handled = true;
            }
        }

        if !handled {
            if byte == b'\'' && (inside_quotes || input[index + 1..].contains(&b'\'')) {
                inside_quotes = !inside_quotes;
                if inside_quotes {
                    end = output.len();
                }
            } else {
                output.push(byte);
                if byte != b' ' || inside_quotes {
                    end = output.len();
                }
            }
            index += 1;
        }
    }

    output.truncate(end);
    output
}

/// 空白分隔 `switch_separate_string()` 等价实现：支持 `^^<delim>` 前缀、
/// 引号分组（FreeSWITCH 约定单引号）、反斜杠转义；最后一个槽位保留
/// 命令未拆分的剩余部分，因此 `metadata` 可以包含空格。
pub fn separate_args(cmd: &[u8], max: usize) -> Vec<Vec<u8>> {
    let mut input = cmd;
    let mut delim = b' ';
    if input.len() >= 4 && input[0] == b'^' && input[1] == b'^' {
        delim = input[2];
        input = &input[3..];
    }

    let tokens = if delim == b' ' {
        separate_blank_delim(input, max)
    } else {
        separate_char_delim(input, delim, max)
    };
    tokens
        .iter()
        .map(|token| cleanup_separated_string(token, if delim == b' ' { 0 } else { delim }))
        .collect()
}

fn separate_blank_delim(input: &[u8], max: usize) -> Vec<&[u8]> {
    #[derive(PartialEq)]
    enum State {
        Start,
        SkipInitialSpace,
        FindDelim,
        SkipEndingSpace,
    }

    let mut tokens = Vec::new();
    let mut state = State::Start;
    let mut inside_quotes = false;
    let mut token_start = 0usize;
    let mut index = 0usize;
    let mut open_token = false;

    while index < input.len() && tokens.len() < max {
        match state {
            State::Start => {
                token_start = index;
                open_token = true;
                state = State::SkipInitialSpace;
            }
            State::SkipInitialSpace => {
                if input[index] == b' ' {
                    index += 1;
                } else {
                    state = State::FindDelim;
                }
            }
            State::FindDelim => {
                if input[index] == ESCAPE_META {
                    index += 1;
                } else if input[index] == b'\'' {
                    inside_quotes = !inside_quotes;
                } else if input[index] == b' ' && !inside_quotes {
                    tokens.push(&input[token_start..index]);
                    open_token = false;
                    state = State::SkipEndingSpace;
                }
                index += 1;
            }
            State::SkipEndingSpace => {
                if input[index] == b' ' {
                    index += 1;
                } else {
                    state = State::Start;
                }
            }
        }
    }

    if open_token {
        tokens.push(&input[token_start..]);
    }
    tokens
}

fn separate_char_delim(input: &[u8], delim: u8, max: usize) -> Vec<&[u8]> {
    #[derive(PartialEq)]
    enum State {
        Start,
        FindDelim,
    }

    let mut tokens = Vec::new();
    let mut state = State::Start;
    let mut inside_quotes = false;
    let mut token_start = 0usize;
    let mut index = 0usize;
    let mut open_token = false;

    while index < input.len() && tokens.len() < max {
        match state {
            State::Start => {
                token_start = index;
                open_token = true;
                state = State::FindDelim;
            }
            State::FindDelim => {
                if input[index] == ESCAPE_META {
                    index += 1;
                } else if input[index] == b'\''
                    && (inside_quotes || input[index + 1..].contains(&b'\''))
                {
                    inside_quotes = !inside_quotes;
                } else if input[index] == delim && !inside_quotes {
                    tokens.push(&input[token_start..index]);
                    open_token = false;
                    state = State::Start;
                }
                index += 1;
            }
        }
    }

    if open_token {
        tokens.push(&input[token_start..]);
    }
    tokens
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_args_splits_on_blanks() {
        let args = separate_args(b"s3_upload /rec/a.wav bucket key mp3", 8);
        let texts: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
        assert_eq!(
            texts,
            vec![
                &b"s3_upload"[..],
                b"/rec/a.wav",
                b"bucket",
                b"key",
                b"mp3"
            ]
        );
    }

    #[test]
    fn separate_args_keeps_quoted_metadata_intact() {
        // metadata 以引号包裹时空格不拆分，整体保留在最后一个槽位。
        let args = separate_args(b"s3_upload /r/a.wav b k '{\"a\": 1}'", 6);
        assert_eq!(args.len(), 5);
        assert_eq!(args[4], b"{\"a\": 1}");
    }

    #[test]
    fn separate_args_supports_custom_delimiter() {
        // ^^ 分隔符覆盖：^^,<delim> 开头时按 <delim> 拆分。
        let args = separate_args(b"^^|one|two|three", 4);
        assert_eq!(args[0], b"one");
        assert_eq!(args[1], b"two");
        assert_eq!(args[2], b"three");
    }

    #[test]
    fn separate_args_groups_quoted_sections() {
        let args = separate_args(b"cmd upload '/rec/my file.wav' mono", 5);
        assert_eq!(args[2], b"/rec/my file.wav");
    }

    #[test]
    fn separate_args_unescapes_backslash_sequences() {
        let args = separate_args(b"cmd a\\tb c", 4);
        assert_eq!(args[1], b"a\tb");
    }

    #[test]
    fn cstring_round_trip() {
        let c = cstring("mod_s3");
        assert_eq!(c.to_str().unwrap(), "mod_s3");
    }
}
