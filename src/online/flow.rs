//! 在线 resize 的编排：grow 与 shrink 两条路径。

use super::*;
use crate::dev::FileSource;
use crate::fsops::run;

pub fn resize_online(
    mountpoint: &Path,
    decide: impl FnOnce(&FileSource) -> Result<Option<u64>, Fail>,
    allow_partition_grow: bool,
) -> Outcome {
    // 解析与 FS 识别都是只读的，且先于 part_resize/sfdisk 的首次写盘 → Infra
    let t = match resolve_target(mountpoint) {
        Ok(t) => t,
        Err(e) => return Outcome::infra(e.to_string()),
    };
    // 独占权必须在 FS 步之前取得：本函数既可能写 FS（fs_grow）也可能写表
    // （part_resize），等到写表才取锁会让前面的 FS 写入落在锁外
    let (_owned, locked_ident) = match lock_disk(&t) {
        Ok(v) => v,
        Err(o) => return o,
    };
    // 同 resize_pv：锁下重取 sysfs 快照（理由见彼处）。mountpoint 在窗口内被重挂载
    // 会解析到别的分区——两次解析目标不一致即拒绝，绝不在 A 的锁下操作 B
    let fresh = match resolve_target(mountpoint) {
        Ok(t) => t,
        Err(e) => return Outcome::infra(e.to_string()),
    };
    // 先比身份再比路径串：身份是锁的载体，路径串只是它的一个投影
    if let Err(o) = verify_lock_identity(&locked_ident, &fresh) {
        return o;
    }
    if fresh.part_dev != t.part_dev {
        return Outcome::infra(format!(
            "mountpoint {} changed target during locking ({} -> {}) — retry",
            mountpoint.display(),
            t.part_dev.display(),
            fresh.part_dev.display()
        ));
    }
    let t = fresh;
    // 现场闸口：必须在 FS 步（fs_grow）与写表（sfdisk）之前
    if let Err(o) = refuse_if_scene_active(&t) {
        return o;
    }
    // 活动 swap 的锁下复核（命令层那次是锁前粗查，理由同 resize_pv）：本路径的前提
    // 是"挂载中"，挂载本身放行，唯独 swap 激活不可——swapon 不守本工具的锁
    let Some(dn) = t.disk_dev.file_name().map(|s| s.to_string_lossy().into_owned()) else {
        return Outcome::refused(format!("cannot derive disk name from {}", t.disk_dev.display()));
    };
    match crate::online::swap_active(&dn, t.pno) {
        Ok(true) => {
            return Outcome::refused(format!("partition {} became active swap during locking — run swapoff first", t.pno));
        }
        Ok(false) => {}
        Err(e) => return Outcome::infra(format!("cannot read /proc/swaps: {e}")),
    }
    let fstype = match fstype_of(&t) {
        Ok(f) => f,
        Err(e) => return Outcome::infra(e.to_string()),
    };
    // 目标长度在锁下解析：请求的锚定值（`+N`/`+N%` 的当前尺寸、`grow` 的右侧空闲）
    // 必须取自独占权之后的盘上现状，锁前快照在窗口内可能已作废。只读打开整盘，
    // 解析失败按 Fail 的出口语义直接折叠
    let disk = match FileSource::open_read_only(&t.disk_dev) {
        Ok(s) => s,
        Err(e) => return Outcome::infra(format!("open failed: {e}")),
    };
    let size = match decide(&disk) {
        Ok(v) => v,
        Err(f) => return f.into_outcome(),
    };
    let online_shrink = fstype == Some(crate::fsid::FsKind::Btrfs);
    if size.is_some_and(|s| s < t.part_len_bytes) {
        // xfs 离线同样不可缩（shrink_fs 兜底拒绝），单独说明以免用户改走离线路径
        if fstype == Some(crate::fsid::FsKind::Xfs) {
            return Outcome::refused(
                "xfs does not support shrinking (online or offline); only growing is possible".to_string(),
            );
        }
        if !online_shrink {
            return Outcome::refused(format!(
                "{} cannot shrink online (only btrfs can); unmount and use the offline path where supported",
                crate::fsid::fs_name(fstype)
            ));
        }
        // 本入口（resizefs --online）不改分区表，而缩容必须让分区跟着 FS 一起缩
        // （下方 shrink 臂的 part_resize 会写表）——承诺优先于能力，哪怕 btrfs 缩得动。
        // 与上面的拒绝同层：都在任何 FS 写入与写表之前
        if !allow_partition_grow {
            return Outcome::refused(
                "resizefs --online does not change the partition table — shrinking the filesystem \
                 requires shrinking the partition with it; use `diskedit resize` instead"
                    .to_string(),
            );
        }
    }
    // FS 步未完成 → 一条 Pending（补救命令与 fs_grow 同源）
    let fs_pending = |detail: String| {
        Pending::new(t.pno, PendingKind::Fs, detail, fs_grow_hint(&t, fstype))
    };
    // fs_grow 的三态映射（唯一处）：无在线工具 = 成因在请求（换离线路径）⇒ refused；
    // spawn 失败 = 什么都没动（infra）；非零退出 = 工具可能改了一半（failed）。
    // 适用前提：本分支没有更早的写盘步骤
    let fs_grow_outcome = |r: Result<(), FsGrowError>| match r {
        Ok(()) => Outcome::applied_with(Vec::new()),
        Err(FsGrowError::NoTool(m)) => Outcome::refused(m),
        Err(FsGrowError::Spawn(err)) => Outcome::infra(err.to_string()),
        Err(FsGrowError::Exit(m)) => Outcome::failed(m),
    };

    // 请求 == 现分区尺寸与"未指定尺寸"同性质：分区不动、无其他写盘步骤，合并同一臂
    let size = match size {
        Some(b) if b == t.part_len_bytes => None,
        other => other,
    };
    match size {
        // 扩满现分区：分区不动，FS 工具直接吃满（内核视图无需变更）
        None => {
            // FS 工具以 /dev 节点传递的内核尺寸为准：内核视图过期时（上一次 partx/BLKPG
            // 失败、或表已缩而内核仍持旧大尺寸）grow 会空跑到旧尺寸、shrink 会把 FS 扩过
            // 盘上分区末端。盘上表长度与内核不一致即在任何 FS 写入之前拒绝
            let disk_len = match crate::gpt_policy::partition_bytes(&disk, t.pno) {
                Ok((_, len)) => len,
                Err(f) => return f.into_outcome(),
            };
            if disk_len != t.part_len_bytes {
                eprintln!(
                    "warning: the kernel reports partition {} as {} bytes but the on-disk table says {}",
                    t.pno, t.part_len_bytes, disk_len
                );
                let mut o = Outcome::applied_with(vec![fs_pending(format!(
                    "the kernel partition view is stale (kernel {} bytes, table {} bytes) — refresh it \
                     (partprobe / partx) and re-run so the filesystem grows to the on-disk size",
                    t.part_len_bytes, disk_len
                ))]);
                o.mark_kernel_stale();
                return o;
            }
            if let Err(e) = refuse_btrfs_multi_device(&t, fstype) {
                return Fail::from(e).into_outcome();
            }
            fs_grow_outcome(fs_grow(&t, fstype))
        }
        Some(bytes) => {
            // resizefs --online 只扩文件系统，不改分区表：越界请求在任何写盘之前拒绝，
            // 分区几何的协调由 `resize` 负责
            if bytes > t.part_len_bytes && !allow_partition_grow {
                return Outcome::refused(format!(
                    "resizefs --online grows the filesystem inside the current partition ({} bytes) — \
                     enlarge the partition with `diskedit resize` first",
                    t.part_len_bytes
                ));
            }
            if let Err(f) = check_new_range(&t, bytes, &disk) {
                return f.into_outcome();
            }
            if bytes > t.part_len_bytes {
                // grow：先自查（内核 EBUSY 兜底），sfdisk 写表 → partx 同步 → FS 工具。
                // 无在线工具的判定必须在写表前——fs_grow 对无工具情形不启动任何进程，
                // 借它把拒绝挡在写表之前，不留下"表已扩、FS 未扩"
                match fs_grow_cmd(&t, fstype) {
                    None => return fs_grow_outcome(fs_grow(&t, fstype)),
                    // 映射里有工具不等于 PATH 里有：缺二进制同样必须在写表前拒绝，
                    // 否则表已扩才发现，只剩 PARTIAL 加手工补救
                    Some((prog, _)) => {
                        if let Err(e) = crate::fsops::require_tool(&prog) {
                            return fs_grow_outcome(Err(FsGrowError::Spawn(e)));
                        }
                    }
                }
                if let Err(e) = refuse_btrfs_multi_device(&t, fstype) {
                    return Fail::from(e).into_outcome();
                }
                if let Err(e) = part_resize(&t, bytes) {
                    // 表已写、内核同步失败：FS 扩步也因此没做，两条事实一并报
                    let detail = e.detail().to_string();
                    if matches!(&e, PartResizeError::Partial(_)) {
                        return e.into_outcome_with(vec![fs_pending(format!(
                            "partition resized but FS grow skipped: {detail}"
                        ))]);
                    }
                    return e.into_outcome();
                }
                if let Err(e) = verify_part_size(&t, bytes) {
                    // 两个正交事实同时成立：内核视图过期 + FS 步未做
                    eprintln!("warning: partition table updated but kernel reports a different size: {e}");
                    let mut o = Outcome::applied_with(vec![fs_pending(format!("partition resized but FS grow skipped: {e}"))]);
                    o.mark_kernel_stale();
                    return o;
                }
                match fs_grow(&t, fstype) {
                    Ok(()) => Outcome::applied_with(Vec::new()),
                    // 分区已扩：无论 spawn 失败还是非零退出，FS 步都是一条未满足后置条件
                    Err(e) => Outcome::applied_with(vec![fs_pending(format!("partition resized but FS grow skipped: {}", e.detail()))]),
                }
            } else {
                // shrink（仅 btrfs；--online 入口已在上方拒绝，能到这里说明本入口
                // 的职责包含分区几何）：FS 先缩（内核校验占用与 256MiB 下限），持久化缩分区
                if let Err(e) = refuse_btrfs_multi_device(&t, fstype) {
                    return Fail::from(e).into_outcome();
                }
                let mnt = t.mnt.to_string_lossy().into_owned();
                match run("btrfs", &["filesystem", "resize", &bytes.to_string(), &mnt]) {
                    // ToolMissing / spawn 失败发生在本次首次写盘之前 → Infra；
                    // 启动后拿不到退出状态则可能已改 FS → Failed（映射见 Fail::from）
                    Err(e) => return Fail::from(e).into_outcome(),
                    Ok(out) if !out.status.success() => {
                        return Outcome::failed(format!("btrfs shrink failed: {}", String::from_utf8_lossy(&out.stderr)));
                    }
                    Ok(_) => {}
                }
                // FS 已缩：此后任何失败都不再是"未动盘"，表未写的情形只能报 Failed
                match part_resize(&t, bytes) {
                    Ok(()) => {}
                    // 表已写、仅内核同步失败：FS 已缩 + 内核视图过期，两条正交事实都记上
                    Err(e @ PartResizeError::Partial(_)) => return e.into_outcome(),
                    // 表未写或无法断言：FS 已缩，本次已改变盘上内容 → 不得报"未动盘"
                    Err(e) => {
                        return Outcome::failed(format!(
                            "btrfs shrunk but partition resize skipped: {}",
                            e.detail()
                        ));
                    }
                }
                match verify_part_size(&t, bytes) {
                    Ok(()) => Outcome::applied_with(Vec::new()),
                    Err(e) => {
                        eprintln!("warning: partition table shrunk but kernel reports a different size: {e}");
                        Outcome::applied_stale_kernel()
                    }
                }
            }
        }
    }
}