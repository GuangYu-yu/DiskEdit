//! mkfs：外部工具选择、能力检查与执行。

use super::*;
use crate::dev::WriteSource;

/// `--fs`/`mkfs` 请求要创建的具体类型。与识别域 [`FsKind`] 分属两件事：识别器只到 ext
/// 家族（0xEF53 区分不出 2/3/4），而 mkfs 请求必须点名 ext2/3/4（`mke2fs -t extN`）。
/// 两者不互相包含：本枚举独有 ext2/3/4，[`FsKind`] 独有 squashfs/erofs/hfsplus/apfs。
/// 交集按转调共享 [`FsKind::as_str`] 维持，字符串只维护一处
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MkfsFs {
    Ext2,
    Ext3,
    Ext4,
    Vfat,
    Exfat,
    Ntfs,
    Xfs,
    Btrfs,
    F2fs,
    Swap,
    /// 能被解析、随后被 [`mkfs_tool`] 逐字节拒绝（`cannot mkfs an LVM2 PV …`）。
    /// 枚举必须含它，否则那条文案与失败分类会变
    Lvm2Pv,
}

impl MkfsFs {
    /// 对外规范名（`--fs` 接受的值、`-t` 参数、文案都由它生成）
    pub fn as_str(&self) -> &'static str {
        match self {
            MkfsFs::Ext2 => "ext2",
            MkfsFs::Ext3 => "ext3",
            MkfsFs::Ext4 => "ext4",
            // 与识别域共享的 8 项转调其规范名，不复制字符串
            MkfsFs::Vfat => FsKind::Vfat.as_str(),
            MkfsFs::Exfat => FsKind::Exfat.as_str(),
            MkfsFs::Ntfs => FsKind::Ntfs.as_str(),
            MkfsFs::Xfs => FsKind::Xfs.as_str(),
            MkfsFs::Btrfs => FsKind::Btrfs.as_str(),
            MkfsFs::F2fs => FsKind::F2fs.as_str(),
            MkfsFs::Swap => FsKind::Swap.as_str(),
            MkfsFs::Lvm2Pv => FsKind::Lvm2Pv.as_str(),
        }
    }

    /// CLI 拼写 → 类型。只认规范拼写，不做大小写归一化
    pub fn from_cli_name(name: &str) -> Option<MkfsFs> {
        Some(match name {
            "ext2" => MkfsFs::Ext2,
            "ext3" => MkfsFs::Ext3,
            "ext4" => MkfsFs::Ext4,
            "vfat" => MkfsFs::Vfat,
            "exfat" => MkfsFs::Exfat,
            "ntfs" => MkfsFs::Ntfs,
            "xfs" => MkfsFs::Xfs,
            "btrfs" => MkfsFs::Btrfs,
            "f2fs" => MkfsFs::F2fs,
            "swap" => MkfsFs::Swap,
            "lvm2_pv" => MkfsFs::Lvm2Pv,
            _ => return None,
        })
    }

    /// 识别域口径的类型：ext2/3/4 折叠为 [`FsKind::Ext`]（e2fsck/resize2fs 对三者通用）。
    /// 供需要 `FsKind` 的消费点（如 [`rescue_hint`]）跨域取值
    pub fn as_fs_kind(self) -> FsKind {
        match self {
            MkfsFs::Ext2 | MkfsFs::Ext3 | MkfsFs::Ext4 => FsKind::Ext,
            MkfsFs::Vfat => FsKind::Vfat,
            MkfsFs::Exfat => FsKind::Exfat,
            MkfsFs::Ntfs => FsKind::Ntfs,
            MkfsFs::Xfs => FsKind::Xfs,
            MkfsFs::Btrfs => FsKind::Btrfs,
            MkfsFs::F2fs => FsKind::F2fs,
            MkfsFs::Swap => FsKind::Swap,
            MkfsFs::Lvm2Pv => FsKind::Lvm2Pv,
        }
    }
}

impl std::fmt::Display for MkfsFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `--fs` 值 → 类型。失败归 `UnsupportedFs`（拒绝/10）：拼错的类型名改参数即可解
pub fn parse_fstype(name: &str) -> Result<MkfsFs, FsError> {
    MkfsFs::from_cli_name(name).ok_or_else(|| {
        FsError::unsupported(format!(
            "unsupported fstype {name} (supported: ext2/3/4, xfs, btrfs, f2fs, vfat, exfat, ntfs, swap)"
        ))
    })
}

