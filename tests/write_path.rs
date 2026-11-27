//! 目标写入只能经过写通道。
//!
//! 写通道负责声明区间、长度一致、目标上界与写后回读；执行器直接调用 `write_at`
//! 就绕开了这些校验。这条守卫扫描整棵源码树：原始写只允许出现在写通道自身与
//! `platform` 子树（写原语的唯一实现地）。

use std::path::{Path, PathBuf};

/// 允许出现原始写入的位置：写通道自身。
const CHANNEL: &str = "src/app/execute/write.rs";

/// 参与扫描的源码根。
const ROOTS: [&str; 2] = ["src", "crates"];

/// 扫描结果：越界文件与确实看过的文件。
#[derive(Default)]
struct Scan {
    offenders: Vec<String>,
    visited: Vec<String>,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn is_exempt(relative: &str) -> bool {
    relative == CHANNEL || relative.starts_with("src/platform/")
}

fn rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
    if !directory.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(directory).expect("源码目录可读") {
        let path = entry.expect("目录项可读").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// 去掉行注释后再匹配，避免注释里的示例把守卫绊红。
fn code_only(text: &str) -> String {
    text.lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn scan(root: &Path) -> Scan {
    let mut files = Vec::new();
    for directory in ROOTS {
        rust_files(&root.join(directory), &mut files);
    }
    let mut scan = Scan::default();
    for path in files {
        let relative = path
            .strip_prefix(root)
            .expect("源文件在仓库内")
            .to_string_lossy()
            .replace('\\', "/");
        scan.visited.push(relative.clone());
        if is_exempt(&relative) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("源文件可读");
        if code_only(&text).contains(".write_at(") {
            scan.offenders.push(relative);
        }
    }
    scan
}

// covers: I17
#[test]
fn only_the_write_channel_calls_the_raw_write() {
    let scan = scan(&repo_root());
    assert!(
        scan.offenders.is_empty(),
        "这些源码绕过写通道直接写目标：{:?}",
        scan.offenders
    );
}

// covers: I17
#[test]
fn the_guard_covers_every_source_tree_not_just_the_executor() {
    let scan = scan(&repo_root());
    for prefix in [
        "src/app/decide/",
        "src/app/observe/",
        "src/app/cli/",
        "src/app/execute/",
        "crates/core/src/",
    ] {
        assert!(
            scan.visited.iter().any(|path| path.starts_with(prefix)),
            "守卫没有扫描 {prefix}：共扫描 {} 个文件",
            scan.visited.len()
        );
    }
}

// covers: I17
#[test]
fn the_guard_flags_a_synthetic_bypass_outside_the_platform() {
    let root = PathBuf::from("target/fixtures/write-path-guard");
    drop(std::fs::remove_dir_all(&root));
    let bypass = root.join("src/app/decide/bypass.rs");
    std::fs::create_dir_all(bypass.parent().expect("有父目录")).expect("夹具目录可建");
    std::fs::write(
        &bypass,
        "fn f(handle: &mut H) { handle.write_at(0, &[]); }\n",
    )
    .expect("夹具可写");
    assert_eq!(
        scan(&root).offenders,
        vec!["src/app/decide/bypass.rs".to_owned()],
        "守卫必须把 platform 之外出现的 write_at 报红"
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
    handle
        .write_at(2, &[0xde, 0xad, 0xbe])
        .expect_err("越过目标末尾的写必须被平台层拒绝");
    drop(handle);
    assert_eq!(
        std::fs::read(&path).expect("目标可读"),
        b"abcd",
        "被拒绝的写不能扩展或改动目标"
    );
}

// covers: I11
#[cfg(target_os = "linux")]
#[test]
fn the_platform_reads_the_current_length_before_writing() {
    use diskedit::platform::{Platform, host};

    let directory = PathBuf::from("target/fixtures/write-current-length");
    std::fs::create_dir_all(&directory).expect("夹具目录可建");
    let path = directory.join("grow-before-write.img");
    std::fs::write(&path, b"abcd").expect("目标可写");

    let mut handle = host().open_target(&path).expect("目标可打开");
    assert_eq!(handle.len(), 4);
    // 打开之后、写入之前目标被外部截断：缓存长度是 4，此刻长度是 2。
    std::fs::write(&path, b"ab").expect("目标可截断");
    handle
        .write_at(2, &[0xde, 0xad])
        .expect_err("平台必须按此刻长度拒绝越界写");
    drop(handle);
    assert_eq!(
        std::fs::read(&path).expect("目标可读"),
        b"ab",
        "被拒绝的写不能扩展或改动目标"
    );
}
