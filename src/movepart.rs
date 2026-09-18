//! 分区搬移与中间分区扩容闭环。
//!
//! 前向恢复、无回滚；单分区 = 单事务；checkpoint 原子写
//! （tmp → sync → rename → fsync 父目录）；复制方向：delta ≥ 0 且尾→头推进，
//! 写点恒在未读源之上（右移时目的地址总是大于已读位置），顺序固化不提供方向参数。

use crate::dev::FileSource;
use crate::gpt_policy::{self, RepairAction};
use crate::outcome::{Fail, Outcome, Pending, PendingKind};
use crate::table::{self, RawGpt};
use std::io;
use std::path::PathBuf;

pub const CKPT_MAGIC: &[u8; 8] = b"DKECKPT1";
pub const CKPT_VERSION: u32 = 3;

/// checkpoint 批量提交粒度：性能参数，不属于恢复协议。恢复永远从最近一次
/// durable checkpoint 继续；此值只决定提交频率，即崩溃后最多重做多少个
/// 已完成但未持久化的 chunk。调整（或换成按字节/自适应策略）无需改恢复逻辑
const CKPT_BATCH_CHUNKS: u64 = 16;

/// chunk 字节数：命令行 --chunk-size（MiB）传入；checkpoint 记录该值，
/// 续传时不一致即拒绝（chunks_done 是按 chunk 计数的，换大小会错位）
pub fn chunk_bytes(mib: u64) -> io::Result<u64> {
    if !(1..=1024).contains(&mib) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--chunk-size out of range (1..=1024 MiB)"));
    }
    Ok(mib * 1024 * 1024)
}

/// 拒绝搬移（起始 LBA 变化）的 GPT 类型 GUID：LUKS/LVM PV——搬移后其元数据语义不明。
/// 纯扩缩（起点不动、数据不搬）不受此限。swap 不在本表：它在 plan 层有专门处理
/// （内容可弃 → 不搬数据、落位后 mkswap 重建），只有 resize-part 的单分区路径会
/// 一并拒绝它（那里没有重建流程）
/// 同时匹配磁盘字节序（混合端）与内存自然序，两种布局都拦。
const FORBIDDEN_TYPE_GUIDS: [[u8; 16]; 4] = [
    // LUKS 7DD3CF25-5C31-4734-ADBF-A47E954A9D24
    [
        0x25, 0xCF, 0xD3, 0x7D, 0x31, 0x5C, 0x34, 0x47, 0xAD, 0xBF, 0xA4, 0x7E, 0x95, 0x4A, 0x9D, 0x24,
    ],
    [0x7D, 0xD3, 0xCF, 0x25, 0x5C, 0x31, 0x47, 0x34, 0xAD, 0xBF, 0xA4, 0x7E, 0x95, 0x4A, 0x9D, 0x24],
    // LVM PV E6D6D379-F507-44C2-A23C-238F2A3DF928
    [
        0x79, 0xD3, 0xD6, 0xE6, 0x07, 0xF5, 0xC2, 0x44, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D, 0xF9, 0x28,
    ],
    [0xE6, 0xD6, 0xD3, 0x79, 0xF5, 0x07, 0x44, 0xC2, 0xA2, 0x3C, 0x23, 0x8F, 0x2A, 0x3D, 0xF9, 0x28],
];

#[derive(Clone)]
pub struct PlanEntry {
    pub part_num: u32,
    pub first_lba: u64,
    pub len_lba: u64,
    pub delta_lba: u64,
    /// swap：不搬数据，落位后 mkswap 重建（UUID/PARTUUID/分区号保持）
    pub is_swap: bool,
}

/// swap 类型 GUID 的自然序形态：同一标准文本按 RFC 4122 字段顺序摆放（而非 GPT 要求的
/// 混合端）。识别两种都认——两种布局的盘都真实存在
const SWAP_TYPE_GUID_NATURAL: [u8; 16] = [
    0x06, 0x57, 0xFD, 0x6D, 0xA4, 0xAB, 0x43, 0xC4, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
];

/// swap 类型判定（磁盘序与 table::SWAP_TYPE_GUID 同源，不另抄字面量）
fn is_swap_guid(g: &[u8; 16]) -> bool {
    *g == table::SWAP_TYPE_GUID || *g == SWAP_TYPE_GUID_NATURAL
}

pub struct Plan {
    pub ss: u64,
    pub last_usable_lba: u64,
    pub grow_part: u32,
    pub moves: Vec<PlanEntry>,
    /// 待执行的修复动作：plan 只记录，apply 执行（plan 本身不写盘）
    pub repair: RepairAction,
}

/// plan 生成的公共前奏：解析 → 修复判定 → 需搬迁时按修复后的 last_usable 返回。
/// 修复后的值取自动作本身（决策期已算好），执行期直接取用。
/// 规划不写盘，故失败只有两类：无表（请求与现状不匹配）与盘内容故障（表非法、
/// 容器与表不自洽）——二者退出码不同，必须在压平成 io::Error 之前分开
fn plan_geometry(src: &mut FileSource) -> Result<(RawGpt, RepairAction), Fail> {
    let mut g = match table::load_gpt(src) {
        Ok(Some(g)) => g,
        Ok(None) => return Err(Fail::refused("no GPT on target")),
        Err(e) => return Err(Fail::infra(format!("parse failed: {e}"))),
    };
    let file_last_lba = src.size / g.ss - 1;
    // 这些拒绝（PMBR SizeInLBA 越出容器、容器装不下备份头跨度、分区越出修复后的可用区）
    // 都是盘/容器自身的异常：改 grow 参数也无解，故归 infra
    let repair = gpt_policy::classify_repair(&g, file_last_lba).map_err(|e| Fail::infra(e.to_string()))?;
    if let Some(new_last_usable) = repair.new_last_usable() {
        g.header.last_usable_lba = new_last_usable;
    }
    Ok((g, repair))
}

/// plan 生成：只解析 + 验证 + 计算，不写盘。stale 表按"修复后"的几何生成计划，
/// 并把修复动作记入 plan.repair，由 apply 执行
pub fn make_plan(src: &mut FileSource, grow_part: u32) -> Result<Plan, Fail> {
    let (g, repair) = plan_geometry(src)?;
    let ss = g.ss;

    // 目标分区必须存在
    let target = g.entries.get((grow_part - 1) as usize)
        .ok_or_else(|| Fail::refused(format!("partition {grow_part} not found")))?;
    if target.ending_lba == 0 {
        return Err(Fail::refused(format!("partition {grow_part} is empty")));
    }

    // movable = 目标分区之后的所有分区（按 first_lba 升序）
    let mut movable: Vec<(u32, &gptman::GPTPartitionEntry)> = g
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.ending_lba != 0 && e.starting_lba > target.ending_lba)
        .map(|(i, e)| ((i + 1) as u32, e))
        .collect();
    movable.sort_by_key(|(_, e)| e.starting_lba);

    let mut moves = Vec::new();
    let mut cursor = g.header.last_usable_lba;
    for (num, e) in movable.iter().rev() {
        // 条目端点倒挂属盘内容故障（解析层已拦，这里是纵深防守）
        if e.ending_lba < e.starting_lba {
            return Err(Fail::infra(format!("partition {num} has ending LBA below starting LBA (corrupted table)")));
        }
        let len = e.ending_lba - e.starting_lba + 1;
        // LUKS/LVM 拒绝；swap 走"重建"而非搬移：内容可弃，UUID/PARTUUID/分区号保持
        if FORBIDDEN_TYPE_GUIDS.contains(&e.partition_type_guid) {
            return Err(Fail::refused(format!("partition {num} is LUKS/LVM PV — relocation refused")));
        }
        // 尾部打包：末端对齐 cursor（含），不留缝隙
        let new_first = cursor.checked_sub(len - 1).ok_or_else(|| {
            Fail::refused("insufficient tail space for relocation")
        })?;
        let delta = new_first as i64 - e.starting_lba as i64;
        if delta < 0 {
            return Err(Fail::refused("insufficient tail space for relocation"));
        }
        // saturating：new_first 理论上 ≥1（LBA0 保护 MBR + LBA1 主头），
        // 极端输入下防下溢 panic，退化为 0 后由下轮 checked_sub 报空间不足
        cursor = new_first.saturating_sub(1);
        moves.push(PlanEntry { part_num: *num, first_lba: e.starting_lba, len_lba: len, delta_lba: delta as u64, is_swap: is_swap_guid(&e.partition_type_guid) });
    }
    Ok(Plan { ss, last_usable_lba: g.header.last_usable_lba, grow_part, moves, repair })
}

/// 尾打包 plan 的恢复感知版本：ckpt 存在时以 ckpt 的 moves 为准（见 resume_plan），
/// 否则照常现算
pub fn make_plan_resuming(src: &mut FileSource, grow_part: u32) -> Result<Plan, Fail> {
    match resume_plan(src, grow_part)? {
        Some(p) => Ok(p),
        None => make_plan(src, grow_part),
    }
}

