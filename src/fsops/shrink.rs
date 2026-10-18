//! shrink：FS 先缩的调用链与缩后读回复核。

use super::*;

/// 缩容后的**实际落点**核对结论。请求值只说明"我们要了多大"——工具的取整粒度各不相同
/// （ext 按块、ntfs 按簇、btrfs 按 sectorsize），真正决定"分区能不能缩到请求尺寸"的是
/// 文件系统实际落在哪儿，故工具成功返回后必须回读一次
pub enum ShrinkVerify {
    /// 回读到的实际尺寸（实测三种工具的取整方向都向下，故它 ≤ 请求值）
    Within(u64),
    /// 回读不出来（工具缺失、输出解析不了、缩容把卷标成待检查而工具拒答）：缩容**可能已经
    /// 完成**，只是落点无法证实。这是"结果无法验证"，不是"缩容失败"——故它随成功结果
    /// 浮出成一条待办，而不上抛成错误
    Unverifiable { why: String, hint: String },
}

impl ShrinkVerify {
    /// 缩容成功那一行：请求值与实际落点都给（工具的取整差异因此可见）。
    /// 回读不出来时没有落点可报，这一类改由 [`Self::pending`] 浮出
    pub fn ok_line(&self, requested: u64) -> Option<String> {
        match self {
            Self::Within(actual) => Some(format!("fs shrunk to {actual} bytes (requested {requested})")),
            Self::Unverifiable { .. } => None,
        }
    }

    /// 回读不出来 → 一条后置条件待办
    pub fn pending(self, part: u32) -> Option<crate::outcome::Pending> {
        match self {
            Self::Within(_) => None,
            Self::Unverifiable { why, hint } => Some(crate::outcome::Pending::new(
                part,
                crate::outcome::PendingKind::Other("fs shrink"),
                why,
                hint,
            )),
        }
    }
}

/// 缩容这一动的结果：核对结论 + 挂载点没卸掉（只有 btrfs 路径会产生）
pub struct ShrinkOutcome {
    pub verify: ShrinkVerify,
    pub leak: Option<MountLeak>,
}

