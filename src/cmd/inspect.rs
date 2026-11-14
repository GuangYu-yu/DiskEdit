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
use crate::cmd::CmdResult;
#[cfg(target_os = "linux")]
use crate::outcome::{RunOutcome, EXIT_OK};
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

path:
  PATH is dispatched by structure: a direct entry inside a recovery-record
  directory (`<image>.diskedit/...` or `<state_root>/recovery/<identity-hash>/...`,
  excluding `record.lock`) is itself decoded as evidence. Entries in the
  quarantine area (`.abandoned[.N]`, `.torn[.N]`) are marked as such and do not
  count as active records. Any other path is treated as a TARGET.

output:
  the evidence path, the record fields (transaction, plan, operation, progress,
  commit and retention states), or the structural failure that prevents a verdict.
  `identity check: ForeignCheckUnavailable` means the platform gave no stable
  target identity; the record is not compared.

exit status:
  0  a readable result: no record, a complete record, an unfinished record (the
     output names the rollback / set-aside commands), or an identity check that
     cannot be made
  3  torn/truncated bytes, CRC or magic mismatch, identity mismatch, a non-regular
     entry, or the anchor directory, the record, or the target cannot be read
  6  this platform does not provide the command set
  `Unverified` records are annotated and keep their verdict's exit status"#;

#[cfg(target_os = "linux")]
pub(crate) fn cmd_inspect(a: &InspectRequest) -> CmdResult {
    debug_assert!(a.part.is_none(), "the parser refuses a partition suffix for inspect");
    let path = Path::new(&a.target);
    // 先按路径结构分派：记录后端的直接子项本身就是证据，不必再当目标探测
    if let Some(spot) = crate::dev::record_spot(path) {
        return inspect_record_entry(&spot, a);
    }
    let spec = TargetSpec::new(a.target.as_str(), None);
    let probe = match HostPlatform.identify(&spec) {
        Ok(probe) => probe,
        // 身份是写命令的门槛（「写命令 → 6」）。只读命令取不到身份时
        // 标 `ForeignCheckUnavailable`：不做比对，也不因身份缺失失败
        Err(PlatformError::Unidentifiable(_)) => {
            println!("target: {}", a.target);
            println!("identity check: ForeignCheckUnavailable");
            println!("verdict: identity unavailable — the record cannot be compared");
            report_candidates(crate::dev::TargetIdentity::resolve_for_cleanup(path).as_ref());
            report_occupancy(path);
            return CmdResult::Code(EXIT_OK);
        }
        Err(e) => bail_fail(platform_failure(&a.target, e)),
    };
    let current = scene_of_target(&probe, path);
    // 记录落点由目标身份给出：镜像是其伴随文件，loop 设备是 backing 旁的文件；
    // 非 loop 块设备没有决定过的落点
    let identity = crate::dev::TargetIdentity::resolve_for_cleanup(path);
    let Some(record) = identity
        .as_ref()
        .and_then(crate::dev::TargetIdentity::record_path)
        .map(Path::to_path_buf)
    else {
        println!("target: {}", a.target);
        println!("recovery state root: no record path is resolved for this target");
        println!("verdict: nothing inspected");
        report_candidates(identity.as_ref());
        report_occupancy(path);
        return CmdResult::Code(EXIT_OK);
    };
    println!("evidence: {}", record.display());
    let evidence = read_evidence(&record, Some(&current));
    report(&evidence);
    report_header(&record, &evidence);
    report_occupancy(path);
    let (code, note) = verdict(&evidence, a);
    if let Some(note) = note {
        eprintln!("{note}");
    }
    CmdResult::Code(code)
}

