//! S3 上传：窄 trait `ObjectStore` + aws-sdk-s3 实现。
//!
//! trait 只暴露 `upload` 单动词，便于将来平替为手写 SigV4 PUT
//! 而不影响胶水层与任务管线。

use std::fs::File;
use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use futures_util::future::join_all;

use crate::config::S3Settings;

/// 上传错误。
#[derive(Debug)]
pub enum UploadError {
    /// aws-sdk 服务端/网络错误。
    S3(String),
    /// 文件无法作为流打开。
    Io(std::io::Error),
    /// HEAD 校验不一致（size 或 sha256 metadata 不匹配）。
    VerifyMismatch { expected_size: u64, actual_size: i64 },
    /// 缺少凭证或 bucket。
    Config(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::S3(e) => write!(f, "s3 error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::VerifyMismatch {
                expected_size,
                actual_size,
            } => write!(
                f,
                "head verify mismatch: expected {expected_size}, got {actual_size}"
            ),
            Self::Config(e) => write!(f, "config error: {e}"),
        }
    }
}

impl std::error::Error for UploadError {}

impl From<std::io::Error> for UploadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// 上传进度回调：`(已上传字节, 总字节)`。
/// multipart 每批分片完成后回调；单次 PUT 仅回调 0 与总量两次。
pub type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// 上传请求：worker 已完成转码/sha256/size 计算后的最小输入。
#[derive(Clone)]
pub struct UploadRequest {
    /// 实际上传的文件（源文件或 temp 转码产物）。
    pub file: PathBuf,
    pub bucket: String,
    pub key: String,
    pub content_type: String,
    /// 预计算的 sha256 hex，写入 object Metadata 并参与 HEAD 校验。
    pub sha256: String,
    pub size: u64,
    /// 可选进度回调；`None` 不产生进度事件。
    pub progress: Option<ProgressCallback>,
}

/// 上传回执。
#[derive(Clone, Debug, Default)]
pub struct UploadReceipt {
    pub etag: Option<String>,
    pub size: u64,
}

/// presign 的 HTTP 方法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresignMethod {
    /// 下载链接（最常用：前端/回放直接 GET）。
    Get,
    /// 上传链接（PUT 预签名，客户端直传 S3）。
    Put,
}

/// presign 请求。
#[derive(Clone, Debug)]
pub struct PresignRequest {
    pub bucket: String,
    pub key: String,
    pub method: PresignMethod,
    /// 链接有效期（秒）；调用方负责钳制上限（S3 最大 7 天）。
    pub expires_secs: u64,
}

/// 对象存储抽象：窄动词集、对象安全、Future boxed。
pub trait ObjectStore: Send + Sync {
    fn upload<'a>(
        &'a self,
        req: UploadRequest,
    ) -> Pin<Box<dyn Future<Output = Result<UploadReceipt, UploadError>> + Send + 'a>>;

    /// 生成预签名 URL（纯本地 SigV4 签名，无网络 IO）。
    fn presign<'a>(
        &'a self,
        req: PresignRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, UploadError>> + Send + 'a>>;
}

/// multipart 分片大小：32 MiB（S3 下限 5 MiB，上限 5 GiB/片，
/// 10000 片上限 → 单对象理论上限 ~312 TiB，远超录音场景）。
/// 与 `PART_CONCURRENCY` 一起决定峰值内存：32 MiB × 4 ≈ 128 MiB。
const PART_SIZE: u64 = 32 * 1024 * 1024;

/// 单批并行上传的分片数。
const PART_CONCURRENCY: usize = 4;

/// aws-sdk-s3 实现。
pub struct AwsObjectStore {
    client: aws_sdk_s3::Client,
    /// 超过该字节数走 multipart；0 表示始终单次 put_object。
    multipart_threshold: u64,
}

impl AwsObjectStore {
    /// 构造 client；`cfg.credentials_present()` 为 false 时仍可构建
    /// （匿名/自定义 endpoint 场景），首个请求会以 SDK 错误暴露。
    pub fn new(cfg: &S3Settings) -> Self {
        let mut builder = aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new(cfg.region.clone()))
            .force_path_style(cfg.force_path_style);

        if let Some(endpoint) = &cfg.endpoint {
            builder = builder.endpoint_url(endpoint.clone());
        }
        if cfg.credentials_present() {
            let creds = aws_credential_types::Credentials::new(
                cfg.access_key.clone().unwrap_or_default(),
                cfg.secret_key.clone().unwrap_or_default(),
                cfg.session_token.clone(),
                None,
                "mod_s3",
            );
            builder = builder.credentials_provider(creds);
        }

