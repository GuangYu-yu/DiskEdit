#![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
use super::*;
use gptman::{GPT, GPTPartitionEntry};
use std::io::Cursor;

/// 测试用的可操作几何：从夹具盘上解析后走生产同一构造点（含重叠拒绝）。
/// checkpoint 的合法边界由它提供，故测试不再自带一套边界数字
pub(crate) fn geom_of(src: &FileSource) -> ValidatedGeometry {
    let g = table::load_gpt(src).unwrap().expect("fixture must carry a GPT");
    ValidatedGeometry::new(&g, src.size / g.ss - 1, None).unwrap()
}

/// checkpoint 字段判据用的几何边界（合成值，形状与生产一致：分区号上界 = entry_count）
pub(crate) fn lim(entry_count: u32) -> GeometryLimits {
    GeometryLimits {
        sector_size: 512,
        entry_count,
        first_usable_lba: 34,
        last_usable_lba: 200_000,
        file_last_lba: 200_000,
    }
}

/// 测试的无 ckpt 规划入口：生产路径只保留 resuming 变体（几何解析一次），
/// 本函数显式解析后走同一实现，避免测试与生产各有一条规划路径
pub(crate) fn make_plan(src: &mut FileSource, grow_part: u32) -> Result<Plan, Fail> {
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
    plan_tail_packed(&g, repair, grow_part)
}

// ---- 测试便利入口：显式解析几何后走生产路径（生产调用方在锁下解析一次后传参） ----

pub(crate) fn plan_resuming(src: &mut FileSource, grow_part: u32) -> Result<Plan, Fail> {
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
    make_plan_resuming(src, &g, repair, grow_part)
}

/// `shift = None` 在测试里表示缩容请求（与生产入口的 ShiftIntent::Shrink 同义）
pub(crate) fn plan_shift(src: &mut FileSource, grow_part: u32, shift: Option<u64>) -> Result<Plan, Fail> {
    let intent = match shift {
        Some(s) => ShiftIntent::Grow(s),
        None => ShiftIntent::Shrink,
    };
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
    make_plan_shift_resuming(src, &g, repair, grow_part, intent)
}

/// 尺寸与现状相同的请求（表已提交、只剩收尾时的原样重跑）
pub(crate) fn plan_shift_noop(src: &mut FileSource, grow_part: u32) -> Result<Plan, Fail> {
    let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
    make_plan_shift_resuming(src, &g, repair, grow_part, ShiftIntent::NoOp)
}

pub(crate) fn pending_relocation(src: &FileSource, grow_part: u32) -> Result<bool, Fail> {
    let (g, _repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
    has_pending_relocation(src, &g, grow_part)
}

pub(crate) fn rsize(src: &mut WriteSource, part: u32, start: u64, end: u64, chunk: u64, no_fs: bool, log: &mut dyn FnMut(&str)) -> Outcome {
    let (g, repair) = gpt_policy::resolve_geometry(src).unwrap().unwrap();
    resize_part(src, &g, repair, part, start, end, chunk, no_fs, log)
}

pub(crate) fn ap(src: &mut WriteSource, plan: &Plan, chunk: u64, no_fs: bool, log: &mut dyn FnMut(&str)) -> Outcome {
    let (g, _repair) = gpt_policy::resolve_geometry(src).unwrap().unwrap();
    apply(src, &g, plan, chunk, no_fs, log)
}

/// make_plan 边界：多 movable 尾部打包（末→首、末端对齐）、swap 标记、LUKS 拒绝、
/// 尾部空间不足拒绝（总 movable 超出目标之后的容量）
pub(crate) fn plan_fixture(entries: &[(u32, [u8; 16], u64, u64)]) -> (WriteSource, std::path::PathBuf) {
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
    // 唯一文件名：PID + 进程内自增计数，防并行测试同名互踩
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("diskedit_plan_{}_{:x}.img", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&tmp, cur.into_inner()).unwrap();
    let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
    let size = std::fs::metadata(&tmp).unwrap().len();
    let mut src = WriteSource::unowned(FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(&tmp),
        file: f,
        path: tmp.clone(),
        sector_size: 512,
        size,
        is_block: false,
        journal: None,
        fingerprint: Default::default(),
        loop_mapping: None,
    });
    // gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
    crate::table::ensure_protective_mbr(&mut src, ss).unwrap();
    (src, tmp)
}

pub(crate) fn plan_open(path: &std::path::Path) -> WriteSource {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    let size = std::fs::metadata(path).unwrap().len();
    WriteSource::unowned(FileSource {
        identity: crate::dev::TargetIdentity::resolve_image(path),
        file: f,
        path: path.into(),
        sector_size: 512,
        size,
        is_block: false,
        journal: None,
        fingerprint: Default::default(),
        loop_mapping: None,
    })
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