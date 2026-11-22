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
/// 分区表，故本层只判定形式（大小写不敏感，与 `--start end` 一致）
pub fn parse_target(s: &str) -> Result<(String, Option<PartSelector>), &'static str> {
    match s.rfind(':') {
        // 尾冒号（`img:`）多半是分区号漏写的笔误：按整路径打开只会报"文件不存在"，
        // 不如当场说清缺的是什么
        Some(pos) if pos + 1 == s.len() => Err("missing partition number after ':'"), // audit-allow: index-in-bounds
        Some(pos) if s[pos + 1..].eq_ignore_ascii_case("last") => { // audit-allow: index-in-bounds
            Ok((s[..pos].to_string(), Some(PartSelector::Last)))
        }
        Some(pos) if s[pos + 1..].chars().all(|c| c.is_ascii_digit()) => { // audit-allow: index-in-bounds
            let n: u32 = s[pos + 1..].parse().map_err(|_| "partition number out of range")?; // audit-allow: index-in-bounds
            if n == 0 {
                return Err("partition number is 1-based (:0 is not a partition)");
            }
            // `PartSelector::Number` 的解析值域是 1..=128（GPT 条目数上限）；
            // MBR 目标上 5..=128 由主分区槽位守卫拒绝（3），解析期不区分盘型
            if n > 128 {
                return Err("partition number out of range (1..=128)");
            }
            Ok((s[..pos].to_string(), Some(PartSelector::Number(n))))
        }
        _ => Ok((s.to_string(), None)),
    }
}

/// `<part dir>/partition` → 分区号（内核 sysfs-block ABI：属性内容即分区号，整盘目录
/// 没有该属性）。属性读不出时 `NotFound` 由调用方按自己的语义解释（遍历时=不是分区，
/// 直接定位时=不是分区目标）；读得出但解析不了、或值超出 u32，都是环境故障，不截断
#[cfg(target_os = "linux")]
pub(crate) fn sysfs_partition_no(sys_dir: &Path) -> io::Result<u32> {
    let path = sys_dir.join("partition");
    let text = std::fs::read_to_string(&path)?;
    let n: u64 = text
        .trim()
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("unparsable sysfs value in {}", path.display())))?;
    u32::try_from(n).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, format!("sysfs partition number {n} overflows u32"))
    })
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

/// `/dev` 下的节点必须与 sysfs 记录的设备号同源：名字只是路径，实际写入落点由设备号
/// 决定。盘名/分区名都由 sysfs 目录名推得，同名节点若指向另一台设备，"几何来自 A、
/// 写入落在 B"——sfdisk / partx / BLKPG / pvresize 只拿到这个路径，无从自行发现。
///
/// 判定走**已打开的句柄**的 rdev 而不是再次 stat 路径：核对与结论之间不再有一步
/// "按名字重新解析"。返回的句柄由调用方持有到外部工具返回，期间设备对象不会被回收；
/// 外部工具自己按路径打开时仍可能落在被替换的同名节点上，这段窗口不在本函数可及范围
#[cfg(target_os = "linux")]
pub(crate) fn ensure_node_is_device(node: &Path, sys_dir: &Path) -> io::Result<std::fs::File> {
    use std::os::linux::fs::MetadataExt;
    use std::os::unix::fs::FileTypeExt;
    let want = std::fs::read_to_string(sys_dir.join("dev"))
        .map_err(|e| io::Error::other(format!("cannot read the sysfs device number of {}: {e}", sys_dir.display())))?;
    let Some((dmaj, dmin)) = want
        .trim()
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
    else {
        return Err(io::Error::other(format!("unparsable sysfs device number {want:?} in {}", sys_dir.display())));
    };
    // O_NONBLOCK：可移动介质在无盘时会阻塞普通 open。句柄只用于身份核对，
    // 按读写打开以便调用方直接拿它做块设备 ioctl（BLKPG 需要可写句柄）
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(node)
        .map_err(|e| io::Error::other(format!("device node {} is unusable: {e}", node.display())))?;
    let md = file
        .metadata()
        .map_err(|e| io::Error::other(format!("cannot read the metadata of {}: {e}", node.display())))?;
    if !md.file_type().is_block_device() {
        return Err(io::Error::other(format!("{} is not a block device node", node.display())));
    }
    let (nmaj, nmin) = (libc::major(md.st_rdev()), libc::minor(md.st_rdev()));
    if (nmaj, nmin) != (dmaj, dmin) {
        return Err(io::Error::other(format!(
            "{} is device {nmaj}:{nmin} but sysfs reports {dmaj}:{dmin} for {} — refusing to write through a mismatched node",
            node.display(),
            sys_dir.display()
        )));
    }
    Ok(file)
}

