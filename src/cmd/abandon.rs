//! abandon：放弃一次未收尾的操作——把它的恢复现场（undo journal 与搬移 checkpoint）
//! 确定性地改名为 `*.abandoned`，让目标重新可用。
//!
//! 与 undo 的分工：undo 把 journal 回放回盘，因此要求记录可解析、且没记过不可回滚标记；
//! abandon 不回放任何字节，只放弃"还能回滚"这个能力。所以它**不依赖记录的可解析性**——
//! 一份损坏或陌生的 journal 同样必须能被释放，否则恢复路径上会出现"目标永久锁死"，
//! 而那恰恰是恢复路径最不该有的性质。
//!
//! 改名**不是**一个事务：文件系统不能让多个独立 pathname 的 rename 变成一次原子操作。
//! 因此这里不追求"全部成功"，而是**单文件原子转换 + 重跑幂等 + 最终收敛**：
//! 中途崩溃后再次运行会继续收敛（已改成 `.abandoned` 的不再是现场，剩余的下一次补齐）

use crate::dev::{suffix_path, FileSource, RecoveryData, TargetIdentity};
use crate::support::{
    active_recovery_records, bail_fail, legacy_disk_guid, RecoveryRecord, EXIT_OK, Fail,
};
use crate::targetlock::TargetLock;
use crate::args::Args;

pub(crate) const HELP: &str = r#"diskedit abandon <TARGET> --yes

  Give up on recovering an unfinished operation: rename its recovery state
  (undo journal, relocation checkpoint) to *.abandoned so the target can be
  written again. No byte of the target itself is touched.

  Unlike `undo` this does not need the journal to be readable — a damaged or
  foreign file can still be released. The ability to roll back is lost."#;

pub(crate) fn cmd_abandon(a: &Args) -> u8 {
    if !a.yes {
        bail_fail(Fail::refused(
            "`abandon` gives up on recovering this target for good; pass --yes to confirm",
        ));
    }
    let path = std::path::Path::new(&a.target);
    // 身份要先解析出来才能谈锁：块设备的锁文件落点由设备层身份决定
    let identity = TargetIdentity::resolve_path(path)
        .unwrap_or_else(|| bail_fail(Fail::infra(format!("cannot determine the identity of {}", path.display()))));

    if identity.is_block() {
        // 块设备的独占凭据只能是 O_EXCL 打开：有人正在操作就打开失败（拒绝），成功则顺带
        // 读得到表里的 Disk GUID——历史命名的 checkpoint 以它落点，不认它就会漏掉一份现场
        let src = FileSource::open(path, a.sector_size)
            .unwrap_or_else(|e| bail_fail(Fail::infra(format!("open failed: {e}"))));
        let legacy = legacy_disk_guid(&src);
        let _owned = TargetLock::acquire(&src.identity).unwrap_or_else(|f| bail_fail(f));
        abandon_records(&src.identity, legacy)
    } else {
        // 镜像不必打开内容：abandon 只改目标的兄弟文件，碰不到盘上字节
        let _owned = TargetLock::acquire(&identity).unwrap_or_else(|f| bail_fail(f));
        abandon_records(&identity, None)
    }
}

fn abandon_records(identity: &TargetIdentity, legacy: Option<[u8; 16]>) -> u8 {
    let records = active_recovery_records(identity, legacy);
    if records.is_empty() {
        // 幂等：目标已经是"没有 active transaction"，空跑即成功
        println!("nothing to abandon: no unfinished operation on this target");
        return EXIT_OK;
    }
    // 先把要放弃的东西如实报出来。放弃的是一种能力而非数据，所以解析不出来的那些
    // 尤其要说清楚：用户无法知道自己在放弃什么
    for r in &records {
        match r {
            RecoveryRecord::Journal { path, entries: Some(v) } => {
                println!("{}: undo journal, {} recorded change(s)", path.display(), v.len());
                // 清单按记录里的 mutation 说人话：放弃的是"还能回滚"这个能力，
                // 用户得看得见自己放弃的是哪一类改动
                for rec in v {
                    match &rec.recovery {
                        RecoveryData::Barrier => println!(
                            "    {} — already past the point of rolling back",
                            rec.mutation.describe()
                        ),
                        RecoveryData::PreImage { off, bytes } => println!(
                            "    {} ({} byte(s) at offset {off})",
                            rec.mutation.describe(),
                            bytes.len()
                        ),
                    }
                }
            }
            RecoveryRecord::Journal { path, entries: None } => eprintln!(
                "warning: {} is not readable as a journal — what it could roll back is unknown, \
                 and abandoning it gives that up for good",
                path.display()
            ),
            RecoveryRecord::Checkpoint { path } => println!(
                "{}: relocation checkpoint from an interrupted move \
                 (re-run the command that started it to resume instead)",
                path.display()
            ),
        }
    }

    let mut failed: Vec<String> = Vec::new();
    for r in &records {
        let from = r.path();
        let to = suffix_path(from, ABANDONED_SUFFIX);
        if let Err(e) = std::fs::rename(from, &to) {
            failed.push(format!("{}: {e}", from.display()));
        }
    }
    if failed.is_empty() {
        println!("abandoned {} recovery record(s) on this target", records.len());
        EXIT_OK
    } else {
        // 已经改成的那些不会退回，所以这不是"什么都没做"——但也不是盘上状态被改坏。
        // 收敛点只有一个：再跑一次，剩下的继续改
        bail_fail(Fail::infra(format!(
            "could not abandon every recovery record ({}); re-run `diskedit abandon` to converge",
            failed.join("; ")
        )))
    }
}

/// 放弃后的落点名。固定、无时间戳、无序号：recovery 现场的状态转换是
/// `active → abandoned` 一次到位，不是生成无穷历史
const ABANDONED_SUFFIX: &str = ".abandoned";