        Self {
            client: aws_sdk_s3::Client::from_conf(builder.build()),
            multipart_threshold: cfg.multipart_threshold,
        }
    }

    /// put_object（小文件）或 multipart（大文件）+ head_object 校验。
    pub async fn upload_inner(
        &self,
        req: &UploadRequest,
    ) -> Result<UploadReceipt, UploadError> {
        let etag = if self.multipart_threshold > 0 && req.size > self.multipart_threshold {
            self.upload_multipart(req).await?
        } else {
            self.upload_single(req).await?
        };
        self.verify_head(req).await?;
        Ok(UploadReceipt {
            etag,
            size: req.size,
        })
    }

    /// 单次 put_object。
    async fn upload_single(&self, req: &UploadRequest) -> Result<Option<String>, UploadError> {
        let body = ByteStream::from_path(&req.file)
            .await
            .map_err(|e| UploadError::Io(std::io::Error::other(e.to_string())))?;

        if let Some(cb) = &req.progress {
            cb(0, req.size);
        }
        let put = self
            .client
            .put_object()
            .bucket(&req.bucket)
            .key(&req.key)
            .body(body)
            .content_type(&req.content_type)
            .metadata("sha256", &req.sha256)
            .send()
            .await
            .map_err(|e| UploadError::S3(e.to_string()))?;
        if let Some(cb) = &req.progress {
            cb(req.size, req.size);
        }
        Ok(put.e_tag().map(str::to_owned))
    }

    /// HEAD 校验：size 一致 + sha256 metadata 一致（与 sidecar 语义对齐）。
    async fn verify_head(&self, req: &UploadRequest) -> Result<(), UploadError> {
        let head = self
            .client
            .head_object()
            .bucket(&req.bucket)
            .key(&req.key)
            .send()
            .await
            .map_err(|e| UploadError::S3(format!("head_object: {e}")))?;

        let actual_size = head.content_length().unwrap_or(-1);
        if actual_size != req.size as i64 {
            return Err(UploadError::VerifyMismatch {
                expected_size: req.size,
                actual_size,
            });
        }
        if let Some(md) = head.metadata() {
            if md.get("sha256").map(String::as_str) != Some(req.sha256.as_str()) {
                return Err(UploadError::S3(
                    "head_object sha256 metadata mismatch".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// multipart 上传：create → 顺序 upload_part → complete；
    /// 任一步失败 best-effort abort，避免残留分片计费。
    async fn upload_multipart(&self, req: &UploadRequest) -> Result<Option<String>, UploadError> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&req.bucket)
            .key(&req.key)
            .content_type(&req.content_type)
            .metadata("sha256", &req.sha256)
            .send()
            .await
            .map_err(|e| UploadError::S3(format!("create_multipart_upload: {e}")))?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| UploadError::S3("create_multipart_upload: missing upload_id".into()))?
            .to_string();

        match self.multipart_parts(req, &upload_id).await {
            Ok(etag) => Ok(etag),
            Err(e) => {
                // best-effort abort：失败不掩盖原始错误。
                let _ = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&req.bucket)
                    .key(&req.key)
                    .upload_id(&upload_id)
                    .send()
                    .await;
                Err(e)
            }
        }
    }

    /// 分片上传并 complete；返回 complete 响应的 etag。
    ///
    /// 按 `PART_CONCURRENCY` 分批：批内顺序读盘（32 MiB/片），
    /// 批内 `join_all` 并行 `upload_part`，批间串行。峰值内存约
    /// `PART_SIZE × PART_CONCURRENCY`。complete 前 parts 按
    /// part_number 升序（批内顺序保证）。
    async fn multipart_parts(
        &self,
        req: &UploadRequest,
        upload_id: &str,
    ) -> Result<Option<String>, UploadError> {
        let num_parts = req.size.div_ceil(PART_SIZE).max(1);
        let mut parts: Vec<CompletedPart> = Vec::with_capacity(num_parts as usize);
        let mut file = File::open(&req.file)?;
        let mut uploaded: u64 = 0;

        let mut first = 1u64;
        while first <= num_parts {
            let batch_end = (first + PART_CONCURRENCY as u64 - 1).min(num_parts);

            // 批内顺序预读：offset 定位 + read_exact 补齐短读。
            let mut batch: Vec<(u64, Vec<u8>)> = Vec::with_capacity((batch_end - first + 1) as usize);
            for part_number in first..=batch_end {
                let offset = (part_number - 1) * PART_SIZE;
                let want = PART_SIZE.min(req.size - offset) as usize;
                file.seek(SeekFrom::Start(offset))?;
                let mut buf = vec![0u8; want];
                file.read_exact(&mut buf)?;
                batch.push((part_number, buf));
            }

            // 批内并行 upload_part；join_all 保序返回。
            let futs: Vec<_> = batch
                .iter()
                .map(|(part_number, data)| {
                    self.client
                        .upload_part()
                        .bucket(&req.bucket)
                        .key(&req.key)
                        .upload_id(upload_id)
                        .part_number(*part_number as i32)
                        .body(ByteStream::from(data.clone()))
                        .send()
                })
                .collect();
            let results = join_all(futs).await;

            for ((part_number, data), result) in batch.iter().zip(results) {
                let resp = result.map_err(|e| {
                    UploadError::S3(format!("upload_part {part_number}: {e}"))
                })?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(*part_number as i32)
                        .e_tag(resp.e_tag().unwrap_or_default())
                        .build(),
                );
                uploaded += data.len() as u64;
                if let Some(cb) = &req.progress {
                    cb(uploaded, req.size);
                }
            }
            first = batch_end + 1;
        }

        if parts.is_empty() {
            return Err(UploadError::Io(std::io::Error::other(
                "multipart upload produced zero parts",
            )));
        }

        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        let done = self
            .client
            .complete_multipart_upload()
            .bucket(&req.bucket)
            .key(&req.key)
            .upload_id(upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| UploadError::S3(format!("complete_multipart_upload: {e}")))?;
        Ok(done.e_tag().map(str::to_owned))
    }

    /// 生成预签名 URL：本地 SigV4 签名，无网络 IO。
    /// 有效期钳制在 [1s, 7d]（S3 presign 上限）。
    pub async fn presign_inner(&self, req: &PresignRequest) -> Result<String, UploadError> {
        let expires = Duration::from_secs(req.expires_secs.clamp(1, 7 * 24 * 3600));
        let cfg = aws_sdk_s3::presigning::PresigningConfig::expires_in(expires)
            .map_err(|e| UploadError::Config(format!("presigning config: {e}")))?;
        let presigned = match req.method {
            PresignMethod::Get => self
                .client
                .get_object()
                .bucket(&req.bucket)
                .key(&req.key)
                .presigned(cfg)
                .await
                .map_err(|e| UploadError::S3(format!("presign get_object: {e}")))?,
            PresignMethod::Put => self
                .client
                .put_object()
                .bucket(&req.bucket)
                .key(&req.key)
                .presigned(cfg)
                .await
                .map_err(|e| UploadError::S3(format!("presign put_object: {e}")))?,
        };
        Ok(presigned.uri().to_string())
    }
}

