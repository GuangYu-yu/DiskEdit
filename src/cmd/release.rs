//! `recover --release`：放弃一次未收尾的操作——把它的恢复记录移进隔离区，让目标重新可用。
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
//! 与 `recover` 默认形态的分工：默认形态把记录里的字节回放回盘，因此要求记录可解析、
//! 且没记过不可回滚标记；`--release` 形态不回放任何字节，只放弃"还能回滚"这个能力。
//!
//! 转换**不是**一个事务：多个独立 pathname 的文件操作没有跨文件的原子性。因此这里
//! 不追求"全部成功"，而是**单文件原子创建 + 重跑幂等 + 最终收敛**：每个现场先 hard_link
//! 成隔离条目再删原件，中断只会留下"原件还在、副本已建"的中间态——原件仍是现场，
//! 重跑一次即收敛（无论先看到哪一份，判据都是 inode 而不是文件名）。
//!
//! 转换**绝不覆盖**已有的隔离条目：那是已经放弃过一次的记录，替用户丢掉它不属于
//! 本次操作的权限范围。首选名被别的文件占用时顺延到 `.N`——既不覆盖，也不因为
//! "名字被占了"而把目标锁死；同一个 inode 的残局则直接收敛（判据是 inode，不是文件名）

use crate::dev::{FileSource, TargetIdentity};
use crate::support::{
    active_recovery_records, bail_fail, refuse_partition_node_target,
    RecoveryRecord, EXIT_OK, RunOutcome,
};
use crate::cli::RecoverRequest;
use crate::targetlock::TargetLock;

