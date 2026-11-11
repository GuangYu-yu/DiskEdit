#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
#![cfg_attr(not(target_os = "linux"), allow(dead_code))] // 记录型续跑用例只在 Linux 编译
use super::*;
use gptman::{GPT, GPTPartitionEntry};
use std::io::Cursor;

/// 测试用的可操作几何：从夹具盘上解析后走生产同一构造点（含重叠拒绝）。
/// checkpoint 的合法边界由它提供，故测试不再自带一套边界数字
pub(crate) fn geom_of(src: &FileSource) -> ValidatedGeometry {
    let g = table::load_gpt(src).unwrap().expect("fixture must carry a GPT");
    ValidatedGeometry::new(&g, src.size / g.ss - 1, None).unwrap()
}

/// 给镜像测试夹具挂上一份恢复记录，模拟"命令层打开后"的写句柄。
/// 记录是此类作业唯一的持久进度点，故续跑/崩溃用例必须先有它。
/// `request` 是这条记录要执行的请求（分区、新起点、新末端）：摘要必须在首次落盘前
/// 绑定，取法与生产入口一致——操作取记录自己的，扇区大小与盘大小取目标现场
#[cfg(target_os = "linux")]
pub(crate) fn attach_record(
    src: &mut WriteSource,
    op: crate::recovery::OpKind,
    old: (u64, u64),
    request: (u32, u64, u64),
) -> std::path::PathBuf {
    let record_path = src.identity.record_path().expect("image targets have a record path").to_path_buf();
    std::fs::create_dir_all(record_path.parent().unwrap()).unwrap();
    let probe = crate::platform::probe::probe_of(src).unwrap();
    let scene = crate::recovery::SceneIdentity::of_source(&probe, src);
    let mut journal = crate::recovery::RecordJournal::create_image(
        &record_path,
        scene,
        op,
        src.sector_size,
        crate::dev::DiskGuidState::Unavailable,
        None,
    )
    .unwrap();
    let digest = crate::recovery::PlanDigest::of_request(
        journal.op(),
        request.0,
        request.1,
        request.2,
        src.sector_size,
        src.size,
    );
    journal.bind_plan(digest).expect("a fresh record binds the request");
    journal.bind_old_range(old).unwrap();
    src.adopt_record_journal(journal);
    record_path
}

// 搬移的持久状态在记录里，读法与生产只读路径一致：由记录旁的落点探测解码，再按目标场景比对

/// 目标落点上属于**本目标**的搬移状态。落点不存在、解不出或场景不符时为空
pub(crate) fn relocation_of(src: &FileSource) -> Option<crate::recovery::Relocation> {
    let path = src.identity.record_path()?;
    let here = crate::platform::probe::probe_of(src)
        .ok()
        .map(|probe| crate::recovery::SceneIdentity::of_source(&probe, src))?;
    match crate::recovery::probe_companion(path) {
        crate::recovery::CompanionProbe::Decoded(record) if record.target.matches(&here) => {
            record.relocation
        }
        _ => None,
    }
}

/// 给镜像测试夹具挂上记录并写入一份搬移计划：搬移作业的持久进度只有记录一处，
/// 续跑/崩溃用例因此必须先有它
#[cfg(target_os = "linux")]
pub(crate) fn attach_relocation(
    src: &mut WriteSource,
    old: (u64, u64),
    request: (u32, u64, u64),
    reloc: crate::recovery::Relocation,
    lvm: Option<crate::recovery::LvmPost>,
) -> std::path::PathBuf {
    let path = attach_record(src, crate::recovery::OpKind::Resize, old, request);
    src.record_journal_mut()
        .expect("attach_record adopts a record journal")
        .bind_relocation(reloc, lvm)
        .expect("a legal relocation must bind");
    path
}

