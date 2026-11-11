//! `records`：记录仓库的只读视图，三个子命令共用同一套发现与状态判定。
//!
//! - `records list [TARGET]`：列出可解码的记录；省略目标时列主机仓库里的全部记录。
//!   未知格式不进列表，隔离项标出但不参与活跃判定。
//! - `records verify <ID> <TARGET>`：按记录编号在记录后端里定位唯一一条，并对目标重新
//!   探测身份。未找到或命中两处及以上都退 3，文案区分。
//! - `records scan <ROOT>`：根是容器目录——块设备侧 `<state_root>/recovery`，镜像侧是
//!   镜像所在目录。深度 ≤3、单文件 ≤64 MiB、总量 ≤512 MiB、30 s；任一超出即"结果不完整"
//!   退 4，不返回半份结果。
//!
//! 没有索引：条目全部在内存里枚举并标 `registered: false`。因此
//! `LocationUnavailable` 不会出现——那是"索引登记了而文件不在"的状态，没有索引就没有
//! 这份知识，不能凭空造。
//!
//! 输出只有 JSON（与 `info` 一致）：键名 snake_case，缺值写 `null`，不省略键；不提供
//! `--format` / `--json` 开关。

// 命令本体只在 Linux 上编译；帮助常量仍要在所有平台供命令表引用
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::cli::{RecordsListRequest, RecordsScanRequest, RecordsVerifyRequest};
use crate::cmd::CmdResult;
use crate::dev::{RecordBackend, TargetIdentity};
use crate::outcome::{RunOutcome, EXIT_NO_MUTATION, EXIT_OK, EXIT_REFUSED};
use crate::recovery::{Evidence, Record};
use crate::support::{bail_fail, json_escape};

pub(crate) const DETAILS_LIST: &str = r#"  List recovery records as JSON. Only decodable records are listed; an entry in
  an unknown format is left out (use `records scan` to see it). Quarantine
  entries (`.abandoned[.N]`, `.torn[.N]`) are listed and marked, but they are not
  part of the active record set.

  There is no index yet: every row carries "registered": false, and records are
  enumerated in memory. Sidecars that are not addressed by TARGET are not
  searched — pass TARGET to see one, or use `records scan` with the directory
  that holds the images."#;

pub(crate) const DETAILS_VERIFY: &str = r#"  Locate the record whose id is ID (32 lowercase hex digits, as printed by
  `records list`), re-probe the identity of TARGET, and print the verdict as
  JSON.

  Exit status:
    0  exactly one record, and the identity check did not RunOutcome
    3  the id was not found (A4), or it exists in two or more places, or the
       single record fails its identity / backend / structure check
    4  the record, its directory, or the target could not be read"#;

pub(crate) const DETAILS_SCAN: &str = r#"  Enumerate records under a container root and print them as JSON.

  ROOT is one of:
    <state_root>/recovery   every direct child is an identity-hash record directory
    <image directory>       every direct child named <name>.diskedit is a record directory

  A subdirectory of <state_root> other than `recovery` is refused: a record
  directory is not a container. Scanning is bounded to depth 3, 64 MiB per file,
  512 MiB in total and 30 seconds. Exceeding a bound reports an incomplete
  result and exits 4; no partial list is printed.

  There is no index yet: every row carries "registered": false."#;

/// 单个文件的读取上限
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// 一次枚举的读取总量上限
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// 一次枚举的时间上限
const MAX_ELAPSED: Duration = Duration::from_secs(30);

/// 七态。`LocationUnavailable` 需要索引才能产生，本版没有索引
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Valid,
    Damaged,
    IdentityMismatch,
    Unfinished,
    UnknownFormat,
    Unreadable,
    BackendMismatch,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Damaged => "damaged",
            Self::IdentityMismatch => "identity_mismatch",
            Self::Unfinished => "unfinished",
            Self::UnknownFormat => "unknown_format",
            Self::Unreadable => "unreadable",
            Self::BackendMismatch => "backend_mismatch",
        }
    }

    /// `records list` 是否收录该状态（不含未知格式）
    fn listed(self) -> bool {
        !matches!(self, Self::UnknownFormat)
    }
}