/// 最小位移 plan：目标分区增长 `shift` 扇区，其右侧所有挡路分区整体右移让位，间隙保持。
/// 与 make_plan（尾打包）相对，用于精确大小的扩容（resize SIZE + --allow-move）。
/// 挡路分区的位移量 = shift − 目标与首个挡路者之间的间隙：首个挡路分区新起点恰为
/// 目标原末端 + shift + 1，apply 的扩容终点（min(new_first) − 1）因此精确落在
/// 请求的新末端——既不多吞目标与挡路者之间的间隙，也不少给。
/// moves 按起始 LBA 降序排列（apply 升序处理 → 最右侧先搬）：
/// 各分区的目的区要么落在尾部空闲，要么落在其右侧已被搬空分区的旧位置，拷贝永不踩源
pub fn make_plan_shift(src: &mut FileSource, grow_part: u32, shift: u64) -> Result<Plan, Fail> {
    let (g, repair) = plan_geometry(src)?;
    let ss = g.ss;

    let target = g.entries.get((grow_part - 1) as usize)
        .ok_or_else(|| Fail::refused(format!("partition {grow_part} not found")))?;
    if target.ending_lba == 0 {
        return Err(Fail::refused(format!("partition {grow_part} is empty")));
    }
    if shift == 0 {
        return Err(Fail::refused("shift must be positive"));
    }

    let mut movable: Vec<(u32, &gptman::GPTPartitionEntry)> = g
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.ending_lba != 0 && e.starting_lba > target.ending_lba)
        .map(|(i, e)| ((i + 1) as u32, e))
        .collect();
    movable.sort_by_key(|(_, e)| e.starting_lba);
    if movable.is_empty() {
        return Err(Fail::refused("no partition to relocate (right side is free — no --allow-move needed)"));
    }

    // 位移量锚定首个挡路者：扣掉目标右侧的既有间隙后才是各分区需要让出的量
    let leading_gap = movable[0].1.starting_lba - target.ending_lba - 1;
    let delta = shift.checked_sub(leading_gap).filter(|d| *d >= 1)
        .ok_or_else(|| Fail::refused("shift does not exceed the free gap before the first blocker — relocation not needed"))?;

    let mut moves: Vec<PlanEntry> = Vec::with_capacity(movable.len());
    for (num, e) in &movable {
        // 条目端点倒挂属盘内容故障（解析层已拦，这里是纵深防守）
        if e.ending_lba < e.starting_lba {
            return Err(Fail::infra(format!("partition {num} has ending LBA below starting LBA (corrupted table)")));
        }
        let len = e.ending_lba - e.starting_lba + 1;
        if FORBIDDEN_TYPE_GUIDS.contains(&e.partition_type_guid) {
            return Err(Fail::refused(format!("partition {num} is LUKS/LVM PV — relocation refused")));
        }
        let new_first = e.starting_lba.checked_add(delta).ok_or_else(|| {
            Fail::refused("relocation target LBA overflows — refusing")
        })?;
        let new_end = new_first.checked_add(len - 1).ok_or_else(|| {
            Fail::refused("relocation target LBA overflows — refusing")
        })?;
        if new_end > g.header.last_usable_lba {
            return Err(Fail::refused("insufficient tail space to relocate blockers by the requested size"));
        }
        moves.push(PlanEntry { part_num: *num, first_lba: e.starting_lba, len_lba: len, delta_lba: delta, is_swap: is_swap_guid(&e.partition_type_guid) });
    }
    moves.reverse(); // 升序构造 → 降序排列：最右侧先搬
    Ok(Plan { ss, last_usable_lba: g.header.last_usable_lba, grow_part, moves, repair })
}

/// 中断感知的最小位移 plan：ckpt 存在时以 ckpt 的 moves 为准（见 resume_plan），
/// 否则按 shift 现算。`shift = None` 表示本次请求是缩容——缩容不搬移任何分区
/// （新末端更靠左，右侧只会更空），此时只有 ckpt 能构成走本路径的理由
pub fn make_plan_shift_resuming(src: &mut FileSource, grow_part: u32, shift: Option<u64>) -> Result<Plan, Fail> {
    if let Some(p) = resume_plan(src, grow_part)? {
        return Ok(p);
    }
    let shift = shift.ok_or_else(|| Fail::refused(
        "shrink relocates nothing, but an unfinished relocation job is pending on this target — re-run the command that started it to resume",
    ))?;
    make_plan_shift(src, grow_part, shift)
}

/// 该分区是否有未收尾的 plan 型搬移作业。命令入口据此分流：几何上"右侧已空、可直接扩容"
/// 并不代表作业已完成——右侧变空本身可能正是搬了一半的结果，收尾步骤（剩余搬移、swap 重建、
/// FS 扩容）都还没做
pub fn has_pending_relocation(src: &FileSource, grow_part: u32) -> Result<bool, Fail> {
    Ok(resume_plan(src, grow_part)?.is_some())
}

/// ckpt 里记录的 plan（仅当它属于该分区）。plan 型搬移的**任何**生成路径在恢复期都必须
/// 以它为准：盘上几何已被部分执行改变，重算出的 delta 与 ckpt 记的不一致，会撞上 apply
/// 的恢复校验而使续传永久失败。ckpt 存在 ⇒ repair 已在 apply 开头执行过（在初始 ckpt
/// 写入之前），故 repair 记 None
fn resume_plan(src: &FileSource, grow_part: u32) -> Result<Option<Plan>, Fail> {
    match read_checkpoint(src)? {
        CheckpointSlot::Ambiguous(paths) => Err(ambiguous_checkpoint(&paths)),
        CheckpointSlot::Relocation(c) => Ok((c.grow_part == grow_part).then_some(Plan {
            ss: c.ss,
            last_usable_lba: c.last_usable_lba,
            grow_part: c.grow_part,
            moves: c.moves,
            repair: RepairAction::None,
        })),
        _ => Ok(None),
    }
}

// ---------- checkpoint ----------

#[derive(Clone)]
pub struct Checkpoint {
    pub disk_size: u64,
    pub ss: u64,
    pub grow_part: u32,
    pub last_usable_lba: u64,
    pub moves: Vec<PlanEntry>,
    pub cur_index: u32,   // 正在搬的 moves 下标
    pub chunks_done: u64, // 该分区已完成的 chunk 数
    pub chunk_bytes: u64, // chunk 大小（续传不一致即拒绝）
}

impl Checkpoint {
    fn serialize(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(CKPT_MAGIC);
        b.extend_from_slice(&CKPT_VERSION.to_le_bytes());
        b.extend_from_slice(&self.disk_size.to_le_bytes());
        b.extend_from_slice(&self.ss.to_le_bytes());
        b.extend_from_slice(&self.grow_part.to_le_bytes());
        b.extend_from_slice(&self.last_usable_lba.to_le_bytes());
        b.extend_from_slice(&(self.moves.len() as u32).to_le_bytes());
        for m in &self.moves {
            b.extend_from_slice(&m.part_num.to_le_bytes());
            b.extend_from_slice(&m.first_lba.to_le_bytes());
            b.extend_from_slice(&m.len_lba.to_le_bytes());
            b.extend_from_slice(&m.delta_lba.to_le_bytes());
            b.push(m.is_swap as u8);
        }
        b.extend_from_slice(&self.cur_index.to_le_bytes());
        b.extend_from_slice(&self.chunks_done.to_le_bytes());
        b.extend_from_slice(&self.chunk_bytes.to_le_bytes());
        let crc = table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    fn deserialize(b: &[u8]) -> io::Result<Self> {
        if b.len() < 8 + 4 + 8 + 8 + 4 + 8 + 4 + 4 + 4 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint truncated"));
        }
        if &b[0..8] != CKPT_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint magic mismatch"));
        }
        let ver = u32::from_le_bytes(b[8..12].try_into().unwrap());
        if ver != CKPT_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unsupported checkpoint version {ver}")));
        }
        let mut off = 12;
        let rd = |off: usize, n: usize| -> io::Result<&[u8]> {
            b.get(off..off + n).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "checkpoint truncated"))
        };
        let disk_size = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        let ss = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        let grow_part = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
        off += 4;
        let last_usable_lba = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        let count = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap()) as usize;
        off += 4;
        // count 是盘上可控值：GPT 分区数上限 128，超界即损坏，防巨型 with_capacity
        if count > 128 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint move count"));
        }
        let mut moves = Vec::with_capacity(count);
        for _ in 0..count {
            let part_num = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
            let first_lba = u64::from_le_bytes(rd(off + 4, 8)?.try_into().unwrap());
            let len_lba = u64::from_le_bytes(rd(off + 12, 8)?.try_into().unwrap());
            let delta_lba = u64::from_le_bytes(rd(off + 20, 8)?.try_into().unwrap());
            let is_swap = rd(off + 28, 1)?[0] != 0;
            moves.push(PlanEntry { part_num, first_lba, len_lba, delta_lba, is_swap });
            off += 29;
        }
        let cur_index = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
        off += 4;
        let chunks_done = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        let chunk_bytes = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        let stored = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
        if table::crc32(&b[..off]) != stored {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint CRC mismatch"));
        }
        Ok(Checkpoint { disk_size, ss, grow_part, last_usable_lba, moves, cur_index, chunks_done, chunk_bytes })
    }
}

/// 读 checkpoint 现场：在身份给出的候选落点上（首项为本次命名，其后是历史命名）取首个
/// **有效**者，不做目录扫描。多份候选同时有效即报歧义——猜错会把中断的搬移现场丢掉。
/// 块设备的历史命名带 GPT Disk GUID，而 GUID 只在表可读时存在，故先尽力取一次
///
/// 只有"不存在"算空槽。权限 / I/O 失败、以及文件在而解不出来，都上抛：把"存在但读不了"
/// 当成"没有 checkpoint"，会把中断的搬移降级成一次全新规划
fn read_checkpoint(src: &FileSource) -> Result<CheckpointSlot, Fail> {
    let legacy = table::load_gpt(src).ok().flatten().map(|g| g.header.disk_guid);
    let mut found: Vec<(PathBuf, CheckpointSlot)> = Vec::new();
    for path in src.identity.checkpoint_candidates(legacy) {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            // 全程在读盘阶段，尚未写盘，故按 Infra 而不是"盘可能已改变"
            Err(e) => return Err(Fail::infra(format!("checkpoint read failed: {}: {e}", path.display()))),
        };
        let relocation = Checkpoint::deserialize(&bytes);
        let resize = RsCheckpoint::deserialize(&bytes);
        match (relocation, resize) {
            (Ok(c), _) => found.push((path, CheckpointSlot::Relocation(Box::new(c)))),
            (_, Ok(c)) => found.push((path, CheckpointSlot::Resize(Box::new(c)))),
            (Err(e), _) => return Err(Fail::infra(format!("checkpoint unreadable: {}: {e}", path.display()))),
        }
    }
    if found.is_empty() {
        return Ok(CheckpointSlot::Empty);
    }
    if found.len() == 1 {
        return Ok(found.swap_remove(0).1);
    }
    Ok(CheckpointSlot::Ambiguous(found.into_iter().map(|(p, _)| p).collect()))
}

/// 多份 checkpoint 同时可用 ⇒ 不猜：猜错会把中断的搬移现场丢掉
fn ambiguous_checkpoint(paths: &[PathBuf]) -> Fail {
    let listed: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    Fail::refused(format!("multiple checkpoints exist for this target — refusing: {}", listed.join(", ")))
}

