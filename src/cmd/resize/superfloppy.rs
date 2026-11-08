//! 无分区表（superfloppy）目标的 resize。

use super::*;

/// superfloppy resize（无分区表，FS 即整盘）：无表可写，唯一有意义的是 FS grow
/// 到盘/镜像末端——缩无处可缩（无分区边界），显式 SIZE 只接受等于当前值。
/// 镜像/盘须已是大尺寸（dd 后或 truncate 预扩），本命令不负责扩文件本身
pub(crate) fn cmd_resize_superfloppy(a: &ResizeRequest) -> crate::outcome::RunOutcome {
    if let Some(n) = a.part {
        bail_fail(RunOutcome::refused(format!("target has no partition table — drop :{n} (the FS occupies the whole device)")));
    }
    if a.grow_lv || a.lv.is_some() {
        bail_fail(RunOutcome::refused("--grow-lv/--lv needs an LVM PV — superfloppy has no partitions".to_string()));
    }
    // superfloppy 无分区可改，唯一动作就是 FS 扩容：--no-fs 会让命令无事可做，
    // --allow-move 无分区可搬。静默照常执行会做出旗标明确排除的事
    if a.no_fs {
        bail_fail(RunOutcome::refused("--no-fs leaves nothing to do on a superfloppy — there is no partition to change, the only action is the filesystem grow".to_string()));
    }
    if a.allow_move {
        bail_fail(RunOutcome::refused("--allow-move has nothing to do on a superfloppy — there are no partitions to relocate".to_string()));
    }
    if a.chunk_given {
        bail_fail(RunOutcome::refused(
            "--chunk has no effect on a superfloppy — there is nothing to relocate".to_string(),
        ));
    }
    // 只取所有权，不建撤销凭据：本路径唯一的写盘是外部 FS 工具，不经过写句柄，
    // 落点上一个字节都不由本事务记录（与 resizefs 同一条边界）
    let src = open_target_owned(a).unwrap_or_else(|f| bail_fail(f));
    refuse_if_pending_recovery(&src, "resize").unwrap_or_else(|f| bail_fail(f));
    // 锁下取权威容器尺寸并解析 SIZE：+N/+% 锚定"当前尺寸"，而容器大小不受本工具锁约束
    //（镜像可被 truncate、盘可被第三方改写）——锁前锚点会算错方向，与分区路径同理
    let cur_bytes = src.size;
    let (target, _) = resolve_size_request(a.size, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    if let Some(t) = target {
        if t < cur_bytes {
            bail_fail(RunOutcome::refused("superfloppy cannot shrink — the FS occupies the whole device, there is no partition boundary to shrink to".to_string()));
        }
        if t > cur_bytes {
            bail_fail(RunOutcome::refused("target exceeds device/image size — extend the image or replace the disk first (this tool does not resize the container)".to_string()));
        }
        // SIZE == 当前值：与 grow 等价（FS 可能仍小于盘）
    }
    // FS 类型在锁下识别：整盘可在取锁前被重新格式化，类型判据必须锚定独占权之后的内容
    let fstype = fsid::identify(&src, 0, cur_bytes)
        .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("identify failed: {e}"))));
    if matches!(
        fstype,
        fsid::ContentKind::Lvm2Pv | fsid::ContentKind::Swap | fsid::ContentKind::Unknown
    ) {
        bail_fail(RunOutcome::refused(format!(
            "whole-device {} is not a resizable filesystem (no partition table on target)",
            fsid::content_name(fstype)
        )));
    }
    // 事前能力探测（check_grow 的契约：必须在首次写盘之前）。本路径的 e2fsck -fp 一类
    // 修复步骤本身写盘，缺工具若在那时才暴露，本次运行只能按 Partial(5) 报，
    // 无法再声称"确定未写盘"。
    // 类型接线判断不在此拦：不接线的类型不会写盘，由 resize_fs_in 按类型给出准确的
    // 拒绝文案；grow_support 的 "--no-fs" 提示对无分区的 superfloppy 不适用
    if fstype != fsid::ContentKind::Unknown {
        match fsops::check_grow(fstype, src.is_block, fsops::needs_loop(&fsops::DeviceScope::Whole, src.is_block)) {
            Err(fsops::FsError::UnsupportedFs(_)) => {}
            other => other.unwrap_or_else(|e| bail_fail(RunOutcome::from(e))),
        }
    }
    // FS grow 本身不改分区表，无 kernel_resync 必要。
    // 挂载点没卸掉是一条未完成的后置条件：它随成功结果浮出，退出码因此是 5。
    // 目标没有分区表，待办里的分区号记 0
    let pending = match fsops::resize_fs_whole(&src, Some(fstype)) {
        Ok(Some(leak)) => {
            eprintln!("warning: {leak}");
            vec![leak.into_pending(0)]
        }
        Ok(None) => Vec::new(),
        Err(e) => bail_fail(RunOutcome::from(e).context("FS grow failed")),
    };
    // 无分区表可写：FS 已扩，事务完成；清理待办随结果浮出
    let out = crate::outcome::RunOutcome::applied_with(pending);
    // 整体成功文案只在后置条件全部满足时打印；有待办时退出码是 5，此时只报 FS 扩容已做
    announce_applied(
        &out,
        || {
            format!(
                "superfloppy: {} grown to full device ({cur_bytes} bytes) — verify with: diskedit info {}",
                fsid::content_name(fstype),
                a.target
            )
        },
        || {
            format!(
                "superfloppy: {} filesystem grow step completed for the whole device ({cur_bytes} bytes); follow-up steps are pending — verify with: diskedit info {}",
                fsid::content_name(fstype),
                a.target
            )
        },
    );
    // 待办必须让人看到：本路径的返回值直达分派，没有别处会把它呈现出来
    out.report();
    out
}