//! `inspect`：只读呈现恢复现场，给出可执行结论。
//!
//! 参数按路径结构分派：路径是记录后端的直接子项时，把它本身当证据解码；
//! 否则按目标探测记录落点。命令不写目标字节，也不产生清理资格。
//! 退出码口径：0 是读出了结论（无记录、完整记录、未收尾记录，后者在输出里给出
//! 下一步命令）；3 是证据不可用（结构损坏、身份不符、条目不是普通记录文件、读不出）；
//! 6 是平台不提供命令集。`Unverified` 只做标注。

#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use crate::cli::InspectRequest;
#[cfg(target_os = "linux")]
use crate::dev::DeclaredIdentity;
#[cfg(target_os = "linux")]
use crate::outcome::RunOutcome;
#[cfg(target_os = "linux")]
use crate::platform::probe::HostPlatform;
#[cfg(target_os = "linux")]
use crate::platform::{IdentityMaterial, Platform, PlatformError, TargetSpec};
#[cfg(target_os = "linux")]
use crate::recovery::{Evidence, Record, SceneIdentity};
#[cfg(target_os = "linux")]
use crate::support::bail_fail;

pub(crate) const DETAILS: &str = r#"target:
  TARGET is a whole image or block device; a `:N` partition suffix is refused.
  `--sector-size` is not accepted: inspect reports the record, not the disk geometry.

identity:
  `--identity NAME` names a block device that exposes no WWN, EUI or repeatable
  serial (nbd, rbd, dm, md). The record is looked up under that name, and the
  identity check compares the same name. Images and devices with a stable
  identity of their own reject the option. `--identity` is not accepted for a
  PATH that is itself a record entry.

path:
  PATH is dispatched by structure: a direct entry inside a recovery-record
  directory (`<image_state_root>/images/<key-hash>/...`,
  `<image>.diskedit/...` or `<state_root>/recovery/<identity-hash>/...`,
  excluding `record.lock`) is itself decoded as evidence. Entries in the
  quarantine area (`.abandoned[.N]`) are marked as such and do not count as
  active records. Any other path is treated as a TARGET.

output:
  the evidence path, the record fields (transaction, plan, operation, progress,
  commit and retention states), or the structural failure that prevents a verdict.
  `identity check: unavailable` means the record was not compared with the
  target; the rest of the line gives the reason (no stable target identity, or
  a declared name that could not be bound to the opened target).

exit status:
  0  a readable result: no record, a complete record, an unfinished record (the
     output names the rollback / set-aside commands), or an identity check that
     cannot be made
  3  truncated bytes, CRC or magic mismatch, identity mismatch, a non-regular
     entry, or the anchor directory, the record, or the target cannot be read
  6  this platform does not provide the command set
  `Unverified` records are annotated and keep their verdict's exit status"#;

#[cfg(target_os = "linux")]
pub(crate) fn cmd_inspect(a: &InspectRequest) -> RunOutcome {
    debug_assert!(
        a.part.is_none(),
        "the parser refuses a partition suffix for inspect"
    );
    let path = Path::new(&a.target);
    // 先按路径结构分派：记录后端的直接子项本身就是证据，不必再当目标探测
    if let Some(spot) = crate::dev::record_spot(path) {
        // 声明名命名的是目标；条目本身已经是证据，没有可命名的东西
        if a.identity.is_some() {
            bail_fail(RunOutcome::usage(format!(
                "`--identity` names a target, but {} is a record entry — drop the option",
                path.display()
            )));
        }
        return inspect_record_entry(&spot, a);
    }
    let declared = a.identity.as_deref().map(|raw| {
        DeclaredIdentity::parse(raw).unwrap_or_else(|e| bail_fail(RunOutcome::usage(e)))
    });
    let spec = TargetSpec::new(a.target.as_str(), None);
    let probe = match HostPlatform.identify(&spec) {
        Ok(probe) => probe,
        // 身份是写命令的门槛（「写命令 → 6」）。只读命令取不到身份时
        // 标 `identity check: unavailable`：不做比对，也不因身份缺失失败。
        // 给了声明名时不再走这一支：名字已经指名了目标，身份仍解析不出来即拒绝
        Err(PlatformError::Unidentifiable(_)) if declared.is_none() => {
            println!("target: {}", a.target);
            println!("identity check: unavailable — the platform gave no stable target identity");
            println!("verdict: identity unavailable — the record cannot be compared");
            let failed =
                report_candidates(crate::dev::TargetIdentity::resolve_for_cleanup(path).as_ref());
            report_occupancy(path);
            if let Some(why) = failed {
                return RunOutcome::refused(why);
            }
            return RunOutcome::Checked;
        }
        Err(e) => bail_fail(platform_failure(&a.target, e)),
    };
    let current = scene_of_target(&probe, path, declared.as_ref());
    // 记录落点由目标身份给出：镜像是其状态根下的键目录，loop 设备按 backing 派生；
    // 非 loop 块设备没有决定过的落点，身份不可得的目标由 `--identity` 补上名字
    let identity = crate::dev::TargetIdentity::resolve_for_cleanup(path).map(|identity| {
        identity
            .with_declared_name(a.identity.as_deref())
            .unwrap_or_else(|e| bail_fail(RunOutcome::usage(e)))
    });
    let Some(record) = identity
        .as_ref()
        .and_then(crate::dev::TargetIdentity::record_path)
        .map(Path::to_path_buf)
    else {
        println!("target: {}", a.target);
        println!("recovery state root: no record path is resolved for this target");
        println!("verdict: nothing inspected");
        let failed = report_candidates(identity.as_ref());
        report_occupancy(path);
        if let Some(why) = failed {
            return RunOutcome::refused(why);
        }
        return RunOutcome::Checked;
    };
    let evidence = read_evidence(&record, current.as_ref());
    println!("evidence: {}", record.display());
    report(&evidence);
    if declared.is_some() && current.is_none() {
        println!(
            "identity check: unavailable — the declared identity could not be bound to the opened target"
        );
    }
    report_header(&record, &evidence);
    report_occupancy(path);
    let (outcome, note) = verdict(&evidence, a);
    if let Some(note) = note {
        eprintln!("{note}");
    }
    outcome
}

