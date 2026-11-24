//! 残留检索：旧实现路径、临时标记与备份文件不得留在源码树里。

use std::path::{Path, PathBuf};

/// 已被纵切移出的旧路径；S9 之前不得以同名文件重新出现。
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
            if path.file_name().and_then(|name| name.to_str()) == Some("target") {
                continue;
            }
            collect_all(&path, found);
        } else {
            found.push(path);
        }
    }
}
