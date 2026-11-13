//! abandon：放弃一次未收尾的操作——把它的恢复记录移进隔离区，让目标重新可用。
//!
//! 终态只有两种：目标不再被未完成作业占用（后续写命令可以正常开始），或明确拒绝并说明
//! 现场仍被哪一条占着。放弃的是**回滚能力**，不是数据：目标字节一个不改，记录字节以
//! 隔离条目原样保留。记录目录里的 `record` 改名为同目录的 `.abandoned[.N]`。
//!
//! 旧 journal 与旧 checkpoint 不是恢复记录：本命令不认领、不改名、不删除它们。
//! 它们不参与枚举，也不阻塞任何命令。
//! 只放弃**属于本目标**的记录：目标身份读不出（打不开、探测失败、身份材料不足）⇒ 无法
//! 证明归属，整体拒绝，一份也不释放；记录头里的目标载体与当前目标不符 ⇒ 同样整体拒绝，
//! 一处也不改名。记录字节自身读不出来不妨碍释放——否则恢复路径上会出现"目标永久锁死"，
//! 而那恰恰是恢复路径最不该有的性质。
//!
//! 与 undo 的分工：undo 把记录里的字节回放回盘，因此要求记录可解析、且没记过不可回滚
//! 标记；abandon 不回放任何字节，只放弃"还能回滚"这个能力。
//!
//! 转换**不是**一个事务：多个独立 pathname 的文件操作没有跨文件的原子性。因此这里
//! 不追求"全部成功"，而是**单文件原子创建 + 重跑幂等 + 最终收敛**：每个现场先 hard_link
//! 成隔离条目再删原件，中断只会留下"原件还在、副本已建"的中间态——原件仍是现场，
//! 重跑一次即收敛（无论先看到哪一份，判据都是 inode 而不是文件名）。
//!
//! 转换**绝不覆盖**已有的隔离条目：那是已经放弃过一次的记录，替用户丢掉它不属于
//! 本次操作的权限范围。首选名被别的文件占用时顺延到 `.N`——既不覆盖，也不因为
//! "名字被占了"而把目标锁死；同一个 inode 的残局则直接收敛（判据是 inode，不是文件名）

use std::path::Path;

use crate::dev::{FileSource, TargetIdentity};
use crate::support::{
    active_recovery_records, bail_fail, refuse_partition_node_target,
    RecoveryRecord, EXIT_OK, RunOutcome,
};
use crate::cli::AbandonRequest;
use crate::targetlock::TargetLock;

/// 详助正文：用法行与选项块由 `cli` 的 schema 生成，这里只写命令语义
pub(crate) const DETAILS: &str = r#"  Give up on recovering an unfinished operation: move its recovery record into
  the record directory's quarantine area (.abandoned[.N]) so the target can be
  written again. No byte of the target itself is touched.

  Unlike `undo` this does not need the record to be readable — a damaged or
  unreadable file can still be released. A record that is readable but was
  recorded for another target is refused: renaming it would destroy that
  target's rollback evidence. The ability to roll back is lost.

  A legacy undo journal or an old relocation checkpoint is not a recovery
  record; this command leaves such files in place.

  When the scene still holds at least one range `undo` could replay — or the
  payload is damaged, so the count is unknown — the command refuses until
  --discard-replay is given. A scene with nothing left to replay needs no
  flag."#;

