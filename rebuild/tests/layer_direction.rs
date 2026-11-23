//! 结构仪器：依赖方向由 crate 图与可见性共同保证，此处核对 crate 图。

use std::process::Command;

fn metadata() -> serde_json::Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata 应可执行");
    assert!(
        output.status.success(),
        "cargo metadata 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata 输出应为 JSON")
}

#[test]
fn core_does_not_depend_on_the_package_root() {
    let meta = metadata();
    let packages = meta["packages"].as_array().expect("packages 应为数组");
    let core = packages
        .iter()
        .find(|pkg| pkg["name"] == "diskedit_core")
        .expect("工作区应包含 diskedit_core");
    let deps = core["dependencies"]
        .as_array()
        .expect("dependencies 应为数组");
    for dep in deps {
        assert_ne!(
            dep["name"].as_str(),
            Some("DiskEdit"),
            "纯核不得依赖包根: {dep:?}"
        );
        assert_eq!(dep["path"].as_str(), None, "纯核不得依赖路径包: {dep:?}");
    }
}

/// 平台假设只许出现在 `src/platform/`。
///
/// `app` 与纯核一旦写下系统接口、虚拟文件系统路径或具体工具名，移植时每一处都要
/// 重新判断语义，所以这里把它们当作结构不变量静态核对。
#[test]
fn app_and_core_do_not_leak_platform_assumptions() {
    const FORBIDDEN: &[&str] = &[
        "platform::linux",
        "platform::windows",
        "platform::macos",
        "libc",
        "nix",
        "std::os::unix",
        "std::os::linux",
        "std::os::windows",
        "std::process::Command",
        "target_os",
        "cfg(unix)",
        "errno",
        "/sys/",
        "/proc/",
        "/dev/",
        "ioctl",
        "BLKPG",
        "BTRFS_IOC",
        "mkfs.",
        "e2fsck",
        "resize2fs",
        "dumpe2fs",
        "tune2fs",
        "ntfsresize",
        "ntfsfix",
        "ntfslabel",
        "fsck.",
        "resize.f2fs",
        "xfs_growfs",
        "xfs_repair",
        "xfs_admin",
        "fatresize",
        "fatlabel",
        "exfatlabel",
        "mkswap",
        "swaplabel",
        "pvresize",
        "mkntfs",
        "losetup",
        "sfdisk",
        "partx",
    ];

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in ["src/app", "crates/core/src"] {
        let dir = root.join(relative);
        let mut files = Vec::new();
        collect_rust_files(&dir, &mut files);
        assert!(!files.is_empty(), "{} 下应存在源文件", dir.display());
        for file in files {
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|error| panic!("读 {} 失败: {error}", file.display()));
            for needle in FORBIDDEN {
                assert!(
                    !mentions(&text, needle),
                    "{} 出现了平台假设 `{needle}`；它只许留在 src/platform/",
                    file.display()
                );
            }
        }
    }
}

/// 平台选择只许出现在 `src/platform/mod.rs`。
///
/// 条件编译散落各处时，换目录不能降低移植成本；这里把「只在平台层选平台」变成
/// 可静态核对的约束。
#[test]
fn platform_selection_lives_in_the_platform_module_only() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_rust_files(&root.join("src"), &mut files);
    let platform_root = root.join("src/platform");
    for file in files {
        if file.starts_with(&platform_root) {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("读 {} 失败: {error}", file.display()));
        for needle in ["target_os =", "target_family =", "windows_subsystem"] {
            assert!(
                !mentions(&text, needle),
                "{} 出现了平台选择 `{needle}`；它只许留在 src/platform/",
                file.display()
            );
        }
    }
}

#[test]
fn guard_detector_matches_identifiers_not_substrings() {
    assert!(mentions("let fd = libc::open(path);", "libc"));
    assert!(mentions("use std::os::unix::fs::FileExt;", "std::os::unix"));
    assert!(mentions("std::fs::read(\"/sys/block\")", "/sys/"));
    assert!(mentions("command: \"partx\"", "partx"));
    assert!(!mentions("Exfatlabel::write", "fatlabel"));
    assert!(!mentions("Repartx", "partx"));
    assert!(!mentions("mkfs_xfs", "mkfs."));
}

/// 以标识符边界查找：`Exfatlabel` 不算出现 `fatlabel`。
fn mentions(text: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(offset) = text[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|ch| !is_ident(ch));
        let after_ok = if needle.ends_with(|ch: char| is_ident(ch)) {
            text[end..].chars().next().is_none_or(|ch| !is_ident(ch))
        } else {
            true
        };
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len();
    }
    false
}

fn is_ident(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn collect_rust_files(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("读目录 {} 失败: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("目录项可读");
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}
