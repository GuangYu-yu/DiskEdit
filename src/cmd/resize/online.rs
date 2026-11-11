//! 在线 resize 的旗标拒绝、尺寸归一与调用。

#[cfg(target_os = "linux")]
use super::*;
#[cfg(target_os = "linux")]
use crate::cli::SizeSpec;

/// 在线路径前置守卫：活动 swap 拒绝（run swapoff 后重试）。/proc/swaps 读不出来
/// 不是"没有 swap"——折叠成 false 就是给锁下复核开 fail-open 的口子
#[cfg(target_os = "linux")]
pub(crate) fn refuse_swap_active(dn: &str, part: u32) {
    let active = crate::online::swap_active(dn, part)
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("cannot check active swap for partition {part}: {e}"))));
    if active {
        bail_fail(RunOutcome::refused(format!("partition {part} is active swap — run swapoff first")));
    }
}

/// 块设备在线路径：**表类型无关**（写表经 sfdisk / BLKPG）。目标长度由 `decide` 在
/// **持有目标锁之后**、基于只读打开的整盘解析——`+N`/`+N%` 锚定"当前尺寸"、`grow` 锚定
/// 右侧空闲，锁前的快照在窗口内可能已作废，用它算出的扩量会传导给 pvresize/lvextend。
/// 返回 Some(结果) = 已按在线路径处理完毕；None = 不适用（镜像，或未挂载的非 PV）
/// → 调用方继续离线路径。派生不出盘名即拒绝而非继续：此时无法探测挂载状态，
/// 退到离线路径可能在 FS 挂载中写表
#[cfg(target_os = "linux")]
pub(crate) fn resize_online(
    a: &ResizeRequest,
    src: &FileSource,
    part: u32,
    is_pv: bool,
    decide: impl FnOnce(&FileSource) -> Result<Option<u64>, crate::outcome::RunOutcome>,
) -> Option<crate::outcome::RunOutcome> {
    if !src.is_block {
        return None;
    }
    // 盘名取打开后的规范路径：`/dev/disk/by-id/…`、`/dev/mapper/…` 这类符号链接的文件名
    // 在 /proc/swaps 与 /proc/mounts 里都对不上，会把仍在使用的目标判成未挂载
    let resolved = std::fs::canonicalize(&a.target)
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("cannot resolve {} to a kernel disk name: {e}", a.target))));
    let dn = resolved
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| bail_fail(RunOutcome::refused(format!("cannot derive disk name from {}", a.target))));
    refuse_swap_active(&dn, part);

    // PV：分区层必须先按新尺寸出现在内核里，pvresize 才能吸收；活跃 LV 经 dm 持有分区使
    // BLKRRPART 返回 EBUSY，故走 sfdisk+partx 同步路径
    if is_pv {
        refuse_online_move_flags(a);
        // PV 链在表提交之后才启动：缺工具必须在这里拒绝，不能等 sfdisk 写完表才发现。
        // 在线路径只对块设备开放，分区链直接用现成节点，不需要 losetup
        preflight_lvm_tools(a.grow_lv, false).unwrap_or_else(|e| bail_fail(RunOutcome::from(e)));
        // 锁下、写表之前的 LVM 判据：LV 选择与"新增是否够一个 extent"都要用锁下同时
        // 可信的旧尺寸与将写入的新尺寸。在线路径没有 checkpoint，算出的收尾事实随
        // 独占权一路带进收尾链
        let mut facts: Option<movepart::LvmPostFacts> = None;
        let (o, old_bytes, pv_lock) = crate::online::resize_pv_online(
            &dn,
            part,
            decide,
            |disk, start_bytes, old_bytes, new_bytes| {
                let sel = if a.grow_lv {
                    Some(preflight_lv_selection(disk, part, true, start_bytes, a.lv.as_deref())
                        .map_err(lvm_preflight_fail)?)
                } else {
                    None
                };
                facts = movepart::lvm_post_for(true, sel.as_ref(), old_bytes, new_bytes)?;
                Ok(())
            },
        );
        if !o.is_complete() {
            return Some(o.report_final());
        }
        // 锁贯穿 pvresize/lvextend：它们仍属同一次操作（见 resize_done 的持有说明）。
        // 表已提交，收尾失败必须落 Partial（不承诺"未写盘"）
        return Some(match resize_done(a, None, part, true, true, None, old_bytes, pv_lock, facts) {
            Ok(done) => done,
            Err(cause) => crate::outcome::RunOutcome::partial_with(cause, Vec::new()).report_final(),
        });
    }

    // 非 PV：仅挂载中的分区能在线扩（在线不能搬移，只吃连续空闲）
    // 探测失败按 NoMutation 拒绝而非退离线路径：读不出来 ≠ 没挂载
    let mnt = match crate::online::find_mountpoint(&dn, part) {
        Ok(Some(crate::online::MountView::Here(mnt))) => mnt,
        // 占用判定取全部命名空间：只在别的命名空间里挂着同样是占用，这里既卸不掉也不能改。
        // 那个挂载点不在本命名空间的路径里，必须点明它挂在哪里，否则用户按提示去找会找不到
        Ok(Some(crate::online::MountView::Elsewhere { mount_point, pid })) => bail_fail(RunOutcome::refused(format!(
            "partition {part} of {dn} is mounted at {mount_point} in another mount namespace{} — it cannot be resized from this one; unmount it there or run inside that namespace",
            match pid {
                Some(pid) => format!(" (pid {pid})"),
                None => " (the namespace id could not be determined)".to_string(),
            }
        ))),
        Ok(None) => return None,
        Err(e) => bail_fail(RunOutcome::no_mutation(format!("cannot probe mount state: {e}"))),
    };
    // 两个旗标只挡在线路径：未挂载的盘走离线分支，那里 --allow-move / --chunk 有效
    refuse_online_move_flags(a);
    // 在线 grow 的表写与 FS 扩是一体的，且在线 shrink（btrfs）方向相反——先缩 FS 再写表。
    // 若只写表不动 FS，shrink 会得到"FS 大于分区"的损坏现场，故 --no-fs 不放行在线：
    // 未挂载的分区自然落离线路径，那里 --no-fs 照常生效
    if a.no_fs {
        bail_fail(RunOutcome::refused(
            "--no-fs is not available online — the online path always includes the filesystem step; unmount the partition to skip it".to_string(),
        ));
    }
    let o = crate::online::resize_online_planned(&mnt, decide);
    if o.is_complete() {
        println!("resized online (verify with: diskedit info {})", a.target);
        Some(o)
    } else {
        Some(o.report_final())
    }
}