/// 尽力清理临时资源（临时挂载点等）：失败只影响资源占用，且调用点通常已有主错误/告警
// audit-allow: A-12
#[allow(clippy::let_underscore_must_use)]
pub(crate) fn best_effort_rmdir(dir: &std::path::Path) {
    // audit-allow: D-16
    let _ = std::fs::remove_dir(dir);
}

/// 尽力而为的目录 fsync：把新建/重命名后的目录项推入持久存储。
/// 失败**不**影响安全性——只影响"崩溃后还能看到多新的元数据"：
/// - ckpt 目录项丢失 → 恢复点回退到上一个 checkpoint（重做已完成部分，安全，不会超前）
/// - journal 目录项丢失 → 该 journal 可能整份消失（回滚能力丢失，前向恢复不受影响）
///
/// 两者都不会产生"超前于数据"的持久化状态，故此处有意忽略失败。
/// **新建 journal 不走这一路**：那时回滚能力是唯一的前向保证，新建的目录项必须
/// 真正落盘（见 `platform::evidence_store` 的 `sync_dir`）。
#[cfg(unix)]
// audit-allow: A-13
#[allow(clippy::let_underscore_must_use)] // 有意忽略：失败只使元数据回退
pub(crate) fn best_effort_dir_fsync(path: &std::path::Path) {
    if let Some(dir) = path.parent()
        // audit-allow: S2-20
        && let Ok(d) = std::fs::File::open(dir)
    {
        // audit-allow: D-17
        let _ = d.sync_all();
    }
}

#[cfg(not(unix))]
pub(crate) fn best_effort_dir_fsync(_path: &std::path::Path) {}

/// 改名成功后的持久性说明。
/// 改名只作用于 `from` 这个路径入口，`rename` 不跟随符号链接
#[cfg(unix)]
pub(crate) fn note_rename_unverified(_from: &std::path::Path, _to: &std::path::Path) {}

#[cfg(not(unix))]
pub(crate) fn note_rename_unverified(from: &std::path::Path, to: &std::path::Path) {
    eprintln!(
        "note: moved {} to {}; persistence of the directory entry was not verified on this platform",
        from.display(),
        to.display()
    );
}

/// 退路改名：先 `create_new` 占住目标名（占位是空文件），再由 `rename` 覆盖本进程刚建的
/// 占位——"名字空闲"的判据是 `create_new` 的返回值，不是 `stat`。用于不支持硬链接的文件
/// 系统（FAT/exFAT、部分网络 FS；镜像常放在这类盘上，现场文件就在镜像旁边）。
///
/// 占位已被占用（另一份记录占着该名字）⇒ `AlreadyExists`，由调用方顺延下一个名字；
/// 占位删不掉只留下一个空文件，不改变"这次改名失败"的结论
pub(crate) fn rename_over_placeholder(from: &std::path::Path, to: &std::path::Path) -> io::Result<()> {
    std::fs::OpenOptions::new().write(true).create_new(true).open(to)?;
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 占位可能已经不存在（rename 部分生效），清理结果不影响本次失败的结论
            drop(std::fs::remove_file(to));
            Err(e)
        }
    }
}