/// 一条证据的呈现：路径、后端、隔离标记、解码结果与身份核对结论
struct Row {
    path: PathBuf,
    backend: RecordBackend,
    quarantine: Option<&'static str>,
    evidence: Evidence,
    identity_checked: bool,
}

impl Row {
    fn state(&self) -> Option<State> {
        let state = match &self.evidence {
            Evidence::Absent => return None,
            Evidence::Recognized(record) => {
                if backend_mismatch(record, self.backend) {
                    State::BackendMismatch
                } else if record.committed() {
                    State::Valid
                } else {
                    State::Unfinished
                }
            }
            Evidence::ForeignTarget { .. } => State::IdentityMismatch,
            Evidence::UnrecognizedFormat { .. } => State::UnknownFormat,
            Evidence::Incomplete { .. } | Evidence::CrcMismatch { .. } | Evidence::NotOurs { .. } => {
                State::Damaged
            }
            Evidence::Unreadable { .. } => State::Unreadable,
        };
        Some(state)
    }

    fn json(&self) -> String {
        let state = self.state();
        let record = match &self.evidence {
            Evidence::Recognized(record) => Some(record),
            _ => None,
        };
        format!(
            "{{\"id\":{},\"path\":\"{}\",\"backend\":\"{}\",\"state\":{},\"quarantine\":{},\
             \"op\":{},\"ranges\":{},\"commit\":{},\"retention\":{},\"identity_check\":\"{}\",\
             \"registered\":false}}",
            opt_str(record.map(|r| crate::support::hex_lower(&r.tx_id.bytes()))),
            json_escape(&self.path.to_string_lossy()),
            self.backend.as_str(),
            opt_str(state.map(|s| s.as_str().to_string())),
            opt_str(self.quarantine.map(str::to_string)),
            opt_str(record.map(|r| op_name(r.op).to_string())),
            record.map(|r| r.restore.len().to_string()).unwrap_or_else(|| "null".into()),
            opt_str(record.map(|r| match r.commit {
                crate::recovery::CommitState::Prepared => "prepared".to_string(),
                crate::recovery::CommitState::Committed => "committed".to_string(),
            })),
            opt_str(record.map(|r| retention_name(r.retention).to_string())),
            if self.identity_checked { "checked" } else { "unavailable" },
        )
    }
}

fn opt_str(value: Option<String>) -> String {
    match value {
        Some(v) => format!("\"{}\"", json_escape(&v)),
        None => "null".to_string(),
    }
}

fn op_name(op: crate::recovery::OpKind) -> &'static str {
    use crate::recovery::OpKind;
    match op {
        OpKind::New => "new",
        OpKind::Add => "add",
        OpKind::Delete => "delete",
        OpKind::ResizePart => "resize-part",
        OpKind::Move => "move",
        OpKind::Copy => "copy",
        OpKind::Mkfs => "mkfs",
        OpKind::ResizeFs => "resizefs",
        OpKind::Set => "set",
        OpKind::Resize => "resize",
    }
}

fn retention_name(retention: crate::recovery::Retention) -> &'static str {
    use crate::recovery::Retention;
    match retention {
        Retention::Active => "active",
        Retention::CommittedPending => "committed_pending",
        Retention::Unverified => "unverified",
        Retention::Quarantined => "quarantined",
    }
}

/// 记录头的身份形态与它所在的后端不符：sidecar 只服务镜像，主机仓库只服务块设备。
/// 形态判据取记录头里的 `IdentityMaterial`，不靠目录名猜
fn backend_mismatch(record: &Record, backend: RecordBackend) -> bool {
    let recorded_is_block = matches!(
        record.target.id().material(),
        crate::platform::IdentityMaterial::Block { .. }
    );
    match backend {
        RecordBackend::Sidecar => recorded_is_block,
        RecordBackend::HostRepo => !recorded_is_block,
    }
}