pub(crate) fn cmd_abandon(a: &AbandonRequest) -> u8 {
    // abandon 释放的是整盘的恢复现场：`:N` 指定了也会被静默忽略（与 info 同判据）
    if let Some(n) = a.part {
        bail_fail(RunOutcome::refused(format!("`abandon` releases the whole target's recovery scene — drop :{n}")));
    }
    // 回放类命令的载体必须是整盘：现场按整盘身份落点，在分区节点上释放的会是
    // 另一个（或残缺的）现场集合
    refuse_partition_node_target("abandon", &a.target);
    let path = std::path::Path::new(&a.target);
    // 身份要先解析出来才能谈锁：块设备的锁文件落点由设备层身份决定。abandon 用
    // cleanup 语义的解析——目标可能已不存在，而现场文件可能比目标活得更久
    let identity = TargetIdentity::resolve_for_cleanup(path)
        .unwrap_or_else(|| bail_fail(RunOutcome::no_mutation(format!("cannot determine the identity of {}", path.display()))));

    // 锁的独占权针对"对本目标的操作"，目标不存在也照取：隔离条目的编号分配与占位命名
    // 以"持锁 ⇒ 窗口内无并发 diskedit 抢名"为前提，跳锁会让该前提失效。
    // 锁文件是纯运行时 artifact，残留不构成阻挡（判据是锁本身，见 targetlock）
    // 锁文件落在记录目录内，目录必须先于取锁就位。目标可能已不存在，
    // 这里拿不到目标文件的所有者，镜像侧只按 0700 建目录、不做同主复核
    crate::dev::prepare_record_dir(&identity, None).unwrap_or_else(|e| {
        bail_fail(match e {
            crate::dev::HostRepoError::NotRoot => RunOutcome::unsupported(
                "the host recovery repository must be owned by uid 0; `abandon` on a block device is \
                 refused when this process is not root",
            ),
            crate::dev::HostRepoError::Insecure(detail) if identity.kind == crate::dev::TargetKind::Block => {
                RunOutcome::unsupported(format!("the host recovery repository is refused: {detail}"))
            }
            crate::dev::HostRepoError::Insecure(detail) => {
                RunOutcome::refused(format!("the recovery directory is refused: {detail}"))
            }
            crate::dev::HostRepoError::Io(e) => {
                RunOutcome::no_mutation(format!("cannot prepare the recovery directory: {e}"))
            }
        })
    });
    let _owned = TargetLock::acquire(&identity, None).unwrap_or_else(|f| bail_fail(f));

    // 记录头的身份来自写句柄本身：块设备取节点的 rdev/序列号，镜像取文件身份。
    // loop 设备的记录落在 backing 旁，但写入时打开的是设备节点，身份也按节点取；
    // 比对必须按同一对象打开，否则记录会被判成"另一个目标"
    let disk = if target_is_block_node(path) {
        FileSource::open_read_only(path).ok()
    } else {
        FileSource::open_read_only_image(path, None).ok()
    };
    abandon_records(&identity, disk.as_ref(), a.discard_replay)
}

/// 可按逆序回放的区间条数；`None` 表示载荷损坏、条数未知。
fn replay_ranges(records: &[RecoveryRecord]) -> Option<usize> {
    let mut total = 0usize;
    for r in records {
        match r.probe.as_ref() {
            crate::recovery::CompanionProbe::Decoded(record) => {
                // 与 undo 的豁免同一口径：Move/Copy 的 restore_blocked 只封锁
                // 数据区，表区 preimage 仍可回写。restore_blocked 按 0 计会让 abandon
                // 静默跳过 --discard-replay 的确认，无声放弃表区回滚能力
                if !record.restore_blocked || crate::recovery::table_only_replay(record.op) {
                    total += record.restore.len();
                }
            }
            crate::recovery::CompanionProbe::Absent => {}
            crate::recovery::CompanionProbe::Damaged(_)
            | crate::recovery::CompanionProbe::Foreign { .. }
            | crate::recovery::CompanionProbe::Unreadable(_)
            | crate::recovery::CompanionProbe::NotOurs(_) => return None,
        }
    }
    Some(total)
}

#[cfg(not(target_os = "linux"))]
fn target_is_block_node(_path: &Path) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn target_is_block_node(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    std::fs::metadata(path).map(|m| m.file_type().is_block_device()).unwrap_or(false)
}