/// 身份不可证明时的候选清单：按结构解码记录根下的条目并逐条报告。
///
/// 身份读不出时既拼不出块设备的记录目录名，也无法做身份比对；此时若直接返回，
/// 这些记录就对 `inspect` 消失了。候选一律按结构展示，并说明它们不能与本目标匹配
/// （列出，不认领）。候选集来自 [`crate::recovery::active_recovery_records`]：
/// `abandon` 的候选列表与这里取自同一处枚举，不各自拼落点
#[cfg(target_os = "linux")]
fn report_candidates(identity: Option<&crate::dev::TargetIdentity>) {
    let Some(identity) = identity else {
        return;
    };
    let candidates = match crate::recovery::active_recovery_records(identity, None) {
        Ok(candidates) => candidates,
        Err(why) => {
            eprintln!("cannot enumerate the candidate records: {why}");
            return;
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
    Ok(scene_of_target(&probe, Path::new(target)))
}

/// 记录目录里的一个条目：解码它本身，而不是去猜它服务的目标。
///
/// 身份核对只有 sidecar 做得到——目录名去掉 `.diskedit` 就是目标路径。主机仓库目录名是
/// `identity-hash`，不可逆，所以给 `ForeignCheckUnavailable`，不判
/// `ForeignTarget`。隔离项不参与活跃判定：它可读、可报告，但不计入未收尾现场，
/// 也不要求 `undo`
#[cfg(target_os = "linux")]
fn inspect_record_entry(spot: &crate::dev::RecordSpot, a: &InspectRequest) -> CmdResult {
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
    let (code, note) = match (&evidence, spot.quarantine) {
        (Evidence::Recognized(_), Some(mark)) => (
            EXIT_OK,
            Some(format!(
                "quarantined: this {} entry is kept as evidence and is not part of the active record set",
                mark.as_str()
            )),
        ),
        (_, Some(mark)) => {
            let note = format!(
                "quarantined: this {} entry is kept as evidence and is not part of the active record set",
                mark.as_str()
            );
            let (code, _) = verdict_with(&evidence, &a.target);
            (code, Some(note))
        }
        _ => {
            let hint = target
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<target>".to_string());
            verdict_with(&evidence, &hint)
        }
    };
    if let Some(note) = note {
        eprintln!("{note}");
    }
    CmdResult::Code(code)
}

/// 判定"同一条记录"要有与写法同一套事实：容量、载体文件与 loop 映射。探测结果只够
/// 认镜像，这里再开一次只读句柄把其余事实补齐；开不出来时退回探测结果（判据更弱，
/// 但不会把可读的记录误判成别人的）
#[cfg(target_os = "linux")]
fn scene_of_target(probe: &crate::platform::TargetProbe, path: &Path) -> SceneIdentity {
    let opened = if block_node(path) {
        crate::dev::FileSource::open_read_only(path)
    } else {
        crate::dev::FileSource::open_read_only_image(path, None)
    };
    match opened.ok().and_then(|src| {
        crate::platform::probe::probe_of(&src)
            .ok()
            .map(|probe| SceneIdentity::of_source(&probe, &src))
    }) {
        Some(scene) => scene,
        None => SceneIdentity::from_probe(probe),
    }
}

#[cfg(target_os = "linux")]
fn block_node(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    std::fs::metadata(path).map(|m| m.file_type().is_block_device()).unwrap_or(false)
}

/// 占用与挂载事实：复用 [`crate::dev::describe`] 的同一份探测，只读打开目标。
/// 打不开目标或子系统读不到都只报「不可判定」，退出码仍只由路径分派与证据状态决定
#[cfg(target_os = "linux")]
fn report_occupancy(path: &Path) {
    let opened = if block_node(path) {
        crate::dev::FileSource::open_read_only(path)
    } else {
        crate::dev::FileSource::open_read_only_image(path, None)
    };
    match opened {
        Ok(src) => {
            let (state, detail) = crate::dev::describe(&src);
            println!("occupancy: {state}");
            if let Some(detail) = detail {
                println!("occupancy evidence: {detail}");
            }
        }
        Err(e) => println!("occupancy: undecidable — the target cannot be opened read-only: {e}"),
    }
}

#[cfg(target_os = "linux")]
fn platform_failure(target: &str, err: PlatformError) -> RunOutcome {
    match err {
        PlatformError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
            RunOutcome::refused(format!("target {target} does not exist — nothing to inspect"))
        }
        PlatformError::Io(e) => RunOutcome::refused(format!("cannot read {target}: {e}")),
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
    let relative = RelativePath::try_new(name)
        .unwrap_or_else(|e| bail_fail(RunOutcome::internal(format!("companion file name is not anchor-relative: {e:?}"))));
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
        Err(EvidenceIo::Io(e) | EvidenceIo::RenamedNotDurable(e)) => Evidence::unreadable(Cause::Io(e.to_string())),
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
        Evidence::NotOurs { .. } => "not checked — the entry is not a regular record file",
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
            if let Some(name) = crate::recovery::legacy_layout(*magic) {
                println!("  layout: previous record layout ({name})");
            }
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
        Evidence::NotOurs { kind } => {
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
fn verdict(evidence: &Evidence, a: &InspectRequest) -> (u8, Option<String>) {
    verdict_with(evidence, &a.target)
}

/// `target` 只进"下一步该运行哪条命令"的措辞：目标式 inspect 用命令行参数，
/// 路径式用 sidecar 还原出的目标路径
#[cfg(target_os = "linux")]
fn verdict_with(evidence: &Evidence, target: &str) -> (u8, Option<String>) {
    match evidence {
        Evidence::Absent => (EXIT_OK, None),
        Evidence::Recognized(record) => map_recognized(record, target),
        Evidence::UnrecognizedFormat { magic, .. } => match crate::recovery::legacy_layout(*magic) {
            Some(name) => (3, Some(format!(
                "structure: the record was {}",
                crate::recovery::legacy_layout_refusal(name)
            ))),
            None => (3, Some("structure: the record's magic is not recognized or its payload does not match the layout this build reads — keep the file; `diskedit abandon` is the only command that will move it aside, and nothing may write to this target until then".to_string())),
        },
        Evidence::ForeignTarget { .. } => {
            (3, Some("identity: the record belongs to a different target — it must not be cleaned up or replayed for this target".to_string()))
        }
        Evidence::Incomplete { .. } => {
            (3, Some("structure: the record is shorter than its declared length — keep the file for manual inspection".to_string()))
        }
        Evidence::CrcMismatch { .. } => {
            (3, Some("structure: the record's CRC does not match — keep the file for manual inspection".to_string()))
        }
        Evidence::NotOurs { .. } => {
            (3, Some("path: the entry is not a regular record file — evidence paths never follow symlinks or directories".to_string()))
        }
        Evidence::Unreadable { .. } => {
            (3, Some("io: the record could not be read; no structural conclusion was reached".to_string()))
        }
    }
}

#[cfg(target_os = "linux")]
fn map_recognized(record: &Record, target: &str) -> (u8, Option<String>) {
    use crate::recovery::Retention;

    let retention_note = match record.retention {
        Retention::Unverified => Some("retention: unverified — this record is annotated but keeps its verdict's exit status".to_string()),
        Retention::Quarantined => Some("retention: quarantined — this record is annotated but keeps its verdict's exit status".to_string()),
        Retention::Active | Retention::CommittedPending => None,
    };
    // 完成与否只看提交点（`Record::committed`），未提交的其余阶段差异不影响这里的码值
    if record.committed() {
        return (EXIT_OK, retention_note);
    }
    // 回放资格与 `undo` 共用同一份 `TableReplay`：提示不会许诺 `undo` 不接受的事。
    // 相交的 move 在无许可时被 `undo` 拒绝（3），带 `--replay-table-only` 则按
    // `MoveForced` 回放表区；这里的出路是续跑、表区回放或 abandon
    let replay = crate::recovery::TableReplay::of(record, false);
    let mut note = match replay {
        crate::recovery::TableReplay::MoveBlocked => format!(
            "unfinished: the record is not committed and restoring its table could point the old layout \
             at overwritten data — `diskedit undo {target} --yes` refuses; re-run the recorded move with \
             the same request to finish the relocation, replay its partition-table regions with \
             `diskedit undo {target} --yes --replay-table-only`, or `diskedit abandon {target} --yes \
             --discard-replay` sets it aside"
        ),
        crate::recovery::TableReplay::MoveNoPreimage => format!(
            "unfinished: the record is not committed and holds no replayable copy of the old partition \
             table — `diskedit undo {target} --yes` refuses; re-run the recorded move with the same \
             request to finish the relocation, or `diskedit abandon {target} --yes --discard-replay` \
             sets it aside"
        ),
        crate::recovery::TableReplay::CopyTableOnly | crate::recovery::TableReplay::MoveClear => format!(
            "unfinished: the record is not committed and its data writes are past the point of rolling \
             back — `diskedit undo {target} --yes` replays only the recorded table regions ({replay}), or \
             `diskedit abandon {target} --yes --discard-replay` sets it aside"
        ),
        crate::recovery::TableReplay::Refused => format!(
            "unfinished: the record is not committed and past the point of rolling back — run \
             `diskedit abandon {target} --yes` to set it aside, then run the whole command again"
        ),
        crate::recovery::TableReplay::Full | crate::recovery::TableReplay::MoveForced => format!(
            "unfinished: the record is not committed — run `diskedit undo {target} --yes` to roll back, \
             or `diskedit abandon {target} --yes --discard-replay` to set it aside"
        ),
    };
    if let Some(extra) = retention_note {
        note.push('\n');
        note.push_str(&extra);
    }
    (EXIT_OK, Some(note))
}

#[cfg(target_os = "linux")]
fn describe_record(record: &Record) -> Vec<String> {
    let lines = vec![
        format!("transaction: {}", crate::support::hex_lower(&record.tx_id.bytes())),
        format!("plan: {} version {}", crate::support::hex_lower(&record.plan_id.bytes()), record.plan_version.get()),
        format!("operation: {:?}", record.op),
        format!("plan digest: {}", crate::support::hex_lower(&record.plan_digest.bytes())),
        format!(
            "progress: steps={} cursor={} fs_phase={}",
            record.progress.completed_steps, record.progress.transfer_cursor, record.progress.fs_phase
        ),
        format!("entry table steps: {}", describe_entry_table_steps(record)),
        format!("repair: {}", describe_repair(record)),
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
    let in_flight = record.entry_table_in_flight;
    let committed = reloc.entry_index as usize + usize::from(in_flight == Some(reloc.entry_index));
    match in_flight {
        Some(entry) => format!(
            "{committed}/{total} committed (entry {entry} table write in flight, counted as committed)"
        ),
        None => format!("{committed}/{total} committed"),
    }
}

#[cfg(target_os = "linux")]
fn describe_repair(record: &Record) -> String {
    use crate::recovery::{RepairKind, RepairState};

    let Some(fact) = record.repair else {
        return "none".to_string();
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
    use crate::recovery::TableFence;

    if record.relocation.is_some() {
        return "not applicable (per-entry table steps)".to_string();
    }
    match record.table_fence {
        TableFence::Pending => "pending".to_string(),
        TableFence::InFlight => "table step in flight".to_string(),
        TableFence::Landed => "landed".to_string(),
    }
}

#[cfg(target_os = "linux")]
fn describe_identity(scene: &SceneIdentity) -> String {
    match scene.id().material() {
        IdentityMaterial::Block { major, minor, device_id } => match device_id {
            Some(id) => format!("block {major}:{minor} ({id})"),
            None => format!("block {major}:{minor} (no stable device ID)"),
        },
        IdentityMaterial::Image { volume_serial, file_id } => match volume_serial {
            Some(serial) => format!("image file-id {} (volume serial {serial:#x})", crate::support::hex_lower(file_id)),
            None => format!("image file-id {}", crate::support::hex_lower(file_id)),
        },
    }
}


#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::platform::TargetId;
    use crate::recovery::{
        CommitState, CrcRegion, EntryKind, OpKind, PlanDigest, PlanId, PlanVersion, Record, RelocEntry,
        RelocKind, Relocation, Retention, TxId,
    };

    fn request(target: &str) -> InspectRequest {
        InspectRequest { target: target.to_string(), part: None }
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
                RelocEntry { partition: 1, src_first: 2048, len: 4096, delta: 2048, is_swap: false },
                RelocEntry { partition: 2, src_first: 8192, len: 2048, delta: 0, is_swap: true },
            ],
            entry_index: 1,
            chunks_in_entry: 0,
            chunk_bytes: 1 << 20,
            kind: RelocKind::Shift,
            grow_part: 0,
            disk_size: 16 * 1024 * 1024,
            ss: 512,
        });
        record.entry_table_in_flight = Some(1);
        assert_eq!(
            describe_entry_table_steps(&record),
            "2/2 committed (entry 1 table write in flight, counted as committed)"
        );
    }

    /// 记录条目不指向路径与目标认不出是两种原因，不能合成同一句
    #[test]
    fn a_record_entry_separates_no_target_from_an_unidentifiable_one() {
        let dir = crate::testdir::test_dir("inspect_scene_kinds");
        let spot = |dir: std::path::PathBuf, backend: crate::dev::RecordBackend| crate::dev::RecordSpot {
            dir,
            name: crate::dev::RECORD_NAME.into(),
            backend,
            quarantine: None,
        };

        let host = spot(dir.join("0123456789abcdef0123456789abcdef"), crate::dev::RecordBackend::HostRepo);
        assert_eq!(sidecar_scene(&host).unwrap_err(), "this record path does not name a target");

        let missing = spot(dir.join("gone.diskedit"), crate::dev::RecordBackend::Sidecar);
        let why = sidecar_scene(&missing).unwrap_err();
        assert!(
            why.starts_with("the target this record names could not be identified:"),
            "{why}"
        );
    }

    #[test]
    fn absent_is_clean() {
        assert_eq!(verdict(&Evidence::Absent, &request("disk.img")), (0, None));
    }

    #[test]
    fn committed_record_is_clean_and_unverified_only_annotates() {
        let (code, note) = verdict(
            &Evidence::Recognized(record(CommitState::Committed, Retention::Active)),
            &request("disk.img"),
        );
        assert_eq!(code, 0);
        assert!(note.is_none());

        let (code, note) = verdict(
            &Evidence::Recognized(record(CommitState::Committed, Retention::Unverified)),
            &request("disk.img"),
        );
        assert_eq!(code, 0, "Unverified must not change the verdict's exit status");
        assert!(note.unwrap().contains("unverified"));
    }

    #[test]
    fn prepared_record_is_zero_with_both_commands_named() {
        let (code, note) =
            verdict(&Evidence::Recognized(record(CommitState::Prepared, Retention::Active)), &request("disk.img"));
        assert_eq!(code, EXIT_OK);
        let note = note.unwrap();
        assert!(note.contains("diskedit undo disk.img --yes"), "{note}");
        assert!(note.contains("diskedit abandon disk.img"), "{note}");
    }

    #[test]
    fn prepared_record_past_the_rollback_point_names_only_abandon() {
        let mut r = record(CommitState::Prepared, Retention::Active);
        r.restore_blocked = true;
        r.op = OpKind::Resize;
        let (code, note) = verdict(&Evidence::Recognized(r), &request("disk.img"));
        assert_eq!(code, EXIT_OK);
        let note = note.unwrap();
        assert!(note.contains("diskedit abandon disk.img"), "{note}");
        assert!(!note.contains("diskedit undo"), "a blocked record cannot be rolled back: {note}");
    }

    #[test]
    fn prepared_unverified_record_keeps_zero_and_annotates() {
        let (code, note) = verdict(
            &Evidence::Recognized(record(CommitState::Prepared, Retention::Unverified)),
            &request("disk.img"),
        );
        assert_eq!(code, EXIT_OK);
        assert!(note.unwrap().contains("unverified"));
    }

    #[test]
    fn structural_failures_are_three() {
        let structural = [
            Evidence::UnrecognizedFormat { magic: *b"JRNL", recorded: None },
            Evidence::ForeignTarget { recorded: current(), current: current() },
            Evidence::Incomplete { observed: 3, expected: 64 },
            Evidence::CrcMismatch { region: CrcRegion::Payload },
            Evidence::NotOurs { kind: EntryKind::Symlink },
        ];
        for evidence in structural {
            assert_eq!(verdict(&evidence, &request("disk.img")).0, 3, "{evidence:?}");
        }
    }

    /// 读不出的记录退 3，且报告里点明"没有得出结论"——读失败本身不能被静默当成
    /// 结构结论或"没有记录"
    #[test]
    fn unreadable_evidence_is_three() {
        let evidence = Evidence::Unreadable {
            cause: crate::recovery::Cause::Inaccessible("permission denied".to_string()),
        };
        let (code, note) = verdict(&evidence, &request("disk.img"));
        assert_eq!(code, 3, "{note:?}");
        assert!(
            note.as_deref().is_some_and(|n| n.starts_with("io: the record could not be read")),
            "{note:?}"
        );
    }

    #[test]
    fn companion_path_sits_beside_the_image() {
        let record = |p: &str| {
            crate::dev::TargetIdentity::resolve_for_cleanup(Path::new(p))
                .and_then(|i| i.record_path().map(Path::to_path_buf))
        };
        assert_eq!(record("disk.img"), Some(Path::new("disk.img.diskedit").join("record")));
        assert_eq!(
            record("/tmp/disk.img"),
            Some(Path::new("/tmp").join("disk.img.diskedit").join("record"))
        );
    }
}
