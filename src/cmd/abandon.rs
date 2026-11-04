//! abandon：放弃一次未收尾的操作——把它的恢复现场（undo journal 与搬移 checkpoint）
//! 确定性地改名为 `*.abandoned`，让目标重新可用。
//!
//! 终态只有两种：目标不再被未完成作业占用（后续写命令可以正常开始），或明确拒绝并说明
//! 现场仍被哪一条占着。放弃的是**回滚能力**，不是数据：目标字节一个不改，记录字节以
//! `*.abandoned` 原样保留。
//!
//! 只放弃**属于本目标**的现场：记录头里的创建时目标与当前目标不符、或两族记录互不同源
//! ⇒ 整体拒绝，一处也不改名。读到身份的前提是记录可读；读不出来的记录没有可比的证据，
//! 仍按"损坏现场必须能被释放"处理——否则恢复路径上会出现"目标永久锁死"，而那恰恰是
//! 恢复路径最不该有的性质。
//!
//! 与 undo 的分工：undo 把 journal 回放回盘，因此要求记录可解析、且没记过不可回滚标记；
//! abandon 不回放任何字节，只放弃"还能回滚"这个能力。
//!
//! 转换**不是**一个事务：多个独立 pathname 的文件操作没有跨文件的原子性。因此这里
//! 不追求"全部成功"，而是**单文件原子创建 + 重跑幂等 + 最终收敛**：每个现场先 hard_link
//! 成 `.abandoned` 再删原件，中断只会留下"原件还在、副本已建"的中间态——原件仍是现场，
//! 重跑一次即收敛（无论先看到哪一份，判据都是 inode 而不是文件名）。
//!
//! 转换**绝不覆盖**已有的 `.abandoned`：那是已经放弃过一次的记录，替用户丢掉它不属于
//! 本次操作的权限范围。首选名被别的文件占用时顺延到 `.abandoned.N`——既不覆盖，也不
//! 因为"名字被占了"而把目标锁死；同一个 inode 的残局则直接收敛（判据是 inode，不是文件名）

use crate::dev::{suffix_path, FileSource, Journal, RecoveryData, SceneIdentity, TargetIdentity};
use crate::movepart::read_scene_identity;
use crate::support::{
    active_recovery_records, bail_fail, refuse_partition_node_target,
    RecoveryRecord, EXIT_OK, Fail,
};
use crate::targetlock::TargetLock;
use crate::args::Args;

pub(crate) const HELP: &str = r#"diskedit abandon <TARGET> --yes

  Give up on recovering an unfinished operation: rename its recovery state
  (undo journal, relocation checkpoint) to *.abandoned so the target can be
  written again. No byte of the target itself is touched.

  Unlike `undo` this does not need the journal to be readable — a damaged or
  unreadable file can still be released. A record that is readable but was
  recorded for another target is refused: renaming it would destroy that
  target's rollback evidence. The ability to roll back is lost."#;

pub(crate) fn cmd_abandon(a: &Args) -> u8 {
    // abandon 释放的是整盘的恢复现场：`:N` 指定了也会被静默忽略（与 info 同判据）
    if let Some(n) = a.part {
        bail_fail(Fail::refused(format!("`abandon` releases the whole target's recovery scene — drop :{n}")));
    }
    // 回放类命令的载体必须是整盘：现场按整盘身份落点，在分区节点上释放的会是
    // 另一个（或残缺的）现场集合
    refuse_partition_node_target("abandon", &a.target);
    if !a.yes {
        bail_fail(Fail::refused(
            "`abandon` gives up on recovering this target for good; pass --yes to confirm",
        ));
    }
    let path = std::path::Path::new(&a.target);
    // 身份要先解析出来才能谈锁：块设备的锁文件落点由设备层身份决定。abandon 用
    // cleanup 语义的解析——目标可能已不存在，而现场文件可能比目标活得更久
    let identity = TargetIdentity::resolve_for_cleanup(path)
        .unwrap_or_else(|| bail_fail(Fail::infra(format!("cannot determine the identity of {}", path.display()))));

    // 锁的独占权针对"对本目标的操作"，目标不存在也照取：abandon_one 的 rename 兜底
    // 以"持锁 ⇒ 窗口内无并发 diskedit 抢名"为前提，跳锁会让该前提失效。
    // 锁文件是纯运行时 artifact，残留不构成阻挡（判据是锁本身，见 targetlock）
    let _owned = TargetLock::acquire(&identity, None).unwrap_or_else(|f| bail_fail(f));

    // 打开目标只为一件事：把恢复记录头里的目标身份与当前目标比一遍（见 abandon_records）；
    // 打不开（目标已不存在）只影响这次比对，现场照常放弃。abandon 只碰恢复现场、
    // 不碰盘上字节，没有理由要求排他写打开——块设备用只读，镜像按镜像语义
    let disk = if identity.kind == crate::dev::TargetKind::Block {
        FileSource::open_read_only(path).ok()
    } else {
        FileSource::open_read_only_image(path, None).ok()
    };
    abandon_records(&identity, disk.as_ref())
}

