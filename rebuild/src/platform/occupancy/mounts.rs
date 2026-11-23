//! 挂载视图：解析 `/proc/self/mountinfo`。
//!
//! `mountinfo` 每行先给出挂载标识、父标识与设备号（`major:minor`），随后是根与
//! 挂载点。本模块只取设备号与挂载点，用于判断目标所在设备是否已被挂载。

use std::path::{Path, PathBuf};

use super::super::{PlatformError, PlatformResult};

/// 一条挂载记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    /// 设备号的高位。
    pub major: u32,
    /// 设备号的低位。
    pub minor: u32,
    /// 挂载点，已还原转义序列。
    pub mount_point: PathBuf,
}

impl MountEntry {
    /// `major:minor` 文本形式。
    pub fn device(&self) -> String {
        format!("{}:{}", self.major, self.minor)
    }
}

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
    let (major, minor) = text.split_once(':').ok_or_else(|| {
        PlatformError::parse(format!("mountinfo 第 {line} 行的设备号不是 major:minor"))
    })?;
    let major = major
        .parse::<u32>()
        .map_err(|_| PlatformError::parse(format!("mountinfo 第 {line} 行主设备号无效")))?;
    let minor = minor
        .parse::<u32>()
        .map_err(|_| PlatformError::parse(format!("mountinfo 第 {line} 行次设备号无效")))?;
    Ok((major, minor))
}

/// 还原 `\ooo` 形式的八进制转义。
pub(super) fn decode_octal(text: &str) -> String {
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0usize;
    while index < raw.len() {
        let byte = raw[index];
        if byte == b'\\' && index + 3 < raw.len() {
            let digits = &raw[index + 1..index + 4];
            if digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                let value = (digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0');
                out.push(value);
                index += 4;
                continue;
            }
        }
        out.push(byte);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 判断路径是否相等；先按读取值比较，再按规范化后的路径比较。
pub fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        (Ok(_), Err(_)) | (Err(_), Ok(_)) | (Err(_), Err(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_device_and_mount_point() {
        let text = "36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw\n\
                    37 36 8:1 / /boot rw - ext4 /dev/sda1 rw\n";
        let entries = parse(text).expect("可解析");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].major, 98);
        assert_eq!(entries[0].minor, 0);
        assert_eq!(entries[0].mount_point, PathBuf::from("/mnt2"));
        assert_eq!(entries[1].device(), "8:1");
        assert_eq!(entries[1].mount_point, PathBuf::from("/boot"));
    }

    #[test]
    fn parse_restores_escaped_spaces() {
        let text = "1 0 8:1 / /media/My\\040Disk rw - ext4 /dev/sda1 rw\n";
        let entries = parse(text).expect("可解析");
        assert_eq!(
            entries[0].mount_point,
            PathBuf::from("/media/My Disk")
        );
    }

    #[test]
    fn parse_rejects_short_lines() {
        assert!(parse("1 2 3:4\n").is_err());
        assert!(parse("1 2 not-a-device / / rw\n").is_err());
    }
}
