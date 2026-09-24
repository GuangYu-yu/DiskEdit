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
    // 身份要先解析出来才能谈锁：块设备的锁文件落点由设备层身份决定。abandon 用
    // cleanup 语义的解析——目标可能已不存在，而现场文件可能比目标活得更久
    let identity = TargetIdentity::resolve_for_cleanup(path)
        .unwrap_or_else(|| bail_fail(Fail::infra(format!("cannot determine the identity of {}", path.display()))));

    // 目标不存在时跳过锁：现场候选是兄弟文件/持久落点，锁的独占权针对的是"对目标
    // 的操作"，而目标已无可操作；跳过也避免给一个不存在的目标留下新的锁文件残骸
    let _owned = std::fs::metadata(path).is_ok()
        .then(|| TargetLock::acquire(&identity).unwrap_or_else(|f| bail_fail(f)));

    if identity.has_block_legacy_naming() {
        // 独占权来自锁文件（与镜像同一机制），不再依赖 O_EXCL 打开——abandon 只碰恢复
        // 现场不碰盘上字节，没有理由要求排他写打开。打开内容只为读表里的 Disk GUID：
        // 历史命名的 checkpoint 以它落点，不认它就会漏掉一份现场；表读不出来（abandon
        // 的常态之一）不挡这条路，按 None 处理。loop 归一后身份是 Image kind，但其
        // 历史落点按 Block 规则生成，同样要走这条补列路径
        let legacy = match FileSource::open_read_only(path) {
            Ok(src) => legacy_disk_guid(&src),
            Err(e) => {
                eprintln!("warning: cannot open {} to enumerate legacy checkpoint names ({e}) — \
                           a checkpoint under the GUID-based name will not be listed",
                    path.display());
                None
            }
        };
        abandon_records(&identity, legacy)
    } else {
        // 镜像不必打开内容：abandon 只改目标的兄弟文件，碰不到盘上字节
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
        if let Err(e) = abandon_one(from) {
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
                return Ok(to);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if same_file(from, &to) {
                    std::fs::remove_file(from)?;
                    return Ok(to);
                }
                continue;
            }
            // 该文件系统不支持硬链接（FAT/exFAT、部分网络 FS——镜像常就放在这类盘上，
            // 现场文件就在镜像旁边）：退到 rename。它会覆盖同名文件，故只在名字空闲时用；
            // 本命令持目标锁，窗口内不会有第二个 diskedit 来抢名字。这里的选择是"宁可退到
            // 会覆盖的 rename"，不能是"在不支持硬链接的盘上永远无法释放现场"
            Err(_) if !to.exists() => {
                std::fs::rename(from, &to)?;
                return Ok(to);
            }
            Err(e) => return Err(e),
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