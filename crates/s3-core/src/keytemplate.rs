//! object key 模板渲染：`{var}` 占位符 + 时间变量 + 安全字符校验。
//!
//! 变量来源优先级：调用方传入的 `vars`（channel variables / 事件头）> 时间变量。
//! 未知变量渲染为空串；渲染结果经 `sanitize_key` 收敛到 S3 安全字符集。

use std::collections::BTreeMap;
use std::time::SystemTime;

use time::OffsetDateTime;

/// 渲染 key 模板。`file_stem`/`file_ext`/`dst_ext` 等派生变量由调用方
/// 在 `vars` 中预置（本函数不感知文件系统）。
pub fn render(
    template: &str,
    vars: &BTreeMap<String, String>,
    default_template: &str,
) -> String {
    let template = if template.trim().is_empty() {
        default_template
    } else {
        template
    };

    let mut time_vars = BTreeMap::new();
    collect_time_vars(&mut time_vars, SystemTime::now());

    let mut out = String::with_capacity(template.len() + 32);
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = template[i + 1..].find('}') {
                let name = &template[i + 1..i + 1 + end];
                if let Some(value) = vars.get(name).or_else(|| time_vars.get(name)) {
                    out.push_str(value);
                }
                i += end + 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    sanitize_key(&out)
}

/// 默认 key 模板：`{prefix}/{yyyy}/{MM}/{dd}/{file}`。
pub const DEFAULT_TEMPLATE: &str = "{prefix}/{yyyy}/{MM}/{dd}/{file}";

/// 收集时间变量（UTC）。
pub fn collect_time_vars(vars: &mut BTreeMap<String, String>, _now: SystemTime) {
    let now = OffsetDateTime::now_utc();
    vars.insert("yyyy".into(), format!("{:04}", now.year()));
    vars.insert("MM".into(), format!("{:02}", now.month() as u8));
    vars.insert("dd".into(), format!("{:02}", now.day()));
    vars.insert("HH".into(), format!("{:02}", now.hour()));
    vars.insert("mm".into(), format!("{:02}", now.minute()));
    vars.insert("ss".into(), format!("{:02}", now.second()));
}

/// S3 key 字符收敛：只保留 `[A-Za-z0-9._/-]`，其余替换为 `-`；
/// 折叠连续 `/`、去首尾 `/` 与 `.` 段。
pub fn sanitize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut last_slash = true; // 折叠开头的斜杠
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch);
            last_slash = false;
        } else if ch == '/' {
            if !last_slash {
                out.push('/');
                last_slash = true;
            }
        } else {
            out.push('-');
            last_slash = false;
        }
    }
    while out.ends_with('/') || out.ends_with('.') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_vars_and_time() {
        let mut vars = BTreeMap::new();
        vars.insert("uuid".into(), "abc-123".to_string());
        vars.insert("file".into(), "rec.wav".to_string());
        vars.insert("prefix".into(), "callcenter/recordings".to_string());
        let key = render(DEFAULT_TEMPLATE, &vars, DEFAULT_TEMPLATE);
        assert!(key.starts_with("callcenter/recordings/"));
        assert!(key.ends_with("/rec.wav"));
    }

    #[test]
    fn unknown_vars_render_empty() {
        let key = render("{prefix}/{uuid}/{file}", &BTreeMap::new(), "{file}");
        // prefix/uuid 为空 → 折叠斜杠
        assert_eq!(key, "");
    }

    #[test]
    fn sanitizes_unsafe_chars() {
        assert_eq!(sanitize_key("a b/c:d*.wav"), "a-b/c-d-.wav");
        assert_eq!(sanitize_key("//a//b//"), "a/b");
        assert_eq!(sanitize_key("../../etc/passwd"), "../../etc/passwd");
    }

    #[test]
    fn custom_template_with_dst_ext() {
        let mut vars = BTreeMap::new();
        vars.insert("file_stem".into(), "call-1".to_string());
        vars.insert("dst_ext".into(), "mp3".to_string());
        vars.insert("prefix".into(), "rec".to_string());
        let key = render("{prefix}/{file_stem}.{dst_ext}", &vars, DEFAULT_TEMPLATE);
        assert_eq!(key, "rec/call-1.mp3");
    }
}