/// 由规划结果生成记录里的搬移计划。条目顺序即执行顺序（编码链式依赖），原样保留
pub(crate) fn reloc_for(plan: &Plan, chunk: u64, disk_size: u64) -> crate::recovery::Relocation {
    crate::recovery::Relocation {
        entries: plan.moves.iter().map(crate::recovery::RelocEntry::from).collect(),
        entry_index: 0,
        chunks_in_entry: 0,
        chunk_bytes: chunk,
        kind: crate::recovery::RelocKind::from(plan.kind),
        grow_part: plan.grow_part,
        disk_size,
        ss: plan.ss,
    }
}

// ---- 测试便利入口：显式解析几何后走生产路径（生产调用方在锁下解析一次后传参） ----

/// 测试的无记录规划入口：生产路径只保留 resuming 变体（几何解析一次），
/// 本函数显式解析后走同一实现，避免测试与生产各有一条规划路径
pub(crate) fn make_plan(src: &mut FileSource, grow_part: u32) -> Result<Plan, RunOutcome> {
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| RunOutcome::refused("no GPT on target"))?;
    plan_tail_packed(src, &g, repair, grow_part, None)
}

pub(crate) fn plan_resuming(src: &mut FileSource, grow_part: u32) -> Result<Plan, RunOutcome> {
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| RunOutcome::refused("no GPT on target"))?;
    let reloc = relocation_of(src);
    make_plan_resuming(src, reloc.as_ref(), &g, repair, grow_part)
}

/// `shift = None` 在测试里表示缩容请求（与生产入口的 ShiftIntent::Shrink 同义）
pub(crate) fn plan_shift(src: &mut FileSource, grow_part: u32, shift: Option<u64>) -> Result<Plan, RunOutcome> {
    let intent = match shift {
        Some(s) => ShiftIntent::Grow(s),
        None => ShiftIntent::Shrink,
    };
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| RunOutcome::refused("no GPT on target"))?;
    let reloc = relocation_of(src);
    make_plan_shift_resuming(src, reloc.as_ref(), &g, repair, grow_part, intent)
}

/// 尺寸与现状相同的请求（表已提交、只剩收尾时的原样重跑）
pub(crate) fn plan_shift_noop(src: &mut FileSource, grow_part: u32) -> Result<Plan, RunOutcome> {
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| RunOutcome::refused("no GPT on target"))?;
    let reloc = relocation_of(src);
    make_plan_shift_resuming(src, reloc.as_ref(), &g, repair, grow_part, ShiftIntent::NoOp)
}

pub(crate) fn pending_relocation(src: &FileSource, grow_part: u32) -> Result<bool, RunOutcome> {
    let (g, _repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| RunOutcome::refused("no GPT on target"))?;
    let reloc = relocation_of(src);
    has_pending_relocation(reloc.as_ref(), &g, grow_part)
}

pub(crate) fn rsize(src: &mut WriteSource, part: u32, start: u64, end: u64, chunk: u64, no_fs: bool, log: &mut dyn FnMut(&str)) -> RunOutcome {
    rsize_deferred(src, part, start, end, chunk, no_fs, false, log)
}

/// `defer = true` 对应 PV 的收尾形态：记录留给调用方的后续步骤撤除
/// 走生产入口：`resize-part` 的表与 FS 步骤都由装配层与执行器落地。测试因此验的是
/// 实际发布的那条链，而不是一份平行的实现
#[allow(clippy::too_many_arguments)] // 与生产入口同一组入参，测试辅助不另造参数对象
pub(crate) fn rsize_deferred(src: &mut WriteSource, part: u32, start: u64, end: u64, chunk: u64, no_fs: bool, defer: bool, log: &mut dyn FnMut(&str)) -> RunOutcome {
    let (g, repair) = gpt_policy::resolve_geometry(src).unwrap().unwrap();
    crate::cmd::layout::resize_part_via_plan_with(
        src, &g, repair, part, start, end, chunk, no_fs, None, defer, log,
    )
}

pub(crate) fn ap(src: &mut WriteSource, plan: &mut Plan, chunk: u64, no_fs: bool, log: &mut dyn FnMut(&str)) -> RunOutcome {
    let (g, _repair) = gpt_policy::resolve_geometry(src).unwrap().unwrap();
    apply(src, &g, plan, chunk, no_fs, false, None, log)
}