/// 身份不可证明时的候选清单：按结构解码记录根下的条目并逐条报告。
///
/// 身份读不出时既拼不出块设备的记录目录名，也无法做身份比对；此时若直接返回，
/// 这些记录就对 `inspect` 消失了。候选一律按结构展示，并说明它们不能与本目标匹配
/// （列出，不认领）。候选集来自 [`crate::recovery::active_recovery_records`]：
/// `recover --release` 的候选列表与这里取自同一处枚举，不各自拼落点
///
/// 返回候选枚举失败的原因：读不出候选集时调用方要按拒绝退出，不能把"什么都没列出来"
/// 当作"没有候选"。原因只交回调用方（由拒绝结论统一打印），不在这里另打一遍
#[cfg(target_os = "linux")]
fn report_candidates(identity: Option<&crate::dev::TargetIdentity>) -> Option<String> {
    let identity = identity?;
    let candidates = match crate::recovery::active_recovery_records(identity, None) {
        Ok(candidates) => candidates,
        Err(why) => {
            return Some(format!("cannot enumerate the candidate records: {why}"));
        }
    };
    let mut shown = 0usize;
    for record in candidates {
        let path = record.path();
        println!("candidate: {}", path.display());
        let evidence = read_evidence(path, None);
        report(&evidence);
        shown += 1;
    }
    if shown > 0 {
        eprintln!(
            "the target's identity could not be established, so none of the {shown} candidate record(s) above \
             can be matched to it — they are listed, not claimed"
        );
    }
    None
}

/// 记录条目指向的目标场景。条目本身不指向路径、名字不是 UTF-8、目标认不出是三种
/// 不同的"身份核对不可用"，分开报，不合成一句
#[cfg(target_os = "linux")]
fn sidecar_scene(spot: &crate::dev::RecordSpot) -> Result<SceneIdentity, String> {
    let Some(named) = spot.sidecar_target() else {
        return Err("this record path does not name a target".to_string());
    };
    let Some(target) = named.to_str() else {
        return Err("the target this record names is not valid UTF-8".to_string());
    };
    let probe = HostPlatform
        .identify(&TargetSpec::new(target, None))
        .map_err(|e| format!("the target this record names could not be identified: {e:?}"))?;
    Ok(scene_without_name(&probe, Path::new(target)))
}

