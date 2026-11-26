//! `mod_s3.conf.xml` 解析：param name/value → `s3_core::config::ModuleConfig`。

use std::path::PathBuf;
use std::time::Duration;

use s3_core::config::{ModuleConfig, TranscodeMode};

use crate::sysutil;

/// 从 FreeSWITCH conf 目录加载 `mod_s3.conf`；文件缺失时返回默认配置
/// 并打警告日志（便于首次部署）。
pub fn load() -> ModuleConfig {
    let mut cfg = ModuleConfig::default();
    let Some(xml) = fswtch::XmlConfig::open(sysutil::CONFIG_NAME) else {
        sysutil::log_info(&format!(
            "{}: {} not found, using defaults",
            sysutil::MODULE_NAME,
            sysutil::CONFIG_NAME
        ));
        if cfg.limits.allowed_roots.is_empty() {
            cfg.limits.allowed_roots.push(cfg.recordings_root.clone());
        }
        cfg.apply_env_credentials();
        return cfg;
    };

    if let Some(params) = xml.settings().and_then(|node| node.child("settings")) {
        apply_params(params, &mut cfg);
    }
    // allowed-roots 未配置时默认收敛到 recordings-root，
    // 避免"忘了配白名单导致全部上传被拒"的误配。
    if cfg.limits.allowed_roots.is_empty() {
        cfg.limits.allowed_roots.push(cfg.recordings_root.clone());
    }
    cfg.apply_env_credentials();
    cfg
}

fn apply_params(node: fswtch::XmlNode, cfg: &mut ModuleConfig) {
    let mut param = node.child("param");
    while let Some(p) = param {
        let name = p.attr("name").unwrap_or_default();
        let value = p.attr("value").unwrap_or_default();
        apply_param(&name, &value, cfg);
        param = p.next();
    }
}

fn apply_param(name: &str, value: &str, cfg: &mut ModuleConfig) {
    match name {
        // S3 端点
        "endpoint" => cfg.s3.endpoint = nonempty(value),
        "region" => cfg.s3.region = value.to_owned(),
        "bucket" => cfg.s3.default_bucket = nonempty(value),
        "force-path-style" => cfg.s3.force_path_style = truthy(value),
        "access-key" => cfg.s3.access_key = nonempty(value),
        "secret-key" => cfg.s3.secret_key = nonempty(value),
        "session-token" => cfg.s3.session_token = nonempty(value),
        "multipart-threshold" => {
            // MiB 单位；0 = 始终单次 put_object。
            if let Ok(mb) = value.trim().parse::<u64>() {
                cfg.s3.multipart_threshold = mb.saturating_mul(1024 * 1024);
            }
        }
        // 自动上传
        "auto-upload" => cfg.auto_upload = truthy(value),
        "recordings-root" => cfg.recordings_root = PathBuf::from(value),
        "key-prefix" => cfg.key_prefix = value.to_owned(),
        "key-template" => cfg.key_template = value.to_owned(),
        // 转码
        "transcode" => cfg.transcode.mode = TranscodeMode::parse(value),
        "target-format" => {
            cfg.transcode.target_format = match value.trim().to_ascii_lowercase().as_str() {
                "" | "orig" | "none" | "copy" => None,
                other => Some(other.to_owned()),
            };
        }
        "ffmpeg-bin" => cfg.transcode.ffmpeg_bin = value.to_owned(),
        "ffmpeg-args" => {
            cfg.transcode.ffmpeg_args = value.split_whitespace().map(str::to_owned).collect();
        }
        "transcode-timeout" => {
            if let Ok(secs) = value.trim().parse::<u64>() {
                cfg.transcode.timeout = Duration::from_secs(secs.max(1));
            }
        }
        "temp-dir" => cfg.transcode.temp_dir = PathBuf::from(value),
        // 删除
        "delete-after-upload" => cfg.delete.after_upload = truthy(value),
        "delete-on-failure" => cfg.delete.on_failure = truthy(value),
        // 安全边界
        "allowed-roots" => {
            cfg.limits.allowed_roots = split_list(value)
                .into_iter()
                .map(PathBuf::from)
                .collect();
        }
        "allowed-ext" => {
            cfg.limits.allowed_ext = split_list(value)
                .into_iter()
                .map(|e| {
                    let e = e.to_ascii_lowercase();
                    if e.starts_with('.') {
                        e
                    } else {
                        format!(".{e}")
                    }
                })
                .collect();
        }
        "max-bytes" => {
            if let Ok(n) = value.trim().parse::<u64>() {
                cfg.limits.max_bytes = n;
            }
        }
        "min-age-ms" => {
            if let Ok(ms) = value.trim().parse::<u64>() {
                cfg.limits.min_age = Duration::from_millis(ms);
            }
        }
        // 运行参数
        "workers" => {
            if let Ok(n) = value.trim().parse::<usize>() {
                cfg.workers = n.clamp(1, 16);
            }
        }
        "queue-size" => {
            if let Ok(n) = value.trim().parse::<usize>() {
                cfg.queue_size = n.clamp(1, 4096);
            }
        }
        "job-retries" => {
            if let Ok(n) = value.trim().parse::<u32>() {
                cfg.job_retries = n.min(10);
            }
        }
        _ => {}
    }
}

fn nonempty(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_owned())
    }
}

fn truthy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on")
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(|c: char| c == ',' || c == ';' || c == ' ')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_params() {
        let mut cfg = ModuleConfig::default();
        apply_param("endpoint", "http://rustfs:9000", &mut cfg);
        apply_param("bucket", "my-bucket", &mut cfg);
        apply_param("transcode", "auto", &mut cfg);
        apply_param("target-format", "mp3", &mut cfg);
        apply_param("delete-after-upload", "true", &mut cfg);
        apply_param("max-bytes", "1048576", &mut cfg);
        apply_param("allowed-ext", ".wav, .mp3 ; .ogg", &mut cfg);
        apply_param("ffmpeg-args", "-ac 1 -b:a 32k", &mut cfg);
        apply_param("multipart-threshold", "512", &mut cfg);

        assert_eq!(cfg.s3.endpoint.as_deref(), Some("http://rustfs:9000"));
        assert_eq!(cfg.s3.default_bucket.as_deref(), Some("my-bucket"));
        assert_eq!(cfg.transcode.mode, TranscodeMode::Auto);
        assert_eq!(cfg.transcode.target_format.as_deref(), Some("mp3"));
        assert!(cfg.delete.after_upload);
        assert_eq!(cfg.limits.max_bytes, 1048576);
        assert_eq!(cfg.limits.allowed_ext, vec![".wav", ".mp3", ".ogg"]);
        assert_eq!(cfg.transcode.ffmpeg_args, vec!["-ac", "1", "-b:a", "32k"]);
        assert_eq!(cfg.s3.multipart_threshold, 512 * 1024 * 1024);
    }

    #[test]
    fn defaults_when_empty() {
        let cfg = ModuleConfig::default();
        assert_eq!(cfg.transcode.mode, TranscodeMode::None);
        assert!(!cfg.delete.after_upload);
        assert!(cfg.auto_upload);
    }
}