/// fstype → 外部工具与参数。**唯一**一份支持列表：`mkfs` 与命令层的"先问再做"都读它，
/// 免得"能不能做"与"怎么做"两份清单分叉。
///
/// - ext 系直调 mke2fs -t extN（mkfs.extN 只是它的符号链接，man mke2fs）
/// - swap 用 mkswap（裸设备遇已有盘标头须 -f，分区节点不受影响，man mkswap）
/// - btrfs/f2fs/xfs 检测到已有文件系统时默认拒绝写入，须 -f 覆盖
///   （man mkfs.btrfs / mkfs.f2fs / mkfs.xfs）
/// - `-f` 语义各工具不类推：mkntfs 的 -f 是 fast 格式化而非 force（force 为 -F），
///   故 ntfs 不传 -f，让工具自身拒绝已有文件系统（man mkntfs）
pub(crate) struct MkfsTool {
    pub(crate) program: String,
    pub(crate) force: bool,
    /// `mke2fs -t <ext_type>`；只有 ext2/3/4 携带
    pub(crate) ext_type: Option<MkfsFs>,
}

pub(crate) fn mkfs_tool(fstype: MkfsFs) -> Result<MkfsTool, FsError> {
    let (program, force, ext_type) = match fstype {
        // mkfs.extN 只是 mke2fs 等价 -t extN 的符号链接（man mke2fs），直调 mke2fs 免依赖链接布局
        MkfsFs::Ext2 | MkfsFs::Ext3 | MkfsFs::Ext4 => ("mke2fs".to_string(), false, Some(fstype)),
        MkfsFs::Vfat | MkfsFs::Exfat | MkfsFs::Ntfs => (format!("mkfs.{}", fstype.as_str()), false, None),
        MkfsFs::Xfs | MkfsFs::Btrfs | MkfsFs::F2fs => (format!("mkfs.{}", fstype.as_str()), true, None),
        MkfsFs::Swap => ("mkswap".to_string(), false, None),
        MkfsFs::Lvm2Pv => {
            return Err(FsError::unsupported(
                "cannot mkfs an LVM2 PV — to (re)create the PV use pvcreate(8), to wipe it use wipefs(8)",
            ));
        }
    };
    Ok(MkfsTool { program, force, ext_type })
}

/// 命令层"先问再做"的入口：这个 FS 类型**具备被创建的能力**吗——类型有没有接线、
/// 对应的外部工具在不在 PATH 且可执行。两问都必须在这里答完
///
/// 必须先问再开事务：拒绝的语义是"什么都没写"，而 mkfs 一旦开了事务就先落一条不可回滚
/// 屏障——一个拼错的类型名、或一个没装的工具包，都不该把目标锁在"未收尾"状态里等 `abandon`
pub fn mkfs_capability(fstype: MkfsFs) -> Result<(), FsError> {
    let tool = mkfs_tool(fstype)?;
    find_tool(&tool.program).map(|_| ()).map_err(|e| match e {
        FsError::ToolMissing(_) => FsError::missing(format!(
            "cannot mkfs {fstype}: requires `{}` (package: {}) — not found in PATH",
            tool.program,
            tool_package(&tool.program)
        )),
        e => e,
    })
}