/// 记录目录里的一个条目：解码它本身，而不是去猜它服务的目标。
///
/// 身份核对只有 sidecar 做得到——目录名去掉 `.diskedit` 就是目标路径。主机仓库目录名是
/// `identity-hash`，不可逆，所以身份不可比对（`identity check: unavailable`），不判
/// `ForeignTarget`。隔离项不参与活跃判定：它可读、可报告，但不计入未收尾现场，
/// 也不要求 `recover`
#[cfg(target_os = "linux")]
fn inspect_record_entry(spot: &crate::dev::RecordSpot, a: &InspectRequest) -> RunOutcome {
    let path = spot.path();
    println!("evidence: {}", path.display());
    println!("backend: {}", spot.backend.as_str());
    if let Some(mark) = spot.quarantine {
        println!("state: quarantined ({})", mark.as_str());
    }
    let target = spot.sidecar_target();
    let (current, unavailable) = match sidecar_scene(spot) {
        Ok(scene) => (Some(scene), None),
        Err(why) => (None, Some(why)),
    };
    let evidence = read_evidence(&path, current.as_ref());
    report(&evidence);
    report_header(&path, &evidence);
    if let Some(why) = unavailable {
        println!("identity check: unavailable — {why}");
    }
    // 可解码的隔离项直接退 0；其余保留各自的判定码，只补一条"这是隔离项"的注记。
    // 判定附带的"下一步该运行哪条命令"与目标式 inspect 同一出口输出
    let quarantined_note = spot.quarantine.map(|mark| {
        format!(
            "quarantined: this {} entry is kept as evidence and is not part of the active record set",
            mark.as_str()
        )
    });
    let (outcome, verdict_note) = if spot.quarantine.is_some() {
        match &evidence {
            Evidence::Recognized(_) => (RunOutcome::Checked, None),
            _ => verdict_with(&evidence, &a.target),
        }
    } else {
        let hint = target
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<target>".to_string());
        verdict_with(&evidence, &hint)
    };
    if let Some(note) = quarantined_note {
        eprintln!("{note}");
    }
    if let Some(note) = verdict_note {
        eprintln!("{note}");
    }
    outcome
}

/// 没有声明名时的场景：探测结果认得出目标，这里再开一次只读句柄把容量、载体文件与
/// loop 映射补齐；开不出来就用探测结果（判据更弱，但不会把可读的记录误判成别人的）
#[cfg(target_os = "linux")]
fn scene_without_name(probe: &crate::platform::TargetProbe, path: &Path) -> SceneIdentity {
    // audit-allow: S2-26 只读命令：打不开目标时退回探测证据，身份核对降级为 unavailable（src/cmd/inspect.rs:129），不产生任何写
    let Ok(src) = open_target(path, None) else {
        return SceneIdentity::from_probe(probe);
    };
    match crate::platform::probe::probe_of(&src) {
        Ok(probe) => SceneIdentity::of_source(&probe, &src),
        Err(_) => SceneIdentity::from_probe(probe),
    }
}

/// 给了声明名时，句柄必须能套上同一个名字，否则返回 `None`：记录是 `Declared` 材料，
/// 探测结果里的 `Block` 不可比
#[cfg(target_os = "linux")]
fn scene_of_target(
    probe: &crate::platform::TargetProbe,
    path: &Path,
    declared: Option<&DeclaredIdentity>,
) -> Option<SceneIdentity> {
    let Some(name) = declared else {
        return Some(scene_without_name(probe, path));
    };
    let mut src = open_target(path, None).ok()?;
    src.identity = src.identity.with_declared(name).ok()?;
    let probe = crate::platform::probe::probe_of(&src).ok()?;
    Some(SceneIdentity::of_source(&probe, &src))
}

/// 按节点形态只读打开目标；`stat` 判不出形态时带原因失败，不按镜像去开
#[cfg(target_os = "linux")]
fn open_target(path: &Path, sector_size: Option<u64>) -> Result<crate::dev::FileSource, String> {
    use crate::domain::Fact;

    let opened = match crate::dev::target_node_is_block(path) {
        Fact::Known(true) => crate::dev::FileSource::open_read_only(path),
        Fact::Known(false) | Fact::Absent => {
            crate::dev::FileSource::open_read_only_image(path, sector_size)
        }
        Fact::Undecidable(why) => {
            return Err(format!(
                "the target's node shape cannot be determined: {why}"
            ))
        }
    };
    opened.map_err(|e| format!("the target cannot be opened read-only: {e}"))
}

/// 占用与挂载事实：复用 [`crate::dev::describe`] 的同一份探测，只读打开目标。
/// 打不开目标或子系统读不到都只报「不可判定」，退出码仍只由路径分派与证据状态决定
#[cfg(target_os = "linux")]
fn report_occupancy(path: &Path) {
    match open_target(path, None) {
        Ok(src) => {
            let (state, detail) = crate::dev::describe(&src);
            println!("occupancy: {state}");
            if let Some(detail) = detail {
                println!("occupancy evidence: {detail}");
            }
        }
        Err(why) => println!("occupancy: undecidable — {why}"),
    }
}

#[cfg(target_os = "linux")]
fn platform_failure(target: &str, err: PlatformError) -> RunOutcome {
    match err {
        PlatformError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => RunOutcome::refused(
            format!("target {target} does not exist — nothing to inspect"),
        ),
        PlatformError::Io(e) => RunOutcome::refused(crate::support::cannot_read(target, e)),
        PlatformError::Unidentifiable(reason) => RunOutcome::refused(reason),
        PlatformError::Unsupported(reason) => RunOutcome::unsupported(reason),
    }
}