/// FS 缩容到指定字节数（先于分区边界收缩执行）。契约是 `new_bytes` ≤ 旧**分区**尺寸
/// 而非"≤ 当前 FS 大小"：FS 小于分区时（曾扩过分区或中断过的现场）此调用实为扩容，
/// 方向良性——FS 工具把 FS 对到目标尺寸即可，不构成失败
pub fn shrink_fs(src: &FileSource, part: u32, fstype: Option<FsKind>, new_bytes: u64) -> Result<ShrinkOutcome, FsError> {
    match fstype {
        Some(FsKind::Ext) => {
            // 纯参数校验必须在任何写盘之前：e2fsck -fp 本身会写盘，把它排在后面
            // 会让一次参数错误先改文件系统、再以 Refused 退出
            if !new_bytes.is_multiple_of(512) {
                return Err(FsError::invalid("shrink size must be a multiple of 512"));
            }
            let verify = with_partition_device(src, part, |dev| {
                let out = run("e2fsck", &["-fp", dev])?;
                check_e2fsck_for_resize(out.status.code().unwrap_or(-1))?;
                // resize2fs 裸数字单位是"文件系统块数"而非字节（man resize2fs）；
                // 's' 后缀 = 512 字节扇区。分区尺寸必为 sector_size(≥512) 整数倍
                let sectors = format!("{}s", new_bytes / 512);
                let out = run("resize2fs", &[dev, &sectors]).map_err(|e| tool_write_maybe_started("resize2fs", dev, e))?;
                if !out.status.success() {
                    return Err(FsError::CommandFailed(format!(
                        "resize2fs shrink failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                verify_shrink(FsKind::Ext, dev, new_bytes, "resize2fs")
            })?;
            Ok(ShrinkOutcome { verify, leak: None })
        }
        Some(FsKind::Ntfs) => {
            let verify = with_partition_device(src, part, |dev| {
                // -s 无后缀 = 字节，k/M/G = 10³/10⁶/10⁹（man ntfsresize OPTIONS）；本工具只传裸字节
                let out = run("ntfsresize", &["-f", "-f", "-s", &new_bytes.to_string(), dev])?;
                if !out.status.success() {
                    return Err(FsError::CommandFailed(format!(
                        "ntfsresize shrink failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                verify_shrink(FsKind::Ntfs, dev, new_bytes, "ntfsresize")
            })?;
            Ok(ShrinkOutcome { verify, leak: None })
        }
        Some(FsKind::Btrfs) => {
            let (off, len) = partition_byte_range(src, part)?;
            refuse_btrfs_multi_device_at(src, off, len)?;
            // 核对结论由闭包写进外层：回读失败（工具没落在请求范围内）要从 `with_mount`
            // 里上抛，那条路正是"核心失败 + 挂载点没卸掉"的既有处置（告警随错误一起出），
            // 把 leak 留在闭包外会让它被错误出口吞掉
            let mut verify = None;
            // 多设备闸看的区间与真正映射的设备必须同源：再解析一次表会在两读之间留窗口
            let leak = with_scope_device_ranged(src, &DeviceScope::Partition(part), off, len, |dev| {
                with_mount(dev, |mnt| {
                    // 裸数字 = 绝对字节数、必须挂载态（man btrfs-filesystem resize）
                    let out = run("btrfs", &["filesystem", "resize", &new_bytes.to_string(), mnt])?;
                    if !out.status.success() {
                        return Err(FsError::CommandFailed(format!(
                            "btrfs shrink failed: {} (minimum size via `btrfs inspect-internal min-dev-size` on a mount)",
                            String::from_utf8_lossy(&out.stderr).trim()
                        )));
                    }
                    verify = Some(verify_shrink(FsKind::Btrfs, dev, new_bytes, "btrfs filesystem resize")?);
                    Ok(())
                })
            })?;
            let verify = verify.ok_or_else(|| {
                FsError::Io(io::Error::other("btrfs shrink did not run (internal invariant broken)"))
            })?;
            Ok(ShrinkOutcome { verify, leak })
        }
        other => Err(FsError::unsupported(format!("fs {} cannot shrink", fs_name(other)))),
    }
}

/// 缩容工具成功返回后的核对：回读实际落点，与请求值比对。三种结局对应三条出路——
/// 落点在请求范围内 = 正常成功；落点超出请求 = 工具行为不符合预期（分区不能按请求缩进去）；
/// 回读不出来 = 结果无法验证（工具可能已经缩好了，故不上抛成失败）
pub(crate) fn verify_shrink(kind: FsKind, dev: &str, requested: u64, tool: &str) -> Result<ShrinkVerify, FsError> {
    classify_shrink(read_fs_bytes(kind, dev), requested, tool, &size_read_back_hint(kind))
}

/// 回读结果 → 核对结论。与外部调用分开，判定本身才能单测
pub(crate) fn classify_shrink(read: io::Result<u64>, requested: u64, tool: &str, hint: &str) -> Result<ShrinkVerify, FsError> {
    match read {
        Ok(actual) if actual <= requested => Ok(ShrinkVerify::Within(actual)),
        Ok(actual) => Err(FsError::CommandFailed(format!(
            "{tool} reported success but left the filesystem at {actual} bytes, more than the requested \
             {requested} — the partition is not shrunk to a size the filesystem does not fit in"
        ))),
        Err(e) => Ok(ShrinkVerify::Unverifiable {
            why: format!(
                "{tool} reported success but the filesystem size could not be read back: {e} — \
                 the shrink may have been applied; verify the new size before relying on it"
            ),
            hint: hint.to_string(),
        }),
    }
}

/// 回读缩容后**实际**文件系统尺寸。三者都是只读查询：
/// - ext：`dumpe2fs -h`（man dumpe2fs：-h 只打印超级块）的 `Block count:` × `Block size:`
/// - ntfs：`ntfsresize --info` 的 `Current volume size:`（--info 是查询，不调整大小，故只读）。
///   `-f` 是必需的：真实调整大小会把卷标成"待检查"，不带 `-f` 的 `--info` 会以该理由拒答
/// - btrfs：`btrfs inspect-internal dump-super -f`（-f = --full）的 `total_bytes`。它读的是
///   超级块；该命令除签名验证外不做全面合理性检查，故只用来报尺寸、不用来判断卷是否健康
///
/// 输出格式非稳定机器接口（随工具版本可能变化），解析失败即报错、不猜默认值
pub(crate) fn read_fs_bytes(kind: FsKind, dev: &str) -> io::Result<u64> {
    match kind {
        FsKind::Ext => {
            let text = run_text("dumpe2fs", &["-h", dev])?;
            let blocks = parse_num_lead(&text, "Block count:")?;
            let block_size = parse_num_lead(&text, "Block size:")?;
            blocks.checked_mul(block_size).ok_or_else(|| {
                io::Error::other(format!("dumpe2fs block count × block size overflows ({blocks} × {block_size})"))
            })
        }
        FsKind::Ntfs => parse_num_lead(&run_text("ntfsresize", &["-f", "--info", dev])?, "Current volume size:"),
        FsKind::Btrfs => parse_num_lead(
            &run_text("btrfs", &["inspect-internal", "dump-super", "-f", dev])?,
            "total_bytes",
        ),
        other => Err(io::Error::other(format!("no size read-back wired for {}", fs_name(Some(other))))),
    }
}

/// 回读命令的写法（待办里给用户的补救信息）。设备路径留给用户填：镜像目标上的
/// 分区节点是临时 loop，写进待办会指向一个随命令结束即消失的路径
pub(crate) fn size_read_back_hint(kind: FsKind) -> String {
    match kind {
        FsKind::Ext => "dumpe2fs -h <partition device>  (Block count × Block size)".into(),
        FsKind::Ntfs => "ntfsresize -f --info <partition device>".into(),
        FsKind::Btrfs => "btrfs inspect-internal dump-super -f <partition device>".into(),
        _ => String::new(),
    }
}

/// 跑一个只读查询工具并取回 stdout+stderr 的全文本：查询的成败由调用方按各自的输出格式
/// 判定（它们要解析的字段不同），此处只保证"工具跑起来了且成功退出"
pub(crate) fn run_text(tool: &str, args: &[&str]) -> io::Result<String> {
    let out = run(tool, args).map_err(io::Error::from)?;
    if !out.status.success() {
        return Err(io::Error::from(FsError::command(tool, &out)));
    }
    Ok(format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}