/// 在线路径不搬移也不收 chunk：两个旗标的承诺在在线入口无从兑现。调用点只在实际
/// 走在线路径时调用——未挂载的非 PV 会落到离线分支，那里两个旗标照常有效
#[cfg(target_os = "linux")]
pub(crate) fn refuse_online_move_flags(a: &ResizeRequest) {
    if a.allow_move {
        bail_fail(RunOutcome::refused(
            "--allow-move is not available online — the online path cannot relocate blocking partitions".to_string(),
        ));
    }
    if a.chunk_given {
        bail_fail(RunOutcome::refused(
            "--chunk has no effect online — the online path does not relocate data".to_string(),
        ));
    }
}

/// 锁下尺寸判定：入参只有 `want` 与锁下重读的 `cur`。缩容走 `resize` 唯一的缩容出口
/// （`refuse_shrink`），等于现状原样返回——下游按"分区不动"收成 nothing to do（0），
/// 大于现状照目标长度放行。
///
/// 属性只为非 Linux 的正式构建不带一个无人调用的条目：判定本身与平台无关，
/// 各平台的测试走同一份实现
#[cfg(any(target_os = "linux", test))]
fn online_size_gate(want: u64, cur: u64) -> Result<Option<u64>, crate::outcome::RunOutcome> {
    if want < cur {
        return Err(super::refuse_shrink());
    }
    Ok(Some(want))
}

