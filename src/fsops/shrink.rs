//! shrink：FS 先缩的调用链与缩后读回复核。

use super::*;

/// 缩容这一动的结果。两个变体是两条互斥的通道：落点已回读证实，或回读不出、结果未证实。
/// "未证实"只出现在 [`Self::Unverified`] 上，与已证实的尺寸互斥，不藏在成功变体里
pub enum ShrinkRunOutcome {
    /// 回读证实的实际落点；`leak` 是没卸掉的挂载点（只有 btrfs 路径会产生）
    Verified { actual: u64, leak: Option<MountLeak> },
    /// 工具成功返回但落点回读不出：缩容可能已应用，尺寸未知
    Unverified { why: String, hint: String, leak: Option<MountLeak> },
}

impl ShrinkRunOutcome {
    /// 缩容成功那一行：请求值与实际落点都给（工具的取整差异因此可见）。
    /// 未证实时没有落点可报
    pub fn ok_line(&self, requested: u64) -> Option<String> {
        match self {
            Self::Verified { actual, .. } => Some(format!("fs shrunk to {actual} bytes (requested {requested})")),
            Self::Unverified { .. } => None,
        }
    }

    /// 临时挂载点是否没能卸掉。为真时分区仍被内核视为挂载中：搬移或改表会与活动挂载
    /// 的元数据互相覆盖，调用方必须停在缩容这一步
    pub fn release_failed(&self) -> bool {
        match self {
            Self::Verified { leak, .. } | Self::Unverified { leak, .. } => leak.is_some(),
        }
    }

    /// 随结果浮出的待办：未证实的原因与补救、以及没卸掉的挂载点。两者都不改写
    /// "缩容已经执行"这一事实，只作为后置条件呈现
    pub fn pending(self, part: u32) -> Vec<crate::outcome::Pending> {
        let mut pending = Vec::new();
        let leak = match self {
            Self::Verified { leak, .. } => leak,
            Self::Unverified { why, hint, leak } => {
                pending.push(crate::outcome::Pending::new(
                    part,
                    crate::outcome::PendingKind::Other("fs shrink"),
                    why,
                    hint,
                ));
                leak
            }
        };
        if let Some(leak) = leak {
            pending.push(leak.into_pending(part));
        }
        pending
    }
}

/// 核对结论 → 结果的两种变体。转换只在此一处，调用方不自行解读 [`SizeVerify`]
pub(crate) fn shrink_outcome(verify: SizeVerify, leak: Option<MountLeak>) -> ShrinkRunOutcome {
    match verify {
        SizeVerify::Confirmed(actual) => ShrinkRunOutcome::Verified { actual, leak },
        SizeVerify::Unverifiable { why, hint } => ShrinkRunOutcome::Unverified { why, hint, leak },
    }
}

/// FS 缩容到指定字节数（先于分区边界收缩执行）。要求是 `new_bytes` ≤ 旧**分区**尺寸
/// 而非"≤ 当前 FS 大小"：FS 小于分区时（曾扩过分区或中断过的现场）此调用实为扩容，
/// 方向良性——FS 工具把 FS 对到目标尺寸即可，不构成失败
pub fn shrink_fs(src: &FileSource, part: u32, fstype: Option<ContentKind>, new_bytes: u64) -> Result<ShrinkRunOutcome, FsError> {
    let r = shrink_fs_step(src, part, fstype, new_bytes);
    // 成功才提示：失败路径已有的"盘可能已改变"警示不额外承担回滚范围的说明
    if r.is_ok() {
        crate::outcome::note_fs_writes_not_journaled();
    }
    r
}

