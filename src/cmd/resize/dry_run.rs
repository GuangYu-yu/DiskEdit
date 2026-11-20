//! `resize --dry-run`：只读盘点。数值来自与执行路径同一份判据
//! （`resolve_size_request`、`free_right_*`、搬移计划与 `prepare_resize_part`），
//! 落点打印之后退出，不取写权、不建记录。

use super::*;

/// 预览的统一出口：没有写盘，也没有需要续跑的内容
fn stop() -> RunOutcome {
    println!("dry run: nothing written");
    RunOutcome::applied_with(Vec::new())
}

/// `FsAction` 在计划里的读法：它说明表提交前后各跑哪一步
fn fs_action_label(action: movepart::FsAction) -> &'static str {
    match action {
        movepart::FsAction::Leave => "filesystem unchanged",
        movepart::FsAction::Grow => "filesystem grows after the table",
        movepart::FsAction::Shrink => "filesystem shrinks first",
    }
}

/// GPT 预览：落点、文件系统动作与（需要时）搬移计划
pub(crate) fn preview_gpt(
    a: &ResizeRequest,
    src: &FileSource,
    g: &crate::geometry::ValidatedGeometry,
    repair: crate::gpt_policy::RepairAction,
    part: u32,
) -> RunOutcome {
    let e = crate::gpt_policy::live_entry(g, part).unwrap_or_else(|f| bail_fail(f));
    let (start, end, ss) = (e.starting_lba, e.ending_lba, g.sector_size());
    let cur_bytes = lba_range_bytes(start, end, ss);
    let (target, grow_to_end) = resolve_size_request(a.size, cur_bytes, ss).unwrap_or_else(|f| bail_fail(f));
    let fstype = fsid::identify(src, lba_bytes(start, ss), cur_bytes)
        .unwrap_or_else(|e| bail_fail(RunOutcome::refused(crate::support::identify_failed(&e))));
    if fstype == fsid::ContentKind::Lvm2Pv {
        let shrinking = target.is_some_and(|t| t < cur_bytes);
        println!(
            "note: {}",
            if shrinking {
                "an LVM PV shrink runs pvresize --setphysicalvolumesize before the table is committed"
            } else {
                "the PV/LV chain is not part of this preview"
            }
        );
    }
    let last_usable = g.last_usable_lba();

    // `grow`：吃满右侧连续空闲；挡路分区只有在 --allow-move 下才搬
    if grow_to_end {
        let free = free_right_gpt(g, part);
        if end == last_usable {
            println!("partition {part} already reaches last_usable_lba {last_usable} — nothing to grow");
        } else if free > 0 {
            println!("plan: grow partition {part} : LBA {start}..{end} → {start}..{}", end + free);
        } else {
            if !a.allow_move {
                bail_fail(RunOutcome::refused("not enough contiguous free space to the right — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
            }
            let plan = movepart::make_plan_resuming(src, None, g, repair, part).unwrap_or_else(|f| bail_fail(f));
            crate::cmd::info_grow::print_plan(&plan, start)
                .unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("plan failed: {e}"))));
        }
        return stop();
    }

    let Some(bytes) = target else {
        bail_fail(RunOutcome::refused(
            "`resize` needs a size: diskedit resize <TARGET>[:N] <SIZE> (see diskedit help resize)".to_string(),
        ));
    };
    let Some(new_end) = start.checked_add(bytes / ss).and_then(|e| e.checked_sub(1)) else {
        bail_fail(RunOutcome::refused(format!("size {bytes} overflows the LBA range")));
    };
    if new_end > last_usable {
        bail_fail(RunOutcome::refused(format!(
            "size {bytes} exceeds usable range (partition would end past last_usable_lba {last_usable})"
        )));
    }
    crate::support::note_sector_truncation("resizing to", bytes, ss);

    if new_end == end {
        println!("partition {part} is already {cur_bytes} bytes — nothing to do");
        return stop();
    }
    if new_end < end {
        let planned = prepare(src, g, repair, part, start, new_end, a);
        println!(
            "plan: shrink partition {part} : LBA {start}..{end} → {start}..{new_end} \
             ({} bytes; {})",
            lba_range_bytes(start, new_end, ss),
            fs_action_label(planned.fs_action)
        );
        return stop();
    }

    let need = new_end - end;
    if need <= free_right_gpt(g, part) {
        let planned = prepare(src, g, repair, part, start, new_end, a);
        println!(
            "plan: grow partition {part} : LBA {start}..{end} → {start}..{new_end} \
             ({} bytes; {})",
            lba_bytes(new_end - start + 1, ss),
            fs_action_label(planned.fs_action)
        );
        return stop();
    }
    if !a.allow_move {
        bail_fail(RunOutcome::refused("not enough contiguous free space to the right — pass --allow-move to relocate the blocking partitions (plan will be printed; --yes confirms)".to_string()));
    }
    let intent = movepart::ShiftIntent::Grow(need);
    let plan = movepart::make_plan_shift_resuming(src, None, g, repair, part, intent).unwrap_or_else(|f| bail_fail(f));
    crate::cmd::info_grow::print_moves(&plan).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(format!("plan failed: {e}"))));
    println!("plan: grow partition {part} to end LBA {new_end} (blockers relocated)");
    stop()
}

