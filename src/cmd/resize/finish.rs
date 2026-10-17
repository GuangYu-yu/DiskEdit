//! resize 收尾：outcome 映射与成功路径。

use super::*;

/// resize 的收尾：布局结果 →（PV 时才继续）LVM 链，取两者中更严重的退出码。
/// 未写入 → 直接返回；已写入但后置条件未全满足 → 非 PV 也直接返回，不打印成功字样
/// （否则与 PARTIAL 矛盾），PV 则仍需跑 pvresize/lvextend 链。
/// `wsrc` 是持锁带 journal 的写句柄（在线路径无 journal，为 None）：PV 屏障
/// （见 resize_done）只能由它落。
/// `part` 是**本次操作的那个分区**（入口处解析一次后随行）：收尾要对它做 pvresize/
/// lvextend，出错的代价是写到别的分区上，故不按写后的表重新解释一次
pub(crate) fn finish_resize(a: &Args, o: crate::outcome::Outcome, wsrc: Option<&mut FileSource>, part: u32, is_pv: bool, is_block: bool, old_bytes: u64) -> u8 {
    if !o.is_applied() {
        return o.exit_code();
    }
    if !o.is_complete() && !is_pv {
        return o.exit_code();
    }
    resize_done(a, wsrc, part, is_pv, is_block, old_bytes, None).max(o.exit_code())
}

/// 分区扩容收尾：从盘上表项重读实际新尺寸（搬移路径的扩容终点由计划决定，
/// 不能用操作前的预估）。PV 一律走 pvresize（--grow-lv 再传 LV）：块设备直接对
/// 分区节点；镜像经 losetup 临时映射该分区（attach → pvresize/lvextend → detach）。
pub(crate) fn resize_done(
    a: &Args,
    wsrc: Option<&mut FileSource>,
    part: u32,
    is_pv: bool,
    is_block: bool,
    old_bytes: u64,
    online_lock: Option<crate::targetlock::TargetLock>,
) -> u8 {
    // 在线路径的独占权要覆盖到 LVM 链结束：pvresize/lvextend 是本次操作的后置条件，
    // 在它们跑之前释放锁，另一个 diskedit 就能在这段窗口内改写同一块盘
    let _online_lock = online_lock;
    if !is_pv {
        println!("resized (verify with: diskedit info {})", a.target);
        return EXIT_OK;
    }
    #[cfg(target_os = "linux")]
    {
        // 屏障要经可变借用落（mark_non_reversible 是 &mut self），只有 Linux 路径用到
        let mut wsrc = wsrc;
        // 写句柄在手（离线路径）时直接复用它读盘上现状；在线路径无 journal 句柄，
        // 才自开只读句柄
        let mut opened = None;
        // 表项重读：GPT 与 MBR 的分派、表自身 ss 的换算都在 gpt_policy::partition_bytes 一处
        let new_bytes = {
            let ro: &FileSource = match wsrc.as_deref() {
                Some(w) => w,
                None => opened.get_or_insert_with(|| open_target_ro(a).unwrap_or_else(|f| bail_fail(f))),
            };
            match crate::gpt_policy::partition_bytes(ro, part) {
                Ok((_, len)) => len,
                // 分区号在 resize 入口已由锁下几何验证过一次，此时查不到即盘内容异常。
                // 表已写，报 Failed 而非 Infra：后者向用户承诺"本次没写盘"，在此为假
                //（into_io_error 正是"此处已越界"的取消息方式）
                Err(f) => bail_fail(Fail::failed(format!("post-resize: {}", crate::outcome::into_io_error(f)))),
            }
        };
        let delta = new_bytes.saturating_sub(old_bytes);
        // pvresize/lvextend 的写入不可回滚：此后回滚表项即"表与内容自相矛盾"，屏障必须
        // 先于该写入落。落点是它存在的唯一位置——GPT/MBR、镜像/块设备的 PV 收尾在此
        // 汇合，且只有 wsrc 带 journal；比藏在各自表写入函数里少一处各自演化（GPT 旧实现
        // 在 finalize_growth 落，距实际写入隔了整个收尾流，死亡窗口会无谓锁死 undo）。
        // 在线路径不经 journal（sfdisk 用自己的 fd 写盘，事后也无 undo 可谈），无需屏障
        if let Some(ref mut w) = wsrc {
            w.set_mutation(crate::dev::Mutation::ExternalFsTool);
            w.mark_non_reversible().unwrap_or_else(|e| bail_fail(Fail::from(e)));
        }
        let r: Result<(), LvmError> = if is_block {
            lvm_grow_chain(&part_dev_path(&a.target, part), new_bytes, delta, a.grow_lv, a.lv.as_deref())
        } else {
            // offset+sizelimit 映射出的 loop 设备 = 该分区的整块设备，PV 整设备语义下
            // pvresize/lvextend 直接可用，无需 -P partscan。
            // wsrc 为 None 只发生在块设备在线路径（镜像恒走离线、带写句柄），
            // 故走到镜像分支时 opened 必已在上面的 new_bytes 块中填好
            let ro: &FileSource = match wsrc.as_deref() {
                Some(w) => w,
                None => opened.as_ref().unwrap(),
            };
            fsops::with_partition_device(ro, part, |pv| {
                lvm_grow_chain(pv, new_bytes, delta, a.grow_lv, a.lv.as_deref())
            })
        };
        match r {
            Ok(()) => EXIT_OK,
            // LVM 链失败 = 后置条件未满足：经 Outcome 的 Pending 通道报告并换算 PARTIAL，
            // 不在此手拼退出码（退出码映射的唯一处是 outcome）
            Err(e) => {
                // 补救指引随错误正文走（见 LvmError::hint）：缺的可能不是 lvm2 的工具
                let hint = e.hint();
                let o = crate::outcome::Outcome::applied_with(vec![crate::outcome::Pending::new(
                    part,
                    crate::outcome::PendingKind::Other("lvm chain (pvresize/lvextend)"),
                    e.to_string(),
                    hint,
                )]);
                o.report();
                o.exit_code()
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (is_block, old_bytes, wsrc);
        let o = crate::outcome::Outcome::applied_with(vec![crate::outcome::Pending::new(
            part,
            crate::outcome::PendingKind::Other("lvm chain (pvresize/lvextend)"),
            "pvresize/lvextend require Linux — run pvresize on the partition manually",
            "",
        )]);
        o.report();
        o.exit_code()
    }
}