/// 原子写：tmp → sync_all → rename → fsync 父目录。
/// 临时名带 PID + 进程内自增序号：同目录下的两个 ckpt 可能由同一进程并行写
/// （测试里就会发生），只用 PID 会撞同一个临时路径
fn atomic_write_ckpt(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    // 落点目录归文件自己保证：路径由身份派生，身份不知道目录是否存在
    crate::dev::best_effort_mkdir(parent);
    let mut tmp = parent.to_path_buf().into_os_string();
    tmp.push(format!(
        "/.diskedit.ckpt.tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = std::path::PathBuf::from(tmp);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // fsync 父目录（unix 专属；目录 fd 只读打开后 sync）
    #[cfg(unix)]
    {
        let dir = std::fs::File::open(parent)?;
        dir.sync_all()?;
    }
    Ok(())
}

// ---------- apply ----------

/// 崩溃注入点的唯一声明处：一条声明同时生成"注入"与"存根"两个版本，签名不可能只在
/// 一侧成立。test-faults 构建下比对 DISKEDIT_FAULT——无参点比 `标签`、带参点比
/// `标签:参数`——命中即 abort；其余构建下是空函数（参数照收，调用点在两种构建下同形）。
/// 注入路径不写日志、不做清理：任何额外输出都会改变被测对象
macro_rules! fault_points {
    ($(
        $(#[$doc:meta])*
        $name:ident ( $( $arg:ident : $ty:ty ),* ) = $tag:literal $(, $tag_arg:ident)?;
    )*) => {
        $(
            $(#[$doc])*
            #[cfg(feature = "test-faults")]
            fn $name($( $arg: $ty ),*) {
                let Ok(v) = std::env::var("DISKEDIT_FAULT") else { return };
                let want = String::from($tag);
                $( let want = format!("{want}:{}", $tag_arg); )?
                if v == want {
                    std::process::abort();
                }
            }
        )*
        $(
            #[cfg(not(feature = "test-faults"))]
            fn $name($( $arg: $ty ),*) {
                $( let _ = $arg; )*
            }
        )*
    };
}

fault_points! {
    /// 第 N 个 chunk 落盘后 abort
    fault_chunk(n: u64) = "chunk", n;
    /// chunk 拷贝全部完成、表项 commit 前 abort
    fault_before_entry_commit() = "before-entry-commit";
    /// 表项 commit + ckpt 推进后（下一分区拷贝开始前的条目间隙）abort
    fault_after_entry_commit(mi: usize) = "after-entry-commit", mi;
    /// 全部条目完成、扩容收尾前 abort
    fault_before_grow() = "before-grow";
    /// swap 条目落位重建完成后 abort
    fault_after_swap_entry(mi: usize) = "after-swap-entry", mi;
    /// 修复完成、初始 ckpt 写入前 abort
    fault_after_repair() = "after-repair";
    /// swap 条目 GPT commit 后、ckpt 推进前 abort（重放须幂等：GPT 绝对赋值 + 从旧位置重读 UUID 重建）
    fault_after_swap_commit(mi: usize) = "after-swap-commit", mi;
    /// resize_part 路径第 N 个 chunk 落盘后 abort
    fault_rs_chunk(n: u64) = "rs-chunk", n;
    /// FS 缩容完成（fs_shrunk 已落盘）、数据搬移前 abort
    fault_rs_after_fs_shrink() = "rs-after-fs-shrink";
    /// 数据搬移完成、表项提交前 abort
    fault_rs_before_commit() = "rs-before-commit";
    /// 表项提交后、FS 扩容前 abort
    fault_rs_after_commit() = "rs-after-commit";
}

/// 扩容终点：`moves` 是按"末→首"的尾部紧凑打包序，各分区新起点中最小者即本次腾出空间的
/// 左边界，故 grow_end = min(new_first) − 1；无 movable 时扩到 last_usable_lba。
/// 加法一律 checked：越界/损坏表下报错而非回绕（回绕会把荒谬的 LBA 写进表再交给 resize）
pub fn grow_end_for(plan: &Plan) -> io::Result<u64> {
    let mut leftmost: Option<u64> = None;
    for m in &plan.moves {
        let new_first = m.first_lba.checked_add(m.delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        leftmost = Some(leftmost.map_or(new_first, |f| f.min(new_first)));
    }
    let grow_end = match leftmost {
        // movable 满足 starting_lba > 目标分区 ending_lba ≥ 1 ⇒ new_first ≥ 2，减法不下溢
        Some(f) => f - 1,
        None => plan.last_usable_lba,
    };
    if grow_end > plan.last_usable_lba {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "relocation target beyond last usable LBA — refusing"));
    }
    Ok(grow_end)
}

pub fn apply(src: &mut FileSource, plan: &Plan, chunk_len: u64, no_fs: bool, log: &mut dyn FnMut(&str)) -> Outcome {
    // 后置条件中未完成的部分：布局写完后才可能产生，交调用方统一换算退出码
    let mut pending: Vec<Pending> = Vec::new();
    let r = apply_inner(src, plan, chunk_len, no_fs, log, &mut pending);
    crate::outcome::finish(r, pending)
}

fn apply_inner(
    src: &mut FileSource,
    plan: &Plan,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
    pending: &mut Vec<Pending>,
) -> Result<(), crate::outcome::Fail> {
    // 读表失败分两类（与 main 的 parse failed / no GPT on target 同判据）：无表 = 请求与
    // 目标现状不匹配；表在但结构非法 = 盘内容故障。此处尚未写盘，故都不带"可能已改变"提示
    let g0 = match table::load_gpt(src) {
        Ok(Some(g)) => g,
        Ok(None) => return Err(Fail::refused("no GPT on target")),
        Err(e) => return Err(Fail::infra(format!("parse failed: {e}"))),
    };
    // 写盘前的 preflight：FS 扩展属本次操作的后置条件，工具缺失必须现在拒绝——
    // 一旦开始写盘才发现，就会留下"分区已改、FS 未扩"的中间态。
    // 不依赖命令层是否检查过：续传路径不经过 plan，本处才是唯一必经关口
    if !no_fs
        && let Some(ge) = g0.entries.get((plan.grow_part - 1) as usize)
        && ge.ending_lba != 0
    {
        // LBA 的单位是**表自身**的 ss（plan.ss）。此处位于 perform_repair / 搬移之前，
        // 本次调用尚未写目标盘 → Infra
        let ft = crate::fsid::identify(
            src,
            ge.starting_lba * plan.ss,
            (ge.ending_lba - ge.starting_lba + 1) * plan.ss,
        )
        .map_err(Fail::infra_io)?;
        crate::fsops::check_grow(ft).map_err(crate::outcome::Fail::refused)?;
    }
    // 恢复三态：有效 → 续传 / 槽位被另一族作业占用 → 拒绝 / 空 → 新建。
    // 必须先于 perform_repair 读（槽位落点只由目标身份决定，与表无关）：修复会写盘，
    // 而"拒绝"承诺的是本次未写盘
    let slot = read_checkpoint(src)?;
    // plan 不写盘：修复动作（备份头搬移 / 保护 MBR 重写）在 apply 里先执行，
    // 使后续所有写入都基于修复后的几何
    if let Some(what) = plan.repair.describe() {
        gpt_policy::perform_repair(src, &plan.repair)?;
        log(&format!("[repair] {what}"));
    }
    fault_after_repair();
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();

    let mut ckpt = match slot {
        CheckpointSlot::Ambiguous(paths) => return Err(ambiguous_checkpoint(&paths)),
        CheckpointSlot::Resize(_) => return Err(Fail::refused(
            "an unfinished single-partition resize job occupies the checkpoint slot — re-run that resize to finish it before applying a relocation plan",
        )),
        CheckpointSlot::Relocation(c) => {
            let c = *c;
            // plan 参数与磁盘现状必须一致（盘大小/扇区/计划/chunk 全等），否则拒绝
            let fresh = Checkpoint {
                disk_size: src.size,
                ss: plan.ss,
                grow_part: plan.grow_part,
                last_usable_lba: plan.last_usable_lba,
                moves: plan.moves.clone(),
                cur_index: c.cur_index.min(plan.moves.len() as u32),
                chunks_done: c.chunks_done,
                chunk_bytes: chunk_len,
            };
            if c.disk_size != fresh.disk_size || c.ss != fresh.ss || c.grow_part != fresh.grow_part
                || c.last_usable_lba != fresh.last_usable_lba || c.moves.len() != fresh.moves.len()
                || c.chunk_bytes != fresh.chunk_bytes
                || c.moves.iter().zip(&fresh.moves).any(|(a, b)| a.part_num != b.part_num || a.first_lba != b.first_lba || a.len_lba != b.len_lba || a.delta_lba != b.delta_lba)
            {
                // 写盘前的校验：此刻盘上尚未改动，属事前拒绝而非执行失败
                return Err(crate::outcome::Fail::refused("existing checkpoint does not match current disk/plan — refusing"));
            }
            // Y = durable 恢复点（ckpt 文件里的值），不是本进程内存进度
            log(&format!("resuming at entry {} chunk {} (durable checkpoint)", c.cur_index, c.chunks_done));
            fresh
        }
        CheckpointSlot::Empty => Checkpoint {
            disk_size: src.size,
            ss: plan.ss,
            grow_part: plan.grow_part,
            last_usable_lba: plan.last_usable_lba,
            moves: plan.moves.clone(),
            cur_index: 0,
            chunks_done: 0,
            chunk_bytes: chunk_len,
        },
    };
    atomic_write_ckpt(&ckpt_path, &ckpt.serialize())?;
    // 本操作含数据搬移：数据字节不入 journal（前向恢复、无回滚），只留一条标记
    // 使 undo 拒绝回滚 —— 否则表被回滚而数据未回滚，布局不一致
    if !plan.moves.is_empty() {
        src.mark_relocation()?;
    }

    // 恢复从最近一次 durable checkpoint 继续；batching（CKPT_BATCH_CHUNKS）只决定
    // checkpoint 的提交频率，即崩溃后最多重做多少个已完成但未持久化的 chunk。
    // chunk 级进度只对"中断时正在搬的那一条"有效（后续条目从头计数）：自重叠搬移
    // （delta < 分区长度）重跑已完成 chunk 会读到被自己覆写过的源数据，chunk 级续传正是为此设计
    let resume_index = ckpt.cur_index;
    let resume_chunks = ckpt.chunks_done;
    // 搬移循环：单分区 = 单事务；尾→头推进
    for mi in (ckpt.cur_index as usize)..plan.moves.len() {
        let m = &plan.moves[mi];
        ckpt.cur_index = mi as u32;
        // durable 起点：只有"中断时正在搬的那一条"带 chunk 级进度，后续条目从 0 开始
        let entry_resume = if mi as u32 == resume_index { resume_chunks } else { 0 };
        ckpt.chunks_done = entry_resume;
        // swap：内容可弃 → 不搬数据，表项落位后 mkswap 重建（UUID/卷标保持）
        if m.is_swap {
            let mut g = table::load_gpt(src).map_err(table::flatten)?.ok_or_else(|| io::Error::other("GPT vanished mid-apply"))?;
            // PARTUUID/分区号由条目原样保留（unique guid 不动），只改位置。
            // 绝对赋值而非 += delta：commit 后、ckpt 更新前崩溃的重放幂等
            {
                let e = &mut g.entries[(m.part_num - 1) as usize];
                e.starting_lba = m.first_lba + m.delta_lba;
                e.ending_lba = m.first_lba + m.len_lba - 1 + m.delta_lba;
            }
            let last_lba = src.size / plan.ss - 1;
            table::commit_gpt(src, &g, last_lba)?;
            fault_after_swap_commit(mi);
            // 旧 swap 签名区在源位置（数据区未搬移），从那里读 UUID/卷标
            let ident = read_swap_identity(src, m.first_lba, m.len_lba, plan.ss);
            match crate::fsops::recreate_swap(src, m.part_num, ident) {
                Ok(()) => log(&format!("swap {} recreated at new location (UUID preserved)", m.part_num)),
                // 表项已落位、内容可弃：swap 未重建属"后续步骤未完成"，不是本次操作失败
                Err(e) => pending.push(Pending::new(
                    m.part_num,
                    PendingKind::Swap,
                    e.to_string(),
                    crate::fsops::rescue_hint("swap", &crate::dev::part_dev_hint(src, m.part_num, m.first_lba * plan.ss), false),
                )),
            }
            ckpt.chunks_done = 0;
            ckpt.cur_index = mi as u32 + 1;
            atomic_write_ckpt(&ckpt_path, &ckpt.serialize())?;
            fault_after_swap_entry(mi);
            continue;
        }
        let src_off = m.first_lba * plan.ss;
        let dst_off = (m.first_lba + m.delta_lba) * plan.ss;
        let total = m.len_lba * plan.ss;
        let chunk_from_tail: Vec<(u64, u64)> = {
            // (offset_within, len) 从尾部向头部
            let mut v = Vec::new();
            let mut pos = total;
            while pos > 0 {
                let len = chunk_len.min(pos);
                v.push((pos - len, len));
                pos -= len;
            }
            v
        };
        let total_chunks = chunk_from_tail.len() as u64;
        // volatile 进度：已完成但尚未 checkpoint 的 chunk 数；durable 边界始终只在 ckpt 文件里
        let mut pending_chunks = 0u64;
        for (i, (within, len)) in chunk_from_tail.iter().enumerate() {
            if (i as u64) < entry_resume {
                continue; // durable 边界之前视为完成：恢复只认 ckpt 值，绝不用内存进度跳过
            }
            let mut buf = vec![0u8; *len as usize];
            src.read_at(src_off + within, &mut buf)?;
            src.write_data_at(dst_off + within, &buf)?;
            src.sync_data()?; // 数据 chunk 用 sync_data；表结构提交用 sync_all
            ckpt.chunks_done = i as u64 + 1; // 内存进度：递增发生在 sync_data 之后，永不超前于 durable 数据
            pending_chunks += 1;
            // 批量提交：攒满一批或到达末 chunk 才落盘。落后只导致重复执行
            // （重做读到的仍是原始源数据），超前才会跳过未落盘区间——单向不等式不可破坏
            if pending_chunks >= CKPT_BATCH_CHUNKS || i as u64 + 1 == total_chunks {
                atomic_write_ckpt(&ckpt_path, &ckpt.serialize())?;
                pending_chunks = 0;
            }
            fault_chunk(i as u64 + 1);
        }
        fault_before_entry_commit();
        // 本分区完成 → 按崩溃安全四结构序列提交表项（整表重写）。
        // 绝对赋值（非 += delta）：commit 后 ckpt 更新前崩溃的重放幂等
        let mut g = table::load_gpt(src).map_err(table::flatten)?.ok_or_else(|| io::Error::other("GPT vanished mid-apply"))?;
        {
            let e = &mut g.entries[(m.part_num - 1) as usize];
            e.starting_lba = m.first_lba + m.delta_lba;
            e.ending_lba = m.first_lba + m.len_lba - 1 + m.delta_lba;
        }
        let last_lba = src.size / plan.ss - 1;
        table::commit_gpt(src, &g, last_lba)?;
        // 起始位置变化的 NTFS 分区需修 HiddenSectors（数据是字节拷贝，boot sector 带着旧值）
        let new_first = m.first_lba + m.delta_lba;
        if crate::fsid::identify(src, new_first * plan.ss, m.len_lba * plan.ss)? == "ntfs" {
            fix_ntfs_hidden_sectors(src, new_first, plan.ss, log)?;
        }
        log(&format!("partition {} relocated (delta {} sectors)", m.part_num, m.delta_lba));
        ckpt.chunks_done = 0;
        ckpt.cur_index = mi as u32 + 1;
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize())?;
        fault_after_entry_commit(mi);
    }
    fault_before_grow();

    // 扩容收尾：目标分区扩到 movable 新区域之前（防止与刚打包的分区重叠）；
    // 无 movable 时到 last_usable。commit → resize FS
    let last_lba = src.size / plan.ss - 1;
    let mut g = table::load_gpt(src).map_err(table::flatten)?.ok_or_else(|| io::Error::other("GPT vanished before grow"))?;
    let (grow_start, grow_len);
    {
        let grow_end = grow_end_for(plan)?;
        let te = &mut g.entries[(plan.grow_part - 1) as usize];
        te.ending_lba = grow_end;
        grow_start = te.starting_lba;
        grow_len = te.ending_lba - te.starting_lba + 1;
    }
    table::commit_gpt(src, &g, last_lba)?;
    table::ensure_protective_mbr(src)?;
    log(&format!("partition {} extended (FS area ends before relocated partitions)", plan.grow_part));

    crate::dev::warn_if_remove_failed(&ckpt_path);

    // FS resize 是契约的一部分：失败即后置条件未满足（由调用方换算为 PARTIAL）。
    // 这里不能只打日志当成功——脚本会据此认为空间已可用
    let fstype = crate::fsid::identify(src, grow_start * g.ss, grow_len * g.ss)?;
    if no_fs {
        log("partition extended (--no-fs: filesystem left untouched)");
    } else if matches!(fstype, "unknown" | "swap" | "lvm2_pv") {
        log("partition extended (no resizable filesystem inside)");
    } else {
        match crate::fsops::resize_fs(src, plan.grow_part, fstype) {
            Ok(()) => log("filesystem resized"),
            Err(e) => pending.push(Pending::new(
                plan.grow_part,
                PendingKind::Fs,
                e.to_string(),
                crate::fsops::rescue_hint(fstype, &crate::dev::part_dev_hint(src, plan.grow_part, grow_start), false),
            )),
        }
    }
    Ok(())
}

/// 尽力读取附加信息（如卷标）：失败只意味着该项缺失，不影响正确性
#[allow(clippy::let_underscore_must_use)]
fn best_effort_read(src: &FileSource, off: u64, buf: &mut [u8]) {
    let _ = src.read_at(off, buf);
}

/// 从 swap 首部读 UUID/卷标（内核 include/linux/swap.h union swap_header：info @1024，
/// sws_uuid@1036、sws_volume@1052）。非 swap 签名或读取失败 → None（mkswap 生成随机 UUID）
pub(crate) fn read_swap_identity(src: &FileSource, first_lba: u64, len_lba: u64, ss: u64) -> (Option<[u8; 16]>, Option<String>) {
    let base = first_lba * ss;
    // 签名位置由唯一探测器判定；候选集取 libblkid 口径（含 32K）——本函数只读元数据，
    // 宽于"swapon 可激活"的范围是有意的：读得到就能保住 UUID/PARTUUID
    if crate::fsid::probe_swap_header(src, base, len_lba * ss, &crate::fsid::blkid_known_pages()).is_none() {
        return (None, None); // 未格式化或非 swap → 随机 UUID
    }
    // UUID/卷标偏移与页大小无关：libblkid struct swap_header_v1_2 中 uuid @1036、volume @1052
    // （与内核 union swap_header 的 info 区同布局，均在第一页内、bootbits 之后）
    let mut uuid = [0u8; 16];
    let mut vol = [0u8; 16];
    if src.read_at(base + 1036, &mut uuid).is_err() {
        return (None, None);
    }
    // 卷标是附加信息：读失败只意味着重建后没有卷标（swap 通常按 UUID 挂载），
    // 不影响可用性；UUID 读失败已在上方显式返回 None，由 mkswap 生成新值
    best_effort_read(src, base + 1052, &mut vol);
    let vol_s: String = vol.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
    let uuid_opt = if uuid == [0u8; 16] { None } else { Some(uuid) };
    let vol_opt = if vol_s.is_empty() { None } else { Some(vol_s) };
    (uuid_opt, vol_opt)
}

// ---------- 通用 resize-part：grow / shrink / move 三合一 ----------

const CKPT2_MAGIC: &[u8; 8] = b"DKECKPT2";
/// 字段变动即升版本：旧 ckpt 解析失败 → 当作"无 ckpt"重跑（搬移与 commit 都幂等，重跑安全）。
/// 不靠长度巧合兜底——CRC 覆盖长度随字段变化，旧布局必然对不上
const CKPT2_VERSION: u32 = 3;

/// resize-part 的 checkpoint（v3）：搬移覆盖源区，中途断电后新旧两处都不完整，
/// 已完成位置只能由 checkpoint 判定。"表项是否已提交"不在这里存——盘上的分区几何
/// 就是那个事实，另存一份标记只会与它分叉（见 resize_part_inner 的恢复分支）
struct RsCheckpoint {
    disk_size: u64,
    ss: u64,
    part: u32,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    new_end: u64,
    fs_shrunk: bool,
    chunks_done: u64,
    chunk_bytes: u64,
}

impl RsCheckpoint {
    fn serialize(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(CKPT2_MAGIC);
        b.extend_from_slice(&CKPT2_VERSION.to_le_bytes());
        b.extend_from_slice(&self.disk_size.to_le_bytes());
        b.extend_from_slice(&self.ss.to_le_bytes());
        b.extend_from_slice(&self.part.to_le_bytes());
        b.extend_from_slice(&self.old_start.to_le_bytes());
        b.extend_from_slice(&self.old_end.to_le_bytes());
        b.extend_from_slice(&self.new_start.to_le_bytes());
        b.extend_from_slice(&self.new_end.to_le_bytes());
        b.push(self.fs_shrunk as u8);
        b.extend_from_slice(&self.chunks_done.to_le_bytes());
        b.extend_from_slice(&self.chunk_bytes.to_le_bytes());
        let crc = table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }
    fn deserialize(b: &[u8]) -> io::Result<Self> {
        // 最短完整布局 = 8(magic)+4(ver)+6×u64+4(part)+1(fs_shrunk)+8(chunks_done)
        // +8(chunk_bytes)+4(crc) = 85；CRC 4 字节必须计入：截断文件走 InvalidData
        // 而非在尾部切片时 panic
        if b.len() < 8 + 4 + 8 * 6 + 4 + 1 + 8 + 8 + 4 || &b[0..8] != CKPT2_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint invalid"));
        }
        if u32::from_le_bytes(b[8..12].try_into().unwrap()) != CKPT2_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported resize checkpoint version"));
        }
        let mut o = 12usize;
        fn rd64(b: &[u8], o: &mut usize) -> io::Result<u64> {
            let v = b.get(*o..*o + 8).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?;
            *o += 8;
            Ok(u64::from_le_bytes(v.try_into().unwrap()))
        }
        let disk_size = rd64(b, &mut o)?;
        let ss = rd64(b, &mut o)?;
        let part = u32::from_le_bytes(b.get(o..o + 4).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?.try_into().unwrap()); o += 4;
        let old_start = rd64(b, &mut o)?;
        let old_end = rd64(b, &mut o)?;
        let new_start = rd64(b, &mut o)?;
        let new_end = rd64(b, &mut o)?;
        let fs_shrunk = *b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))? != 0; o += 1;
        let chunks_done = rd64(b, &mut o)?;
        let chunk_bytes = rd64(b, &mut o)?;
        if table::crc32(&b[..o]) != u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint CRC mismatch"));
        }
        // 字段一致性：区间端点不得倒挂，chunk_bytes 不得为 0（防除零/死循环回放）
        if old_start > old_end || new_start > new_end {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint range inconsistent"));
        }
        if chunk_bytes == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint chunk_bytes invalid"));
        }
        Ok(RsCheckpoint { disk_size, ss, part, old_start, old_end, new_start, new_end, fs_shrunk, chunks_done, chunk_bytes })
    }
}

