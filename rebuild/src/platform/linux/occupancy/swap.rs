//! 交换区视图：解析 `/proc/swaps`。
//!
//! 第一行是表头，其后每行以路径开头，第二字段为类型。路径中的空格以 `\040`
//! 形式转义，这里还原后保存。

use std::path::PathBuf;

use crate::platform::{PlatformError, PlatformResult, SwapEntry};

use super::mounts::decode_octal;

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
            path: PathBuf::from(decode_octal(fields[0])),
            kind: fields[1].to_string(),
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_header_and_parses_paths() {
        let text = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
                    /dev/sda2                               partition\t1048572\t\t0\t\t-2\n\
                    /swap\\040file                          file\t\t524284\t\t0\t\t-3\n";
        let entries = parse(text).expect("可解析");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, PathBuf::from("/dev/sda2"));
        assert_eq!(entries[0].kind, "partition");
        assert_eq!(entries[1].path, PathBuf::from("/swap file"));
    }
}
