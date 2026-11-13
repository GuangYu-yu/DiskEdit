//! 守卫 target 共用的只读工具：源码读取、标记顺序、进程级命令入口。
//!
//! 这些 target 把「计划步骤只有一个执行器」「码值只有一份映射」「记录自证且无版本号」
//! 三条结构约束固化成对源码的可机械复核断言；其中 `cmd_info`、`cmd_plan` 另以进程级
//! 只读命令核对输出字段与退出码。目标字节的零写检查由既有 `no_write_probe`（Linux，
//! 递归 sha256 快照）与 `cli/platform_gate`（非 Linux）承担。结构与行为两类断言不互相替代。
//!
//! 每个 integration test 是独立 crate，各自只用到本模块的一部分函数；未使用的 `pub`
//! 项在该 crate 内必然触发 `dead_code`，故模块级放行（全仓唯一的模块级 allow）。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// 读一份源码；缺文件直接失败，不静默跳过（否则守卫会随文件改名失效）
pub fn source(rel: &str) -> String {
    let path = src_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// `src/` 下全部 `.rs` 的 (相对路径, 内容)。跳过 `tests/` 子目录——测试夹具里的直写
/// 不属于生产写点，不该被「不得新增写路径」的断言误伤
pub fn rust_sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .map(|e| e.expect("read a directory entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "tests") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(src_root())
                    .expect("walk started at src/")
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
                out.push((rel, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(&src_root(), &mut out);
    out
}

pub fn pos(text: &str, needle: &str, what: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{what}: expected marker {needle:?} is gone"))
}

pub fn assert_order(text: &str, needles: &[&str], what: &str) {
    let mut at = 0usize;
    for needle in needles {
        let p = text[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("{what}: {needle:?} is missing after byte {at}"));
        at += p + needle.len();
    }
}

pub fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_DiskEdit"))
        .args(args)
        .output()
        .expect("spawn DiskEdit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
