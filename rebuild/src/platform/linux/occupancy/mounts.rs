//! 挂载视图：解析 `/proc/self/mountinfo`。
//!
//! 每行先给出挂载标识、父标识与设备号（`major:minor`），随后是根与挂载点。
//! 本模块只取设备号与挂载点，用于判断目标所在设备是否已被挂载。

use std::path::PathBuf;

use crate::platform::{MountEntry, PlatformError, PlatformResult};

/// 读取当前进程的挂载视图；procfs 不存在时返回空集合。
pub fn read() -> PlatformResult<Vec<MountEntry>> {
    let path = super::super::proc_root().join("self/mountinfo");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io(&path, error)),
    };
    parse(&text)
}

/// 解析 `mountinfo` 文本。
pub fn parse(text: &str) -> PlatformResult<Vec<MountEntry>> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            return Err(PlatformError::parse(format!(
                "mountinfo 第 {} 行字段不足",
                index + 1
            )));
        }
        let (major, minor) = parse_device(fields[2], index + 1)?;
        entries.push(MountEntry {
            major,
            minor,
            mount_point: PathBuf::from(decode_octal(fields[4])),
        });
    }
    Ok(entries)
}

fn parse_device(text: &str, line: usize) -> PlatformResult<(u32, u32)> {
    let (major, minor) = text
        .split_once(':')
        .ok_or_else(|| PlatformError::parse(format!("mountinfo 第 {line} 行设备号缺少冒号")))?;
    let major = major
        .parse::<u32>()
        .map_err(|_| PlatformError::parse(format!("mountinfo 第 {line} 行主号无效")))?;
    let minor = minor
        .parse::<u32>()
        .map_err(|_| PlatformError::parse(format!("mountinfo 第 {line} 行次号无效")))?;
    Ok((major, minor))
}

/// 还原 mountinfo 的八进制转义（`\040` 等）。
pub fn decode_octal(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let digits = &bytes[index + 1..index + 4];
            if digits.iter().all(u8::is_ascii_digit) {
                if let Ok(value) = u8::from_str_radix(
                    std::str::from_utf8(digits).unwrap_or("000"),
                    8,
                ) {
                    out.push(value);
                    index += 4;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_major_minor_and_mount_point() {
        let text = "36 35 8:1 / /boot rw,relatime - ext4 /dev/sda1 rw\0";
        let entries = parse(text).expect("可解析");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].device(), "8:1");
        assert_eq!(entries[0].mount_point, PathBuf::from("/boot"));
    }

    #[test]
    fn decodes_octal_escapes() {
        assert_eq!(decode_octal("/mnt/a\\040b"), "/mnt/a b");
        assert_eq!(decode_octal("/plain"), "/plain");
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(parse("1 2 not-a-device / /mnt rw - ext4 /dev/x rw").is_err());
        assert!(parse("1 2").is_err());
    }
}
