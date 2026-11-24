//! loop 设备参数与输出解析。
//!
//! 只构造参数、解析结果；执行经平台端口，错误分类由调用方统一处理。

use std::path::{Path, PathBuf};

use crate::platform::{PlatformError, PlatformResult};

/// 绑定后备文件所需的参数。
pub fn attach_args(backing: &Path, offset: u64, size: u64) -> Vec<String> {
    vec![
        "--find".to_string(),
        "--show".to_string(),
        "--offset".to_string(),
        offset.to_string(),
        "--sizelimit".to_string(),
        size.to_string(),
        backing.to_string_lossy().into_owned(),
    ]
}

/// 解绑参数。
pub fn detach_args(device: &Path) -> Vec<String> {
    vec!["-d".to_string(), device.to_string_lossy().into_owned()]
}

/// 从 `losetup --show` 的输出取设备路径。
pub fn parse_device(stdout: &str) -> PlatformResult<PathBuf> {
    let device = stdout.trim();
    if device.is_empty() {
        return Err(PlatformError::parse(
            "losetup did not output a ready device path",
        ));
    }
    if !device.starts_with('/') {
        return Err(PlatformError::parse(format!(
            "losetup output is not an absolute path: {device:?}"
        )));
    }
    Ok(PathBuf::from(device))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_attach_arguments() {
        assert_eq!(
            attach_args(Path::new("/img/a.raw"), 512, 4096),
            vec![
                "--find",
                "--show",
                "--offset",
                "512",
                "--sizelimit",
                "4096",
                "/img/a.raw"
            ]
        );
    }

    #[test]
    fn parses_device_and_rejects_garbage() {
        assert_eq!(
            parse_device("/dev/loop7\n").expect("可解析"),
            PathBuf::from("/dev/loop7")
        );
        assert!(parse_device("   ").is_err());
        assert!(parse_device("loop7").is_err());
    }
}
