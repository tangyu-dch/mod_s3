//! s3-core 模块根：协议无关的 S3 上传核心。
//!
//! 本 crate 不依赖 FreeSWITCH；所有 FS 相关逻辑位于 `mod-s3` 胶水层。

pub mod config;
pub mod job;
pub mod keytemplate;
pub mod pathguard;
pub mod sha256;
pub mod transcode;
pub mod uploader;
