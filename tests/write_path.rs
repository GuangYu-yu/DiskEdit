//! 目标写入只能经过写通道。
//!
//! 写通道负责区间声明、长度一致、目标上界与写后回读；执行器自己调用
//! `write_at` 就绕开了这些校验，等于出现第二条写入路径。这条守卫让"又写了一个
//! 直写"在编译期之外也能变红。

use std::path::{Path, PathBuf};

/// 允许出现原始写入的位置：写通道自身。
const CHANNEL: &str = "src/app/execute/write.rs";

#[test]
fn only_the_write_channel_calls_the_raw_write() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/app/execute");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut offenders = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .expect("源文件都在仓库内")
            .to_string_lossy()
            .replace('\\', "/");
        if relative == CHANNEL || relative.ends_with("/tests.rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("源文件可读");
        if source.contains(".write_at(") {
            offenders.push(relative);
        }
    }
    assert!(
        offenders.is_empty(),
        "这些执行器绕过写通道直接写目标：{offenders:?}"
    );
}

fn rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("执行器目录可读") {
        let path = entry.expect("目录项可读").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}
