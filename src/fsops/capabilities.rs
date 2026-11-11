//! FS 工具能力矩阵：grow/shrink 支持与缺工具时的提示。

use super::*;

/// FS 的 resize 支持情况——三态而非布尔，"不适用"与"不支持"必须分开：
/// 前者是本工具的边界（不该因此报错），后者是认得却做不了（必须事前拒绝）
pub(crate) enum ToolSupport {
    /// 需要这些工具；任一缺失即事前拒绝
    Tools(&'static [&'static str]),
    /// 不负责：没有可扩的 FS 可谈（空区域 / 类型未识别 / LVM PV）。它不是失败，
    /// 也不等于"做完了"——两种后果由下游按自己的契约分开处置，见 [`Growable`]
    NotApplicable,
    /// 认得出来但本操作不接线。**理由随变体携带**：笼统的"未接线"对用户无从下手，
    /// 而各调用点各写一句理由（lvm2_pv 要 lvreduce 链 / unknown 会写坏数据 / 其余未接线）
    /// 正是"同一事实多处来源"——那样每加一个类型都要改多处
    Unsupported(&'static str),
}

/// 扩容做不了时唯一可行的替代：只改分区表。措辞指向 `resize --no-fs` 而不是裸
/// `--no-fs`——同一条拒绝也会从 `resize-part` / `resizefs` 这类不声明该选项的命令面
/// 报出，指向它们不认识的开关就是错误指引。所有扩容拒绝文案引用这一句
pub(crate) const CHANGE_PARTITION_ONLY: &str = "change the partition only with `resize --no-fs`";

pub(crate) fn grow_support(fstype: ContentKind) -> ToolSupport {
    use ToolSupport::*;
    match fstype {
        ContentKind::Filesystem(FsKind::Ext) => Tools(&["e2fsck", "resize2fs"]),
        ContentKind::Filesystem(FsKind::Ntfs) => Tools(&["ntfsresize"]),
        ContentKind::Filesystem(FsKind::F2fs) => Tools(&["fsck.f2fs", "resize.f2fs"]),
        ContentKind::Filesystem(FsKind::Xfs) => Tools(&["xfs_growfs"]),
        ContentKind::Filesystem(FsKind::Btrfs) => Tools(&["btrfs"]),
        ContentKind::Filesystem(FsKind::Vfat) => Tools(&["fatresize"]),
        // swap 没有"原地 resize"：空间靠 mkswap 重建吸收（resize 的收尾负责），
        // 但这里仍探测 mkswap——那条收尾路径要在写表之前知道工具在不在
        ContentKind::Swap => Tools(&["mkswap"]),
        ContentKind::Lvm2Pv => NotApplicable,
        ContentKind::Filesystem(_) | ContentKind::Unknown => {
            // 出路在渲染点追加（[`CHANGE_PARTITION_ONLY`]）：本层不知道调用它的
            // 命令面是否真的声明了 --no-fs
            Unsupported("not wired to a tool")
        }
    }
}

pub(crate) fn shrink_support(fstype: Option<ContentKind>) -> ToolSupport {
    use ToolSupport::*;
    match fstype {
        // 缩容前要先算 FS 最小尺寸（`fs_min_bytes`：resize2fs -P × dumpe2fs 的块大小），
        // 两个工具都必须"在写盘之前确认存在"，否则表提交后才发现缺 dumpe2fs
        Some(ContentKind::Filesystem(FsKind::Ext)) => Tools(&["e2fsck", "resize2fs", "dumpe2fs"]),
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => Tools(&["ntfsresize"]),
        Some(ContentKind::Filesystem(FsKind::Btrfs)) => Tools(&["btrfs"]),
        // LVM PV 缩容要求新末端之后没有已分配的 extent，需经 lvreduce/pvresize 链，本工具不做；
        // 对用户的拒绝在 `check_shrink` 入口按「选项与目标形态不符」落 3
        Some(ContentKind::Lvm2Pv) => Unsupported(
            "requires the lvreduce/pvresize chain (not implemented here; see pvresize(8))",
        ),
        // 类型认不出来就无法先缩 FS：缩分区后 FS 越界写坏数据
        None | Some(ContentKind::Unknown) => Unsupported(
            "filesystem type unrecognized — shrinking the partition without resizing the FS first would corrupt data",
        ),
        // 其余认得却不会缩的类型：唯一安全路径是先由该 FS 自己的工具缩。
        // 提示不能给出扩容侧的 `--no-fs` 出路：--no-fs 与缩容互斥，那样等于给出错误指引
        _ => Unsupported(
            "not wired to a tool; the filesystem must be shrunk by its own tool first, and --no-fs cannot stand in (the new partition end would cut into filesystem metadata)",
        ),
    }
}

