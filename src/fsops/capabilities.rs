//! FS 工具能力矩阵：grow/shrink 支持与缺工具时的提示。

use super::*;

/// FS 的 resize 支持情况——三态而非布尔，"不适用"与"不支持"必须分开：
/// 前者是本工具的契约边界（不该因此报错），后者是我们认得却做不了（必须事前拒绝）
pub(crate) enum ToolSupport {
    /// 需要这些工具；任一缺失即事前拒绝
    Tools(&'static [&'static str]),
    /// 契约上不负责：没有可扩的 FS 可谈（空区域 / 类型未识别 / LVM PV）。它不是失败，
    /// 也不等于"做完了"——两种后果由下游按自己的契约分开处置，见 [`Growable`]
    NotApplicable,
    /// 认得出来但本操作不接线。**理由随变体携带**：笼统的"未接线"对用户无从下手，
    /// 而各调用点各写一句理由（lvm2_pv 要 lvreduce 链 / unknown 会写坏数据 / 其余未接线）
    /// 正是"同一事实多处来源"——那样每加一个类型都要改多处
    Unsupported(&'static str),
}

pub(crate) fn grow_support(fstype: FsKind) -> ToolSupport {
    use ToolSupport::*;
    match fstype {
        FsKind::Ext => Tools(&["e2fsck", "resize2fs"]),
        FsKind::Ntfs => Tools(&["ntfsresize"]),
        FsKind::F2fs => Tools(&["fsck.f2fs", "resize.f2fs"]),
        FsKind::Xfs => Tools(&["xfs_growfs"]),
        FsKind::Btrfs => Tools(&["btrfs"]),
        FsKind::Vfat => Tools(&["fatresize"]),
        // swap 没有"原地 resize"：空间靠 mkswap 重建吸收（resize 的收尾负责），
        // 但这里仍探测 mkswap——那条收尾路径要在写表之前知道工具在不在
        FsKind::Swap => Tools(&["mkswap"]),
        FsKind::Lvm2Pv => NotApplicable,
        _ => Unsupported("not wired to a tool — pass --no-fs to change the partition only"),
    }
}

pub(crate) fn shrink_support(fstype: Option<FsKind>) -> ToolSupport {
    use ToolSupport::*;
    match fstype {
        // 缩容前要先算 FS 最小尺寸（`fs_min_bytes`：resize2fs -P × dumpe2fs 的块大小），
        // 两个工具都必须"在写盘之前确认存在"，否则表提交后才发现缺 dumpe2fs
        Some(FsKind::Ext) => Tools(&["e2fsck", "resize2fs", "dumpe2fs"]),
        Some(FsKind::Ntfs) => Tools(&["ntfsresize"]),
        Some(FsKind::Btrfs) => Tools(&["btrfs"]),
        // LVM PV 缩容要求新末端之后没有已分配的 extent，需经 lvreduce/pvresize 链，本工具不做
        Some(FsKind::Lvm2Pv) => Unsupported(
            "requires the lvreduce/pvresize chain (not implemented here; see pvresize(8))",
        ),
        // 类型认不出来就无法先缩 FS：缩分区后 FS 越界写坏数据
        None => Unsupported(
            "filesystem type unrecognized — shrinking the partition without resizing the FS first would corrupt data",
        ),
        // 其余认得却不会缩的类型：唯一安全路径是先由该 FS 自己的工具缩。
        // 提示不能是 grow 的 "pass --no-fs"：--no-fs 与缩容互斥，那样等于给出错误指引
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
pub fn check_grow(fstype: FsKind, is_block: bool, needs_loop: bool) -> Result<(), FsError> {
    check_support("grow", fstype.as_str(), grow_support(fstype))?;
    check_mount_layer_tools(Some(fstype), is_block, needs_loop)
}

/// 缩容前置检查，语义同上。`None`（未识别）在 shrink_support 里对应"类型认不出来"，
/// 故不必在这里另开一臂
pub fn check_shrink(fstype: Option<FsKind>, is_block: bool, needs_loop: bool) -> Result<(), FsError> {
    check_support("shrink", fs_name(fstype), shrink_support(fstype))?;
    check_mount_layer_tools(fstype, is_block, needs_loop)
}

/// FS 步是否要先建 loop 映射：镜像目标恒需（没有任何现成设备节点指向它）；
/// 块设备只有 Range 目标需要——Partition / Whole 都有现成的节点可定位
pub(crate) fn needs_loop(scope: &DeviceScope, is_block: bool) -> bool {
    !is_block || matches!(scope, DeviceScope::Range(..))
}

/// mount 层的公共前置：xfs/btrfs 只能挂载态扩缩（grow 与 shrink 都经 with_mount），
/// 镜像目标还要先经 losetup 映射。这些工具不进 grow_support/shrink_support 的
/// FS 工具清单——它们属于挂载/映射层，不随 FS 类型走；但不探测就会在表已提交、
/// FS 工具已动盘之后才暴露缺工具 ⇒ `Io → Infra`，违背"确定未写盘"的承诺
pub(crate) fn check_mount_layer_tools(fstype: Option<FsKind>, is_block: bool, needs_loop: bool) -> Result<(), FsError> {
    // 执行侧 `with_scope_device_ranged` 对每个目标都先过 require_linux/require_root；
    // 镜像目标（任何 FS）与块设备的 Range 目标还要 `losetup` 建映射。这些检查必须与
    // FS 工具一同落在写盘前，否则表提交后才以 Io → Infra(30) 暴露，谎报"确定未写盘"
    require_linux()?;
    require_root()?;
    let mount_fs = matches!(fstype, Some(FsKind::Xfs | FsKind::Btrfs));
    // 块设备上的非挂载 FS 且不需要映射：执行侧不会碰 losetup，提前返回是准确的
    if !mount_fs && !needs_loop {
        return Ok(());
    }
    let tools: &[&str] = if mount_fs {
        if is_block { &["mount", "umount"] } else { &["mount", "umount", "losetup"] }
    } else {
        // 非挂载 FS：镜像与块设备的 Range 目标都经 attach_loop 建立设备
        &["losetup"]
    };
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
/// 产物是一段 shell 命令文本，属对外呈现，不参与能力判定；`None`（未识别）无补救命令
pub fn rescue_hint(fstype: Option<FsKind>, dev: &str) -> String {
    match fstype {
        Some(FsKind::Ext) => format!("e2fsck -fp {dev} && resize2fs {dev}"),
        Some(FsKind::Ntfs) => format!("ntfsresize -f -f {dev}"),
        Some(FsKind::F2fs) => format!("fsck.f2fs {dev} && resize.f2fs {dev}"),
        Some(FsKind::Xfs) => format!("mount {dev} <mnt> && xfs_growfs <mnt>"),
        Some(FsKind::Btrfs) => format!("mount {dev} <mnt> && btrfs filesystem resize max <mnt>"),
        Some(FsKind::Vfat) => format!("fatresize -s max {dev}"),
        Some(FsKind::Swap) => format!("mkswap --uuid <uuid> {dev}"),
        _ => String::new(),
    }
}