fn shrink_fs_step(src: &FileSource, part: u32, fstype: Option<ContentKind>, new_bytes: u64) -> Result<ShrinkRunOutcome, FsError> {
    match fstype {
        Some(ContentKind::Filesystem(FsKind::Ext)) => {
            // 纯参数校验必须在任何写盘之前：e2fsck -fp 本身会写盘，把它排在后面
            // 会让一次参数错误先改文件系统、再以 Refused 退出
            if !new_bytes.is_multiple_of(512) {
                return Err(FsError::invalid("shrink size must be a multiple of 512"));
            }
            let verify = with_partition_device(src, part, |dev| {
                let out = e2fsck_repair(dev)?;
                check_e2fsck_status_for_resize(&out.status)?;
                // resize2fs 裸数字单位是"文件系统块数"而非字节（man resize2fs）；
                // 's' 后缀 = 512 字节扇区。分区尺寸必为 sector_size(≥512) 整数倍
                let sectors = format!("{}s", new_bytes / 512);
                let out = resize2fs_to(dev, &sectors).map_err(|e| tool_write_maybe_started(Tool::Resize2fs, dev, e))?;
                if !out.status.success() {
                    return Err(FsError::CommandFailed(format!(
                        "resize2fs shrink failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                verify_shrink(FsKind::Ext, dev, new_bytes, "resize2fs")
            })?;
            Ok(shrink_outcome(verify, None))
        }
        Some(ContentKind::Filesystem(FsKind::Ntfs)) => {
            let verify = with_partition_device(src, part, |dev| {
                // -s 无后缀 = 字节，k/M/G = 10³/10⁶/10⁹（man ntfsresize OPTIONS）；本工具只传裸字节
                let out = ntfsresize_shrink_to(dev, &new_bytes.to_string())?;
                if !out.status.success() {
                    return Err(FsError::CommandFailed(format!(
                        "ntfsresize shrink failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
                verify_shrink(FsKind::Ntfs, dev, new_bytes, "ntfsresize")
            })?;
            Ok(shrink_outcome(verify, None))
        }
        Some(ContentKind::Filesystem(FsKind::Btrfs)) => {
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
                    let out = btrfs_resize_to(mnt, &new_bytes.to_string())
                        .map_err(|e| tool_write_maybe_started(Tool::Btrfs, mnt, e))?;
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
                // 闭包必须执行一次：没拿到核对结论说明调用链内部不变量破裂，而不是工具失败
                FsError::CommandFailed(
                    "btrfs shrink did not run (internal invariant broken) — the filesystem may have been resized".to_string(),
                )
            })?;
            Ok(shrink_outcome(verify, leak))
        }
        #[cfg(target_os = "linux")]
        Some(ContentKind::Lvm2Pv) => {
            // PV 缩容只缩 LVM 元数据的物理范围：新末端之后没有已分配 extent 由
            // 调用方在写盘前证明（[`pv_shrink_precheck`]），分区表随后提交
            let verify = with_partition_device(src, part, |dev| {
                crate::lvm::pv_resize_to(dev, new_bytes).map_err(pv_tool_error)?;
                // pvresize 改元数据会触发 udev 的 LVM 规则；PV 重新出现时该规则执行
                // vgchange -aay，把 dm 栈压回这块临时 loop。表项若缩到栈之外，栈就映射到
                // 分区之外，因此提交之前复核持有者，出现即停在这一步（表项未动）
                drop(crate::fsops::udev_settle());
                match crate::dev::collect_node(dev) {
                    Ok(facts) if facts.holders.is_empty() => {}
                    Ok(facts) => {
                        let holders = facts
                            .holders
                            .iter()
                            .map(|h| h.holders.join(" -> "))
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(FsError::CommandFailed(format!(
                            "the VG was activated on {dev} after pvresize (holders: {holders}); the partition table \
                             was left unchanged — deactivate it and stop the re-activation (vgchange -an <vg>; \
                             lvchange -ky <vg/lv>), then retry"
                        )));
                    }
                    // 读不出的持有者不能当成"没有"：表项在这之后就提交，宁可停在这里
                    Err(e) => {
                        return Err(FsError::CommandFailed(format!(
                            "cannot re-check the holders of {dev} after pvresize: {e} — the partition table was \
                             left unchanged; inspect the device and retry"
                        )));
                    }
                }
                verify_pv_shrink(dev, new_bytes)
            })?;
            Ok(shrink_outcome(verify, None))
        }
        #[cfg(not(target_os = "linux"))]
        Some(ContentKind::Lvm2Pv) => Err(FsError::platform(
            "shrinking an LVM PV runs pvresize and is only available on Linux",
        )),
        other => Err(FsError::unsupported(format!(
            "fs {} cannot shrink",
            content_name(other.unwrap_or(ContentKind::Unknown))
        ))),
    }
}

/// pvresize 的执行错误 → FsError：缺工具与命令失败分开，前者装包即可
#[cfg(target_os = "linux")]
fn pv_tool_error(e: crate::lvm::LvmError) -> FsError {
    match e {
        crate::lvm::LvmError::ToolMissing(m) => FsError::missing(m),
        e => FsError::CommandFailed(format!("pvresize --setphysicalvolumesize failed: {e}")),
    }
}

/// pvresize 之后的核对：回读 `pv_size`（PV 可用尺寸，extent 对齐使其可略小于新末端），
/// 不大于请求值即缩到位。超出请求值是工具行为不符预期——分区不能缩到 PV 之外；
/// 回读不出来是未证实，不上抛成失败
#[cfg(target_os = "linux")]
fn verify_pv_shrink(dev: &str, requested: u64) -> Result<SizeVerify, FsError> {
    match crate::lvm::pv_size_report(dev) {
        Ok(r) if r.pv_size <= requested => Ok(SizeVerify::Confirmed(r.pv_size)),
        Ok(r) => Err(FsError::CommandFailed(format!(
            "pvresize reported success but left the PV at {} bytes, more than the requested {requested} — \
             the partition is not shrunk to a size the PV does not fit in",
            r.pv_size
        ))),
        Err(e) => Ok(SizeVerify::Unverifiable {
            why: format!(
                "pvresize reported success but the PV size could not be read back: {e} — \
                 the shrink may have been applied; verify the new size before relying on it"
            ),
            hint: "pvs --noheadings --nosuffix --units b -o pv_size <partition device>".to_string(),
        }),
    }
}

/// PV 缩容的写盘前预检：已分配 extent 的末端不得越过新末端。pvresize 自己也会
/// 拒绝切进已分配 extent，但到那时分区表已经开始收缩；这里在任何写盘之前拒绝。
/// 结论只读，工具缺失归外部能力，其余读不出归"请求与现状对不上"
#[cfg(target_os = "linux")]
pub fn pv_shrink_precheck(src: &FileSource, part: u32, new_bytes: u64) -> Result<(), FsError> {
    with_partition_device(src, part, |dev| {
        let end = crate::lvm::pv_allocated_end_bytes(dev).map_err(|e| match e {
            crate::lvm::LvmError::ToolMissing(m) => FsError::missing(m),
            e => FsError::invalid(format!(
                "cannot bound the allocated extents on {dev} before shrinking the PV: {e}"
            )),
        })?;
        if end > new_bytes {
            return Err(FsError::invalid(format!(
                "allocated extents on {dev} end at {end} bytes, beyond the requested new end {new_bytes} — \
                 move that data off the tail first (pvmove) or pick a larger size"
            )));
        }
        Ok(())
    })
}

/// 非 Linux 没有 lvm2 的 pvresize 链：缩 PV 不可执行，按平台不支持拒绝
#[cfg(not(target_os = "linux"))]
pub fn pv_shrink_precheck(_src: &FileSource, _part: u32, _new_bytes: u64) -> Result<(), FsError> {
    Err(FsError::platform(
        "shrinking an LVM PV runs pvresize and is only available on Linux",
    ))
}

/// 缩容**之前**看 FS 现在落在哪儿：中断续跑用它区分"已缩到位"与"从未缩过"，
/// 避免按盲判把同一次缩容再做一遍。
/// `None` 与 [`SizeVerify::Unverifiable`] 同义——回读不出来，无从判断。
pub fn shrink_already_at(src: &FileSource, part: u32, fstype: Option<ContentKind>, requested: u64) -> Option<bool> {
    // PV 的"尺寸"是 pvs 报告的 pv_size：已不大于目标即无需再缩（缩容幂等）
    #[cfg(target_os = "linux")]
    if fstype == Some(ContentKind::Lvm2Pv) {
        return with_partition_device(src, part, |dev| {
            Ok::<Option<bool>, FsError>(crate::lvm::pv_size_report(dev).ok().map(|r| r.pv_size <= requested))
        })
        .ok()?;
    }
    // 不是文件系统的其余内容（swap / LUKS / BitLocker）缩不了，也谈不上"已缩到位"：
    // 与其余不能缩的类型同判，由 shrink_fs 给出拒绝
    let Some(kind) = fstype?.filesystem() else {
        return Some(false);
    };
    match kind {
        FsKind::Ext | FsKind::Ntfs => with_partition_device(src, part, |dev| {
            Ok::<Option<bool>, FsError>(read_fs_bytes(kind, dev).ok().map(|actual| actual <= requested))
        })
        .ok()?,
        // btrfs 的尺寸由超级块给出（`dump-super -f` 直接读设备），不需要挂载；
        // 设备区间与缩容走同一处 scope 解析，避免两读之间换映射
        FsKind::Btrfs => {
            let (off, len) = partition_byte_range(src, part).ok()?;
            let mut verdict = None;
            with_scope_device_ranged(src, &DeviceScope::Partition(part), off, len, |dev| {
                verdict = read_fs_bytes(FsKind::Btrfs, dev).ok().map(|actual| actual <= requested);
                Ok::<(), FsError>(())
            })
            .ok()?;
            verdict
        }
        // 其余类型不能缩容，轮不到这里判断
        _ => Some(false),
    }
}

/// 缩容工具成功返回后的核对：回读实际落点，与请求值比对。三种结局对应三条出路——
/// 落点在请求范围内 = 正常成功；落点超出请求 = 工具行为不符合预期（分区不能按请求缩进去）；
/// 回读不出来 = 结果无法验证（工具可能已经缩好了，故不上抛成失败）
pub(crate) fn verify_shrink(kind: FsKind, dev: &str, requested: u64, tool: &str) -> Result<SizeVerify, FsError> {
    classify_shrink(read_fs_bytes(kind, dev), requested, tool, &size_read_back_hint(kind))
}

/// 回读结果 → 核对结论。与外部调用分开，判定本身才能单测
pub(crate) fn classify_shrink(read: io::Result<u64>, requested: u64, tool: &str, hint: &str) -> Result<SizeVerify, FsError> {
    match read {
        Ok(actual) if actual <= requested => Ok(SizeVerify::Confirmed(actual)),
        Ok(actual) => Err(FsError::CommandFailed(format!(
            "{tool} reported success but left the filesystem at {actual} bytes, more than the requested \
             {requested} — the partition is not shrunk to a size the filesystem does not fit in"
        ))),
        Err(e) => Ok(SizeVerify::Unverifiable {
            why: format!(
                "{tool} reported success but the filesystem size could not be read back: {e} — \
                 the shrink may have been applied; verify the new size before relying on it"
            ),
            hint: hint.to_string(),
        }),
    }
}

/// 回读命令的写法（待办里给用户的补救信息）。设备路径留给用户填：镜像目标上的
/// 分区节点是临时 loop，写进待办会指向一个随命令结束即消失的路径。
///
/// 只有 ext / ntfs / btrfs 有设备级回读；f2fs / xfs / vfat 没有已接线的回读，核对
/// 只能落到挂载后的尺寸报告：三者查询工具的字段随版本变化，不写进提示
pub(crate) fn size_read_back_hint(kind: FsKind) -> String {
    match kind {
        FsKind::Ext => "dumpe2fs -h <partition device>  (Block count × Block size)".into(),
        FsKind::Ntfs => "ntfsresize -f --info <partition device>".into(),
        FsKind::Btrfs => "btrfs inspect-internal dump-super -f <partition device>".into(),
        FsKind::F2fs | FsKind::Xfs | FsKind::Vfat => {
            "mount <partition device> <mountpoint>, then compare the 1-byte total of \
             `df -B1 <mountpoint>` with the size the range was grown to"
                .into()
        }
        _ => String::new(),
    }
}