/// 搬移 swap 的重建前置：`mkswap` 重建 swap 区；镜像目标的重建对象是 loop 分区
/// 设备，还要 `losetup`。块设备用 /sys 定位到的现成分区节点，不经 loop
pub(crate) fn swap_rebuild_support(is_block: bool) -> ToolSupport {
    let tools: &'static [&'static str] = if is_block { &["mkswap"] } else { &["mkswap", "losetup"] };
    ToolSupport::Tools(tools)
}

/// 工具所属包名（Debian 系）。写进错误信息是为了让自动化脚本能识别缺失项
/// 并自动安装后重试，而不必靠人读日志
pub(crate) fn tool_package(tool: &str) -> &'static str {
    match tool {
        "e2fsck" | "resize2fs" | "dumpe2fs" | "mke2fs" => "e2fsprogs",
        "ntfsresize" => "ntfs-3g",
        "fsck.f2fs" | "resize.f2fs" => "f2fs-tools",
        "xfs_growfs" => "xfsprogs",
        "btrfs" => "btrfs-progs",
        "fatresize" => "fatresize",
        "mkswap" | "swaplabel" => "util-linux",
        "mount" | "umount" | "losetup" => "util-linux",
        // mkfs 的各工具（mkfs_tool 的 program）：由 mkfs_capability 的缺工具提示引用
        "mkfs.vfat" => "dosfstools",
        "mkfs.exfat" => "exfatprogs",
        "mkfs.ntfs" => "ntfs-3g",
        "mkfs.xfs" => "xfsprogs",
        "mkfs.btrfs" => "btrfs-progs",
        "mkfs.f2fs" => "f2fs-tools",
        _ => "unknown",
    }
}

pub(crate) fn check_support(verb: &str, name: &str, support: ToolSupport) -> Result<(), FsError> {
    match support {
        ToolSupport::NotApplicable => Ok(()),
        ToolSupport::Unsupported(reason) => Err(FsError::unsupported(format!("cannot {verb} {name}: {reason}"))),
        ToolSupport::Tools(tools) => {
            for &t in tools {
                if find_tool(t).is_err() {
                    return Err(FsError::missing(format!(
                        "cannot {verb} {name}: requires `{t}` (package: {}) — not found in PATH",
                        tool_package(t)
                    )));
                }
            }
            Ok(())
        }
    }
}

/// 扩容前置检查（纯只读、不写盘）。返回 Err 即"事前拒绝"。
/// 调用方**必须在首次写盘之前**执行，否则会留下"分区已改、FS 未扩"的中间态——
/// 这正是本检查存在的意义：把可预见的失败挡在动手之前
///
/// `needs_loop`：这个目标的 FS 步是否要先建 loop 映射（规则见 [`needs_loop`]）。
/// `losetup` 只在需要映射时才是这一步的必需工具
pub fn check_grow(fstype: ContentKind, is_block: bool, needs_loop: bool) -> Result<(), FsError> {
    check_support("grow", content_name(fstype), grow_support(fstype))?;
    check_mount_layer_tools(Some(fstype), is_block, needs_loop)
}

/// 缩容前置检查，与 `check_grow` 同一形态。`None`（未识别）在 shrink_support 里对应"类型认不出来"，
/// 故不必在这里另开一臂
pub fn check_shrink(fstype: Option<ContentKind>, is_block: bool, needs_loop: bool) -> Result<(), FsError> {
    // LVM PV 不是文件系统：缩它要先 lvreduce 缩 LV、再 pvresize 缩 PV 元数据，本命令
    // 没有可缩的 FS 可谈。这不是能力缺口（6），而是请求与目标形态不符——
    // 「选项与目标形态不符」落 3
    if fstype == Some(ContentKind::Lvm2Pv) {
        return Err(FsError::invalid(
            "target is an LVM2 PV, not a filesystem — shrinking it needs the lvreduce/pvresize chain (see pvresize(8))",
        ));
    }
    check_support("shrink", content_name(fstype.unwrap_or(ContentKind::Unknown)), shrink_support(fstype))?;
    check_mount_layer_tools(fstype, is_block, needs_loop)
}

