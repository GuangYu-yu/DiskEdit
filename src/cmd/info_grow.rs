//! `info --grow` 的只读报告与搬移预览。报告与 `info` 共用同一份生成器；
//! `print_plan`/`print_moves` 供 resize 在写表之前展示将要搬动的分区。

use crate::cli::InfoRequest;
use crate::gpt_policy;
use crate::movepart;
use crate::support::{bail_fail, RunOutcome};

/// 只读几何报告，`--grow` 在同一份文档里多给一个 `preview` 键，不落盘、不产出 `Plan`
pub(crate) fn cmd_info_grow(a: &InfoRequest) -> u8 {
    let Some(selector) = a.part else {
        bail_fail(RunOutcome::refused(
            "`info --grow` previews one partition — write the target as <TARGET>:N",
        ));
    };
    let src = match crate::cmd::open_read_only_target(&a.target, a.sector_size) {
        Ok(s) => s,
        Err(f) => bail_fail(f),
    };
    let Some((g, repair)) = (match gpt_policy::resolve_geometry(&src) {
        Ok(v) => v,
        Err(f) => bail_fail(f),
    }) else {
        bail_fail(RunOutcome::refused("`info --grow` needs a GPT partition table"));
    };
    let part = match gpt_policy::resolve_part(&src, selector) {
        Ok(p) => p,
        Err(f) => bail_fail(f),
    };
    let target_start = match gpt_policy::live_entry(&g, part) {
        Ok(e) => e.starting_lba,
        Err(f) => bail_fail(f),
    };
    // 续跑预览：落点上有属于本目标的搬移记录时按它的有序计划打印，否则现算
    let here = crate::platform::probe::probe_of(&src)
        .ok()
        .map(|probe| crate::recovery::SceneIdentity::of_source(&probe, &src));
    let relocation = src
        .identity
        .record_path()
        .and_then(|path| match crate::recovery::probe_companion(path) {
            // 认领：这份记录是不是本次目标的（含 loop 映射比对）。释放另走
            // `same_carrier`，两者不得互换
            crate::recovery::CompanionProbe::Decoded(record)
                if here.as_ref().is_some_and(|h| record.target.matches(h)) =>
            {
                record.relocation
            }
            _ => None,
        });
    let plan = match movepart::make_plan_resuming(&src, relocation.as_ref(), &g, repair, part) {
        Ok(p) => p,
        Err(f) => bail_fail(f),
    };
    let preview = match preview_json(&plan, target_start) {
        Ok(s) => s,
        Err(e) => bail_fail(RunOutcome::from(e)),
    };
    // 报告与预览是同一份文档：消费方拿到的永远是单个 JSON 对象，不必先剥掉尾部的文本行
    crate::cmd::info::cmd_info_with(
        &InfoRequest {
            target: a.target.clone(),
            part: None,
            sector_size: a.sector_size,
        },
        Some(&preview),
    )
}

/// 搬移条目的落点与字节数（值来自锁下解析的表项/ckpt）：损坏条目的下溢/溢出
/// 宁可拒打，不回绕成假区间
fn move_range(m: &movepart::PlanEntry, ss: u64) -> std::io::Result<(u64, u64)> {
    let last = m
        .first_lba
        .checked_add(m.len_lba)
        .and_then(|e| e.checked_sub(1))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "corrupted move entry: range overflows"))?;
    let bytes = m
        .delta_lba
        .unsigned_abs()
        .checked_mul(ss)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "corrupted move entry: byte size overflows"))?;
    Ok((last, bytes))
}

/// `preview` 键的内容。终点判定与 `print_plan` 同源（`grow_end_for`），只换渲染方式
fn preview_json(plan: &movepart::Plan, target_start: u64) -> std::io::Result<String> {
    let new_end = movepart::grow_end_for(plan, target_start)?;
    let mut tail = format!(
        ",\"preview\":{{\"grow_part\":{},\"new_end_lba\":{new_end},\"sector_size\":{},\"moves\":[",
        plan.grow_part, plan.ss
    );
    for (i, m) in plan.moves.iter().enumerate() {
        let (last, bytes) = move_range(m, plan.ss)?;
        if i > 0 {
            tail.push(',');
        }
        tail.push_str(&format!(
            "{{\"num\":{},\"first_lba\":{},\"last_lba\":{},\"delta_lba\":{},\"bytes\":{},\"swap\":{}}}",
            m.part_num,
            m.first_lba,
            last,
            m.delta_lba,
            bytes,
            if m.is_swap { "true" } else { "false" }
        ));
    }
    tail.push_str("]}");
    Ok(tail)
}

/// 列出各分区的搬移（resize 写表之前的计划打印）
pub(crate) fn print_moves(plan: &movepart::Plan) -> std::io::Result<()> {
    for m in &plan.moves {
        let tag = if m.is_swap { " [swap: recreate, no data move]" } else { "" };
        let (last, bytes) = move_range(m, plan.ss)?;
        println!("move part {} : {}..{} → +{} sectors ({} bytes){}",
            m.part_num, m.first_lba, last, m.delta_lba, bytes, tag);
    }
    Ok(())
}

/// 写盘前的计划呈现：扩容终点按 `grow_end_for` 判定（与实际写入同一实现）
pub(crate) fn print_plan(plan: &movepart::Plan, target_start: u64) -> std::io::Result<()> {
    let new_end = movepart::grow_end_for(plan, target_start)?;
    println!("plan: grow partition {} → end LBA {} (blockers relocated)", plan.grow_part, new_end);
    print_moves(plan)
}