fn abandon_records(identity: &TargetIdentity, disk: Option<&FileSource>) -> u8 {
    let records = active_recovery_records(identity);
    // 比对记录头身份用的当前目标现场：目标打不开时为 None，比对只能靠两族记录互证
    let scene = disk.map(|d| d.scene());
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
                        RecoveryData::PreImage { off, bytes, .. } => println!(
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
            RecoveryRecord::JournalOldFormat { path, version } => eprintln!(
                "warning: {} is a journal in format v{version}, older than this build (v{}); this build cannot \
                 replay or migrate it, and abandoning it gives up that rollback evidence for good",
                path.display(),
                Journal::VERSION
            ),
            RecoveryRecord::NotARecoveryFile { path, shape } => eprintln!("warning: {}", shape.message(path)),
        }
    }
    // 外来条目不是本工具的恢复文件：改名它等于把用户别的东西挪走，删除更是碰不到它指向的
    // 目标。abandon 只转换恢复文件，遇到这类条目就整体拒绝，一处也不改名
    if let Some(message) = records.iter().find_map(RecoveryRecord::foreign_message) {
        bail_fail(Fail::refused(format!(
            "{message} — abandon only renames recovery files, so nothing was renamed; move or remove it by hand"
        )));
    }

    // 现场身份：只读记录头里的创建时目标。读不出来的记录没有可比的证据，按"不可判定"处理
    // （它仍会被释放，见模块注释）；读得出却对不上的，是一份确定属于别处的现场
    let recorded: Vec<Option<SceneIdentity>> = records
        .iter()
        .map(|r| match r {
            RecoveryRecord::Journal { path, .. } => Journal::read_identity(path).ok(),
            RecoveryRecord::Checkpoint { path } => read_scene_identity(path).ok(),
            RecoveryRecord::JournalOldFormat { .. } | RecoveryRecord::NotARecoveryFile { .. } => None,
        })
        .collect();
    let mut foreign: Vec<String> = Vec::new();
    for (r, id) in records.iter().zip(&recorded) {
        let Some(id) = id else { continue };
        // 目标可打开时逐条与当前目标比；打不开时下面的互证是仅剩的同源判据
        if let Some(live) = scene.as_ref()
            && let Some(why) = crate::dev::scene_mismatch(id, live)
        {
            foreign.push(format!("{} ({why})", r.path().display()));
        }
    }
    if scene.is_none() {
        let readable: Vec<&SceneIdentity> = recorded.iter().flatten().collect();
        if let (Some(first), Some(second)) = (readable.first(), readable.get(1))
            && let Some(why) = crate::dev::recorded_scene_mismatch(first, second)
        {
            foreign.push(format!(
                "the journal and the checkpoint disagree about the target scene ({why})"
            ));
        }
    }
    // 不属于本目标的现场：改名它等于替另一个目标销毁回滚凭据。整体拒绝、一处也不改名，
    // 并说清目标仍被挡住与下一步——"看起来释放了、下次仍 busy"正是这里要消除的状态
    if !foreign.is_empty() {
        bail_fail(Fail::refused(format!(
            "{} — this is not the target's own recovery scene; abandoning it here would destroy another \
             target's rollback evidence, so nothing was renamed and this target stays blocked; move the \
             foreign record out of the recovery path by hand, then retry",
            foreign.join("; ")
        )));
    }

    let mut failed: Vec<String> = Vec::new();
    for r in &records {
        let from = r.path();
        match abandon_one(from) {
            Ok(_) => {}
            Err(e) => failed.push(format!("{}: {e}", from.display())),
        }
    }
    if failed.is_empty() {
        println!(
            "abandoned {} recovery record(s) on this target — rollback is no longer possible, the target's \
             bytes were not changed, and the target can be written again (the records are kept as *.abandoned)",
            records.len()
        );
        EXIT_OK
    } else {
        // 已经改成的那些不会退回，所以这不是"什么都没做"——但也不是盘上状态被改坏。
        // 收敛点只有一个：再跑一次，剩下的继续改。因此数量要如实报出来：
        // "放弃到哪一步了"是用户决定下一步的唯一依据。
        // 出口是 Infra（30）而非 undo 的 20：abandon 只改现场文件、不动分区表，部分完成
        // 不含"表可能已写"的含义，两个命令的出口不互相参照
        let done = records.len() - failed.len();
        bail_fail(Fail::infra(format!(
            "abandoned {done} of {} recovery record(s) on this target; the rest failed ({}) — the target stays \
             blocked until they are released, so re-run `diskedit abandon` to converge",
            records.len(),
            failed.join("; ")
        )))
    }
}