/// make_plan 边界：多 movable 尾部打包（末→首、末端对齐）、swap 标记、LUKS 拒绝、
/// 尾部空间不足拒绝（总 movable 超出目标之后的容量）
pub(crate) fn plan_fixture(entries: &[(u32, [u8; 16], u64, u64)]) -> (crate::testdir::TestSource<WriteSource>, std::path::PathBuf) {
    let ss = 512u64;
    let data = vec![0u8; 16 * 1024 * 1024];
    let mut cur = Cursor::new(data);
    let mut gpt = GPT::new_from(&mut cur, ss, [0xCD; 16]).unwrap();
    for &(i, guid, s, e) in entries {
        gpt[i] = GPTPartitionEntry {
            partition_type_guid: guid,
            unique_partition_guid: [i as u8; 16],
            starting_lba: s,
            ending_lba: e,
            attribute_bits: 0,
            partition_name: format!("p{i}").as_str().into(),
        };
    }
    gpt.write_into(&mut cur).unwrap();
    let dir = crate::testdir::test_dir("plan");
    let tmp = dir.join("plan.img");
    std::fs::write(&tmp, cur.into_inner()).unwrap();
    // gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置。
    // 补写直接落在文件字节上：夹具不经写原语，进入被测函数前进程写事实保持为假
    let probe = file_source(&tmp);
    let lba0 = crate::table::protective_mbr_content(&probe, ss)
        .unwrap()
        .expect("a zeroed LBA0 has to be replaced by a protective MBR");
    let mut raw = std::fs::read(&tmp).unwrap();
    raw[..lba0.len()].copy_from_slice(&lba0);
    std::fs::write(&tmp, raw).unwrap();
    let src = plan_open(&tmp);
    (crate::testdir::TestSource::new(dir, src), tmp)
}

/// 打开的镜像夹具。指纹取自已打开的文件（`TargetFingerprint::from_file`，与生产同一
/// 构造点），尺寸与指纹出自同一句柄；零指纹会被 `probe_of` 判为「没有 inode 证据」，
/// 依赖目标身份的用例随即被拒绝
pub(crate) fn plan_open(path: &std::path::Path) -> WriteSource {
    WriteSource::unowned(file_source(path))
}

/// 镜像的 `FileSource` 视图（未挂记录）：夹具在写原语之外读盘面时用它
fn file_source(path: &std::path::Path) -> FileSource {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let fingerprint = crate::dev::TargetFingerprint::from_file(&f).unwrap();
    FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(path),
        file: f,
        path: path.into(),
        sector_size: 512,
        size: fingerprint.size,
        is_block: false,
        record_journal: None,
        fingerprint,
        loop_mapping: None,
        owner: None,
    }
}

/// 把盘上主头的 AlternateLBA 指到一个早于容器末端的值（构造"需要修复"的表）
pub(crate) fn make_stale(path: &std::path::Path) {
    let mut raw = std::fs::read(path).unwrap();
    raw[512 + 32..512 + 40].copy_from_slice(&20000u64.to_le_bytes());
    raw[512 + 16..512 + 20].fill(0);
    let hdr_crc = table::crc32(&raw[512..512 + 92]);
    raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
    std::fs::write(path, &raw).unwrap();
}

/// 模拟 prepare 与执行之间第三方改表：改主头的 disk GUID 并重算头 CRC，
/// 既不是构造时读到的那份，也不是本层要提交的目标值
pub(crate) fn tamper_disk_guid(src: &mut WriteSource) {
    let mut hdr = [0u8; 512];
    src.read_at(512, &mut hdr).unwrap();
    hdr[56..72].copy_from_slice(&[0xEE; 16]);
    hdr[16..20].fill(0);
    let crc = table::crc32(&hdr[..92]);
    hdr[16..20].copy_from_slice(&crc.to_le_bytes());
    src.write_at(512, &hdr).unwrap();
}