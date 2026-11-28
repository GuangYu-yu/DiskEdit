//! 残留检索：不得留在源码树里的旧路径、临时标记与备份文件。

use std::path::{Path, PathBuf};

/// 不得以同名文件重新出现的旧路径。
const RETIRED: &[&str] = &[
    "src/app/carrier",
    "src/app/docsgen",
    "src/app/execute/steps",
    "src/app/execute/online",
    "src/app/decide/common",
];

/// 生产源码里不允许出现的临时标记。
const MARKERS: &[&str] = &[
    "TODO",
    "FIXME",
    "XXX",
    "unimplemented!",
    "todo!",
    "not implemented",
    "not yet wired",
    "deprecated",
];

/// 不属于当前命令面的 token；只对 CLI 源码（src/）做精确匹配。
///
/// 域概念词（recovery、identity、RecoverMode 等）不在禁词之列；`recover` 只禁
/// 命令名字符串字面量与 `diskedit recover` 的消息引用，不禁纯核的 `recover`
/// 恢复器与其调用。`inspect-internal` 是 btrfs 的子命令参数，不匹配
/// `diskedit inspect`。
const RETIRED_FACE_TOKENS: &[&str] = &[
    "\"recover\"", // 命令名字符串字面量（声明、分派与消息）
    "diskedit recover",
    "--replay-table-only",
    "resizefs",
    "records scan",
    "--identity",
    "diskedit info",
    "diskedit inspect",
];

/// 记录层与决定层的旧符号：改名后不得以原名重新出现。
///
/// 记录载荷按字节位置编解码，字段改名不改变格式；这里守的是"同一事实只有一处
/// 名字"，以及"只凭身份就能构造恢复期望"这条已删除的入口。
const RETIRED_IDENTIFIERS: &[&str] = &[
    "legacy_generation",
    "LegacyGeneration",
    "geometry_generation",
    "fsops",
    "impl From<Fingerprint> for RecoveryExpectation",
];

#[test]
fn retired_paths_are_absent() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in RETIRED {
        assert!(
            !root.join(relative).exists(),
            "{relative} 是已移出的旧路径，不得重新出现"
        );
    }
}

#[test]
fn production_sources_have_no_temporary_markers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for relative in ["src", "crates/core/src"] {
        collect(&root.join(relative), &mut files, "rs");
    }
    assert!(!files.is_empty());
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("读 {} 失败: {error}", file.display()));
        for marker in MARKERS {
            assert!(
                !text.contains(marker),
                "{} 含有临时标记 `{marker}`",
                file.display()
            );
        }
    }
}

#[test]
fn no_backup_or_patch_files_are_left_behind() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_all(root, &mut files);
    for file in files {
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let stale = name.ends_with('~')
            || name.ends_with(".bak")
            || name.ends_with(".orig")
            || name.ends_with(".rej");
        assert!(!stale, "{} 是编辑残留", file.display());
    }
}

#[test]
fn cli_sources_have_no_retired_face_tokens() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect(&root.join("src"), &mut files, "rs");
    assert!(!files.is_empty());
    for file in files {
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("读 {} 失败: {error}", file.display()));
        for token in RETIRED_FACE_TOKENS {
            assert!(
                !text.contains(token),
                "{} 含有旧用法面 token {token}——命令面已改名，面词零残留",
                file.display()
            );
        }
    }
}

#[test]
fn retired_identifiers_are_absent() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for relative in ["src", "crates/core/src", "tests"] {
        collect(&root.join(relative), &mut files, "rs");
    }
    assert!(!files.is_empty());
    for file in files {
        // 本文件承载禁词清单本身。
        if file.file_name().and_then(|name| name.to_str()) == Some("residue.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("读 {} 失败: {error}", file.display()));
        for token in RETIRED_IDENTIFIERS {
            assert!(
                !text.contains(token),
                "{} 含有旧符号 `{token}`——同一事实只留一个名字",
                file.display()
            );
        }
    }
}

#[test]
fn readmes_name_the_binary_the_manifest_builds() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml 可读");
    let bin = manifest
        .split("[[bin]]")
        .nth(1)
        .and_then(|section| {
            section.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("name = ")
                    .map(|value| value.trim_matches('"').to_owned())
            })
        })
        .expect("Cargo.toml 必须显式声明 [[bin]] name");
    let expected = format!("target/release/{bin}");
    for readme in ["README.md", "README.zh-CN.md"] {
        let text = std::fs::read_to_string(root.join(readme))
            .unwrap_or_else(|error| panic!("读 {readme} 失败: {error}"));
        assert!(
            text.contains(&expected),
            "{readme} 必须写明产物 `{expected}`，与 [[bin]] 的 name 一致"
        );
    }
}

fn collect(dir: &Path, found: &mut Vec<PathBuf>, extension: &str) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("读目录 {} 失败: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("目录项可读");
        let path = entry.path();
        if path.is_dir() {
            collect(&path, found, extension);
        } else if path.extension().is_some_and(|ext| ext == extension) {
            found.push(path);
        }
    }
}

fn collect_all(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("读目录 {} 失败: {error}", dir.display()));
    for entry in entries {
        let entry = entry.expect("目录项可读");
        let path = entry.path();
        if path.is_dir() {
            // 只查本仓库的编辑残留：构建产物与 git 内部目录不属于本项目的编辑面。
            match path.file_name().and_then(|name| name.to_str()) {
                Some("target") | Some(".git") => continue,
                _ => {}
            }
            collect_all(&path, found);
        } else {
            found.push(path);
        }
    }
}
