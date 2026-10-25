//! 在线 resize 的旗标拒绝、尺寸归一与调用。

#[cfg(target_os = "linux")]
use super::*;

/// 在线路径前置守卫：活动 swap 拒绝（run swapoff 后重试）。/proc/swaps 读不出来
/// 不是"没有 swap"——折叠成 false 就是给锁下复核开 fail-open 的口子
#[cfg(target_os = "linux")]
pub(crate) fn refuse_swap_active(dn: &str, part: u32) {
    let active = crate::online::swap_active(dn, part)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("cannot read /proc/swaps: {e}"))));
    if active {
        bail_fail(Fail::refused(format!("partition {part} is active swap — run swapoff first")));
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
    a: &Args,
    src: &FileSource,
    part: u32,
    is_pv: bool,
    decide: impl FnOnce(&FileSource) -> Result<Option<u64>, crate::outcome::Fail>,
) -> Option<crate::outcome::Outcome> {
    if !src.is_block {
        return None;
    }
    // 盘名取打开后的规范路径：`/dev/disk/by-id/…`、`/dev/mapper/…` 这类符号链接的文件名
    // 在 /proc/swaps 与 /proc/mounts 里都对不上，会把仍在使用的目标判成未挂载
    let resolved = std::fs::canonicalize(&a.target)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("cannot resolve {} to a kernel disk name: {e}", a.target))));
    let dn = resolved
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| bail_fail(Fail::refused(format!("cannot derive disk name from {}", a.target))));
    refuse_swap_active(&dn, part);

    // PV：分区层必须先按新尺寸出现在内核里，pvresize 才能吸收；活跃 LV 经 dm 持有分区使
    // BLKRRPART 返回 EBUSY，故走 sfdisk+partx 同步路径
    if is_pv {
        refuse_online_move_flags(a);
        let (o, old_bytes, pv_lock) = crate::online::resize_pv_online(&dn, part, decide);
        if !o.is_complete() {
            o.report();
            return Some(o);
        }
        // 锁贯穿 pvresize/lvextend：它们仍属同一次操作（见 resize_done 的持有说明）
        return Some(resize_done(a, None, part, true, true, old_bytes, pv_lock));
    }

    // 非 PV：仅挂载中的分区能在线扩（在线不能搬移，只吃连续空闲）
    // 探测失败按 Infra 拒绝而非退离线路径：读不出来 ≠ 没挂载
    let mnt = match crate::online::find_mountpoint(&dn, part) {
        Ok(m) => m,
        Err(e) => bail_fail(Fail::infra(format!("cannot probe mount state: {e}"))),
    };
    // 未挂载 = 在线路径不适用，None 经 `?` 返回调用方走离线路径
    let mnt = mnt?;
    // 两个旗标只挡在线路径：未挂载的盘走离线分支，那里 --allow-move / --chunk-size 有效
    refuse_online_move_flags(a);
    // 在线 grow 的表写与 FS 扩是一体的，且在线 shrink（btrfs）方向相反——先缩 FS 再写表。
    // 若只写表不动 FS，shrink 会得到"FS 大于分区"的损坏现场，故 --no-fs 不放行在线：
    // 未挂载的分区自然落离线路径，那里 --no-fs 照常生效
    if a.no_fs {
        bail_fail(Fail::refused(
            "--no-fs is not available online — the online path always includes the filesystem step; unmount the partition to skip it".to_string(),
        ));
    }
    let o = crate::online::resize_online_planned(&mnt, decide);
    if o.is_complete() {
        println!("resized online (verify with: diskedit info {})", a.target);
    } else {
        o.report();
    }
    Some(o)
}

/// 在线路径不搬移也不收 chunk：两个旗标的承诺在在线入口无从兑现。调用点只在实际
/// 走在线路径时调用——未挂载的非 PV 会落到离线分支，那里两个旗标照常有效
#[cfg(target_os = "linux")]
pub(crate) fn refuse_online_move_flags(a: &Args) {
    if a.allow_move {
        bail_fail(Fail::refused(
            "--allow-move is not available online — the online path cannot relocate blocking partitions".to_string(),
        ));
    }
    if a.seen.contains(&"--chunk-size") {
        bail_fail(Fail::refused(
            "--chunk-size has no effect online — the online path does not relocate data".to_string(),
        ));
    }
}

/// 在线目标长度的公共骨架：锁下取事实（扇区大小、当前字节、右侧空闲扇区、表自身的长度
/// 上界）→ 共享的 `resolve_size_request` → grow/shrink 分支。grow 零空闲时拒绝：本次
/// grow 不会产生分区扩容（表自身的上界已到顶，或下一分区紧贴右侧），在线路径又不能搬移
/// 挡路分区——与离线路径同口径。FS 是否已填满现分区不是这里的判据：只扩 FS 走
/// `resizefs --online`。绝对尺寸同样按该上界拒绝——可用区是表自身的闭区间，越界目标没有
/// 交给外部工具兜底的理由。上界（可用区端点到分区起点的扇区数）由各表自己的几何给出，
/// MBR 无此概念（其容量由 online 层的 `check_new_range` 把关）。PV 收缩在上游
/// `check_pv_intent` 已拒（online 层还有 grow-only 闸兜底），shrink 的下取整仅在锁窗口内
/// 分区被第三方扩大的竞态下可达——与离线路径的字节→LBA 隐式下取整同规则。两族表必须
/// 同语义，差异只在"事实怎么取"，故分支只此一份
#[cfg(target_os = "linux")]
pub(crate) fn online_size<'a>(
    req: &'a SizeRequest,
    is_pv: bool,
    facts: impl FnOnce(&FileSource) -> Result<(u64, u64, u64, Option<u64>), Fail> + 'a,
) -> impl FnOnce(&FileSource) -> Result<Option<u64>, Fail> + 'a {
    move |disk: &FileSource| {
        let (ss, cur, free, max_len_sectors) = facts(disk)?;
        let (target, grow) = resolve_size_request(req, cur)?;
        if grow {
            if free == 0 {
                return if is_pv {
                    Err(Fail::refused("no free space to the right — a PV cannot relocate blocking partitions while LVs may be active".to_string()))
                } else {
                    Err(Fail::refused("grow would not extend the partition — no usable free space to its right; the online path cannot relocate blocking partitions (unmount to grow offline, or fill the filesystem with `resizefs --online`)".to_string()))
                };
            }
            free.checked_mul(ss).and_then(|f| cur.checked_add(f))
                .map(Some)
                .ok_or_else(|| Fail::infra(format!("grown size overflows ({cur} + {free} sectors × {ss} B)")))
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
                return Err(Fail::refused(format!(
                    "size {want} < one sector ({ss} bytes) — a partition must keep at least one sector"
                )));
            }
            // 目标终点不得越过可用区上界：与离线路径同一判据（长度按扇区下取整后比较）
            if max_len_sectors.is_some_and(|lim| want / ss > lim) {
                return Err(Fail::refused(format!(
                    "size {want} exceeds the usable range — the partition would end past the last usable block"
                )));
            }
            Ok(Some(want))
        }
    }
}