/// 与执行路径同一份只读判定：越界、`--no-fs` 与文件系统最小尺寸都在这里被挡下
fn prepare(
    src: &FileSource,
    g: &crate::geometry::ValidatedGeometry,
    repair: crate::gpt_policy::RepairAction,
    part: u32,
    start: u64,
    new_end: u64,
    a: &ResizeRequest,
) -> crate::cmd::layout::PlannedResizeEnd {
    let chunk = resolve_chunk(a.chunk_mib, src);
    crate::cmd::layout::prepare_resize_part(
        src, g, repair, part, start, new_end, chunk, a.no_fs, None, false, &mut |_| {},
    )
    .unwrap_or_else(|f| bail_fail(f))
}

/// `--end` / `--grow-to-end` 预览：原地重划只改末端，判据与执行路径同一份
pub(crate) fn preview_end(a: &crate::cli::ResizeEndRequest) -> RunOutcome {
    let Some(pref) = a.part else {
        bail_fail(RunOutcome::refused(super::NEEDS_PARTITION.to_string()));
    };
    if a.grow_to_end && a.end.is_some() {
        bail_fail(RunOutcome::refused(super::END_AND_GROW_TO_END_EXCLUSIVE.to_string()));
    }
    let src = open_target_ro(a).unwrap_or_else(|f| bail_fail(f));
    let (g, repair) = crate::gpt_policy::require_gpt_geometry(&src, "resize").unwrap_or_else(|f| bail_fail(f));
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Gpt(g.entries()), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let e = crate::gpt_policy::live_entry(&g, part).unwrap_or_else(|f| bail_fail(f));
    let start = e.starting_lba;
    let old_end = e.ending_lba;
    let end = if a.grow_to_end {
        align_end(a.align, start, g.last_usable_lba(), g.sector_size())
    } else {
        let Some(end) = a.end else {
            bail_fail(RunOutcome::refused("`resize` needs --end (or --grow-to-end)".to_string()));
        };
        align_end(a.align, start, end, g.sector_size())
    };
    let planned = crate::cmd::layout::prepare_resize_part(
        &src,
        &g,
        repair,
        part,
        start,
        end,
        crate::execution::RESIZE_CHUNK_BYTES,
        a.no_fs,
        None,
        false,
        &mut |_| {},
    )
    .unwrap_or_else(|f| bail_fail(f));
    if end == old_end {
        println!("partition {part} is already at end LBA {old_end} — nothing to do");
        return stop();
    }
    println!(
        "plan: {} partition {part} : LBA {start}..{old_end} → {start}..{end} ({} bytes; {})",
        if end < old_end { "shrink" } else { "grow" },
        lba_range_bytes(start, end, g.sector_size()),
        fs_action_label(planned.fs_action)
    );
    stop()
}