/// FS 步是否要先建 loop 映射：镜像目标恒需（没有任何现成设备节点指向它）；
/// 块设备只有 Range 目标需要——Partition / Whole 都有现成的节点可定位
pub(crate) fn needs_loop(scope: &DeviceScope, is_block: bool) -> bool {
    !is_block || matches!(scope, DeviceScope::Range(..))
}

/// mount 层的公共前置：xfs/btrfs 只能挂载态扩缩（grow 与 shrink 都经 with_mount），
/// 需要 loop 映射的目标还要先经 losetup 建立设备。这些工具不进 grow_support/shrink_support
/// 的 FS 工具清单——它们属于挂载/映射层，不随 FS 类型走；但不探测就会在表已提交、
/// FS 工具已动盘之后才暴露缺工具 ⇒ `Io → NoMutation`，违背"确定未写盘"的承诺。
/// 目标是不是块设备不参与判定：那一维度已由 `needs_loop` 收下
pub(crate) fn check_mount_layer_tools(fstype: Option<ContentKind>, _is_block: bool, needs_loop: bool) -> Result<(), FsError> {
    // 执行侧 `with_scope_device_ranged` 对每个目标都先过 require_linux/require_root；
    // 需要映射的目标，设备建立这一步走 losetup。这些检查必须与 FS 工具一同落在写盘前，
    // 否则表提交后才以 Io → NoMutation(4) 暴露，谎报"确定未写盘"
    require_linux()?;
    require_root()?;
    let mount_fs = matches!(fstype, Some(ContentKind::Filesystem(FsKind::Xfs | FsKind::Btrfs)));
    // 基础项随 FS 类型取，losetup 随"这一步是否经 attach_loop 建立设备"取：两个维度分开，
    // 镜像目标与块设备的 Range 目标都需要映射。两者都不需要时清单为空，执行侧不碰这两层
    let mut tools: Vec<&str> = Vec::new();
    if mount_fs {
        tools.extend(["mount", "umount"]);
    }
    if needs_loop {
        tools.push("losetup");
    }
    for t in tools {
        if find_tool(t).is_err() {
            return Err(FsError::missing(format!(
                "requires `{t}` (package: {}) — not found in PATH",
                tool_package(t)
            )));
        }
    }
    Ok(())
}

/// 该 FS 的扩容/缩容应交给用户的补救命令（用于 PARTIAL 时的提示）。
/// 产物是一段 shell 命令文本，属对外呈现，不参与能力判定；`None`（未识别）无补救命令。
///
/// `reason` 由失败现场给出（`FsError::unsupported_reason`），本函数不重读
/// 超级块：只有拒绝的那一侧能同时拿到原因与设备，这里只决定措辞
pub(crate) fn rescue_hint(
    fstype: Option<ContentKind>,
    dev: &str,
    reason: Option<UnsupportedReason>,
) -> String {
    match fstype {
        Some(ContentKind::Filesystem(FsKind::Ext)) => format!("e2fsck -fp {dev} && resize2fs {dev}"),
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => format!("ntfsresize -f -f {dev}"),
        Some(ContentKind::Filesystem(FsKind::F2fs)) => format!("fsck.f2fs {dev} && resize.f2fs {dev}"),
        Some(ContentKind::Filesystem(FsKind::Xfs)) => format!("mount {dev} <mnt> && xfs_growfs <mnt>"),
        // `resize` 的 devid 缺省为 1，`max` 只作用于该设备：多设备 fs 上不带 `devid:`
        // 会去扩另一块成员，故先经 `btrfs filesystem show` 取本设备的 devid
        // （btrfs-progs v6.12 `Documentation/btrfs-filesystem.rst:229-249`，查询日期 2026-11-10）
        Some(ContentKind::Filesystem(FsKind::Btrfs))
            if matches!(reason, Some(UnsupportedReason::MultiDeviceBtrfs)) => format!(
            "mount {dev} <mnt> && btrfs filesystem show <mnt> && btrfs filesystem resize <devid>:max <mnt> — \
             take <devid> for {dev} from that output (resize without it targets devid 1)"
        ),
        Some(ContentKind::Filesystem(FsKind::Btrfs)) => format!("mount {dev} <mnt> && btrfs filesystem resize max <mnt>"),
        Some(ContentKind::Filesystem(FsKind::Vfat)) => format!("fatresize -s max {dev}"),
        Some(ContentKind::Swap) => format!("mkswap --uuid <uuid> {dev}"),
        _ => String::new(),
    }
}