/// mkfs：破坏分区数据，调用方须先取确认；执行前先擦残留签名（见 erase_ranges），
/// 防旧 btrfs/ZFS/RAID 备份超级块残留被 blkid 误认
pub fn mkfs(src: &mut WriteSource, part: u32, fstype: MkfsFs) -> Result<(), FsError> {
    let tool = mkfs_tool(fstype)?;
    // 能力检查必须发生在任何写盘之前：wipe_zero 一旦开始就不可回滚，工具缺失时
    // 目标上"什么都没写"的承诺只能在第一次擦除之前作出
    mkfs_capability(fstype)?;
    // 区间只解析一次：擦除区间与实际建立的那个设备必须同源——分两次读表的话，
    // 两次读数之间表被改动会让"擦哪一段"与"擦的是哪个设备"对不上
    let (part_off, part_len) = scope_byte_range(src, &DeviceScope::Partition(part))?;
    // part_len 上界断言（入口拒绝，替代 erase_ranges 内 `as i64` 的回绕面）：区间表
    // 以 i64 承载"分区尾 − 回退量"的中间负值，≥ 2^63 的长度回绕成负数后裁剪逻辑
    // 静默失效。真实设备远到不了这么大，但入口输入来自盘上表——畸形表配巨型镜像
    // 不该以错误的擦除区间收场
    if part_len >= 1u64 << 63 {
        return Err(FsError::invalid(format!(
            "partition length {part_len} exceeds the supported maximum (2^63 bytes) — refusing"
        )));
    }
    let ss = src.sector_size;
    // 建 loop、定位分区节点、开设备都在写盘之前：这些失败必须停在"确定未写盘"这一侧，
    // 所以屏障不能由调用方在更早的位置落——它要在这条路径内部、紧贴 wipe_zero
    let dev = resolve_scope_device(
        src,
        &DeviceScope::Partition(part),
        part_off,
        part_len,
        ImageLoopPolicy::RefuseAny,
    )?;
    // 擦除一旦开始，前镜像就不再完整，undo 必须被拒；屏障写不下去时一个字节都还没动，
    // 故报 Io（→ Infra），不报 CommandFailed
    src.set_mutation(crate::dev::Mutation::Mkfs);
    src.mark_non_reversible().map_err(|e| {
        FsError::from(io::Error::other(format!(
            "cannot persist the transaction barrier: {e} — nothing was written, the target is unchanged"
        )))
    })?;
    // wipe_zero 一旦开始就存在部分清零：之后的任何失败都不再是"确定未写盘"，
    // 统一压成 CommandFailed（→ Failed），报告带"盘上状态可能已改变"的警示
    wipe_zero(dev.path(), &erase_ranges(part_len, ss)).map_err(|e| {
        FsError::CommandFailed(format!("mkfs: signature wipe started but failed, the target may be partially erased: {e}"))
    })?;
    let mut args: Vec<&str> = Vec::new();
    if tool.force {
        args.push("-f");
    }
    if let Some(t) = tool.ext_type {
        args.push("-t");
        args.push(t.as_str());
    }
    args.push(dev.path());
    // 工具已拿到设备：启动失败与非零退出同样无法断言写没写、写了多少
    let out = run(&tool.program, &args).map_err(|e| {
        FsError::CommandFailed(format!("mkfs: {} was about to run but failed to execute, the target may already be partially erased: {e}", tool.program))
    })?;
    if !out.status.success() {
        return Err(FsError::command(&tool.program, &out));
    }
    crate::outcome::note_fs_writes_not_journaled();
    Ok(())
}

/// btrfs 多设备拒绝：主 superblock @分区起点+0x10000 的 num_devices 字段（偏移 0x88，
/// u64 LE，内核 fs/btrfs ctree.h 字段序）。>1 时 resize/max 按 devid 作用于所映射的
/// 单个 member，"分区扩满即 FS 扩满"前提不成立，直接拒绝，多设备布局交用户手动处理。
/// 离线（resize_fs_in / shrink_fs）与在线路径（online::fs_grow / 在线 shrink）共用同一道防线。
/// `part_len` 是该分区的字节长度：读 superblock 前先校验它装得下 superblock 头
/// （≥ 0x10090），不足即拒绝——分区之外读到的是邻居的字节，比"读失败"更危险
pub(crate) fn refuse_btrfs_multi_device_at(src: &FileSource, off: u64, part_len: u64) -> Result<(), FsError> {
    const SB_NEED: u64 = 0x10090; // 0x10000 基址 + 0x90 头内最大字段偏移（num_devices @0x88 是 u64）
    if part_len < SB_NEED {
        return Err(FsError::unsupported(
            "partition is smaller than a btrfs superblock — the identified btrfs header does not fit this partition; refusing",
        ));
    }
    let mut raw = [0u8; 8];
    src.read_at(off + 0x10000 + 0x88, &mut raw).map_err(FsError::from)?;
    if u64::from_le_bytes(raw) > 1 {
        return Err(FsError::unsupported(
            "btrfs filesystem spans multiple devices (num_devices > 1) — resize manually per btrfs-filesystem(8)",
        ));
    }
    Ok(())
}