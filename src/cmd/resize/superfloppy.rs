//! 无分区表（superfloppy）目标的 resize。

use super::*;

/// superfloppy resize（无分区表，FS 即整盘）：无表可写，唯一有意义的是 FS grow
/// 到盘/镜像末端——缩无处可缩（无分区边界），显式 SIZE 只接受等于当前值。
/// 镜像/盘须已是大尺寸（dd 后或 truncate 预扩），本命令不负责扩文件本身
pub(crate) fn cmd_resize_superfloppy(a: &Args, req: &SizeRequest) -> u8 {
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("target has no partition table — drop :{n} (the FS occupies the whole device)")));
    }
    if a.grow_lv || a.lv.is_some() {
        bail_fail(Fail::refused("--grow-lv/--lv needs an LVM PV — superfloppy has no partitions".to_string()));
    }
    // superfloppy 无分区可改，唯一动作就是 FS 扩容：--no-fs 会让命令无事可做，
    // --allow-move 无分区可搬。静默照常执行会做出旗标明确排除的事
    if a.no_fs {
        bail_fail(Fail::refused("--no-fs leaves nothing to do on a superfloppy — there is no partition to change, the only action is the filesystem grow".to_string()));
    }
    if a.allow_move {
        bail_fail(Fail::refused("--allow-move has nothing to do on a superfloppy — there are no partitions to relocate".to_string()));
    }
    if a.seen.contains(&"--chunk-size") {
        bail_fail(Fail::refused(
            "--chunk-size has no effect on a superfloppy — there is nothing to relocate".to_string(),
        ));
    }
    let src = open_target_for_write(a).unwrap_or_else(|f| bail_fail(f));
    // 锁下取权威容器尺寸并解析 SIZE：+N/+% 锚定"当前尺寸"，而容器大小不受本工具锁约束
    //（镜像可被 truncate、盘可被第三方改写）——锁前锚点会算错方向，与分区路径同理
    let cur_bytes = src.size;
    let (target, grow_to_end) = resolve_size_request(req, cur_bytes).unwrap_or_else(|f| bail_fail(f));
    if let Some(t) = target {
        if t < cur_bytes {
            bail_fail(Fail::refused("superfloppy cannot shrink — the FS occupies the whole device, there is no partition boundary to shrink to".to_string()));
        }
        if t > cur_bytes {
            bail_fail(Fail::refused("target exceeds device/image size — extend the image or replace the disk first (this tool does not resize the container)".to_string()));
        }
        // SIZE == 当前值：与 grow 等价（FS 可能仍小于盘）
    } else if !grow_to_end {
        // 防御断言：resolve_size_request 对 (None,false) 已返回拒绝，正常输入到不了这里。
        // 保留它，使将来契约放宽时未知请求也不会被静默当成"扩到盘尾"
        bail_fail(Fail::infra("internal error: size request resolved to neither a target nor grow".to_string()));
    }
    // FS 类型在锁下识别：整盘可在取锁前被重新格式化，类型判据必须锚定独占权之后的内容
    let fstype = fsid::identify(&src, 0, cur_bytes)
        .unwrap_or_else(|e| bail_fail(Fail::infra(format!("identify failed: {e}"))));
    if matches!(fstype, Some(fsid::FsKind::Lvm2Pv | fsid::FsKind::Swap) | None) {
        bail_fail(Fail::refused(format!(
            "whole-device {} is not a resizable filesystem (no partition table on target)",
            fsid::fs_name(fstype)
        )));
    }
    // 事前能力探测（check_grow 的契约：必须在首次写盘之前）。本路径的 e2fsck -fp 一类
    // 修复步骤本身写盘，缺工具若在那时才暴露，Infra(30) 会谎报"确定未写盘"。
    // 类型接线判断不在此拦：不接线的类型不会写盘，由 resize_fs_in 按类型给出准确的
    // 拒绝文案；grow_support 的 "--no-fs" 提示对无分区的 superfloppy 不适用
    if let Some(kind) = fstype {
        match fsops::check_grow(kind, src.is_block, fsops::needs_loop(&fsops::DeviceScope::Whole, src.is_block)) {
            Err(fsops::FsError::UnsupportedFs(_)) => {}
            other => other.unwrap_or_else(|e| bail_fail(Fail::from(e))),
        }
    }
    // FS grow 本身不改分区表，无 kernel_resync 必要
    // 清理待办在此只以告警呈现：本命令的成功出口保持 0，不改既有契约
    match fsops::resize_fs_whole(&src, fstype) {
        Ok(Some(leak)) => eprintln!("warning: {leak}"),
        Ok(None) => {}
        Err(e) => bail_fail(Fail::from(e).context("FS grow failed")),
    }
    println!("superfloppy: {} grown to full device ({cur_bytes} bytes) — verify with: diskedit info {}", fsid::fs_name(fstype), a.target);
    EXIT_OK
}