/// checkpoint 槽位的占用者。两族作业（plan 型搬移 / 单分区 resize）共用同一路径，
/// 于是"解析不出本族的结构"**不等于**"槽位是空的"——把另一族作业的现场当空槽会直接覆盖它，
/// 使被中断的作业（表可能已改到半途）永久失去续传信息。故读取必须点名占用者，
/// 由调用方对"别人的作业"显式拒绝
enum CheckpointSlot {
    Empty,
    Relocation(Box<Checkpoint>),
    Resize(Box<RsCheckpoint>),
    /// 多份候选同时可解析：不猜，交调用方拒绝
    Ambiguous(Vec<PathBuf>),
}

// 执行期失败的类型化区分（写盘前拒绝 / 写盘后失败）定义在 outcome 模块

/// 通用分区重定位：new_start/new_end 任意（grow/shrink/move 组合）。
/// 顺序：缩容 FS（如有）→ 数据搬移（方向感知 + checkpoint 续传）→ 提交表项 → 扩容 FS（如有）。
/// 所有拒绝性校验在第一次写盘前完成（fail-fast）。
/// 对外入口负责把内部失败分类换算为 Outcome（退出码只在这一层定）
pub fn resize_part(
    src: &mut FileSource,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
) -> Outcome {
    let mut pending: Vec<Pending> = Vec::new();
    let r = resize_part_inner(src, part, new_start, new_end, chunk_len, no_fs, log, &mut pending);
    crate::outcome::finish(r, pending)
}

