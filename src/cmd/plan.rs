//! plan 的只读报告与搬移计划的打印。`plan` 与 `info` 共用同一份报告；
//! `print_plan`/`print_moves` 供 resize 与 `plan --grow` 在写表之前展示将要搬动的分区。

use crate::cli::{InfoRequest, PlanRequest};
use crate::gpt_policy;
use crate::movepart;
use crate::support::{bail_fail, RunOutcome, EXIT_OK};

/// plan 的详助：只读报告，字段与 `info` 同源
pub(crate) const DETAILS_PLAN: &str = r#"  Read-only report on a target: geometry, partition table, filesystems and
  space-usage conclusions. The fields are the ones `info` prints; `plan` opens
  no recovery path and probes no write directory.
  With --grow the report is followed by the relocation preview for growing
  partition :N to the last usable LBA. The preview only reads: nothing is
  written and no reusable plan is produced."#;

/// 默认只读几何报告；`--grow` 追加搬迁预览，不落盘、不产出 `Plan`
pub(crate) fn cmd_plan(a: &PlanRequest) -> u8 {
    if !a.grow {
        // 无 --grow 的 plan 就是全目标 info；:N 不是它的输入——拒绝主语用 plan 说，
        // 不能借 info 之手报 "info reports ..."（用户跑的是 plan）
        if a.part.is_some() {
            bail_fail(RunOutcome::refused(
                "`plan` without --grow reports the whole target — drop :N, or add --grow to preview one partition",
            ));
        }
        return crate::cmd::info::cmd_info(&InfoRequest {
            target: a.target.clone(),
            part: None,
            sector_size: a.sector_size,
        });
    }
    let Some(selector) = a.part else {
        bail_fail(RunOutcome::refused(
            "`plan --grow` previews one partition — write the target as <TARGET>:N",
        ));
    };
    let code = crate::cmd::info::cmd_info(&InfoRequest {
        target: a.target.clone(),
        part: None,
        sector_size: a.sector_size,
    });
    // 预览的几何报告失败（识别失败退 4）时整个 plan 同样失败：吞掉返回码会让
    // "info 已识别失败、plan 却退 0" 同时发生
    if code != crate::outcome::EXIT_OK {
        return code;
    }
    let src = match crate::cmd::open_read_only_target(&a.target, a.sector_size) {
        Ok(s) => s,
        Err(f) => bail_fail(f),
    };
    let Some((g, repair)) = (match gpt_policy::resolve_geometry(&src) {
        Ok(v) => v,
        Err(f) => bail_fail(f),
    }) else {
        bail_fail(RunOutcome::refused("`plan --grow` needs a GPT partition table"));
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
            crate::recovery::CompanionProbe::Decoded(record)
                if here.as_ref().is_some_and(|h| record.target.matches(h)) =>
            {
                record.relocation
            }
            _ => None,
        });
    let plan = match movepart::make_plan_resuming(relocation.as_ref(), &g, repair, part) {
        Ok(p) => p,
        Err(f) => bail_fail(f),
    };
    if let Err(e) = print_plan(&plan, target_start) {
        bail_fail(RunOutcome::from(e));
    }
    EXIT_OK
}

/// 列出各分区的搬移（resize 写表之前的计划打印）
pub(crate) fn print_moves(plan: &movepart::Plan) -> std::io::Result<()> {
    for m in &plan.moves {
        let tag = if m.is_swap { " [swap: recreate, no data move]" } else { "" };
        // 值来自锁下解析的表项/ckpt；仍按库规约 checked——损坏条目的下溢/溢出
        // 宁可拒打，不回绕成假区间误导用户
        let last = m.first_lba.checked_add(m.len_lba).and_then(|e| e.checked_sub(1))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "corrupted move entry: range overflows"))?;
        let bytes = m.delta_lba.checked_mul(plan.ss)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "corrupted move entry: byte size overflows"))?;
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