#[cfg(target_os = "linux")]
fn read_evidence(record: &Path, current: Option<&SceneIdentity>) -> Evidence {
    use crate::platform::evidence_store::HostEvidenceStore;
    use crate::platform::{DirFd, EvidenceIo, EvidenceStore, RelativePath};
    use crate::recovery::Cause;

    let name = record
        .file_name()
        .expect("the identity always yields a named record file")
        .to_os_string();
    let relative = RelativePath::try_new(name).unwrap_or_else(|e| {
        bail_fail(RunOutcome::internal(format!(
            "companion file name is not anchor-relative: {e:?}"
        )))
    });
    let dir = match record.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let anchor = DirFd::open_anchor(dir).unwrap_or_else(|e| {
        bail_fail(RunOutcome::refused(format!(
            "the anchor directory {} cannot be opened: {e}",
            dir.display()
        )))
    });
    let store = HostEvidenceStore;
    let file = match store.open_at(&anchor, &relative) {
        Ok(file) => file,
        Err(e) => return Evidence::from_path_error(e),
    };
    match store.read(&file) {
        Ok(bytes) => match current {
            Some(current) => Evidence::classify(&bytes, current),
            None => Evidence::classify_structure(&bytes),
        },
        Err(EvidenceIo::Io(e) | EvidenceIo::RenamedNotDurable(e)) => {
            Evidence::unreadable(Cause::Io(e.to_string()))
        }
        Err(EvidenceIo::Unsupported(reason)) => Evidence::unreadable(Cause::Unsupported(reason)),
    }
}

/// 记录文件的头事实：字节数与头部的 magic、载荷长度，以及按解码结论给出的 CRC 结果。
/// 能解码成记录或判为他人记录，说明头部与正文的 CRC 都已通过。
/// 打开条目带 `O_NONBLOCK`——条目可能是 FIFO，`open` 会等写者；带 `O_NOFOLLOW`——
/// 符号链接不代读，与证据分类的打开语义一致
#[cfg(target_os = "linux")]
fn report_header(path: &Path, evidence: &Evidence) {
    use std::os::unix::fs::OpenOptionsExt;
    let mut head = [0u8; 8];
    let read = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .and_then(|mut file| std::io::Read::read(&mut file, &mut head));
    match (&read, std::fs::metadata(path)) {
        (Ok(n), Ok(meta)) => {
            println!("  bytes: {}", meta.len());
            if *n >= 4 {
                println!("  magic: {}", crate::support::hex_lower(&head[0..4]));
            } else {
                println!("  magic: unavailable — shorter than 4 bytes");
            }
            if *n >= 8 {
                println!(
                    "  payload: {} byte(s)",
                    u32::from_le_bytes([head[4], head[5], head[6], head[7]])
                );
            } else {
                println!("  payload: unavailable — shorter than 8 bytes");
            }
        }
        _ => {
            println!("  bytes: unavailable — the record file could not be read");
            println!("  magic: unavailable — the record file could not be read");
            println!("  payload: unavailable — the record file could not be read");
        }
    }
    let crc = match evidence {
        Evidence::Recognized(_) | Evidence::ForeignTarget { .. } => "ok",
        Evidence::CrcMismatch { .. } => "mismatch",
        Evidence::Incomplete { .. } => "not checked — shorter than the declared length",
        Evidence::UnrecognizedFormat { .. } => {
            "not checked — magic is not recognized or the payload does not match this build's layout"
        }
        Evidence::NotRegular { .. } => "not checked — the entry is not a regular record file",
        _ => "unavailable — the record file could not be read",
    };
    println!("  crc: {crc}");
}

#[cfg(target_os = "linux")]
fn report(evidence: &Evidence) {
    match evidence {
        Evidence::Absent => {
            println!("record: absent");
            println!("verdict: no recovery record at the decided location");
        }
        Evidence::Recognized(record) => {
            println!("record: recognized");
            for line in describe_record(record) {
                println!("  {line}");
            }
        }
        Evidence::UnrecognizedFormat { magic, recorded } => {
            println!("record: unrecognized format");
            println!("  magic: {}", crate::support::hex_lower(magic));
            if let Some(scene) = recorded {
                println!("  recorded identity: {}", describe_identity(scene));
            } else {
                println!("  recorded identity: unavailable");
            }
        }
        Evidence::ForeignTarget { recorded, current } => {
            println!("record: foreign target");
            println!("  recorded identity: {}", describe_identity(recorded));
            println!("  current identity: {}", describe_identity(current));
        }
        Evidence::Incomplete { observed, expected } => {
            println!("record: incomplete");
            println!("  observed bytes: {observed}");
            println!("  expected bytes: {expected}");
        }
        Evidence::CrcMismatch { region } => {
            println!("record: crc mismatch");
            println!("  region: {region:?}");
        }
        Evidence::NotRegular { kind } => {
            println!("record: not ours");
            println!("  entry: {kind:?}");
        }
        Evidence::Unreadable { cause } => {
            println!("record: unreadable");
            println!("  cause: {cause:?}");
        }
    }
}

