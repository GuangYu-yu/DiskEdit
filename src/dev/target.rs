//! 命令行目标解析（TARGET[:N]）与镜像分区节点提示。

use super::*;

/// 分区选择器：用户写下的 `:N` / `:last`（`--grow N|last` 同形）。词法层只能判定形式，
/// 解析成具体分区号要等读到分区表——语义与分派见 `gpt_policy::resolve_part_in`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartSelector {
    Number(u32),
    Last,
}

/// 回显用户写法：`3` / `last`（拒绝文案里 `drop :{n}` 用它拼回原样）
impl std::fmt::Display for PartSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PartSelector::Number(n) => write!(f, "{n}"),
            PartSelector::Last => write!(f, "last"),
        }
    }
}

/// 解析 `<target>[:N]` → (路径, 分区选择器 Option)。
/// 本层只判定合法性、不决定进程怎么退出：数字段溢出 u32 静默当整盘目标会误伤数据，
/// 故作为错误上抛，由调用方（main 的参数层）转成退出码。
/// `:0` 同样拒绝——分区号是 1-based，静默折叠成"整盘"会把一次针对具体分区的操作
/// 放大成对整盘的表操作。
/// `:last` 指**最后一个可操作分区**（不是字面上的末端最大表项）；它落到具体号要等读到
/// 分区表，故本层只判定形式（大小写不敏感，与 `--start end` 同一先例）
pub fn parse_target(s: &str) -> Result<(String, Option<PartSelector>), &'static str> {
    match s.rfind(':') {
        // 尾冒号（`img:`）多半是分区号漏写的笔误：按整路径打开只会报"文件不存在"，
        // 不如当场说清缺的是什么
        Some(pos) if pos + 1 == s.len() => Err("missing partition number after ':'"),
        Some(pos) if s[pos + 1..].eq_ignore_ascii_case("last") => {
            Ok((s[..pos].to_string(), Some(PartSelector::Last)))
        }
        Some(pos) if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u32 = s[pos + 1..].parse().map_err(|_| "partition number out of range")?;
            if n == 0 {
                return Err("partition number is 1-based (:0 is not a partition)");
            }
            Ok((s[..pos].to_string(), Some(PartSelector::Number(n))))
        }
        _ => Ok((s.to_string(), None)),
    }
}

/// 块设备分区节点命名，两种真实惯例并存：
/// - 盘名以数字结尾时分区号加 "p"（/dev/sda→/dev/sda1、/dev/nvme0n1→/dev/nvme0n1p1）；
/// - udev 的 by-id / by-path 链接用 "-part" 后缀（ata-XXX→ata-XXX-part1；
///   systemd 60-persistent-storage.rules：DEVTYPE=="partition" ⇒ PART_SUFFIX="-part%n"）。
///
/// `part_dev_hint`（镜像 losetup 提示）与在线/离线 LVM 链的分区节点路径共用此规则。
/// `base` 是**盘名**（如 /dev/sda、/dev/disk/by-id/ata-XXX），不含分区号
pub(crate) fn part_node_name(base: &str, part: u32) -> String {
    // 前缀判据必须先行：by-id 名（wwn-0x…、nvme-eui.…、序列号）完全可能以数字结尾，
    // 先走"数字结尾 ⇒ 拼 p"会生成 by-id 路径下不存在的 …pN
    let sep = if base.starts_with("/dev/disk/by-id/") || base.starts_with("/dev/disk/by-path/") {
        "-part"
    } else if base.ends_with(|c: char| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    format!("{base}{sep}{part}")
}

/// 补救提示里的设备标识：块设备给出可直接粘贴的分区节点（/dev/sdb→/dev/sdb1、
/// /dev/nvme0n1→/dev/nvme0n1p1，末尾数字需 p 分隔，命名规则见 `part_node_name`）；
/// 镜像文件没有分区节点，给出字节偏移供 `losetup -o` 使用
pub(crate) fn part_dev_hint(src: &FileSource, part: u32, offset_bytes: u64) -> String {
    if src.is_block {
        part_node_name(&src.path.to_string_lossy(), part)
    } else {
        format!("<part {part} of {} at offset {offset_bytes} — e.g. losetup -o {offset_bytes}>", src.path.display())
    }
}

/// 删除持久化元数据（journal / checkpoint）失败：残留不是"无害垃圾"——
/// journal 残留会让下次 undo 重复回滚已回滚的内容，checkpoint 残留会让下次运行
/// 被旧计划阻塞或误续传。属用户必须知道的状态，故告警（不改变本次的成功结论）。
/// 文件本就不存在不算失败：删除的后置条件是"不残留"，此时已然成立
pub(crate) fn warn_if_remove_failed(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!(
            "warning: cannot remove {}: {e} — a later run may re-apply this journal or be blocked by a stale checkpoint",
            path.display()
        ),
    }
}

/// 尽力清理临时资源（临时挂载点等）：失败只影响资源占用，且调用点通常已有主错误/告警
#[allow(clippy::let_underscore_must_use)]
pub(crate) fn best_effort_rmdir(dir: &std::path::Path) {
    let _ = std::fs::remove_dir(dir);
}

/// 尽力而为的目录 fsync：把新建/重命名后的目录项推入持久存储。
/// 失败**不**影响安全性——只影响"崩溃后还能看到多新的元数据"：
/// - ckpt 目录项丢失 → 恢复点回退到上一个 checkpoint（重做已完成部分，安全，不会超前）
/// - journal 目录项丢失 → 该 journal 可能整份消失（undo 能力丢失，前向恢复不受影响）
///
/// 两者都不会产生"超前于数据"的持久化状态，故此处有意忽略失败
#[cfg(unix)]
#[allow(clippy::let_underscore_must_use)] // 有意忽略：失败只使元数据回退，见上
pub(crate) fn best_effort_dir_fsync(path: &std::path::Path) {
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
pub(crate) fn best_effort_dir_fsync(_path: &std::path::Path) {}