// 8 个参数都是必需的输入/输出（源、目标区间、块大小、是否跳过 FS、日志、待办收集）；
// 合并成结构体只会把同一组值搬到另一个类型里，不增加约束力
#[allow(clippy::too_many_arguments)]
fn resize_part_inner(
    src: &mut FileSource,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
    pending: &mut Vec<Pending>,
) -> Result<(), Fail> {
    // checkpoint 槽位只由目标身份定位（与几何无关），故可以在 ensure_geometry **之前**读：
    // 修复（若需要）会写盘，而对"槽位被别的作业占着"的拒绝承诺的是本次未写盘
    let slot = read_checkpoint(src)?;
    let g = gpt_policy::ensure_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let ss = g.ss;
    let e = g.entries.get((part - 1) as usize)
        .ok_or_else(|| Fail::refused(format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    // 只拦搬移（起点变化）：LUKS/LVM PV/swap 的纯扩缩不搬数据，允许。
    // swap 在此一并拒绝：plan/apply 有 mkswap 重建流程，单分区 resize-part 没有
    if (FORBIDDEN_TYPE_GUIDS.contains(&e.partition_type_guid) || is_swap_guid(&e.partition_type_guid))
        && new_start != e.starting_lba
    {
        return Err(Fail::refused("swap/LUKS/LVM PV relocation refused (size-only changes are allowed)"));
    }
    if new_start < g.header.first_usable_lba || new_end > g.header.last_usable_lba || new_start > new_end {
        return Err(Fail::refused(format!(
            "new range {new_start}..{new_end} outside usable {}..{}",
            g.header.first_usable_lba, g.header.last_usable_lba
        )));
    }
    let old_start = e.starting_lba;
    let old_end = e.ending_lba;
    for (i, other) in g.entries.iter().enumerate() {
        if i + 1 == part as usize || other.ending_lba == 0 {
            continue;
        }
        if !(new_end < other.starting_lba || new_start > other.ending_lba) {
            return Err(Fail::refused(format!("new range overlaps partition #{}", i + 1)));
        }
    }
    // 表项 LBA 的单位是表自身的 ss。此处虽在本次搬移之前，但上面的 ensure_geometry
    // 可能已执行过 repair（重写双头/保护 MBR）→ 不满足"本次未写盘"，仍按 Failed
    let fstype = crate::fsid::identify(src, old_start * ss, (old_end - old_start + 1) * ss)?;
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();

    // 恢复：checkpoint 与当前参数一致才允许续传，否则拒绝（槽位在写盘前已读取，见函数开头）
    let existing = match slot {
        CheckpointSlot::Resize(c) => Some(*c),
        // 槽位被 plan 型作业占着：此刻盘上几何可能正停在搬移中途，按"没有 ckpt"重做
        // 会基于错误几何写表，也会覆盖掉那份唯一的续传信息
        CheckpointSlot::Relocation(_) => return Err(Fail::refused(
            "an unfinished relocation job occupies the checkpoint slot — resume it first (re-run the original `resize ... grow` / `apply`)",
        )),
        CheckpointSlot::Ambiguous(paths) => return Err(ambiguous_checkpoint(&paths)),
        CheckpointSlot::Empty => None,
    };
    // Some(搬移前的 old 区间) ⇒ 表项已提交，仅剩 FS 收尾
    let mut committed_old: Option<(u64, u64)> = None;
    let (fs_shrunk, resume_chunks) = match existing {
        Some(c) => {
            // 已提交态：判据是"盘上几何已是目标位置"，而不是 ckpt 里的标记——标记与 commit
            // 之间总有先后，落在这个窗口里中断会留下"标记说未提交、盘已提交"的 ckpt，
            // 只认标记就会让常规校验判为"参数不匹配"而永久拒绝。此时唯一未完成的是
            // FS 扩容收尾（幂等），直接跳过搬移阶段执行收尾
            if c.part == part && c.disk_size == src.size && c.ss == ss
                && old_start == c.new_start && old_end == c.new_end
            {
                log("resize already committed — finishing filesystem step");
                committed_old = Some((c.old_start, c.old_end));
                (c.fs_shrunk, c.chunks_done)
            } else {
                let matches = c.disk_size == src.size && c.ss == ss && c.part == part
                    && c.old_start == old_start && c.old_end == old_end
                    && c.new_start == new_start && c.new_end == new_end
                    && c.chunk_bytes == chunk_len;
                if !matches {
                    return Err(Fail::refused("existing checkpoint does not match this resize — refusing"));
                }
                // Y = durable 恢复点（ckpt 文件里的值），不是本进程内存进度
                log(&format!("resuming at chunk {} (durable checkpoint)", c.chunks_done));
                (c.fs_shrunk, c.chunks_done)
            }
        }
        None => (false, 0),
    };

    // 阶段 0/3 的 FS 大小判据：已提交态下用 ckpt 记录的搬移前形状，否则本次请求的旧形状
    let (fb_start, fb_end) = committed_old.unwrap_or((old_start, old_end));
    let old_bytes = (fb_end - fb_start + 1) * ss;
    let new_bytes = (new_end - new_start + 1) * ss;
    let mut ckpt = RsCheckpoint {
        disk_size: src.size, ss, part,
        old_start, old_end, new_start, new_end,
        fs_shrunk, chunks_done: resume_chunks,
        chunk_bytes: chunk_len,
    };
    let save = |c: &RsCheckpoint| atomic_write_ckpt(&ckpt_path, &c.serialize());

    // 写盘前的 preflight：本操作的后置条件含 FS 调整，工具缺失必须在阶段 0
    // （缩容时 FS shrink 即首次写盘）之前拒绝，否则会留下"表已改、FS 未改"的中间态。
    // --no-fs 与缩容不可共存：分区末端越过未缩的 FS 元数据即数据损坏，这不是"少做一步"
    // 而是另一种操作，必须在写盘前拒绝（已缩过 FS 的续传不在此列）
    if no_fs {
        if new_bytes < old_bytes && !ckpt.fs_shrunk {
            return Err(Fail::refused(
                "--no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the new partition end would cut into filesystem metadata",
            ));
        }
    } else if new_bytes < old_bytes {
        crate::fsops::check_shrink(fstype).map_err(Fail::refused)?;
    } else if new_bytes > old_bytes {
        crate::fsops::check_grow(fstype).map_err(Fail::refused)?;
    }

    // ---- 阶段 0：缩容 FS 先缩（只做一次）----
    // 本块只在 !no_fs 时可达（no_fs + 缩容 + FS 未缩已在上面整体拒绝），因此
    // "能否缩 / 类型是否认得 / 工具是否齐备"三重判据已由上面的 check_shrink 一次性给出。
    // 此处不另设判据，以 check_shrink 的三重结果（能否缩 / 类型 / 工具）为准
    if new_bytes < old_bytes && !ckpt.fs_shrunk {
        // ext 预查 FS 最小尺寸（resize2fs -P × dumpe2fs -h 块大小），缩太小在写盘前拒绝。
        // 归 Failed 的理由同上：ensure_geometry 的 repair 可能已写过盘
        if let Some(min) = crate::fsops::fs_min_bytes(src, part, fstype)?
            && new_bytes < min
        {
            return Err(Fail::refused(format!(
                "target size {new_bytes} < minimum FS size {min} bytes (resize2fs -P)"
            )));
        }
        crate::fsops::shrink_fs(src, part, fstype, new_bytes)?;
        ckpt.fs_shrunk = true;
        save(&ckpt)?;
        log(&format!("fs shrunk to {new_bytes} bytes"));
        fault_rs_after_fs_shrink();
    }

    // ---- 阶段 1：数据搬移（方向感知 + chunk 续传）----
    let delta = new_start as i64 - old_start as i64;
    if delta != 0 && committed_old.is_none() {
        // 含数据搬移：字节不入 journal，留标记令 undo 拒绝（前向恢复、无回滚）
        src.mark_relocation()?;
        let src_off = old_start * ss;
        let dst_off = new_start * ss;
        let total = (old_end - old_start + 1) * ss;
        let order: Vec<(u64, u64)> = {
            let mut v = Vec::new();
            let mut pos = 0u64;
            while pos < total {
                let len = chunk_len.min(total - pos);
                v.push((pos, len));
                pos += len;
            }
            if delta > 0 { v.reverse(); }
            v
        };
        for (i, (within, len)) in order.iter().enumerate() {
            if (i as u64) < ckpt.chunks_done {
                continue;
            }
            let mut buf = vec![0u8; *len as usize];
            src.read_at(src_off + within, &mut buf)?;
            src.write_data_at(dst_off + within, &buf)?;
            src.sync_data()?;
            ckpt.chunks_done = i as u64 + 1;
            save(&ckpt)?;
            fault_rs_chunk(i as u64 + 1);
        }
        log(&format!("data moved by {delta} sectors"));
    }
    // ---- 阶段 2：提交表项（幂等：重复执行结果相同；已提交态跳过）----
    if committed_old.is_none() {
        fault_rs_before_commit();
        let mut g2 = table::load_gpt(src).map_err(table::flatten)?.ok_or_else(|| io::Error::other("GPT vanished mid-resize"))?;
        {
            let te = &mut g2.entries[(part - 1) as usize];
            te.starting_lba = new_start;
            te.ending_lba = new_end;
        }
        let last_lba = src.size / ss - 1;
        table::commit_gpt(src, &g2, last_lba)?;
        table::ensure_protective_mbr(src)?;
        // 提交后不写 ckpt：恢复分支以盘上几何判定"是否已提交"，不需要第二份标记。
        // 此处中断后的重放由表项的绝对赋值保证幂等 —— fault_rs_after_commit 正测这一点
        log(&format!("partition {part} committed at {new_start}..{new_end}"));
        fault_rs_after_commit();
    }

    // 起始 LBA 变了才需要修 NTFS HiddenSectors（扩缩不动 start 时跳过）
    if delta != 0 && fstype == "ntfs" {
        fix_ntfs_hidden_sectors(src, new_start, ss, log)?;
    }

    // ---- 阶段 3：扩容 FS ----
    crate::dev::warn_if_remove_failed(&ckpt_path);
    // 扩容 FS：unknown/LVM PV 无本工具可扩的文件系统，跳过
    if no_fs {
        log("partition resized (--no-fs: filesystem left untouched)");
    } else if new_bytes > old_bytes && !matches!(fstype, "unknown" | "lvm2_pv") {
        if fstype == "swap" {
            // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持；
            // swap 目标拒绝搬移，起始未变，旧头部仍在原位可读）
            let ident = read_swap_identity(src, old_start, old_end - old_start + 1, ss);
            match crate::fsops::recreate_swap(src, part, ident) {
                Ok(()) => log("swap recreated (UUID preserved)"),
                Err(e) => pending.push(Pending::new(
                    part,
                    PendingKind::Swap,
                    e.to_string(),
                    crate::fsops::rescue_hint("swap", &crate::dev::part_dev_hint(src, part, old_start * ss), false),
                )),
            }
        } else {
            match crate::fsops::resize_fs(src, part, fstype) {
                Ok(()) => log("fs grown"),
                Err(e) => pending.push(Pending::new(
                    part,
                    PendingKind::Fs,
                    e.to_string(),
                    crate::fsops::rescue_hint(fstype, &crate::dev::part_dev_hint(src, part, old_start * ss), false),
                )),
            }
        }
    }
    Ok(())
}

fn ntfs_hidden_value(start_lba: u64) -> (u32, bool) {
    if start_lba >> 32 == 0 { (start_lba as u32, false) } else { (0u32, true) }
}

/// NTFS boot sector BPB HiddenSectors（偏移 0x1C，u32 LE，值 = 分区起始 LBA；
/// NTFS 引导扇区布局，ntfs-3g bootsect.c 与微软 NTFS 规范同此定义）。
/// 字段只有 32 位：start ≥ 2^32 时写 0 并警告（Windows 将无法从该分区引导，
/// 数据不受影响）。仅在分区起始 LBA 变化后调用。
fn fix_ntfs_hidden_sectors(src: &mut FileSource, new_first_lba: u64, ss: u64, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    let (val, warn) = ntfs_hidden_value(new_first_lba);
    src.write_at(new_first_lba * ss + 0x1C, &val.to_le_bytes())?;
    src.sync_data()?;
    if warn {
        log("warning: partition starts beyond 2^32; HiddenSectors zeroed — Windows cannot boot from it");
    }
    Ok(())
}

/// 字节级分区复制：源不动，目标范围逐 chunk 复制后新增条目。
/// 目标范围由 add_entry_at 的重叠校验把关；数据复制先于表项提交。
/// 返回 `Fail`：复制已开始时写盘失败须报"可能已改变"（复制阶段不改表，但数据区已动），
/// 而范围/重叠这类事前校验必须报"拒绝"
pub fn copy_part(src: &mut FileSource, part: u32, new_start: u64, name: &str, chunk_len: u64, log: &mut dyn FnMut(&str)) -> Result<u32, Fail> {
    let g = gpt_policy::ensure_geometry(src)?.ok_or_else(|| Fail::refused("no GPT"))?;
    let ss = g.ss;
    let e = g.entries.get((part - 1) as usize)
        .ok_or_else(|| Fail::refused(format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err(Fail::refused(format!("partition {part} is empty")));
    }
    let len = e.ending_lba - e.starting_lba + 1;
    // checked：new_start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过下方边界校验
    let new_end = new_start.checked_add(len - 1)
        .ok_or_else(|| Fail::refused("copy target overflows address space"))?;
    if new_start < g.header.first_usable_lba || new_end > g.header.last_usable_lba {
        return Err(Fail::refused("copy target outside usable range"));
    }
    for (i, other) in g.entries.iter().enumerate() {
        if other.ending_lba == 0 {
            continue;
        }
        if !(new_end < other.starting_lba || new_start > other.ending_lba) {
            return Err(Fail::refused(format!("copy target overlaps partition #{}", i + 1)));
        }
    }
    let src_off = e.starting_lba * ss;
    let dst_off = new_start * ss;
    let total = len * ss;
    // 含数据复制：字节不入 journal，留标记令 undo 拒绝（前向恢复、无回滚）
    src.mark_relocation()?;
    let mut pos = 0u64;
    while pos < total {
        let len_c = chunk_len.min(total - pos);
        let mut buf = vec![0u8; len_c as usize];
        src.read_at(src_off + pos, &mut buf)?;
        src.write_data_at(dst_off + pos, &buf)?;
        src.sync_data()?;
        pos += len_c;
    }
    let guid = e.partition_type_guid;
    let num = table::add_entry_at(src, new_start, new_end, name, guid, e.unique_partition_guid)?;
    // 副本的 boot sector 原样带来旧 HiddenSectors，按新起始位置修正
    if crate::fsid::identify(src, new_start * ss, len * ss)? == "ntfs" {
        fix_ntfs_hidden_sectors(src, new_start, ss, log)?;
    }
    log(&format!("partition {part} copied to #{num} at {new_start}..{new_end}"));
    Ok(num)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::let_underscore_must_use)] // 测试的清理步骤有意忽略失败
    use super::*;
    use gptman::{GPT, GPTPartitionEntry};
    use std::io::Cursor;

    /// HiddenSectors 修正：0x1C 处写入 LE32 起始 LBA；≥2^32 值取 0（纯函数分支 + 落盘各验一次）
    #[test]
    fn ntfs_hidden_sectors_fix() {
        assert_eq!(ntfs_hidden_value(30687), (30687, false));
        assert_eq!(ntfs_hidden_value(u32::MAX as u64), (u32::MAX, false));
        assert_eq!(ntfs_hidden_value(1u64 << 32), (0, true));
        let (src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        drop(src);
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let mut src = FileSource {
            identity: crate::dev::TargetIdentity::resolve(&path, false, size),
            file: f,
            path: path.clone(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
        };
        let mut log = |_: &str| {};
        fix_ntfs_hidden_sectors(&mut src, 30687, 512, &mut log).unwrap();
        let mut b = [0u8; 4];
        src.read_at(30687 * 512 + 0x1C, &mut b).unwrap();
        assert_eq!(b, 30687u32.to_le_bytes());
    }

    /// move 中断续传：模拟"chunk 0 已搬完即崩溃"——预置 checkpoint（chunks_done=1）
    /// + 手工完成 chunk 0 的复制 + 污染源首字节（若续传错误地重搬 chunk 0 会被检出）
    #[test]
    fn resize_part_resume_skips_done_chunks() {
        let ss = 512u64;
        let data = vec![0u8; 16 * 1024 * 1024];
        let mut cur = Cursor::new(data);
        let mut gpt = GPT::new_from(&mut cur, ss, [0xAB; 16]).unwrap();
        gpt[1] = GPTPartitionEntry {
            partition_type_guid: [0x11; 16],
            unique_partition_guid: [0x12; 16],
            starting_lba: 2048,
            ending_lba: 10239, // 8192 扇区 = 4 MiB = 4×1MiB chunk
            attribute_bits: 0,
            partition_name: "a".into(),
        };
        gpt.write_into(&mut cur).unwrap();
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("diskedit_resume_{}.img", std::process::id()));
        std::fs::write(&tmp, cur.into_inner()).unwrap();
        let f = std::fs::OpenOptions::new().read(true).write(true).open(&tmp).unwrap();
        let size = std::fs::metadata(&tmp).unwrap().len();
        let mut src = FileSource {
            identity: crate::dev::TargetIdentity::resolve(&tmp, false, size),
            file: f,
            path: tmp.clone(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
        };
        // gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
        crate::table::ensure_protective_mbr(&mut src).unwrap();

        let chunk: u64 = 1024 * 1024;
        let src_off = 2048u64 * 512;
        let dst_off = 12288u64 * 512; // delta = +4096 扇区
        // 每块填不同指纹
        let mut w = |off: u64, b: u8| {
            let buf = vec![b; chunk as usize];
            src.write_at(off, &buf).unwrap();
        };
        w(src_off, 0x10);
        w(src_off + chunk, 0x11);
        w(src_off + 2 * chunk, 0x12);
        w(src_off + 3 * chunk, 0x13);
        // 模拟崩溃前已完成 chunk 3（右移 = 尾→头推进，chunks_done 按执行序计数）：
        // 手工复制尾块到目标
        let mut last = vec![0u8; chunk as usize];
        src.read_at(src_off + 3 * chunk, &mut last).unwrap();
        src.write_at(dst_off + 3 * chunk, &last).unwrap();
        // 污染源尾块首字节：若续传错误重搬 chunk 3，目标尾块会变 0xEE
        src.write_at(src_off + 3 * chunk, &[0xEEu8]).unwrap();

        // 预置 checkpoint：chunks_done = 1
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        let ckpt = RsCheckpoint {
            disk_size: size, ss, part: 1,
            old_start: 2048, old_end: 10239, new_start: 12288, new_end: 20479,
            fs_shrunk: false, chunks_done: 1, chunk_bytes: chunk,
        };
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

        // 续传：跳过 chunk 0，补齐 chunk 1..3，提交表项
        let o = resize_part(&mut src, 1, 12288, 20479, chunk, false, &mut |_| {});
        assert!(o.is_applied(), "resize must apply (exit {})", o.exit_code());

        // 终验：目标 4 块 = 指纹 0x10..0x13（chunk 0 未被重搬污染）
        for i in 0..4u8 {
            let mut buf = vec![0u8; chunk as usize];
            src.read_at(dst_off + i as u64 * chunk, &mut buf).unwrap();
            assert!(buf.iter().all(|&b| b == 0x10 + i), "chunk {i} mismatch");
        }
        let g2 = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g2.entries[0].starting_lba, 12288);
        assert_eq!(g2.entries[0].ending_lba, 20479);
        assert!(!ckpt_path.exists(), "checkpoint must be removed on success");
        drop(src);
        let _ = std::fs::remove_file(&tmp);
        crate::dev::warn_if_remove_failed(&ckpt_path);
    }

    /// 提交已落盘、ckpt 尚未更新时中断：判据必须落在"盘上几何已是目标"上，否则续传会把
    /// "参数不匹配"判成永久拒绝。反向断言：源区指纹不得出现在目标区——若误走续传路径，
    /// 数据会被再搬一次
    #[test]
    fn resize_part_resume_after_commit_without_ckpt_update() {
        let ss = 512u64;
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 10239)]);
        let (old_start, old_end) = (2048u64, 10239u64);
        let (new_start, new_end) = (12288u64, 20479u64);
        let chunk: u64 = 1024 * 1024;
        src.write_at(old_start * ss, &[0xA5u8; 512]).unwrap();

        // 模拟"commit 已完成"：把表项改到目标位置并落盘
        let mut g = table::load_gpt(&src).unwrap().unwrap();
        {
            let e = &mut g.entries[0];
            e.starting_lba = new_start;
            e.ending_lba = new_end;
        }
        let last_lba = src.size / ss - 1;
        table::commit_gpt(&mut src, &g, last_lba).unwrap();
        table::ensure_protective_mbr(&mut src).unwrap();

        // ckpt 停留在搬移前形状：正是 commit 与 ckpt 更新之间的那个窗口
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        let ckpt = RsCheckpoint {
            disk_size: src.size, ss, part: 1,
            old_start, old_end, new_start, new_end,
            fs_shrunk: false,
            chunks_done: (old_end - old_start + 1) * ss / chunk,
            chunk_bytes: chunk,
        };
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

        let o = resize_part(&mut src, 1, new_start, new_end, chunk, true, &mut |_| {});
        assert!(o.is_applied(), "must finish the FS step instead of refusing (exit {})", o.exit_code());
        assert!(!ckpt_path.exists(), "checkpoint must be removed on success");
        let g2 = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!((g2.entries[0].starting_lba, g2.entries[0].ending_lba), (new_start, new_end));
        let mut b = [0u8; 512];
        src.read_at(new_start * ss, &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 0), "committed resize must not move data again");
        drop(src);
        let _ = std::fs::remove_file(&path);
    }

    /// 两族作业共用一个 checkpoint 槽位。"对方在现场"必须与"空槽"分开对待：当成空槽会覆盖掉
    /// 那份唯一的续传信息，而被中断的作业盘上几何可能正停在半途
    #[test]
    fn checkpoint_slot_ownership_is_enforced() {
        let chunk: u64 = 1024 * 1024;
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
        let g = table::load_gpt(&src).unwrap().unwrap();
        let ss = g.ss;
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        let plan = make_plan(&mut src, 1).unwrap();
        assert!(!plan.moves.is_empty(), "fixture must have a blocker to relocate");

        // (a) 槽位里是单分区 resize 的 ckpt → apply 拒绝，且不得动盘、不得覆盖它
        let rs = RsCheckpoint {
            disk_size: src.size, ss, part: 1,
            old_start: 2048, old_end: 6143, new_start: 2048, new_end: 3071,
            fs_shrunk: false, chunks_done: 0, chunk_bytes: chunk,
        };
        atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();
        let o = apply(&mut src, &plan, chunk, true, &mut |_| {});
        assert_eq!(o.exit_code(), crate::outcome::EXIT_REFUSED, "apply must refuse while a resize job owns the slot");
        assert!(matches!(read_checkpoint(&src).unwrap(), CheckpointSlot::Resize(_)), "the resize checkpoint must survive");
        assert_eq!(table::load_gpt(&src).unwrap().unwrap().entries[1].starting_lba, 8192, "nothing may be relocated");

        // (b) 槽位里是 plan 型搬移的 ckpt → resize_part 拒绝（不能按"无 ckpt"重做）
        std::fs::remove_file(&ckpt_path).unwrap();
        let ck = Checkpoint {
            disk_size: src.size, ss, grow_part: 1, last_usable_lba: plan.last_usable_lba,
            moves: plan.moves.clone(), cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
        };
        atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();
        let o = resize_part(&mut src, 1, 2048, 3071, chunk, false, &mut |_| {});
        assert!(
            matches!(&o, Outcome::Refused(m) if m.contains("relocation job")),
            "resize_part must refuse while a relocation job owns the slot: {o:?}"
        );
        assert!(matches!(read_checkpoint(&src).unwrap(), CheckpointSlot::Relocation(_)), "the relocation checkpoint must survive");

        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&path);
    }

    /// 候选落点不存在即空槽（从未跑过搬移是常态）；文件在而解不出来则是故障——
    /// 若把它当空槽，中断的搬移会被降级成一次全新规划
    #[test]
    fn corrupt_checkpoint_is_an_error_not_an_empty_slot() {
        let (src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();

        assert!(matches!(read_checkpoint(&src).unwrap(), CheckpointSlot::Empty));

        // 头部魔数在、内容被截断：正是断电撕裂写下的样子
        std::fs::write(&ckpt_path, CKPT_MAGIC).unwrap();
        let e = read_checkpoint(&src).err().expect("a corrupt checkpoint must not read as an empty slot");
        assert!(matches!(&e, Fail::Infra(m) if m.contains("unreadable")), "{e:?}");

        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&path);
    }

    /// `shift = None`（缩容请求）不产生新的搬移计划：无 ckpt 必须拒绝而不是现算一个；
    /// 有 ckpt 则以 ckpt 为准（那才是盘上真正在做的事）
    #[test]
    fn shift_plan_none_never_ghost_relocates() {
        let chunk: u64 = 1024 * 1024;
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
        let g = table::load_gpt(&src).unwrap().unwrap();
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();

        let e = match make_plan_shift_resuming(&mut src, 1, None) {
            Err(e) => e,
            Ok(_) => panic!("no checkpoint must mean no relocation plan for a shrink"),
        };
        assert!(matches!(&e, Fail::Refused(m) if m.contains("shrink")), "{e:?}");

        let plan = make_plan(&mut src, 1).unwrap();
        let ck = Checkpoint {
            disk_size: src.size, ss: g.ss, grow_part: 1, last_usable_lba: plan.last_usable_lba,
            moves: plan.moves.clone(), cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
        };
        atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();
        let resumed = make_plan_shift_resuming(&mut src, 1, None).unwrap();
        assert_eq!(resumed.moves.len(), plan.moves.len(), "resume must return the checkpoint's plan");
        assert!(has_pending_relocation(&src, 1).unwrap());
        assert!(!has_pending_relocation(&src, 2).unwrap(), "another partition has no pending job");

        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&path);
    }

    /// make_plan 边界：多 movable 尾部打包（末→首、末端对齐）、swap 标记、LUKS 拒绝、
    /// 尾部空间不足拒绝（总 movable 超出目标之后的容量）
    fn plan_fixture(entries: &[(u32, [u8; 16], u64, u64)]) -> (FileSource, std::path::PathBuf) {
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
        let mut src = FileSource {
            identity: crate::dev::TargetIdentity::resolve(&tmp, false, size),
            file: f,
            path: tmp.clone(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
        };
        // gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
        crate::table::ensure_protective_mbr(&mut src).unwrap();
        (src, tmp)
    }

    fn plan_open(path: &std::path::Path) -> FileSource {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        let size = std::fs::metadata(path).unwrap().len();
        FileSource {
            identity: crate::dev::TargetIdentity::resolve(path, false, size),
            file: f,
            path: path.into(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
        }
    }

    #[test]
    fn make_plan_pack_and_boundaries() {
        const SWAP: [u8; 16] = [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F];
        const LUKS: [u8; 16] = [0x25, 0xCF, 0xD3, 0x7D, 0x31, 0x5C, 0x34, 0x47, 0xAD, 0xBF, 0xA4, 0x7E, 0x95, 0x4A, 0x9D, 0x24];
        // root 2048..6143 | home 6144..10239 | swap 10240..12287
        let (mut src, _p1) = plan_fixture(&[
            (1, [0x11; 16], 2048, 6143),
            (2, [0x22; 16], 6144, 10239),
            (3, SWAP, 10240, 12287),
        ]);
        let plan = make_plan(&mut src, 1).unwrap();
        assert_eq!(plan.moves.len(), 2);
        // 末→首执行序：swap 先（重建），home 后（搬移）；末端对齐 last_usable=32734
        assert!(plan.moves[0].is_swap && plan.moves[0].part_num == 3);
        assert_eq!(plan.moves[0].first_lba + plan.moves[0].delta_lba, 30687, "swap new first_lba");
        assert!(!plan.moves[1].is_swap && plan.moves[1].part_num == 2);
        assert_eq!(plan.moves[1].first_lba + plan.moves[1].delta_lba, 26591, "home new first_lba");
        drop(src);

        // LUKS 在目标之后 → 拒绝
        let (mut src2, _p2) = plan_fixture(&[
            (1, [0x11; 16], 2048, 6143),
            (2, LUKS, 6144, 10239),
        ]);
        assert!(make_plan(&mut src2, 1).is_err());
        drop(src2);

        // 尾部容量不足：条目越过 last_usable（损坏表）→ 打包后 delta<0 拒绝
        // （有效布局下 delta≥0 恒成立，此校验防线针对损坏/越界表）
        // gptman 会拒绝写入越界条目，故先写合法表再手工修补原始字节 + 重算双 CRC
        let (src3, p3) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 30000, 32000)]);
        drop(src3);
        {
            let mut raw = std::fs::read(&p3).unwrap();
            let arr_off = 2 * 512;
            let ent_off = arr_off + 128; // 条目 2（索引 1）
            raw[ent_off + 40..ent_off + 48].copy_from_slice(&34000u64.to_le_bytes()); // ending_lba 越界
            let arr_crc = table::crc32(&raw[arr_off..arr_off + 128 * 128]);
            raw[512 + 88..512 + 92].copy_from_slice(&arr_crc.to_le_bytes());
            raw[512 + 16..512 + 20].fill(0);
            let hdr_crc = table::crc32(&raw[512..512 + 92]);
            raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
            std::fs::write(&p3, &raw).unwrap();
        }
        let mut src3 = plan_open(&p3);
        assert!(make_plan(&mut src3, 1).is_err());
        drop(src3);
        let _ = std::fs::remove_file(&p3);
    }

    /// grow_end 公式的纯算术边界（不依赖任何平台）
    #[test]
    fn grow_end_formula_and_overflow() {
        let plan_of = |moves: Vec<PlanEntry>, last_usable: u64| Plan { ss: 512, last_usable_lba: last_usable, grow_part: 1, moves, repair: RepairAction::None };
        let mv = |first_lba: u64, delta_lba: u64| PlanEntry { part_num: 1, first_lba, len_lba: 10, delta_lba, is_swap: false };

        // 无 movable → last_usable_lba
        assert_eq!(grow_end_for(&plan_of(vec![], 32734)).unwrap(), 32734);
        // last_usable_lba 取满值也不得溢出
        assert_eq!(grow_end_for(&plan_of(vec![], u64::MAX)).unwrap(), u64::MAX);
        // delta = 0（已贴合的 movable 同样进 moves）→ 其新起点 − 1
        assert_eq!(grow_end_for(&plan_of(vec![mv(1000, 0)], 32734)).unwrap(), 999);
        // 多个 movable → 取最小新起点 − 1（100→150、200→250）
        assert_eq!(grow_end_for(&plan_of(vec![mv(100, 50), mv(200, 50)], 32734)).unwrap(), 149);
        // first_lba + delta_lba 溢出 → 报错，不回绕
        assert!(grow_end_for(&plan_of(vec![mv(u64::MAX - 5, 10)], u64::MAX)).is_err());
        // 新起点越过 last_usable_lba → 拒绝（越界表不得写进几何）
        assert!(grow_end_for(&plan_of(vec![mv(40000, 0)], 32734)).is_err());
    }

    /// 真实路径：truncate 预扩后 backup GPT 与保护 MBR 同时过期 —— 解析标记 NeedsRepair、
    /// plan 只记录修复动作（不写盘）、写入路径 repair 到新末端
    #[test]
    fn stale_after_enlarge_detected_then_repaired() {
        use std::io::Write as _;
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        let old_file_last = src.size / 512 - 1;
        drop(src);
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            f.write_all(&vec![0u8; 1024 * 1024]).unwrap();
        }
        let mut src = plan_open(&p);
        let new_file_last = src.size / 512 - 1;
        let g = table::load_gpt(&src).unwrap().expect("扩容后仍应可识别");
        assert_eq!(
            g.state,
            table::GptState::NeedsRepair {
                cause: table::HeaderIssue::BackupLbaStale { expected: new_file_last, actual: old_file_last }
            }
        );
        assert_eq!(
            g.pmbr,
            table::PmbrSize::NeedsRepair { cause: table::PmbrIssue::Stale }
        );
        assert_eq!(g.header.backup_lba, old_file_last, "解析不得偷偷改写盘上表");
        // plan 只记录修复动作，不写盘；计划几何按修复后的值算
        let plan = make_plan(&mut src, 1).unwrap();
        assert!(matches!(
            plan.repair,
            RepairAction::RelocateAndRepair { .. }
        ), "扩容后备份头与保护 MBR 都过期，动作须同时覆盖两者");
        assert_eq!(plan.last_usable_lba, new_file_last - 32 - 1);
        assert_eq!(
            table::load_gpt(&src).unwrap().unwrap().state,
            table::GptState::NeedsRepair {
                cause: table::HeaderIssue::BackupLbaStale { expected: new_file_last, actual: old_file_last }
            },
            "plan 不得写盘"
        );
        // 写入路径：修复到新末端 + 保护 MBR 与容器一致
        let g2 = gpt_policy::ensure_geometry(&mut src).unwrap().unwrap();
        assert_eq!(g2.state, table::GptState::Valid);
        assert_eq!(g2.pmbr, table::PmbrSize::Normal);
        assert_eq!(g2.header.backup_lba, new_file_last);
        assert_eq!(g2.header.last_usable_lba, new_file_last - 32 - 1); // span = 128×128/512
        assert_eq!(gpt_policy::classify_repair(&g2, new_file_last).unwrap(), RepairAction::None, "修复须幂等");
        drop(src);
        let _ = std::fs::remove_file(&p);
    }

    /// 只有 backup 头过期（保护 MBR 与容器一致）时：PMBR 无需重写，只搬移备份头
    #[test]
    fn stale_backup_detected_then_relocated() {
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        let file_last = src.size / 512 - 1;
        drop(src);
        {
            // 把主头的 AlternateLBA 改到 20000（早于末端），重算头 CRC——构造 stale 状态
            let mut raw = std::fs::read(&p).unwrap();
            raw[512 + 32..512 + 40].copy_from_slice(&20000u64.to_le_bytes());
            raw[512 + 16..512 + 20].fill(0);
            let hdr_crc = table::crc32(&raw[512..512 + 92]);
            raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
            std::fs::write(&p, &raw).unwrap();
        }
        let mut src = plan_open(&p);
        let g = table::load_gpt(&src).unwrap().expect("stale GPT must still be identifiable");
        assert_eq!(
            g.state,
            table::GptState::NeedsRepair {
                cause: table::HeaderIssue::BackupLbaStale { expected: file_last, actual: 20000 }
            }
        );
        assert_eq!(g.pmbr, table::PmbrSize::Normal, "夹具的保护 MBR 与容器一致");
        assert_eq!(g.header.backup_lba, 20000, "解析不得偷偷改写盘上表");
        // 写入路径：就地修复并把备份头搬到设备末端
        let g2 = gpt_policy::ensure_geometry(&mut src).unwrap().unwrap();
        assert_eq!(g2.state, table::GptState::Valid);
        assert_eq!(g2.pmbr, table::PmbrSize::Normal);
        assert_eq!(g2.header.backup_lba, file_last);
        assert_eq!(g2.header.last_usable_lba, file_last - 32 - 1); // span = 128×128/512
        assert_eq!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::Valid, "修复须幂等");
        drop(src);
        let _ = std::fs::remove_file(&p);
    }

    /// 几何自洽性在解析层强制：last_usable_lba 越过盘末端即拒绝（不必等下游算术兜底）
    #[test]
    fn geometry_rejects_last_usable_beyond_disk() {
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        drop(src);
        // 对照组：未打补丁时表可解析——证明下面的 Err 来自几何判定而非 CRC 写错
        let clean = plan_open(&p);
        assert!(table::load_gpt(&clean).unwrap().is_some());
        drop(clean);
        {
            // 手改 last_usable_lba 后重算头 CRC，才能穿过签名/CRC 抵达几何判定
            let mut raw = std::fs::read(&p).unwrap();
            raw[512 + 48..512 + 56].copy_from_slice(&u64::MAX.to_le_bytes());
            raw[512 + 16..512 + 20].fill(0);
            let hdr_crc = table::crc32(&raw[512..512 + 92]);
            raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
            std::fs::write(&p, &raw).unwrap();
        }
        let mut src = plan_open(&p);
        // 拒绝发生在解析层（所有消费者共享），ensure_geometry 只是把该错误透传出来
        let err = match table::load_gpt(&src) {
            Err(e) => e,
            Ok(_) => panic!("beyond-container last_usable_lba must be refused at parse time"),
        };
        assert!(
            matches!(err, table::GptError::BeyondContainer { field: "last_usable_lba", .. }),
            "{err}"
        );
        assert!(gpt_policy::ensure_geometry(&mut src).is_err());
        drop(src);
        let _ = std::fs::remove_file(&p);
    }
}