#[cfg(target_os = "linux")]
fn verdict(evidence: &Evidence, a: &InspectRequest) -> (RunOutcome, Option<String>) {
    verdict_with(evidence, &a.target)
}

/// `target` 只进"下一步该运行哪条命令"的措辞：目标式 inspect 用命令行参数，
/// 路径式用 sidecar 还原出的目标路径
#[cfg(target_os = "linux")]
fn verdict_with(evidence: &Evidence, target: &str) -> (RunOutcome, Option<String>) {
    match evidence {
        Evidence::Absent => (RunOutcome::Checked, None),
        Evidence::Recognized(record) => map_recognized(record, target),
        Evidence::UnrecognizedFormat { .. } => (
            RunOutcome::refused(
                "structure: the record's magic is not recognized or its payload does not match the layout this build reads — keep the file; `diskedit recover --release` will not quarantine it automatically, so remove it by hand once you are sure it is stale, and nothing may write to this target until then",
            ),
            None,
        ),
        Evidence::ForeignTarget { .. } => (
            RunOutcome::refused(
                "identity: the record belongs to a different target — it must not be cleaned up or replayed for this target",
            ),
            None,
        ),
        Evidence::Incomplete { .. } => (
            RunOutcome::refused(
                "structure: the record is shorter than its declared length — keep the file for manual inspection",
            ),
            None,
        ),
        Evidence::CrcMismatch { .. } => (
            RunOutcome::refused(
                "structure: the record's CRC does not match — keep the file for manual inspection",
            ),
            None,
        ),
        Evidence::NotRegular { .. } => (
            RunOutcome::refused(
                "path: the entry is not a regular record file — evidence paths never follow symlinks or directories",
            ),
            None,
        ),
        Evidence::Unreadable { .. } => (
            RunOutcome::refused(
                "io: the record could not be read; no structural conclusion was reached",
            ),
            None,
        ),
    }
}

#[cfg(target_os = "linux")]
fn map_recognized(record: &Record, target: &str) -> (RunOutcome, Option<String>) {
    use crate::recovery::Retention;

    let retention_note = match record.retention {
        Retention::Unverified => Some(
            "retention: unverified — this record is annotated but keeps its verdict's exit status"
                .to_string(),
        ),
        Retention::Quarantined => Some(
            "retention: quarantined — this record is annotated but keeps its verdict's exit status"
                .to_string(),
        ),
        Retention::Active | Retention::CommittedPending => None,
    };
    // 完成与否只看提交点（`Record::committed`），未提交的其余阶段差异不影响这里的码值
    if record.committed() {
        return (RunOutcome::Checked, retention_note);
    }
    // 回放资格与 `recover` 共用同一份 `TableReplay`：提示不会许诺 `recover` 不接受的事。
    // 相交的 move 在无许可时被 `recover` 拒绝（3），带 `--replay-table-only` 则按
    // `MoveForced` 回放表区；这里的出路是续跑、表区回放或 `recover --release`
    let replay = crate::recovery::TableReplay::of(record, false);
    let mut note = match replay {
        crate::recovery::TableReplay::MoveBlocked => format!(
            "unfinished: the record is not committed and restoring its table could point the old layout \
             at overwritten data — `diskedit recover {target} --yes` refuses; re-run the recorded move with \
             the same request to finish the relocation, replay its partition-table regions with \
             `diskedit recover {target} --yes --replay-table-only`, or `diskedit recover --release {target} --yes \
             --discard-replay` sets it aside"
        ),
        crate::recovery::TableReplay::MoveNoPreimage => format!(
            "unfinished: the record is not committed and holds no replayable copy of the old partition \
             table — `diskedit recover {target} --yes` refuses; re-run the recorded move with the same \
             request to finish the relocation, or `diskedit recover --release {target} --yes --discard-replay` \
             sets it aside"
        ),
        crate::recovery::TableReplay::CopyTableOnly | crate::recovery::TableReplay::MoveClear => format!(
            "unfinished: the record is not committed and its data writes are past the point of rolling \
             back — `diskedit recover {target} --yes` replays only the recorded table regions ({replay}), or \
             `diskedit recover --release {target} --yes --discard-replay` sets it aside"
        ),
        crate::recovery::TableReplay::Refused => format!(
            "unfinished: the record is not committed and past the point of rolling back — run \
             `diskedit recover --release {target} --yes` to set it aside, then run the whole command again"
        ),
        crate::recovery::TableReplay::Full | crate::recovery::TableReplay::MoveForced => format!(
            "unfinished: the record is not committed — run `diskedit recover {target} --yes` to roll back, \
             or `diskedit recover --release {target} --yes --discard-replay` to set it aside"
        ),
    };
    if let Some(extra) = retention_note {
        note.push('\n');
        note.push_str(&extra);
    }
    (RunOutcome::Checked, Some(note))
}

