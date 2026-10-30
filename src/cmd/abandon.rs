//! abandon：放弃一次未收尾的操作——把它的恢复现场（undo journal 与搬移 checkpoint）
//! 确定性地改名为 `*.abandoned`，让目标重新可用。
//!
//! 与 undo 的分工：undo 把 journal 回放回盘，因此要求记录可解析、且没记过不可回滚标记；
//! abandon 不回放任何字节，只放弃"还能回滚"这个能力。所以它**不依赖记录的可解析性**——
//! 一份损坏或陌生的 journal 同样必须能被释放，否则恢复路径上会出现"目标永久锁死"，
//! 而那恰恰是恢复路径最不该有的性质。
//!
//! 转换**不是**一个事务：多个独立 pathname 的文件操作没有跨文件的原子性。因此这里
//! 不追求"全部成功"，而是**单文件原子创建 + 重跑幂等 + 最终收敛**：每个现场先 hard_link
//! 成 `.abandoned` 再删原件，中断只会留下"原件还在、副本已建"的中间态——原件仍是现场，
//! 重跑一次即收敛（无论先看到哪一份，判据都是 inode 而不是文件名）。
//!
//! 转换**绝不覆盖**已有的 `.abandoned`：那是已经放弃过一次的记录，替用户丢掉它不属于
//! 本次操作的权限范围。首选名被别的文件占用时顺延到 `.abandoned.N`——既不覆盖，也不
//! 因为"名字被占了"而把目标锁死；同一个 inode 的残局则直接收敛（判据是 inode，不是文件名）

use crate::dev::{suffix_path, FileSource, Journal, RecoveryData, TargetIdentity};
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
  foreign file can still be released. The ability to roll back is lost."#;

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

    // 打开目标只为一件事：把 journal 头里的目标身份与当前目标比一遍（见 abandon_records）；
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
    // 比对 journal 头身份用的当前目标现场：目标打不开时为 None，比对跳过、现场按现状处理
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
        }
    }

    let mut failed: Vec<String> = Vec::new();
    // 首部可读、且头里的身份与当前目标不符的现场。它们照常改名（abandon 的职责是把
    // 目标从现场里释放出来），但这份凭据属于另一个目标场景，必须在总结里点名
    let mut foreign: Vec<String> = Vec::new();
    for r in &records {
        let from = r.path();
        // journal 的身份只在头里，读得出就与当前目标比一遍。checkpoint 的身份要连同
        // 记录与几何一起解析，而 abandon 不解析表，故这一族维持现状
        let mut foreign_why: Option<String> = None;
        if let RecoveryRecord::Journal { path, .. } = r
            && let (Ok(recorded), Some(live)) = (Journal::read_identity(path), scene.as_ref())
            && let Some(why) = crate::dev::scene_mismatch(&recorded, live)
        {
            eprintln!(
                "warning: {} was recorded for another target scene ({why}) — abandoning it here gives up \
                 that target's rollback evidence if it returns to this path; the record is still released \
                 so this target can be made writable again",
                path.display()
            );
            foreign_why = Some(format!("{} ({why})", path.display()));
        }
        match abandon_one(from) {
            Ok(_) => {
                if let Some(item) = foreign_why {
                    foreign.push(item);
                }
            }
            Err(e) => failed.push(format!("{}: {e}", from.display())),
        }
    }
    let foreign_note = if foreign.is_empty() {
        String::new()
    } else {
        format!(
            "; {} record(s) were recorded for another target scene, which loses that rollback evidence if it \
             returns to this path: {}",
            foreign.len(),
            foreign.join(", ")
        )
    };
    if failed.is_empty() {
        println!(
            "abandoned {} recovery record(s) on this target{foreign_note}",
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
            "abandoned {done} of {} recovery record(s) on this target; the rest failed ({}) — re-run `diskedit abandon` to converge{foreign_note}",
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
                return Ok(to);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if same_file(from, &to) {
                    std::fs::remove_file(from)?;
                    crate::dev::best_effort_dir_fsync(from);
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
                match std::fs::OpenOptions::new().write(true).create_new(true).open(&to) {
                    Ok(_) => match std::fs::rename(from, &to) {
                        Ok(()) => {
                            // 改名转换要落盘目录项：否则掉电后原件可能复活，目标重新被判成 busy
                            crate::dev::best_effort_dir_fsync(from);
                            return Ok(to);
                        }
                        Err(e) => {
                            // 占位删不掉只会留下一个空文件，不改变"这次改名失败"的结论
                            let _placeholder_gone = std::fs::remove_file(&to);
                            return Err(e);
                        }
                    },
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

/// 两个路径是否指向同一个 inode。非 unix 平台没有可靠的判定手段，一律按"不同文件"
/// 处理——保守方向是不动别人的文件（顺延一个名字），而不是猜测后覆盖
#[cfg(unix)]
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
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