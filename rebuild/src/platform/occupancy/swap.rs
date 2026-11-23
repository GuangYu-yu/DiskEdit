//! 交换区视图：解析 `/proc/swaps`。
//!
//! 第一行是表头，其后每行以路径开头，第二字段为类型。路径中的空格以 `\040`
//! 形式转义，这里还原后保存。

use std::path::PathBuf;

use super::super::{PlatformError, PlatformResult};
use super::mounts::same_path;

/// 一个已启用的交换区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapEntry {
    /// 交换区路径，已还原转义序列。
    pub path: PathBuf,
    /// 交换区类型，例如 `partition`、`file`。
    pub kind: String,
}

/// 读取已启用的交换区；procfs 不存在时返回空集合。
pub fn read() -> PlatformResult<Vec<SwapEntry>> {
    let path = super::super::proc_root().join("swaps");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io(&path, error)),
    };
    parse(&text)
}

/// 解析 `/proc/swaps` 文本，跳过表头。
pub fn parse(text: &str) -> PlatformResult<Vec<SwapEntry>> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if index == 0 || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            return Err(PlatformError::parse(format!(
                "swaps 第 {} 行字段不足",
                index + 1
            )));
        }
        entries.push(SwapEntry {
            path: PathBuf::from(super::mounts::decode_octal(fields[0])),
            kind: fields[1].to_string(),
        });
    }
    Ok(entries)
}

/// 交换区路径是否指向给定目标。
pub fn matches_target(entry: &SwapEntry, target: &std::path::Path) -> bool {
    same_path(&entry.path, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_header() {
        let text = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
                    /dev/sda2                               partition\t8388604\t\t0\t\t-2\n\
                    /swapfile                               file\t\t2097148\t\t0\t\t-3\n";
        let entries = parse(text).expect("可解析");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("/dev/sda2"));
        assert_eq!(entries[0].kind, "partition");
        assert_eq!(entries[1].kind, "file");
    }

    #[test]
    fn parse_restores_escaped_path() {
        let text = "Filename Type Size Used Priority\n/var/swap\\040file file 1 0 -1\n";
        let entries = parse(text).expect("可解析");
        assert_eq!(entries[0].path, PathBuf::from("/var/swap file"));
    }

    #[test]
    fn parse_rejects_missing_kind() {
        assert!(parse("Filename Type Size Used Priority\n/swapfile\n").is_err());
    }
}
