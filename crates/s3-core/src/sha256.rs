//! 文件流式 sha256：上传前计算，写入 S3 Metadata 与完成事件。

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

const CHUNK: usize = 1024 * 1024;

/// 计算文件 sha256，返回小写 hex。
pub fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 已读字节的 sha256 hex（测试/小数据用）。
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn matches_known_vector() {
        // "abc" 的 sha256 已知值
        let dir = std::env::temp_dir().join(format!("mod_s3_sha_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.bin");
        std::fs::File::create(&f)
            .unwrap()
            .write_all(b"abc")
            .unwrap();
        assert_eq!(
            file_sha256(&f).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