pub(crate) fn cmd_release(a: &RecoverRequest) -> u8 {
    // `recover --release` 释放的是整盘的恢复现场：与 info 同判据，`:` 后给了编号即拒绝，不静默忽略
    if let Some(n) = a.part {
        bail_fail(RunOutcome::refused(format!("`recover --release` releases the whole target's recovery scene — drop :{n}")));
    }
    // 回放类命令的载体必须是整盘：现场按整盘身份落点，在分区节点上释放的会是
    // 另一个（或残缺的）现场集合
    refuse_partition_node_target("recover --release", &a.target);
    let path = std::path::Path::new(&a.target);
    // 身份要先解析出来才能谈锁：块设备的锁文件落点由设备层身份决定。`recover --release` 用
    // cleanup 语义的解析——目标可能已不存在，而现场文件可能比目标活得更久
    let identity = TargetIdentity::resolve_for_cleanup(path).unwrap_or_else(|| {
        bail_fail(RunOutcome::refused(format!(
            "cannot determine the identity of '{}' — nothing was renamed; the path must resolve to the target \
             this scene was recorded for, then re-run `diskedit recover --release '{}'`",
            path.display(),
            path.display()
        )))
    });

    // 锁的独占权针对"对本目标的操作"，目标不存在也照取：隔离条目的编号分配与占位命名
    // 以"持锁 ⇒ 窗口内无并发 diskedit 抢名"为前提，跳锁会让该前提失效。
    // 锁文件是纯运行时 artifact，残留不构成阻挡（判据是锁本身，见 targetlock）
    // 锁文件落在记录目录内，目录必须先于取锁就位。目标可能已不存在，
    // 这里拿不到目标文件的所有者，镜像侧只按 0700 建目录、不做同主复核
    crate::dev::prepare_record_dir(&identity, None).unwrap_or_else(|e| {
        bail_fail(match e {
            crate::dev::HostRepoError::NotRoot => RunOutcome::unsupported(
                "the host recovery repository must be owned by uid 0; `recover --release` on a block device is \
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
    // 比对必须按同一对象打开，否则记录会被判成"另一个目标"。节点形态判不出时不按
    // 镜像去开：镜像侧的落点不在这个目标的记录目录里，枚举会空跑成"没有记录"
    let disk = match crate::dev::target_node_is_block(path) {
        crate::domain::Fact::Known(true) => FileSource::open_read_only(path).ok(),
        crate::domain::Fact::Known(false) | crate::domain::Fact::Absent => {
            FileSource::open_read_only_image(path, a.sector_size.map(|s| s.get())).ok()
        }
        crate::domain::Fact::Undecidable(why) => bail_fail(RunOutcome::no_mutation(format!(
            "whether '{}' is a block device or an image cannot be determined ({why}), so its recovery \
             scene cannot be enumerated — nothing was renamed; make the path readable, then re-run \
             `diskedit recover --release '{}'`",
            a.target, a.target
        ))),
    };
    release_records(&a.target, &identity, disk.as_ref(), a.discard_replay)
}

/// 可按逆序回放的区间条数；`None` 表示载荷损坏、条数未知。
fn replay_ranges(records: &[RecoveryRecord]) -> Option<usize> {
    let mut total = 0usize;
    for r in records {
        match r.probe.as_ref() {
            crate::recovery::CompanionProbe::Decoded(record) => {
                // 与 `recover` 同一判据：表区 preimage 仍能回写的记录才计入被放弃的回滚能力。
                // `move` 相交时 `recover` 需要显式许可，但许可存在，仍算可回写
                if crate::recovery::TableReplay::of(record, true).writes_back() {
                    total += record.restore.len();
                }
            }
            crate::recovery::CompanionProbe::Absent => {}
            crate::recovery::CompanionProbe::Damaged(_)
            | crate::recovery::CompanionProbe::Foreign
            | crate::recovery::CompanionProbe::Unreadable(_)
            | crate::recovery::CompanionProbe::NotOurs(_) => return None,
        }
    }
    Some(total)
}

fn release_records(target: &str, identity: &TargetIdentity, disk: Option<&FileSource>, discard_replay: bool) -> u8 {
    // 身份不符的候选照样参与释放：`recover --release` 是显式放弃，不是认领——它不清空目标、
    // 不回放、不阻塞，只把记录挪出活跃集。真正属于别的载体时由下面的 `same_carrier`
    // 整体拒绝，释放前逐条打印会让用户看见自己在放弃什么
    let records = match active_recovery_records(identity, disk) {
        Ok(records) => records,
        Err(why) => bail_fail(RunOutcome::refused(format!(
            "the recovery scene of '{target}' cannot be enumerated: {why} — nothing was renamed; this run \
             established nothing about the target's records"
        ))),
    };
    if records.is_empty() {
        // 幂等：目标已经是"没有 active transaction"，空跑即成功。只说这次枚举看到了什么
        println!("this enumeration found no unfinished operation on this target");
        return EXIT_OK;
    }
    // 设备声明不唯一时，记录可能属于另一块同身份的盘。改名它等于替那块盘销毁回滚凭据，
    // 而且这一步无法靠"记录写的是谁"分辨：两块的声明一模一样
    if let Some(why) = records.iter().find_map(|r| r.unclaimable.as_deref()) {
        bail_fail(RunOutcome::refused(format!(
            "the device identity of '{target}' is not unique among the visible whole disks ({why}) — a record \
             at this target's slot cannot be proven to belong to this disk, so nothing was renamed; resolve the \
             identity conflict (only one disk with that identity may be attached), then re-run `diskedit \
             recover --release '{target}'`"
        )));
    }
    // 归属判定：目标身份读不出时，记录归属无法证明，一份也不释放。
    // 只认同一载体（`same_carrier`，不是含 loop 映射的 `matches` 或完整身份比对）：容量变化
    // 或 loop 映射变化说明记录不可回放，但仍属于这个目标，`recover --release` 存在的意义正是让这种记录
    // 能被释放。逐条打印用的也是这一个口径
    let live = match disk {
        Some(d) => match crate::recovery::ownership_scene(d) {
            Ok(live) => live,
            Err(e) => bail_fail(RunOutcome::refused(format!(
                "the target's identity could not be read ({e:?}), so no recovery record can be proven to \
                 belong to it — nothing was renamed; make '{target}' readable as the target this scene was \
                 recorded for, then re-run `diskedit recover --release '{target}'`"
            ))),
        },
        None => bail_fail(RunOutcome::refused(format!(
            "the target could not be opened, so no recovery record can be proven to belong to it — nothing was \
             renamed; attach or reopen '{target}', then re-run `diskedit recover --release '{target}'`"
        ))),
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
                 and releasing it gives that up for good",
                path.display()
            ),
            crate::recovery::CompanionProbe::Foreign => eprintln!(
                "{}: recovery record in a layout this build does not recognize",
                path.display()
            ),
            crate::recovery::CompanionProbe::Unreadable(why) => eprintln!(
                "warning: {} is a recovery record that cannot be read here ({why}); releasing it gives up \
                 that rollback evidence for good",
                path.display()
            ),
            crate::recovery::CompanionProbe::NotOurs(shape) => {
                eprintln!("warning: {} is {shape}, which is not a recovery file", path.display())
            }
            crate::recovery::CompanionProbe::Absent => {}
        }
    }
    // 自动隔离要求探针解出完整记录（魔数、头 CRC、载荷 CRC、全部尾段）且记录的目标身份属于当前载体；
    // 任一项不通过就不算"归属已确认"，拒绝自动隔离，一处也不改名
    if let Some(path) = records.iter().find_map(|r| match r.probe.as_ref() {
        crate::recovery::CompanionProbe::Foreign => Some(r.path()),
        _ => None,
    }) {
        let dir = path.parent().unwrap_or(path);
        bail_fail(RunOutcome::refused(format!(
            "{} is a recovery record in a layout this build does not recognize — the header check is \
             part of ownership, so `recover --release` will not quarantine it automatically; nothing was renamed. \
             The record directory is {}; remove that file by hand",
            path.display(),
            dir.display()
        )));
    }
    // 外来条目不是本工具的恢复文件：改名它等于把用户别的东西挪走，删除更是碰不到它指向的
    // 目标。本命令只转换恢复文件，遇到这类条目就整体拒绝，一处也不改名
    if let Some((message, path)) = records
        .iter()
        .find_map(|r| r.foreign_message().map(|m| (m, r.path())))
    {
        let dir = path.parent().unwrap_or(path);
        bail_fail(RunOutcome::refused(format!(
            "{message} — `recover --release` only renames recovery files, so nothing was renamed; the record directory is \
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
            "{} — this is not the target's own recovery scene; releasing it here would destroy another \
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
                "releasing this scene gives up {n} recorded change(s) that `recover` could still replay — \
                 nothing was renamed; re-run `diskedit recover --release '{target}' --yes --discard-replay` to confirm"
            ))),
            None => bail_fail(RunOutcome::refused(format!(
                "part of this scene is not readable, so the number of changes `recover` could still replay is \
                 unknown — nothing was renamed; re-run `diskedit recover --release '{target}' --yes --discard-replay` to \
                 confirm giving that up"
            ))),
        }
    }

    let mut failed: Vec<String> = Vec::new();
    let mut released = 0usize;
    // 副本已建、原件未删的条目：目标仍被挡住，但持久状态已经改了
    let mut copied_only = 0usize;
    for r in &records {
        let from = r.path();
        match crate::dev::quarantine_move(from) {
            Ok(crate::dev::Quarantine::Moved) => released += 1,
            Ok(crate::dev::Quarantine::CopiedOnly { to, cause }) => {
                copied_only += 1;
                failed.push(format!(
                    "{}: a quarantine copy was created at {} but the original could not be removed ({cause}) — \
                     re-run to converge",
                    from.display(),
                    to.display()
                ));
            }
            Err(e) => failed.push(format!("{}: {e}", from.display())),
        }
    }
    if failed.is_empty() {
        println!(
            "released {} recovery record(s) on this target — rollback is no longer possible, the target's \
             bytes were not changed, and the target can be written again (the records are kept in the \
             quarantine area as .abandoned[.N])",
            records.len()
        );
        EXIT_OK
    } else {
        // 已经改成的那些不会退回，所以这不是"什么都没做"——但也不是盘上状态被改坏。
        // 收敛点只有一个：再跑一次，剩下的继续改。因此数量要如实报出来：
        // "放弃到哪一步了"是用户决定下一步的唯一依据。
        // 出口与其它命令同一判据：目标字节没写，但有记录文件被移进隔离区，
        // 本次运行就改变了持久状态——不是 NoMutation（4）；全部失败才是 4
        let cause = format!(
            "released {released} of {} recovery record(s) on this target; the rest failed ({}) — the target stays \
             blocked until they are released, so re-run `diskedit recover --release '{target}' --yes{}` to release them",
            records.len(),
            failed.join("; "),
            if discard_replay { " --discard-replay" } else { "" }
        );
        // 只建出副本、没删掉原件的条目同样是持久状态变化：记录目录里多了一条隔离条目
        bail_fail(crate::outcome::PersistedChange::of(false, released + copied_only > 0).failure(cause))
    }
}

/// 调用链：`release_records` 在设备声明判定的四种结论下是否动记录。枚举、判定与
/// 隔离转换都是生产代码；被替换的只有块设备打开（`test_block_source`）与 sysfs 根
/// （`dev::with_sysfs_root`），即"这台设备是谁"的事实来源
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::dev::fixtures::{attr, bind_rdev, disk};
    use std::path::PathBuf;

    struct Case {
        _dir: crate::testdir::TestDir,
        sys: PathBuf,
        state: PathBuf,
        image: PathBuf,
    }

    impl Case {
        fn new(name: &str) -> Self {
            let dir = crate::testdir::test_dir(name);
            let sys = dir.join("sys");
            let state = dir.join("state");
            let image = dir.join("target.bin");
            std::fs::write(&image, vec![0u8; 4096]).unwrap();
            Self { _dir: dir, sys, state, image }
        }

        fn disk(&self, name: &str) -> PathBuf {
            disk(&self.sys, name)
        }

        fn record_of(&self, device_id: &str) -> PathBuf {
            let key = crate::dev::key_for_test(device_id, None);
            crate::dev::record_dir_under(&self.state, &key.hash_hex()).join(crate::dev::RECORD_NAME)
        }

        fn source(&self, device_id: &str) -> FileSource {
            crate::dev::test_block_source(&self.image, &self.state, 8, 0, device_id)
                .expect("the fixture handle must open")
        }
    }

    /// 落点里放一条字节序列，只要求"存在"，不要求它能解码——`recover --release` 面对的正是这类现场
    fn plant_unreadable(case: &Case, device_id: &str) -> PathBuf {
        let record = case.record_of(device_id);
        crate::testdir::private_record_dir(record.parent().unwrap());
        std::fs::write(&record, b"not a recovery record").unwrap();
        record
    }

    /// 落点里放一条**能解码**的记录：它是否被释放因此成为可断言的事实
    fn plant(case: &Case, device_id: &str, scene: crate::recovery::SceneIdentity) -> PathBuf {
        let record = case.record_of(device_id);
        crate::testdir::private_record_dir(record.parent().unwrap());
        let mut journal = crate::recovery::RecordJournal::create_image(
            &record,
            scene,
            crate::recovery::OpKind::Add,
            512,
            crate::dev::DiskGuidState::Unavailable,
            None,
        )
        .expect("the record must be writable in the fixture slot");
        journal.barrier().expect("the record must persist");
        record
    }

    fn own_scene(case: &Case, device_id: &str) -> crate::recovery::SceneIdentity {
        let src = case.source(device_id);
        let probe = crate::platform::probe::probe_of(&src).expect("the fixture has an identity");
        crate::recovery::SceneIdentity::of_source(&probe, &src)
    }

    fn quarantined(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".abandoned"))
            .collect()
    }

    /// 拒绝走 `bail_fail` → `panic_any(ExitRequest::Report)`；测试按 `main` 的方式把
    /// 载荷解回退出码，真正的 panic 继续上抛
    fn run(case: &Case, device_id: &str, src: &FileSource) -> u8 {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::dev::with_sysfs_root(&case.sys, || {
                release_records(device_id, &src.identity, Some(src), true)
            })
        }));
        match outcome {
            Ok(code) => code,
            Err(payload) => match payload.downcast_ref::<crate::support::ExitRequest>() {
                Some(crate::support::ExitRequest::Silent(code)) => *code,
                Some(crate::support::ExitRequest::Report(report)) => report.exit_code(),
                None => std::panic::resume_unwind(payload),
            },
        }
    }

    /// 声明唯一：隔离确实发生——其余三条的"没动"由此才有对照
    #[test]
    fn a_unique_declaration_quarantines_the_record() {
        let case = Case::new("release-match");
        let node = case.disk("sda");
        attr(&node, "device/serial", "SN-1\n");
        bind_rdev(&case.sys, 8, 0, &node);
        let record = plant(&case, "SN-1", own_scene(&case, "SN-1"));
        let src = case.source("SN-1");

        assert_eq!(run(&case, "SN-1", &src), EXIT_OK);
        assert!(!record.exists(), "an accepted scene must be released");
        assert_eq!(quarantined(record.parent().unwrap()).len(), 1, "the record must be quarantined");
    }

    /// 另有设备携带同一声明：记录原样留下，不进隔离区
    #[test]
    fn an_ambiguous_declaration_leaves_the_record_in_place() {
        let case = Case::new("release-ambiguous");
        let node = case.disk("sda");
        attr(&node, "device/serial", "SN-1\n");
        bind_rdev(&case.sys, 8, 0, &node);
        let clone = case.disk("sdb");
        attr(&clone, "device/serial", "SN-1\n");
        let record = plant_unreadable(&case, "SN-1");
        let before = std::fs::read(&record).unwrap();
        let src = case.source("SN-1");

        assert_ne!(run(&case, "SN-1", &src), EXIT_OK);
        assert_eq!(std::fs::read(&record).unwrap(), before, "an ambiguous scene must not be moved");
        assert!(quarantined(record.parent().unwrap()).is_empty());
    }

    /// 记录由另一身份写下：拒绝释放，字节不变
    #[test]
    fn a_record_for_another_target_is_not_released() {
        let case = Case::new("release-mismatch");
        let node = case.disk("sda");
        attr(&node, "device/serial", "SN-1\n");
        bind_rdev(&case.sys, 8, 0, &node);
        let other = crate::recovery::SceneIdentity::from_target(crate::platform::TargetId::from_material(
            crate::platform::IdentityMaterial::image(None, Some([9; 16])).expect("a file ID"),
        ));
        let record = plant(&case, "SN-1", other);
        let before = std::fs::read(&record).unwrap();
        let src = case.source("SN-1");

        assert_ne!(run(&case, "SN-1", &src), EXIT_OK);
        assert_eq!(std::fs::read(&record).unwrap(), before, "a foreign record must not be moved");
        assert!(quarantined(record.parent().unwrap()).is_empty());
    }

    /// 设备读不出声明：无法证明记录归属，拒绝释放
    #[test]
    fn an_undeclared_device_releases_nothing() {
        let case = Case::new("release-insufficient");
        let node = case.disk("sda");
        bind_rdev(&case.sys, 8, 0, &node);
        let record = plant_unreadable(&case, "SN-1");
        let src = case.source("SN-1");

        assert_ne!(run(&case, "SN-1", &src), EXIT_OK);
        assert!(record.exists(), "a scene that cannot be proven ours must stay");
        assert!(quarantined(record.parent().unwrap()).is_empty());
    }
}