/// 从任意路径读出证据。目录打不开、名字不合法都折叠成 `Unreadable`——列表与扫描
/// 要对一条坏条目继续往下走，不因为一处失败放弃整份结果（`inspect` 才把它当命令失败）
#[cfg(target_os = "linux")]
fn probe_at(path: &Path, current: Option<&crate::recovery::SceneIdentity>) -> Evidence {
    use crate::platform::evidence_store::HostEvidenceStore;
    use crate::platform::{DirFd, EvidenceIo, EvidenceStore, RelativePath};
    use crate::recovery::Cause;

    let Some(name) = path.file_name() else {
        return Evidence::unreadable(Cause::Unsupported("the path has no file name".to_string()));
    };
    let relative = match RelativePath::try_new(name.to_os_string()) {
        Ok(relative) => relative,
        Err(e) => return Evidence::unreadable(Cause::Unsupported(format!("{e:?}"))),
    };
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let anchor = match DirFd::open_anchor(dir) {
        Ok(anchor) => anchor,
        Err(e) => return Evidence::unreadable(Cause::Io(e.to_string())),
    };
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
        Err(EvidenceIo::Io(e)) => Evidence::unreadable(Cause::Io(e.to_string())),
        Err(EvidenceIo::Unsupported(reason)) => Evidence::unreadable(Cause::Unsupported(reason)),
    }
}

/// 目标当前的现场身份；目标打不开时没有可比的证据，返回 None 让调用方标
/// `identity_check: unavailable`
#[cfg(target_os = "linux")]
fn current_scene(path: &Path) -> Option<crate::recovery::SceneIdentity> {
    use crate::dev::FileSource;
    let src = if is_block_node(path) {
        FileSource::open_read_only(path).ok()?
    } else {
        FileSource::open_read_only_image(path, None).ok()?
    };
    // 与 inspect 同一套事实：容量、载体文件与 loop 映射都从已打开的句柄取
    let probe = crate::platform::probe::probe_of(&src).ok()?;
    Some(crate::recovery::SceneIdentity::of_source(&probe, &src))
}

#[cfg(target_os = "linux")]
fn is_block_node(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).map(|m| m.file_type().is_block_device()).unwrap_or(false)
}

/// 枚举预算：超限即整体失败，不打印半份结果
struct Budget {
    started: Instant,
    bytes: u64,
}

impl Budget {
    fn new() -> Self {
        Self { started: Instant::now(), bytes: 0 }
    }

    fn admit(&mut self, path: &Path) -> Result<(), String> {
        if self.started.elapsed() > MAX_ELAPSED {
            return Err("the 30 second scan budget was exceeded".to_string());
        }
        let meta = std::fs::symlink_metadata(path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if !meta.file_type().is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!("{} is larger than 64 MiB", path.display()));
        }
        self.bytes = self.bytes.saturating_add(meta.len());
        if self.bytes > MAX_TOTAL_BYTES {
            return Err("the 512 MiB scan budget was exceeded".to_string());
        }
        Ok(())
    }
}

/// 读取一个记录目录的全部条目：跳过锁与残留临时文件，其他条目都解码并保留。
/// `only` 用于 `records verify` 按编号筛选
#[cfg(target_os = "linux")]
fn collect_dir(
    dir: &Path,
    backend: RecordBackend,
    current: Option<&crate::recovery::SceneIdentity>,
    budget: &mut Budget,
    only: Option<&str>,
    out: &mut Vec<Row>,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        if text == crate::dev::RECORD_LOCK_NAME || is_temp_name(text) {
            continue;
        }
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        budget.admit(&path)?;
        let evidence = probe_at(&path, current);
        if let Some(wanted) = only {
            let hit = matches!(&evidence, Evidence::Recognized(r) if crate::support::hex_lower(&r.tx_id.bytes()) == wanted);
            if !hit {
                continue;
            }
        }
        out.push(Row {
            path,
            backend,
            quarantine: crate::dev::quarantine_mark(&name).map(|m| m.as_str()),
            evidence,
            identity_checked: current.is_some(),
        });
    }
    Ok(())
}