#[cfg(target_os = "linux")]
fn describe_record(record: &Record) -> Vec<String> {
    let lines = vec![
        format!(
            "transaction: {}",
            crate::support::hex_lower(&record.tx_id.bytes())
        ),
        format!(
            "plan: {} version {}",
            crate::support::hex_lower(&record.plan_id.bytes()),
            record.plan_version.get()
        ),
        format!("operation: {:?}", record.op),
        format!(
            "plan digest: {}",
            crate::support::hex_lower(&record.plan_digest.bytes())
        ),
        format!(
            "progress: steps={} cursor={} fs_phase={}",
            record.progress.completed_steps,
            record.progress.transfer_cursor,
            record.progress.fs_phase
        ),
        format!("entry table steps: {}", describe_entry_table_steps(record)),
        format!("repair: {}", describe_repair(record)),
        describe_audit(&record.audit),
        format!("table fence: {}", describe_table_fence(record)),
        format!(
            "stage: {}",
            match record.progress_stage() {
                Ok(stage) => format!("{stage:?}"),
                Err(e) => format!("inconsistent ({e})"),
            }
        ),
        format!("commit: {:?}", record.commit),
        format!("retention: {:?}", record.retention),
        format!("action: {:?}", record.action),
        format!("target: {}", describe_identity(&record.target)),
    ];
    lines
}

#[cfg(target_os = "linux")]
fn describe_entry_table_steps(record: &Record) -> String {
    let Some(reloc) = record.relocation.as_ref() else {
        return "not applicable".to_string();
    };
    let total = reloc.entries.len();
    if total == 0 {
        return "none (no relocation entries)".to_string();
    }
    // 表步已开始的条目可能已经落盘：把它计入已提交。少报会让人以为旧布局还没被
    // 改写，而对未写完的条目多报的代价只是"按已改写对待"
    match &record.entry_table_in_flight {
        crate::domain::Fact::Known(entry) => {
            let committed = reloc.entry_index as usize + usize::from(*entry == reloc.entry_index);
            format!(
                "{committed}/{total} committed (entry {entry} table write in flight, counted as committed)"
            )
        }
        crate::domain::Fact::Absent => {
            format!("{}/{} committed", reloc.entry_index as usize, total)
        }
        crate::domain::Fact::Undecidable(why) => format!(
            "{}/{} committed at least (the table step state was not decoded: {why})",
            reloc.entry_index as usize, total
        ),
    }
}

/// 审计尾段的那一行：只呈现、不参与判定。续跑与回滚不读它，这里给出它的形状，
/// 让"这条记录是谁、用哪次构建、按什么放置写的"在事后可查
#[cfg(target_os = "linux")]
fn describe_audit(audit: &crate::recovery::Audit) -> String {
    format!(
        "audit: placement={:?} worker={} tools={}",
        audit.placement,
        match audit.worker_digest {
            Some(digest) => crate::support::hex_lower(&digest[..8]),
            None => "none".to_string(),
        },
        audit.tool_versions.len()
    )
}

#[cfg(all(test, target_os = "linux"))]
mod audit_tail_tests {
    use super::describe_audit;
    use crate::recovery::Audit;

    /// 审计尾段有读路径：默认值也给出完整形状，不靠"有内容才打印"
    #[test]
    fn inspect_shows_the_audit_tail() {
        let line = describe_audit(&Audit::default());
        assert!(
            line.starts_with("audit: placement="),
            "the line names its segment: {line}"
        );
        assert!(
            line.contains("worker=none"),
            "a record written without a carrier says so: {line}"
        );
        assert!(
            line.ends_with("tools=0"),
            "the tool count is always present: {line}"
        );
    }
}

#[cfg(target_os = "linux")]
fn describe_repair(record: &Record) -> String {
    use crate::domain::Fact;
    use crate::recovery::{RepairKind, RepairState};

    let fact = match &record.repair {
        Fact::Known(fact) => fact,
        Fact::Absent => return "none".to_string(),
        Fact::Undecidable(why) => return format!("undecidable ({why})"),
    };
    let state = match fact.state {
        RepairState::InFlight => "in flight",
        RepairState::Landed => "landed",
    };
    match fact.kind {
        RepairKind::RelocateBackup | RepairKind::RelocateAndRepair => format!(
            "{} — backup header at LBA {}, last usable {}; {state}",
            fact.kind.name(),
            fact.new_backup_lba,
            fact.new_last_usable
        ),
        RepairKind::RepairProtectiveMbr => format!("{}; {state}", fact.kind.name()),
    }
}