/// 第 n 个候选落点名：n=1 是首选（`<path>.abandoned`），其后是顺延名
fn abandoned_name(from: &std::path::Path, n: u32) -> std::path::PathBuf {
    if n == 1 {
        suffix_path(from, ABANDONED_SUFFIX)
    } else {
        suffix_path(from, &format!("{ABANDONED_SUFFIX}.{n}"))
    }
}

/// 转换一个现场；返回它落到哪个名字上，供调用方如实报告。
///
/// **绝不覆盖已有的 artifact**：判据是 `link(2)` 本身——它在目标已存在时原子失败
/// （这是不用 `rename` 的理由：POSIX 的 rename 会覆盖）。所以这里不先探测再建：
/// 探测与建立之间的窗口里别人可以抢先，而 link 的返回值就是权威结论。三种结局：
/// - link 成功：删掉原文件，转换完成
/// - 已存在且是**同一个 inode**：上一次 link 成功而删原文件没跑完的残局，删原文件即收敛
/// - 已存在但是**另一个文件**：那是另一次放弃留下的记录，不能覆盖也不能拒绝——拒绝会让
///   目标永久锁死（第二次中断后再无出路），那正是本命令存在的意义所在。换一个名字重试：
///   两个现场各自留下一份记录，而不是一个盖掉另一个
fn abandon_one(from: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    for n in 1..u32::MAX {
        let to = abandoned_name(from, n);
        match std::fs::hard_link(from, &to) {
            Ok(()) => {
                std::fs::remove_file(from)?;
                // 改名转换要落盘目录项：否则掉电后原件可能复活，目标重新被判成 busy
                crate::dev::best_effort_dir_fsync(from);
                crate::dev::note_rename_unverified(from, &to);
                return Ok(to);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if same_file(from, &to) {
                    std::fs::remove_file(from)?;
                    crate::dev::best_effort_dir_fsync(from);
                    crate::dev::note_rename_unverified(from, &to);
                    return Ok(to);
                }
                continue;
            }
            // 该文件系统不支持硬链接（FAT/exFAT、部分网络 FS——镜像常就放在这类盘上，
            // 现场文件就在镜像旁边）：退到 rename。rename 会覆盖同名文件，所以先用
            // create_new 原子占住名字（占位是空文件），再由 rename 覆盖本进程刚建的这个
            // 占位——"名字空闲"的判据是 create_new 的返回值，不是 stat。占位已被占用
            // （另一次放弃留下的记录）就顺延 .N。本命令持目标锁，窗口内不会有第二个
            // diskedit 来抢名字。这里的选择是"宁可退到 rename"，不能是"在不支持硬链接的
            // 盘上永远无法释放现场"
            Err(_) => {
                match crate::dev::rename_over_placeholder(from, &to) {
                    Ok(()) => {
                        // 改名转换要落盘目录项：否则掉电后原件可能复活，目标重新被判成 busy
                        crate::dev::best_effort_dir_fsync(from);
                        crate::dev::note_rename_unverified(from, &to);
                        return Ok(to);
                    }
                    // 名字已被另一份放弃记录占用：顺延 .N。占用本身不能成为拒绝理由——
                    // 拒绝会让目标永久锁死
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Err(std::io::Error::other("no free abandoned name available"))
}

/// 两个路径是否指向同一个 inode。两侧都必须先经 [`crate::dev::RecoveryFile::at`]
/// 确认是普通文件：跟随链接的话，指向原件的 `.abandoned` 链接会与原件同 inode，
/// 随后删掉原件的最后一个名字，只留下一条 dangling 链接。非 unix 平台没有可靠的
/// 判定手段，一律按"不同文件"处理——保守方向是不动别人的文件（顺延一个名字），
/// 而不是猜测后覆盖
#[cfg(unix)]
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (crate::dev::RecoveryFile::at(a), crate::dev::RecoveryFile::at(b)) {
        (Ok(crate::dev::RecoveryEntry::File(x)), Ok(crate::dev::RecoveryEntry::File(y))) => {
            (x.metadata().dev(), x.metadata().ino()) == (y.metadata().dev(), y.metadata().ino())
        }
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_a: &std::path::Path, _b: &std::path::Path) -> bool {
    false
}

/// 放弃后的落点后缀。首选名固定、无时间戳：recovery 现场的状态转换是
/// `active → abandoned` 一次到位，不是生成版本序列。只有首选名已被**另一份**记录占用时
/// 才顺延 `.N`——那是"两个不同的现场各自留下一份记录"这一事实的表达
const ABANDONED_SUFFIX: &str = ".abandoned";