impl ObjectStore for AwsObjectStore {
    fn upload<'a>(
        &'a self,
        req: UploadRequest,
    ) -> Pin<Box<dyn Future<Output = Result<UploadReceipt, UploadError>> + Send + 'a>> {
        Box::pin(async move { self.upload_inner(&req).await })
    }

    fn presign<'a>(
        &'a self,
        req: PresignRequest,
    ) -> Pin<Box<dyn Future<Output = Result<String, UploadError>> + Send + 'a>> {
        Box::pin(async move { self.presign_inner(&req).await })
    }
}

/// 共享句柄类型（worker 与 API 侧 presign 共用）。
pub type SharedStore = Arc<dyn ObjectStore>;

/// 按配置构建默认 ObjectStore（aws-sdk-s3 实现）。
pub fn build_store(cfg: &S3Settings) -> SharedStore {
    Arc::new(AwsObjectStore::new(cfg))
}

/// 按扩展名推导 Content-Type。
pub fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .as_deref()
    {
        Some("wav") => "audio/wav",
        Some("mp3") => "audio/mpeg",
        Some("ogg") | Some("oga") | Some("opus") => "audio/ogg",
        Some("flac") => "audio/flac",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_mapping() {
        assert_eq!(content_type_for(Path::new("a.wav")), "audio/wav");
        assert_eq!(content_type_for(Path::new("a.mp3")), "audio/mpeg");
        assert_eq!(content_type_for(Path::new("a.ogg")), "audio/ogg");
        assert_eq!(content_type_for(Path::new("a.bin")), "application/octet-stream");
    }
}