fn abandon_records(identity: &TargetIdentity, disk: Option<&FileSource>, discard_replay: bool) -> u8 {
    // 身份不符的候选照样参与释放：abandon 是显式放弃，不是认领——它不清空目标、
    // 不回放、不阻塞，只把记录挪出活跃集。真正属于别的载体时由下面的 `same_carrier`
    // 整体拒绝，释放前逐条打印会让用户看见自己在放弃什么
    let records = active_recovery_records(identity, disk);
    if records.is_empty() {
        // 幂等：目标已经是"没有 active transaction"，空跑即成功
        println!("nothing to abandon: no unfinished operation on this target");
        // 旧 journal 不是恢复记录：它不阻塞命令，也不由本命令释放
        if std::fs::symlink_metadata(identity.journal_path()).is_ok() {
            println!(
                "note: a legacy undo journal exists at {}; it is not a recovery record, so `abandon` leaves it \
                 in place — handle it by hand",
                identity.journal_path().display()
            );
        }
        return EXIT_OK;
    }
    // 归属判定：目标身份读不出时，记录归属无法证明，一份也不释放。
    // 只认同一载体（`same_carrier`，不是含 loop 映射的 `matches` 或完整身份比对）：容量变化
    // 或 loop 映射变化说明记录不可回放，但仍属于这个目标，abandon 存在的意义正是让这种记录
    // 能被释放。逐条打印用的也是这一个口径
    let live = match disk {
        Some(d) => match crate::recovery::ownership_scene(d) {
            Ok(live) => live,
            Err(e) => bail_fail(RunOutcome::refused(format!(
                "the target's identity could not be read ({e:?}), so no recovery record can be proven to \
                 belong to it — nothing was renamed; inspect the target and the record with `diskedit inspect` \
                 before retrying"
            ))),
        },
        None => bail_fail(RunOutcome::refused(
            "the target could not be opened, so no recovery record can be proven to belong to it — nothing was \
             renamed; inspect the target and the record with `diskedit inspect` before retrying",
        )),
    };
    // 先把要放弃的东西如实报出来。放弃的是一种能力而非数据，所以解析不出来的那些
    // 尤其要说清楚：用户无法知道自己在放弃什么
    for r in &records {
        let path = r.path();
        match r.probe.as_ref() {
            crate::recovery::CompanionProbe::Decoded(record) => {
                if !record.target.same_carrier(&live) {
                    println!(
                        "{}: recorded identity is a different carrier than the current target",
                        path.display()
                    );
                }
                println!(
                    "{}: recovery record from {:?}, {} written range(s)",
                    path.display(),
                    record.op,
                    record.restore.len()
                );
                if record.restore_blocked {
                    // Move/Copy 仍能回写表区：笼统说"越过不可回滚点"
                    // 对这两条命令是不实陈述
                    let detail = match record.op {
                        crate::recovery::OpKind::Move | crate::recovery::OpKind::Copy => {
                            "data writes are past the point of no return; \
                             only the table regions can still be rolled back"
                        }
                        _ => "already past the point of rolling back",
                    };
                    println!("    {detail}");
                }
            }
            crate::recovery::CompanionProbe::Damaged(why) => eprintln!(
                "warning: {} is not readable as a recovery record ({why}) — what it could roll back is unknown, \
                 and abandoning it gives that up for good",
                path.display()
            ),
            crate::recovery::CompanionProbe::Foreign { magic } => eprintln!(
                "{}: {}",
                path.display(),
                match crate::recovery::legacy_layout(*magic) {
                    Some(name) => format!(
                        "recovery record in the previous layout ({name}); this build reads the current layout only"
                    ),
                    None => "recovery record in a layout this build does not recognize".to_string(),
                }
            ),
            crate::recovery::CompanionProbe::Unreadable(why) => eprintln!(
                "warning: {} is a recovery record that cannot be read here ({why}); abandoning it gives up \
                 that rollback evidence for good",
                path.display()
            ),
            crate::recovery::CompanionProbe::NotOurs(shape) => {
                eprintln!("warning: {} is {shape}, which is not a recovery file", path.display())
            }
            crate::recovery::CompanionProbe::Absent => {}
        }
    }
    // 自动隔离要求头、身份段 CRC、身份链全部通过。头读不懂就不算
    // "归属已确认"，拒绝自动隔离，一处也不改名
    if let Some(path) = records.iter().find_map(|r| match r.probe.as_ref() {
        crate::recovery::CompanionProbe::Foreign { .. } => Some(r.path()),
        _ => None,
    }) {
        let dir = path.parent().unwrap_or(path);
        bail_fail(RunOutcome::refused(format!(
            "{} is a recovery record in a layout this build does not recognize — the header check is \
             part of ownership, so `abandon` will not quarantine it automatically; nothing was renamed. \
             The record directory is {}; remove that file by hand",
            path.display(),
            dir.display()
        )));
    }
    // 外来条目不是本工具的恢复文件：改名它等于把用户别的东西挪走，删除更是碰不到它指向的
    // 目标。abandon 只转换恢复文件，遇到这类条目就整体拒绝，一处也不改名
    if let Some((message, path)) = records
        .iter()
        .find_map(|r| r.foreign_message().map(|m| (m, r.path())))
    {
        let dir = path.parent().unwrap_or(path);
        bail_fail(RunOutcome::refused(format!(
            "{message} — abandon only renames recovery files, so nothing was renamed; the record directory is \
             {}; remove that file by hand",
            dir.display()
        )));
    }

    let foreign: Vec<String> = records
        .iter()
        .filter_map(|r| match r.probe.as_ref() {
            crate::recovery::CompanionProbe::Decoded(record) if !record.target.same_carrier(&live) => {
                Some(format!("{} (recorded for a different target)", r.path().display()))
            }
            _ => None,
        })
        .collect();
    // 不属于本目标的现场：改名它等于替另一个目标销毁回滚凭据。整体拒绝、一处也不改名，
    // 并说清目标仍被挡住与下一步——"看起来释放了、下次仍 busy"正是这里要消除的状态
    if !foreign.is_empty() {
        bail_fail(RunOutcome::refused(format!(
            "{} — this is not the target's own recovery scene; abandoning it here would destroy another \
             target's rollback evidence, so nothing was renamed and this target stays blocked; move the \
             foreign record out of the recovery path by hand, then retry",
            foreign.join("; ")
        )));
    }

    // 放弃还能逆序回放的区间就是放弃回滚能力，必须显式确认
    if !discard_replay {
        match replay_ranges(&records) {
            Some(0) => {}
            Some(n) => bail_fail(RunOutcome::refused(format!(
                "abandoning this scene gives up {n} recorded change(s) that `undo` could still replay — \
                 add --discard-replay to confirm"
            ))),
            None => bail_fail(RunOutcome::refused(
                "part of this scene is not readable, so the number of changes `undo` could still replay is \
                 unknown — add --discard-replay to confirm giving that up",
            )),
        }
    }

    let mut failed: Vec<String> = Vec::new();
    for r in &records {
        let from = r.path();
        match crate::dev::quarantine_move(from, crate::dev::QUARANTINE_ABANDONED) {
            Ok(_) => {}
            Err(e) => failed.push(format!("{}: {e}", from.display())),
        }
    }
    if failed.is_empty() {
        println!(
            "abandoned {} recovery record(s) on this target — rollback is no longer possible, the target's \
             bytes were not changed, and the target can be written again (the records are kept in the \
             quarantine area as .abandoned[.N])",
            records.len()
        );
        EXIT_OK
    } else {
        // 已经改成的那些不会退回，所以这不是"什么都没做"——但也不是盘上状态被改坏。
        // 收敛点只有一个：再跑一次，剩下的继续改。因此数量要如实报出来：
        // "放弃到哪一步了"是用户决定下一步的唯一依据。
        // 出口是 NoMutation（4）而非 undo 的 Partial（5）：abandon 只改现场文件、不动分区表，
        // 部分完成不含"表可能已写"的含义，两个命令的出口不互相参照
        let done = records.len() - failed.len();
        bail_fail(RunOutcome::no_mutation(format!(
            "abandoned {done} of {} recovery record(s) on this target; the rest failed ({}) — the target stays \
             blocked until they are released, so re-run `diskedit abandon` to release them",
            records.len(),
            failed.join("; ")
        )))
    }
}