#[cfg(target_os = "linux")]
fn describe_table_fence(record: &Record) -> String {
    use crate::domain::Fact;
    use crate::recovery::TableFence;

    if record.relocation.is_some() {
        return "not applicable (per-entry table steps)".to_string();
    }
    match &record.table_fence {
        Fact::Known(TableFence::Pending) => "pending".to_string(),
        Fact::Known(TableFence::InFlight) => "table step in flight".to_string(),
        Fact::Known(TableFence::Landed) => "landed".to_string(),
        // 记录编码总带表步围栏：解码只给出 `Known`，尾段没解出来时整条记录落到
        // `Undecidable`，所以这一支只在内存里构造的记录上出现
        Fact::Absent => "absent (the record carries no table fence)".to_string(),
        Fact::Undecidable(why) => format!("undecidable ({why})"),
    }
}

#[cfg(target_os = "linux")]
fn describe_identity(scene: &SceneIdentity) -> String {
    match scene.id().material() {
        IdentityMaterial::Block {
            major,
            minor,
            device_id,
        } => match device_id {
            Some(id) => format!("block {major}:{minor} ({id})"),
            None => format!("block {major}:{minor} (no stable device ID)"),
        },
        IdentityMaterial::Image {
            volume_serial,
            file_id,
        } => match volume_serial {
            Some(serial) => format!(
                "image file-id {} (volume serial {serial:#x})",
                crate::support::hex_lower(file_id)
            ),
            None => format!("image file-id {}", crate::support::hex_lower(file_id)),
        },
        IdentityMaterial::Declared { identity } => {
            format!("block with declared identity {identity}")
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::outcome::{EXIT_OK, EXIT_REFUSED};
    use crate::platform::TargetId;
    use crate::recovery::{
        CommitState, CrcRegion, EntryKind, OpKind, PlanDigest, PlanId, PlanVersion, Record,
        RelocEntry, RelocKind, Relocation, Retention, TxId,
    };

    fn request(target: &str) -> InspectRequest {
        InspectRequest {
            target: target.to_string(),
            part: None,
            identity: None,
        }
    }

    fn current() -> SceneIdentity {
        SceneIdentity::from_target(TargetId::from_material(
            IdentityMaterial::image(None, Some([7; 16])).unwrap(),
        ))
    }

    fn record(commit: CommitState, retention: Retention) -> Record {
        let mut record = Record::new(
            TxId::from_bytes([1; 16]),
            current(),
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(1),
            OpKind::Add,
            PlanDigest::from_bytes([3; 32]),
            crate::dev::DiskGuidState::Unavailable,
        );
        record.commit = commit;
        record.retention = retention;
        record
    }

    /// 表步已开始、游标还没推进的条目不能算作未提交：swap 的表项可能已经落盘，
    /// 少报会让人以为旧布局还没被改写
    #[test]
    fn an_entry_with_a_table_step_in_flight_counts_as_committed() {
        let mut record = record(CommitState::Prepared, Retention::Active);
        record.op = OpKind::Move;
        record.relocation = Some(Relocation {
            entries: vec![
                RelocEntry {
                    partition: 1,
                    src_first: 2048,
                    len: 4096,
                    delta: 2048,
                    is_swap: false,
                },
                RelocEntry {
                    partition: 2,
                    src_first: 8192,
                    len: 2048,
                    delta: 0,
                    is_swap: true,
                },
            ],
            entry_index: 1,
            chunks_in_entry: 0,
            prefix_digest: [0u8; 32],
            chunk_bytes: 1 << 20,
            kind: RelocKind::Shift,
            grow_part: 0,
            disk_size: 16 * 1024 * 1024,
            ss: 512,
        });
        record.entry_table_in_flight = crate::domain::Fact::Known(1);
        assert_eq!(
            describe_entry_table_steps(&record),
            "2/2 committed (entry 1 table write in flight, counted as committed)"
        );
    }

    /// 记录条目不指向路径与目标认不出是两种原因，不能合成同一句
    #[test]
    fn a_record_entry_separates_no_target_from_an_unidentifiable_one() {
        let dir = crate::testdir::test_dir("inspect_scene_kinds");
        let spot =
            |dir: std::path::PathBuf, backend: crate::dev::RecordBackend| crate::dev::RecordSpot {
                dir,
                name: crate::dev::RECORD_NAME.into(),
                backend,
                quarantine: None,
            };

        let host = spot(
            dir.join("0123456789abcdef0123456789abcdef"),
            crate::dev::RecordBackend::HostRepo,
        );
        assert_eq!(
            sidecar_scene(&host).unwrap_err(),
            "this record path does not name a target"
        );

        let missing = spot(
            dir.join("gone.diskedit"),
            crate::dev::RecordBackend::Sidecar,
        );
        let why = sidecar_scene(&missing).unwrap_err();
        assert!(
            why.starts_with("the target this record names could not be identified:"),
            "{why}"
        );
    }

    #[test]
    fn absent_is_clean() {
        let (outcome, note) = verdict(&Evidence::Absent, &request("disk.img"));
        assert_eq!(outcome.exit_code(), EXIT_OK);
        assert!(note.is_none());
    }

    #[test]
    fn committed_record_is_clean_and_unverified_only_annotates() {
        let (outcome, note) = verdict(
            &Evidence::Recognized(record(CommitState::Committed, Retention::Active)),
            &request("disk.img"),
        );
        assert_eq!(outcome.exit_code(), EXIT_OK);
        assert!(note.is_none());

        let (outcome, note) = verdict(
            &Evidence::Recognized(record(CommitState::Committed, Retention::Unverified)),
            &request("disk.img"),
        );
        assert_eq!(
            outcome.exit_code(),
            EXIT_OK,
            "Unverified must not change the verdict's exit status"
        );
        assert!(note.unwrap().contains("unverified"));
    }

    #[test]
    fn prepared_record_is_zero_with_both_commands_named() {
        let (outcome, note) = verdict(
            &Evidence::Recognized(record(CommitState::Prepared, Retention::Active)),
            &request("disk.img"),
        );
        assert_eq!(outcome.exit_code(), EXIT_OK);
        let note = note.unwrap();
        assert!(note.contains("diskedit recover disk.img --yes"), "{note}");
        assert!(
            note.contains("diskedit recover --release disk.img"),
            "{note}"
        );
    }

    #[test]
    fn prepared_record_past_the_rollback_point_names_only_abandon() {
        let mut r = record(CommitState::Prepared, Retention::Active);
        r.restore_blocked = true;
        r.op = OpKind::Resize;
        let (outcome, note) = verdict(&Evidence::Recognized(r), &request("disk.img"));
        assert_eq!(outcome.exit_code(), EXIT_OK);
        let note = note.unwrap();
        assert!(
            note.contains("diskedit recover --release disk.img"),
            "{note}"
        );
        assert!(
            !note.contains("diskedit recover disk.img"),
            "a blocked record cannot be rolled back: {note}"
        );
    }

    #[test]
    fn prepared_unverified_record_keeps_zero_and_annotates() {
        let (outcome, note) = verdict(
            &Evidence::Recognized(record(CommitState::Prepared, Retention::Unverified)),
            &request("disk.img"),
        );
        assert_eq!(outcome.exit_code(), EXIT_OK);
        assert!(note.unwrap().contains("unverified"));
    }

    /// 结构失败必须是拒绝，且原因随结论交回（不是一条裸码）
    #[test]
    fn structural_failures_are_refusals() {
        let structural = [
            Evidence::UnrecognizedFormat {
                magic: *b"JRNL",
                recorded: None,
            },
            Evidence::ForeignTarget {
                recorded: current(),
                current: current(),
            },
            Evidence::Incomplete {
                observed: 3,
                expected: 64,
            },
            Evidence::CrcMismatch {
                region: CrcRegion::Payload,
            },
            Evidence::NotRegular {
                kind: EntryKind::Symlink,
            },
        ];
        for evidence in structural {
            let (outcome, note) = verdict(&evidence, &request("disk.img"));
            assert_eq!(outcome.exit_code(), EXIT_REFUSED, "{evidence:?}");
            assert!(matches!(outcome, RunOutcome::Refused(_)), "{evidence:?}");
            assert!(note.is_none(), "{evidence:?}");
        }
    }

    /// 读不出的记录按拒绝（3）上报，且正文点明"没有得出结论"——读失败本身不能被静默
    /// 当成结构结论或"没有记录"
    #[test]
    fn unreadable_evidence_is_a_refusal_that_says_nothing_was_concluded() {
        let evidence = Evidence::Unreadable {
            cause: crate::recovery::Cause::Inaccessible("permission denied".to_string()),
        };
        let (outcome, note) = verdict(&evidence, &request("disk.img"));
        assert_eq!(outcome.exit_code(), EXIT_REFUSED);
        match &outcome {
            RunOutcome::Refused(message) => assert!(
                message.starts_with("io: the record could not be read"),
                "{message}"
            ),
            other => panic!("a read failure must be a refusal, got {other:?}"),
        }
        assert!(note.is_none());
    }

    #[test]
    fn companion_path_sits_in_the_per_user_image_repository() {
        let record = |p: &str| {
            crate::dev::TargetIdentity::resolve_for_cleanup(Path::new(p))
                .and_then(|i| i.record_path().map(Path::to_path_buf))
        };
        let expected = |p: &str| {
            crate::dev::image_record_dir(Path::new(p))
                .expect("the per-user state root is available in tests")
                .join("record")
        };
        assert_eq!(record("disk.img"), Some(expected("disk.img")));
        assert_eq!(record("/tmp/disk.img"), Some(expected("/tmp/disk.img")));
    }
}