/// MBR 预览：主分区 1..4 的扩与缩，没有搬移与调块
pub(crate) fn preview_mbr(a: &ResizeRequest, src: &FileSource, pref: PartSelector) -> RunOutcome {
    if a.allow_move {
        bail_fail(RunOutcome::refused("--allow-move is not supported for MBR resize — MBR cannot relocate blocking partitions".to_string()));
    }
    if a.chunk_mib.is_some() {
        bail_fail(RunOutcome::refused(
            "--chunk has no effect for MBR resize — MBR cannot relocate blocking partitions".to_string(),
        ));
    }
    let mbr = table::parse_mbr(src)
        .unwrap_or_else(|e| bail_fail(RunOutcome::refused(crate::support::parse_failed(&e))))
        .unwrap_or_else(|| bail_fail(RunOutcome::refused("no MBR on target".to_string())));
    let part = crate::gpt_policy::resolve_part_in(crate::gpt_policy::TableEntries::Mbr(&mbr), pref)
        .unwrap_or_else(|f| bail_fail(f));
    let p = match mbr.iter().find(|p| p.num == part) {
        Some(p) => p,
        None => bail_fail(RunOutcome::refused(format!(
            "partition {part} not found (MBR resize covers primary partitions 1..4 only)"
        ))),
    };
    if p.is_container {
        bail_fail(RunOutcome::refused("extended partition container cannot be resized (logical partitions are out of scope)".to_string()));
    }
    let ss = src.sector_size;
    let cur_bytes = lba_bytes(p.size_lba as u64, ss);
    let (target, grow_to_end) = resolve_size_request(a.size, cur_bytes, ss).unwrap_or_else(|f| bail_fail(f));
    let total_sectors = src.size / ss;
    let free = free_right_msdos(&mbr, p, total_sectors);
    if grow_to_end {
        if free == 0 {
            bail_fail(RunOutcome::refused(super::GROW_NO_FREE_SPACE_MSDOS.to_string()));
        }
        println!(
            "plan: grow partition {part} : LBA {}..{} → {}..{}",
            p.start_lba,
            p.start_lba as u64 + u64::from(p.size_lba) - 1,
            p.start_lba,
            p.start_lba as u64 + u64::from(p.size_lba) + free - 1
        );
        return stop();
    }
    let Some(bytes) = target else {
        bail_fail(RunOutcome::refused(
            "`resize` needs a size: diskedit resize <TARGET>[:N] <SIZE> (see diskedit help resize)".to_string(),
        ));
    };
    crate::support::note_sector_truncation("resizing to", bytes, ss);
    let new_lba = bytes / ss;
    if new_lba == u64::from(p.size_lba) {
        println!("partition {part} is already {cur_bytes} bytes — nothing to do");
        return stop();
    }
    if new_lba < u64::from(p.size_lba) {
        // 收缩：判据与执行路径同一处（--no-fs、工具可缩、ext 最小尺寸），只盘点不跑工具
        if a.no_fs {
            bail_fail(RunOutcome::refused(super::NO_FS_CANNOT_SHRINK_MSDOS.to_string()));
        }
        let fstype = fsid::identify(src, lba_bytes(p.start_lba as u64, ss), cur_bytes)
            .unwrap_or_else(|e| bail_fail(RunOutcome::refused(crate::support::identify_failed(&e))));
        crate::fsops::check_shrink(
            Some(fstype),
            src.is_block,
            crate::fsops::needs_loop(&crate::fsops::DeviceScope::Partition(part), src.is_block),
        )
        .unwrap_or_else(|f| bail_fail(f.into()));
        if fstype == fsid::ContentKind::Lvm2Pv {
            crate::fsops::pv_shrink_precheck(src, part, bytes).unwrap_or_else(|f| bail_fail(f.into()));
        }
        if let Some(min) = crate::fsops::fs_min_bytes(src, part, Some(fstype)).unwrap_or_else(|f| bail_fail(f.into()))
            && bytes < min
        {
            bail_fail(RunOutcome::refused(format!(
                "target size {bytes} < minimum size {min} bytes the filesystem can hold (resize2fs -P)"
            )));
        }
        println!(
            "plan: shrink partition {part} : LBA {}..{} → {}..{} ({} bytes; {})",
            p.start_lba,
            p.start_lba as u64 + u64::from(p.size_lba) - 1,
            p.start_lba,
            p.start_lba as u64 + new_lba - 1,
            lba_bytes(new_lba, ss),
            fs_action_label(movepart::FsAction::Shrink)
        );
        return stop();
    }
    if new_lba > p.size_lba as u64 + free {
        bail_fail(RunOutcome::refused(
            "not enough contiguous free space to the right (MBR resize cannot relocate blocking partitions)".to_string(),
        ));
    }
    println!(
        "plan: grow partition {part} : LBA {}..{} → {}..{}",
        p.start_lba,
        p.start_lba as u64 + u64::from(p.size_lba) - 1,
        p.start_lba,
        p.start_lba as u64 + new_lba - 1
    );
    stop()
}