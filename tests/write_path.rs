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

// covers: I11
#[cfg(target_os = "linux")]
#[test]
fn the_platform_refuses_a_write_past_the_target_length() {
    use diskedit::platform::{Platform, host};

    let directory = PathBuf::from("target/fixtures/write-bounds");
    std::fs::create_dir_all(&directory).expect("夹具目录可建");
    let path = directory.join("four-bytes.img");
    std::fs::write(&path, b"abcd").expect("目标可写");

    let mut handle = host().open_target(&path).expect("目标可打开");
    assert_eq!(handle.len(), 4);
    let error = handle
        .write_at(2, &[0xde, 0xad, 0xbe])
        .expect_err("越过目标末尾的写必须被平台层拒绝");
    assert!(
        !matches!(error, diskedit::platform::PlatformError::NotSupported(_)),
        "这是越界写入，不是缺少能力：{error}"
    );
    drop(handle);
    assert_eq!(
        std::fs::read(&path).expect("目标可读"),
        b"abcd",
        "被拒绝的写不能扩展或改动目标"
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