/// 在线目标长度的公共骨架：锁下取事实（扇区大小、当前字节、右侧空闲扇区、表自身的长度
/// 上界）→ 共享的 `resolve_size_request` → grow/shrink 分支。grow 零空闲时拒绝：本次
/// grow 不会产生分区扩容（表自身的上界已到顶，或下一分区紧贴右侧），在线路径又不能搬移
/// 挡路分区——与离线路径同口径。FS 是否已填满现分区不是这里的判据：只扩 FS 走
/// `resizefs <mountpoint>`。绝对尺寸同样按该上界拒绝——可用区是表自身的闭区间，越界目标没有
/// 交给外部工具兜底的理由。上界（可用区端点到分区起点的扇区数）由各表自己的几何给出，
/// MBR 无此概念（其容量由 online 层的 `check_new_range` 把关）。缩容一律拒绝（与离线
/// 路径同一出口）：锁前快照通过的方向在锁窗口内可能被第三方改写反转，判据只能用锁下
/// 重读的 `cur`；`want` 的下取整与离线路径同规则。两族表必须
/// 同语义，差异只在"事实怎么取"，故分支只此一份
#[cfg(target_os = "linux")]
pub(crate) fn online_size<'a>(
    size: SizeSpec,
    is_pv: bool,
    facts: impl FnOnce(&FileSource) -> Result<(u64, u64, u64, Option<u64>), RunOutcome> + 'a,
) -> impl FnOnce(&FileSource) -> Result<Option<u64>, RunOutcome> + 'a {
    move |disk: &FileSource| {
        let (ss, cur, free, max_len_sectors) = facts(disk)?;
        let (target, grow) = resolve_size_request(size, cur)?;
        if grow {
            if free == 0 {
                return if is_pv {
                    Err(RunOutcome::refused("no free space to the right — a PV cannot relocate blocking partitions while LVs may be active".to_string()))
                } else {
                    Err(RunOutcome::refused("grow would not extend the partition — no usable free space to its right; the online path cannot relocate blocking partitions (unmount to grow offline, or fill the filesystem with `resizefs <mountpoint>`)".to_string()))
                };
            }
            free.checked_mul(ss).and_then(|f| cur.checked_add(f))
                .map(Some)
                .ok_or_else(|| RunOutcome::no_mutation(format!("grown size overflows ({cur} + {free} sectors × {ss} B)")))
        } else {
            // 目标长度一律下取整到扇区：MBR/GPT 的位置字段是 LBA，非整扇区值到落盘时
            // 仍会被隐式下取整。不取整会让下方上界比较（同样按扇区下取整）放过不足一个
            // 扇区的超出
            let want = match target {
                Some(t) => t / ss * ss,
                None => cur,
            };
            // 下界与两条离线路径同判据：不足一个扇区的分区在写表时会被隐式下取整成 0，
            // 而在线的拒绝点在算完 want 之后、任何写入之前
            if want < ss {
                return Err(RunOutcome::refused(format!(
                    "size {want} < one sector ({ss} bytes) — a partition must keep at least one sector"
                )));
            }
            // 目标终点不得越过可用区上界：与离线路径同一判据（长度按扇区下取整后比较）
            if max_len_sectors.is_some_and(|lim| want / ss > lim) {
                return Err(RunOutcome::refused(format!(
                    "size {want} exceeds the usable range — the partition would end past the last usable block"
                )));
            }
            // 缩容会截短分区与文件系统，`undo` 找不回被截掉的字节：锁下重读的 `cur`
            // 是唯一权威，与离线路径同一出口。该判据在任何 sfdisk/FS 工具启动之前
            online_size_gate(want, cur)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::online_size_gate;
    use crate::outcome::{EXIT_REFUSED, RunOutcome};

    /// 锁下判定的三种情形：缩容走 `refuse_shrink`（不是确认门），等于现状原样返回
    /// （`resize` 的在线路径把它收成 nothing to do，0），大于现状放行
    #[test]
    fn lock_under_size_gate_refuses_shrink_and_passes_equal_or_grow() {
        let refused = online_size_gate(4096, 8192).unwrap_err();
        match (refused, super::super::refuse_shrink()) {
            (RunOutcome::Refused(got), RunOutcome::Refused(want)) => assert_eq!(got, want),
            (got, _) => panic!("a smaller target must be refused: {got:?}"),
        }
        assert_eq!(online_size_gate(4096, 8192).unwrap_err().exit_code(), EXIT_REFUSED);
        assert_eq!(
            online_size_gate(8192, 8192).unwrap(),
            Some(8192),
            "a target equal to the current size leaves the partition in place"
        );
        assert_eq!(online_size_gate(16384, 8192).unwrap(), Some(16384));
    }
}