/// 记录目录的残留临时文件：`.<正式名>.<pid>.<32 位 hex>.tmp`
fn is_temp_name(text: &str) -> bool {
    let Some(middle) = text.strip_prefix('.').and_then(|t| t.strip_suffix(".tmp")) else {
        return false;
    };
    let mut parts = middle.rsplitn(3, '.');
    let (Some(random), Some(pid), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    random.len() == 32
        && random.bytes().all(|b| b.is_ascii_hexdigit())
        && pid.parse::<u32>().is_ok()
}

/// 主机仓库里的记录目录：`<recovery>/<hash>`。按结构判定，不要求目标存在
fn host_record_dirs(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", root.display()))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if crate::dev::record_backend(&path) == Some(RecordBackend::HostRepo) {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// sidecar 记录目录：`<root>/<name>.diskedit`
fn sidecar_record_dirs(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(root).map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", root.display()))?;
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if crate::dev::record_backend(&path) == Some(RecordBackend::Sidecar) {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

#[cfg(target_os = "linux")]
pub(crate) fn cmd_records_list(a: &RecordsListRequest) -> CmdResult {
    let mut budget = Budget::new();
    let mut rows = Vec::new();
    let note = match &a.target {
        Some(target) => {
            let path = Path::new(target);
            let identity = TargetIdentity::resolve_for_cleanup(path).unwrap_or_else(|| {
                bail_fail(RunOutcome::no_mutation(format!("cannot determine the identity of {}", path.display())))
            });
            let Some(record) = identity.record_path() else {
                bail_fail(RunOutcome::no_mutation(format!(
                    "{} has no record directory — records are kept beside images and in the host \
                     recovery repository for block devices",
                    path.display()
                )));
            };
            let dir = record.parent().unwrap_or(Path::new("."));
            let backend = crate::dev::record_backend(dir).unwrap_or(RecordBackend::Sidecar);
            let current = current_scene(path);
            if let Err(reason) = collect_dir(dir, backend, current.as_ref(), &mut budget, None, &mut rows) {
                bail_fail(RunOutcome::no_mutation(format!("cannot list records: {reason}")));
            }
            Some(
                "only TARGET's own record directory is listed; sidecars beside other targets are not \
                 included — pass each target, or use `records scan` with the directory that holds the images",
            )
        }
        None => {
            let root = crate::dev::host_state_root().join("recovery");
            // 仓库还没建起来时没有记录可列，不是读失败（缺失按「不存在」处理）
            let dirs = if root.is_dir() {
                host_record_dirs(&root).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(e)))
            } else {
                Vec::new()
            };
            for dir in dirs {
                // 没有目标可比：身份检查不可用，按结构解码
                if let Err(reason) =
                    collect_dir(&dir, RecordBackend::HostRepo, None, &mut budget, None, &mut rows)
                {
                    bail_fail(RunOutcome::no_mutation(format!("cannot list records: {reason}")));
                }
            }
            Some(
                "records are not indexed; every entry is enumerated in memory and marked unregistered. \
                 Sidecars outside the host repository are not searched — pass TARGET or use `records scan`",
            )
        }
    };
    rows.retain(|row| row.state().is_some_and(State::listed));
    print_rows(&rows, note);
    CmdResult::Code(EXIT_OK)
}

#[cfg(target_os = "linux")]
pub(crate) fn cmd_records_verify(a: &RecordsVerifyRequest) -> CmdResult {
    let path = Path::new(&a.target);
    // verify 的语义是拿盘面重新核对一遍身份；目标探测不出来就没有"核对过"这回事
    let Some(current) = current_scene(path) else {
        bail_fail(RunOutcome::no_mutation(format!(
            "cannot probe the identity of {} — the record was not verified",
            path.display()
        )));
    };
    let identity = TargetIdentity::resolve_for_cleanup(path);
    let mut budget = Budget::new();
    let mut rows = Vec::new();
    let mut searched = Vec::new();

    // 目标自己的记录目录（块设备就是它自己的 identity-hash 目录）
    if let Some(identity) = &identity
        && let Some(record) = identity.record_path()
    {
        let dir = record.parent().unwrap_or(Path::new(".")).to_path_buf();
        if let Some(backend) = crate::dev::record_backend(&dir) {
            searched.push(dir.clone());
            if let Err(reason) =
                collect_dir(&dir, backend, Some(&current), &mut budget, Some(&a.id), &mut rows)
            {
                bail_fail(RunOutcome::no_mutation(format!("cannot verify the record: {reason}")));
            }
        }
    }
    // 主机仓库里的其余记录目录：同一编号出现在两处是"命中两处及以上"的唯一来源
    let root = crate::dev::host_state_root().join("recovery");
    if let Ok(dirs) = host_record_dirs(&root) {
        for dir in dirs {
            if searched.contains(&dir) {
                continue;
            }
            if let Err(reason) = collect_dir(
                &dir,
                RecordBackend::HostRepo,
                None,
                &mut budget,
                Some(&a.id),
                &mut rows,
            ) {
                bail_fail(RunOutcome::no_mutation(format!("cannot verify the record: {reason}")));
            }
        }
    }
    if rows.is_empty() {
        println!(
            "{{\"id\":\"{}\",\"hits\":0,\"records\":[],\"complete\":true}}",
            json_escape(&a.id)
        );
        eprintln!(
            "no record with id {} was found in the target's record directory or the host recovery \
             repository — run `diskedit records scan <root>` to look elsewhere",
            a.id
        );
        return CmdResult::Code(EXIT_REFUSED);
    }
    if rows.len() > 1 {
        print_rows(&rows, None);
        eprintln!(
            "the same id exists in {} places — remove the copies by hand; a record must have one home",
            rows.len()
        );
        return CmdResult::Code(EXIT_REFUSED);
    }
    let row = &rows[0];
    print_rows(std::slice::from_ref(row), None);
    match row.state() {
        Some(State::Valid | State::Unfinished) => CmdResult::Code(EXIT_OK),
        Some(State::Unreadable) => CmdResult::Code(EXIT_NO_MUTATION),
        Some(_) => CmdResult::Code(EXIT_REFUSED),
        None => CmdResult::Code(EXIT_REFUSED),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn cmd_records_scan(a: &RecordsScanRequest) -> CmdResult {
    let root = Path::new(&a.root);
    let state_root = crate::dev::host_state_root();
    let host_recovery = state_root.join("recovery");
    // 记录目录本身不是容器：把单条记录目录当根没有语义
    if crate::dev::record_backend(root).is_some() {
        bail_fail(RunOutcome::refused(format!(
            "{} is a record directory, not a container root — pass {} or the directory that holds \
             the images",
            root.display(),
            host_recovery.display()
        )));
    }
    let (dirs, backend) = if root == host_recovery {
        let dirs = host_record_dirs(root).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(e)));
        (dirs, RecordBackend::HostRepo)
    } else if root.starts_with(&state_root) {
        bail_fail(RunOutcome::refused(format!(
            "{} is inside the host recovery repository — its container root is {}; a record \
             directory is not itself a root",
            root.display(),
            host_recovery.display()
        )));
    } else if root.is_dir() {
        let dirs = sidecar_record_dirs(root).unwrap_or_else(|e| bail_fail(RunOutcome::no_mutation(e)));
        (dirs, RecordBackend::Sidecar)
    } else {
        bail_fail(RunOutcome::no_mutation(format!("{} is not a directory", root.display())));
    };

    let mut budget = Budget::new();
    let mut rows = Vec::new();
    for dir in dirs {
        // 扫描没有单个目标可比：身份检查不可用，按结构解码
        if let Err(reason) = collect_dir(&dir, backend, None, &mut budget, None, &mut rows) {
            println!(
                "{{\"root\":\"{}\",\"complete\":false,\"index\":\"unavailable\",\"records\":[],\
                 \"reason\":\"{}\"}}",
                json_escape(&root.to_string_lossy()),
                json_escape(&reason)
            );
            eprintln!("the scan is incomplete: {reason} — no partial result was returned");
            return CmdResult::Code(EXIT_NO_MUTATION);
        }
    }
    println!(
        "{{\"root\":\"{}\",\"complete\":true,\"index\":\"unavailable\",\"records\":[{}],\
         \"note\":\"records are not indexed; every entry is enumerated in memory and marked unregistered\"}}",
        json_escape(&root.to_string_lossy()),
        rows_json(&rows)
    );
    CmdResult::Code(EXIT_OK)
}

#[cfg(target_os = "linux")]
fn rows_json(rows: &[Row]) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&row.json());
    }
    out
}

#[cfg(target_os = "linux")]
fn print_rows(rows: &[Row], note: Option<&str>) {
    let mut out = format!("{{\"index\":\"unavailable\",\"records\":[{}]", rows_json(rows));
    if let Some(note) = note {
        out.push_str(&format!(",\"note\":\"{}\"", json_escape(note)));
    }
    out.push('}');
    println!("{out}");
}

