//! plan / apply：把分区 N 扩到其后的全部空闲区的手动多步形式（含搬移计划打印与执行）。

use crate::support::*;
use crate::args::Args;
use crate::movepart;

pub(crate) const HELP: &str = r#"diskedit plan <TARGET> --grow N
diskedit apply <TARGET> --grow N [--chunk-size MiB]

  Grow partition N into all following free space, relocating intervening
  partitions tail-packed (manual multi-step form of `resize grow`).
  plan prints the operations without touching the disk; apply executes
  them and resumes from its checkpoint if re-run."#;

/// 列出各分区的搬移（plan 命令与 apply 前的计划打印共用）。
/// 头行不共用：两处要给出的数不同——写入前只需扩容终点，`plan` 还要额外给出
/// 尾部打包的上界 last_usable_lba
pub(crate) fn print_moves(plan: &movepart::Plan) {
    for m in &plan.moves {
        let tag = if m.is_swap { " [swap: recreate, no data move]" } else { "" };
        println!("move part {} : {}..{} → +{} sectors ({} bytes){}",
            m.part_num, m.first_lba, m.first_lba + m.len_lba - 1, m.delta_lba, m.delta_lba * plan.ss, tag);
    }
}

pub(crate) fn print_plan(plan: &movepart::Plan) -> std::io::Result<()> {
    // 实际扩容终点由 grow_end_for 判定（与 apply 同一实现）
    let new_end = movepart::grow_end_for(plan)?;
    println!("plan: grow partition {} → end LBA {} (blockers relocated)", plan.grow_part, new_end);
    print_moves(plan);
    Ok(())
}

pub(crate) fn cmd_plan_apply(cmd: &str, a: &Args) -> u8 {
    let Some(grow) = a.grow else { crate::args::usage() };
    // 这一段只读：plan 不写盘，apply 的写由 apply_cmd 用 open_target_for_write 另开一次。
    // 故按只读命令打开——否则一块正被使用的盘上连 `plan` 都跑不出计划
    let mut src = open_target_ro(a).unwrap_or_else(|(c, m)| bail(c, m));
    // 恢复感知：盘上有未收尾的搬移作业时，`plan` 要打印、`apply` 要执行的
    // 都是那份 ckpt 里的计划（现算的 delta 与 ckpt 不一致，会撞上恢复校验）
    let plan = match movepart::make_plan_resuming(&mut src, grow) {
        Ok(p) => p,
        Err(f) => bail_fail(f),
    };
    if cmd == "plan" {
        if movepart::has_pending_relocation(&src, plan.grow_part) {
            println!("[resume] an unfinished relocation job is on the disk — this is the plan it resumes with");
        }
        if let Some(what) = plan.repair.describe() {
            // 修复动作只记录、不执行；apply 会先做这一步再搬数据
            println!("[repair] {what}");
        }
        // 终点与实际写入一致（grow_end_for 是 apply 用的同一实现）；
        // last_usable_lba 单独列出：它是尾部打包的上界，不等于本次扩容终点
        let grow_end = movepart::grow_end_for(&plan)
            .unwrap_or_else(|e| bail(EXIT_REFUSED, format!("plan failed: {e}")));
        println!(
            "grow partition {} → end LBA {} (last usable {})",
            plan.grow_part, grow_end, plan.last_usable_lba
        );
        print_moves(&plan);
        EXIT_OK
    } else {
        apply_cmd(a, plan)
    }
}

fn apply_cmd(a: &Args, plan: movepart::Plan) -> u8 {
    let mut src = match open_target_for_write(a) { Ok(s) => s, Err((c, m)) => { eprintln!("{m}"); return c; } };
    // 表的可解析性由 apply_inner 在写盘前判定（无表 → refused 10，表非法 → infra 30）：
    // 同一事实不设第二判据——两处判据迟早会在某个入口分叉，且自判拒绝时还不报原因
    let chunk = match movepart::chunk_bytes(a.chunk_mib) {
        Ok(c) => c,
        Err(e) => { eprintln!("refused: {e}"); return EXIT_REFUSED; }
    };
    let mut logger = Logger::open(&src);
    let o = movepart::apply(&mut src, &plan, chunk, a.no_fs, &mut |m| logger.log(m));
    // 失败时日志里也留一份：apply 出问题后用户常回看日志
    if let crate::outcome::Outcome::Failed { cause } = &o {
        logger.log(&format!("apply failed: {cause}"));
    }
    settle_layout(o, &src).exit_code()
}