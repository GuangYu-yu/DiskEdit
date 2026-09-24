//! 分区搬移与中间分区扩容闭环。
//!
//! 前向恢复、无回滚；单分区 = 单事务；checkpoint 原子写
//! （tmp → sync → rename → fsync 父目录，其中父目录 fsync 仅 unix 实现——
//! Windows 上承诺降级为 rename 语义，不经此路径写盘所以不影响恢复协议）；
//! 复制方向：delta ≥ 0 且尾→头推进，
//! 写点恒在未读源之上（右移时目的地址总是大于已读位置），顺序固化不提供方向参数。

use crate::dev::FileSource;
use crate::geometry::{GeometryLimits, ValidatedGeometry};
use crate::gpt_policy::{self, RepairAction};
use crate::outcome::{Fail, Outcome, Pending, PendingKind};
use crate::table;
use std::io;
use std::path::PathBuf;

pub const CKPT_MAGIC: &[u8; 8] = b"DKECKPT1";
/// v5：追加目标指纹（3×u64）与 loop 映射（1+2×u64）。旧版本不做兼容读——
/// checkpoint 是活操作的暂存现场而非持久资产，读到旧版本报 unsupported 并指向 abandon
pub const CKPT_VERSION: u32 = 5;

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

/// plan 的进入语义。`grow` 与 `SIZE` 两条命令共用同一个续跑槽位，续跑时必须
/// 对上号：按自己的语义执行对方的 plan，就是把用户的请求静默放大或缩水
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PlanKind {
    /// 吃满右侧空闲的尾部打包（`resize grow` / `plan` / `apply`）
    TailPacked,
    /// 精确落在请求新末端的最小位移（`resize SIZE + --allow-move`）
    Shift,
}

pub struct Plan {
    pub ss: u64,
    pub last_usable_lba: u64,
    pub grow_part: u32,
    pub moves: Vec<PlanEntry>,
    pub kind: PlanKind,
    /// 待执行的修复动作：plan 只记录，apply 执行（plan 本身不写盘）
    pub repair: RepairAction,
}

/// movable 集合与两条硬拒绝的唯一构造处：目标之后所有已定义条目（按起点升序）。
/// 端点倒挂（盘内容故障）与 LUKS/LVM PV（不可搬移）在此一并给出——
/// 两条规划路径共用，拒绝理由不会漂移
fn blockers_after(
    g: &ValidatedGeometry,
    target_end: u64,
) -> Result<Vec<(u32, &gptman::GPTPartitionEntry)>, Fail> {
    let mut movable: Vec<(u32, &gptman::GPTPartitionEntry)> = g
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.ending_lba != 0 && e.starting_lba > target_end)
        .map(|(i, e)| ((i + 1) as u32, e))
        .collect();
    movable.sort_by_key(|(_, e)| e.starting_lba);
    for (num, e) in &movable {
        // 条目端点倒挂属盘内容故障（解析层已拦，这里是纵深防守）
        if e.ending_lba < e.starting_lba {
            return Err(Fail::infra(format!("partition {num} has ending LBA below starting LBA (corrupted table)")));
        }
        if FORBIDDEN_TYPE_GUIDS.contains(&e.partition_type_guid) {
            return Err(Fail::refused(format!("partition {num} is LUKS/LVM PV — relocation refused")));
        }
    }
    Ok(movable)
}

/// plan 生成：只解析 + 验证 + 计算，不写盘。stale 表按"修复后"的几何生成计划，
/// 并把修复动作记入 plan.repair，由 apply 执行。
/// 几何只在此解析一次（[`ValidatedGeometry`] = 修复后将生效的几何），随后的
/// 续传判定与规划共用它——历史实现让 read_checkpoint 与 resolve_geometry 各读一次表
/// 尾打包规划的唯一实现处：几何已由调用方解析，本函数不再读盘、不再重新推导边界。
/// 生产路径只有恢复感知入口（[`make_plan_resuming`]，几何解析一次），本函数由它直接调用
fn plan_tail_packed(g: &ValidatedGeometry, repair: RepairAction, grow_part: u32) -> Result<Plan, Fail> {
    let ss = g.ss;

    // 目标分区必须存在（唯一查找出口见 live_entry）
    let target = gpt_policy::live_entry(g, grow_part)?;

    let movable = blockers_after(g, target.ending_lba)?;

    let mut moves = Vec::new();
    let mut cursor = g.header.last_usable_lba;
    for (num, e) in movable.iter().rev() {
        let len = e.ending_lba - e.starting_lba + 1;
        // 尾部打包：末端对齐 cursor（含），不留缝隙
        let new_first = cursor.checked_sub(len - 1).ok_or_else(|| {
            Fail::refused("insufficient tail space for relocation")
        })?;
        // 目标起点低于源起点 = 尾部空间不足
        let delta = new_first.checked_sub(e.starting_lba).ok_or_else(|| {
            Fail::refused("insufficient tail space for relocation")
        })?;
        // saturating：new_first 理论上 ≥1（LBA0 保护 MBR + LBA1 主头），
        // 极端输入下防下溢 panic，退化为 0 后由下轮 checked_sub 报空间不足
        cursor = new_first.saturating_sub(1);
        moves.push(PlanEntry { part_num: *num, first_lba: e.starting_lba, len_lba: len, delta_lba: delta, is_swap: is_swap_guid(&e.partition_type_guid) });
    }
    Ok(Plan { ss, last_usable_lba: g.header.last_usable_lba, grow_part, moves, kind: PlanKind::TailPacked, repair })
}

/// 尾打包 plan 的恢复感知版本：ckpt 存在时以 ckpt 的 moves 为准（见 resume_outcome），
/// 否则照常现算。几何与修复动作由调用方解析（一条命令解析一次，往下传），本函数不读表
pub fn make_plan_resuming(
    src: &mut FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
) -> Result<Plan, Fail> {
    match resume_outcome(src, g, grow_part)? {
        ResumeOutcome::Belongs(p) => Ok(p),
        // 槽位是别的分区的作业：现算的 plan 与那份作业的现场不符，打印出来会与
        // 盘上未收尾的事自相矛盾，apply 也只会得到一次注定失败的比对
        ResumeOutcome::Occupied => Err(foreign_checkpoint_refusal()),
        // 槽位被单分区 resize 作业占着：与 apply 同一结论、同一文案（那里在 prepare_apply
        // 的分派处拒绝）。只修 apply 不修这里，plan 就会打印一份 apply 必然拒绝的计划并报成功
        ResumeOutcome::ForeignResize => Err(foreign_resize_refusal()),
        ResumeOutcome::Empty => plan_tail_packed(g, repair, grow_part),
    }
}

/// 最小位移 plan：目标分区增长 `shift` 扇区，其右侧所有挡路分区整体右移让位，间隙保持。
/// 与 make_plan（尾打包）相对，用于精确大小的扩容（resize SIZE + --allow-move）。
/// 挡路分区的位移量 = shift − 目标与首个挡路者之间的间隙：首个挡路分区新起点恰为
/// 目标原末端 + shift + 1，apply 的扩容终点（min(new_first) − 1）因此精确落在
/// 请求的新末端——既不多吞目标与挡路者之间的间隙，也不少给。
/// moves 按起始 LBA 降序排列（apply 升序处理 → 最右侧先搬）：
/// 各分区的目的区要么落在尾部空闲，要么落在其右侧已被搬空分区的旧位置，拷贝永不踩源
/// 最小位移规划的唯一实现处：几何已由调用方解析，本函数不再读盘
fn shift_plan_from(g: &ValidatedGeometry, repair: RepairAction, grow_part: u32, shift: u64) -> Result<Plan, Fail> {
    let ss = g.ss;

    let target = gpt_policy::live_entry(g, grow_part)?;
    if shift == 0 {
        return Err(Fail::refused("shift must be positive"));
    }

    let movable = blockers_after(g, target.ending_lba)?;
    if movable.is_empty() {
        return Err(Fail::refused("no partition to relocate (right side is free — no --allow-move needed)"));
    }

    // 位移量锚定首个挡路者：扣掉目标右侧的既有间隙后才是各分区需要让出的量
    let leading_gap = movable[0].1.starting_lba - target.ending_lba - 1;
    let delta = shift.checked_sub(leading_gap).filter(|d| *d >= 1)
        .ok_or_else(|| Fail::refused("shift does not exceed the free gap before the first blocker — relocation not needed"))?;

    let mut moves: Vec<PlanEntry> = Vec::with_capacity(movable.len());
    for (num, e) in &movable {
        let len = e.ending_lba - e.starting_lba + 1;
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
    Ok(Plan { ss, last_usable_lba: g.header.last_usable_lba, grow_part, moves, kind: PlanKind::Shift, repair })
}

/// 中断感知的最小位移 plan：ckpt 存在时以 ckpt 的 moves 为准（见 resume_outcome），
/// 否则按 shift 现算。`shift = None` 表示本次请求是缩容——缩容不搬移任何分区
/// （新末端更靠左，右侧只会更空），此时只有 ckpt 能构成走本路径的理由。
/// 几何与修复动作由调用方解析并下传，本函数不读表
pub fn make_plan_shift_resuming(
    src: &mut FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    grow_part: u32,
    shift: Option<u64>,
) -> Result<Plan, Fail> {
    match resume_outcome(src, g, grow_part)? {
        ResumeOutcome::Belongs(p) => return Ok(p),
        // 与 make_plan_resuming 同判据：别的分区的作业占着槽位，本请求既不能把它
        // 当空槽、也不能按它续跑
        ResumeOutcome::Occupied => return Err(foreign_checkpoint_refusal()),
        // 另一族作业（单分区 resize）占着槽位：同上——现算的位移 plan 与那个作业的现场
        // 不符，它的续跑点由 prepare_resize 按 RsCheckpoint 决定，不在这里
        ResumeOutcome::ForeignResize => return Err(foreign_resize_refusal()),
        ResumeOutcome::Empty => {}
    }
    let shift = shift.ok_or_else(|| Fail::refused(
        "shrink relocates nothing, but an unfinished relocation job is pending on this target — re-run the command that started it to resume",
    ))?;
    shift_plan_from(g, repair, grow_part, shift)
}

/// 该分区是否有未收尾的 plan 型搬移作业。命令入口据此分流：几何上"右侧已空、可直接扩容"
/// 并不代表作业已完成——右侧变空本身可能正是搬了一半的结果，收尾步骤（剩余搬移、swap 重建、
/// FS 扩容）都还没做。几何由调用方解析并下传
pub fn has_pending_relocation(src: &FileSource, g: &ValidatedGeometry, grow_part: u32) -> Result<bool, Fail> {
    Ok(matches!(resume_outcome(src, g, grow_part)?, ResumeOutcome::Belongs(_)))
}

/// 事务门用的归属判定：与 `has_pending_relocation` 同一内核，但把"槽位属于别的分区"
/// 如实报成 `Err`。压成 `false` 的后果是用户只看到通用的"目标被占着"，而真正的原因
/// （该去重跑哪个命令）丢失在领域层
pub(crate) fn relocation_ownership(src: &FileSource, grow_part: u32) -> Result<bool, Fail> {
    match resume_outcome_in(src, grow_part)? {
        ResumeOutcome::Belongs(_) => Ok(true),
        // 空槽与"另一族作业占着"对本事务门是同一结论：都不由它续跑（单分区
        // resize 的现场要用 resize-part / move 续，不是重跑 resize），闸口按 busy 拒绝
        ResumeOutcome::Empty | ResumeOutcome::ForeignResize => Ok(false),
        ResumeOutcome::Occupied => Err(foreign_checkpoint_refusal()),
    }
}

/// 槽位被**别的分区**的搬移作业占用时的拒绝理由。两处 plan 生成路径与事务门共用
/// 同一句话：同一事实的措辞只写一次，否则用户在不同入口看到的原因迟早会漂移
fn foreign_checkpoint_refusal() -> Fail {
    Fail::refused(
        "an unfinished relocation job for a different partition occupies the checkpoint slot — re-run that job's command to finish it first",
    )
}

/// 槽位被**另一族**作业（单分区 resize）占用时的拒绝理由。`prepare_apply` 的分派处与
/// `make_plan_resuming` 共用同一句话：只修一处会让 plan 打印一份 apply 必然拒绝的计划，
/// 而两边给同一事实的措辞也会漂移
fn foreign_resize_refusal() -> Fail {
    Fail::refused(
        "an unfinished single-partition resize job occupies the checkpoint slot — resume it with `resize-part` or `move`, or release it with `diskedit abandon`",
    )
}

/// 三态判定的共用内核：几何在内部解析（调用方手上没有已验证几何）
fn resume_outcome_in(src: &FileSource, grow_part: u32) -> Result<ResumeOutcome, Fail> {
    let (g, _repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
    resume_outcome(src, &g, grow_part)
}

/// resume 判定的三态结论——刻意不压扁成 Option："没有作业"、"是本次请求的作业"、
/// "槽位被别的作业占用"在调用方处各走各路：只有第一种允许现算新计划；第二种以
/// ckpt 为准续跑；第三种必须拒绝——当空槽会覆盖别人的现场，当自己的续跑点会执行
/// 错误的 plan（盘上几何已被那次作业部分改写，现算出的 delta 与其记录不一致）
enum ResumeOutcome {
    Empty,
    /// ckpt 属于本次请求的分区：plan 自持（盘上几何已被部分执行改变，现算必错）
    Belongs(Plan),
    /// 槽位被别的分区的 relocation ckpt 占用；`has_pending_relocation` 只回答"plan 型
    /// 搬移"，搬移路径必须拒绝——现算的 plan 与那份作业的现场不符
    Occupied,
    /// 槽位被**另一族**作业（单分区 resize 的 `RsCheckpoint`）占用。与 `Occupied` 并列而非
    /// 并入，因为两族对它的结论不同：plan 型搬移绝不能在此现算计划——盘上几何已被那次
    /// resize 部分改写，现算的 plan 会打印成功，apply 却必然拒绝；resize/apply 对它
    /// 拒绝（本入口续不了它，出路是 `resize-part` / `move`）
    ForeignResize,
}

/// ckpt 里记录的 plan（仅当它属于该分区）。plan 型搬移的**任何**生成路径在恢复期都必须
/// 以它为准：盘上几何已被部分执行改变，重算出的 delta 与 ckpt 记的不一致，会撞上 apply
/// 的恢复校验而使续传永久失败。ckpt 存在 ⇒ repair 已在 apply 开头执行过（在初始 ckpt
/// 写入之前），故 repair 记 None
fn resume_outcome(src: &FileSource, g: &ValidatedGeometry, grow_part: u32) -> Result<ResumeOutcome, Fail> {
    Ok(match read_checkpoint(src, g)? {
        CheckpointSlot::Ambiguous(paths) => return Err(ambiguous_checkpoint(&paths)),
        CheckpointSlot::Relocation(c) => {
            let c = *c;
            if c.grow_part == grow_part {
                ResumeOutcome::Belongs(Plan {
                    ss: c.ss,
                    last_usable_lba: c.last_usable_lba,
                    grow_part: c.grow_part,
                    moves: c.moves,
                    kind: c.kind,
                    repair: RepairAction::None,
                })
            } else {
                ResumeOutcome::Occupied
            }
        }
        CheckpointSlot::Empty => ResumeOutcome::Empty,
        // 另一族作业（单分区 resize）占着槽位：搬移不得在此现算计划，见 `ForeignResize`
        CheckpointSlot::Resize(_) => ResumeOutcome::ForeignResize,
    })
}

// ---------- checkpoint ----------
//
// 落盘状态的判据：**只持久化盘上无法判定到可枚举有限状态的事实**。按它三选一——
// "表项是否已提交"由盘上的分区几何直接给出（commit 写绝对 LBA、重放幂等），不另存；
// "FS 是否已缩"算得出当前大小、算不出历史（缩过就再也看不出来），必须存（fs_shrunk）；
// "搬到第几 chunk"从数据区只看得出"已覆盖"、看不出"到哪"，必须存（chunks_done）。
// 本文两个 checkpoint（搬移的 Checkpoint 与单分区缩放的 RsCheckpoint）同受此判据约束

pub struct Checkpoint {
    pub disk_size: u64,
    pub ss: u64,
    pub grow_part: u32,
    pub last_usable_lba: u64,
    pub moves: Vec<PlanEntry>,
    pub kind: PlanKind,    // plan 的进入语义（续跑入口据此对号）
    pub cur_index: u32,   // 正在搬的 moves 下标
    pub chunks_done: u64, // 该分区已完成的 chunk 数
    pub chunk_bytes: u64, // chunk 大小（续传不一致即拒绝）
    pub fp: crate::dev::TargetFingerprint, // 目标文件指纹（"同路径换文件"防线）
    pub map: Option<(u64, u64)>,           // 创建时的 loop 映射；None = 非 loop 进入
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
        b.push(self.kind as u8);
        b.extend_from_slice(&self.fp.dev.to_le_bytes());
        b.extend_from_slice(&self.fp.ino.to_le_bytes());
        b.extend_from_slice(&self.fp.size.to_le_bytes());
        b.push(self.map.is_some() as u8);
        if let Some((off, limit)) = self.map {
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&limit.to_le_bytes());
        }
        let crc = table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// 判据来源 `lim` 是**已验证几何**（见 [`GeometryLimits`]）：分区号上界、可用区、
    /// 容器末端都取自盘上那张表的自述几何，故 256 槽位的表与 128 槽位的一样合法
    fn deserialize(b: &[u8], lim: &GeometryLimits) -> io::Result<Self> {
        // magic8 + ver4 + disk_size8 + ss8 + grow_part4 + last_usable8 + count4
        // + cur_index4 + chunks_done8 + chunk_bytes8 + kind1 + fp24 + map1 + crc4：
        // count=0 且 map 缺席时的精确最小值。少算只是把检查让给后面逐字段的 rd 兜底，
        // 常数本身失去防守意义
        if b.len() < 8 + 4 + 8 + 8 + 4 + 8 + 4 + 4 + 8 + 8 + 1 + 24 + 1 + 4 {
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
        // ss 是全链 LBA→字节换算的单位，写地址都经它换算：必须与目标几何一致，
        // 否则 CRC 再对也只会把数据写进错误的位置
        if ss != lim.sector_size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint sector size differs from target geometry"));
        }
        let grow_part = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
        off += 4;
        // 分区号是盘上可控值，下游要拿它直接索引 entries[(n-1)]：上界是该表的条目数
        // （来自已验证几何），超出即损坏
        if !(1..=lim.entry_count).contains(&grow_part) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint grow_part"));
        }
        let last_usable_lba = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        if last_usable_lba > lim.file_last_lba {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint last_usable_lba beyond container"));
        }
        let count = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap()) as usize;
        off += 4;
        // count 是盘上可控值：上界是该表的条目数，超界即损坏，防巨型 with_capacity
        if count as u64 > lim.entry_count as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint move count"));
        }
        let mut moves = Vec::with_capacity(count);
        for _ in 0..count {
            let part_num = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
            // 同上：搬移项的分区号也会被直接索引
            if !(1..=lim.entry_count).contains(&part_num) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint partition number"));
            }
            let first_lba = u64::from_le_bytes(rd(off + 4, 8)?.try_into().unwrap());
            let len_lba = u64::from_le_bytes(rd(off + 12, 8)?.try_into().unwrap());
            let delta_lba = u64::from_le_bytes(rd(off + 20, 8)?.try_into().unwrap());
            let is_swap = rd(off + 28, 1)?[0] != 0;
            // 区间算术全部 checked：这些值会进入 chunk 循环的地址计算
            let end = first_lba
                .checked_add(len_lba)
                .and_then(|v| v.checked_sub(1))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "checkpoint entry range overflow"))?;
            let new_first = first_lba
                .checked_add(delta_lba)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "checkpoint entry target overflow"))?;
            let new_end = new_first
                .checked_add(len_lba)
                .and_then(|v| v.checked_sub(1))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "checkpoint entry target overflow"))?;
            if len_lba == 0
                || first_lba < lim.first_usable_lba
                || end > lim.last_usable_lba
                || new_end > lim.last_usable_lba
            {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint entry outside usable range"));
            }
            moves.push(PlanEntry { part_num, first_lba, len_lba, delta_lba, is_swap });
            off += 29;
        }
        let cur_index = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
        off += 4;
        let chunks_done = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        let chunk_bytes = u64::from_le_bytes(rd(off, 8)?.try_into().unwrap());
        off += 8;
        let kind = match rd(off, 1)?[0] {
            0 => PlanKind::TailPacked,
            1 => PlanKind::Shift,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint plan kind")),
        };
        off += 1;
        // 指纹与映射属于"创建时事实"：解析只负责取值，比对在 read_checkpoint——那里
        // 才有当前目标的指纹与 loop 映射可对
        let fp = crate::dev::TargetFingerprint {
            dev: u64::from_le_bytes(rd(off, 8)?.try_into().unwrap()),
            ino: u64::from_le_bytes(rd(off + 8, 8)?.try_into().unwrap()),
            size: u64::from_le_bytes(rd(off + 16, 8)?.try_into().unwrap()),
        };
        off += 24;
        let map = match rd(off, 1)?[0] {
            0 => None,
            1 => Some((
                u64::from_le_bytes(rd(off + 1, 8)?.try_into().unwrap()),
                u64::from_le_bytes(rd(off + 9, 8)?.try_into().unwrap()),
            )),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint mapping flag")),
        };
        off += 1 + if map.is_some() { 16 } else { 0 };
        let stored = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
        if table::crc32(&b[..off]) != stored {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint CRC mismatch"));
        }
        // 进度字段同属盘上可控值：CRC 自洽但值荒谬的 ckpt 会让续传把该条目的全部 chunk
        // 跳过后仍提交表项——表将指向从未复制过的位置。上界在解析期可算：当前条目的
        // chunk 总数（自重叠搬移重做已完成 chunk 读到的仍是原始源数据，故越界只可能损坏）
        if chunk_bytes == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint chunk_bytes invalid"));
        }
        // cur_index 的合法区间是 0..=moves.len()（等于 len 表示全部条目已提交，此时
        // chunks_done 必为 0）：越界值若放行，恢复比对会把它夹回 len，续传循环空转、
        // 所有搬移被跳过，表却仍按搬移后的几何提交
        if cur_index as usize > moves.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint entry index"));
        }
        let chunks_limit = match moves.get(cur_index as usize) {
            Some(m) => m.len_lba.checked_mul(ss)
                .and_then(|t| t.checked_add(chunk_bytes - 1))
                .map(|t| t / chunk_bytes)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "checkpoint chunk count overflow"))?,
            // 全部条目已提交：进度归零，残留值即损坏
            None => 0,
        };
        if chunks_done > chunks_limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint chunk progress"));
        }
        Ok(Checkpoint { disk_size, ss, grow_part, last_usable_lba, moves, kind, cur_index, chunks_done, chunk_bytes, fp, map })
    }
}

/// 当前目标与 checkpoint 记录的"创建时事实"比对：指纹或 loop 映射任一不符即外来现场。
/// 指纹只对"双方都有意义"的值比对——零指纹（真块设备节点 / 非 Unix 平台）不构成证据；
/// 映射的有无本身参与比对：经 loop 写入的现场不允许改从裸镜像续跑（反之亦然），
/// 两种途径下同一 LBA 指向的字节不同，映射差异不是可忽略的记法差
fn scene_mismatch(
    src: &FileSource,
    fp: &crate::dev::TargetFingerprint,
    map: &Option<(u64, u64)>,
) -> Option<String> {
    if fp.meaningful() && src.fingerprint.meaningful() && *fp != src.fingerprint {
        return Some(format!(
            "file fingerprint changed since the checkpoint was written (was dev:{} ino:{} size:{}, now dev:{} ino:{} size:{})",
            fp.dev, fp.ino, fp.size, src.fingerprint.dev, src.fingerprint.ino, src.fingerprint.size,
        ));
    }
    match (map, &src.loop_mapping) {
        (Some((ro, rl)), Some((co, cl))) if (*ro, *rl) != (*co, *cl) => {
            Some(format!("loop mapping changed since the checkpoint was written (was offset={ro} sizelimit={rl}, now offset={co} sizelimit={cl})"))
        }
        (Some(_), None) => {
            Some("checkpoint was written through a loop device, this target is not one".to_string())
        }
        (None, Some(_)) => {
            Some("checkpoint was written on a non-loop target, this one is reached through a loop device".to_string())
        }
        _ => None,
    }
}

/// 读 checkpoint 现场：在身份给出的候选落点上（首项为本次命名，其后是历史命名）找**可读**
/// 的那份，不做目录扫描。多份候选同时可读即报歧义——猜错会把中断的搬移现场丢掉。
/// 候选的历史命名带 GPT Disk GUID，故调用方须把**已解析的几何**传进来（几何是唯一来源，
/// 本函数不再自己读一次表）。记录的合法边界同样来自该几何（见 [`GeometryLimits`]）
///
/// 候选逐个独立定性（与 `active_recovery_records` / `pick_journal` 同一口径）：
/// 不存在 → 跳过；损坏 → **记录后继续**看后面的候选——提前 return 会让
/// 一份损坏的历史命名候选遮住一份完好的本次命名候选，用户看到的是"目标坏了去 abandon"，
/// 而真实情况是现场完好可续跑。只有"所有候选都不可读"才把损伤如实报成 Infra
fn read_checkpoint(src: &FileSource, g: &ValidatedGeometry) -> Result<CheckpointSlot, Fail> {
    let legacy = Some(g.header.disk_guid);
    let lim = g.limits();
    let mut found: Vec<(PathBuf, CheckpointSlot)> = Vec::new();
    let mut damaged: Vec<(PathBuf, String)> = Vec::new();
    // 现场可读但"创建时事实"对不上：不是损坏（CRC 自洽），也不可取信（继续执行会把
    // 旧现场写进换了文件/换了映射的目标）。单独归档，唯一时给出点名拒绝
    let mut foreign: Vec<(PathBuf, String)> = Vec::new();
    for path in src.identity.checkpoint_candidates(legacy) {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            // 两种"读不出来"性质不同，处置因此不同：**文件的可得性**故障（EIO / EACCES）
            // 说明连这份候选都取不到，其余候选大概率同样取不到 ⇒ 整体失败；**这一份的内容**
            // 故障（CRC / 字段非法）只坏这一份，别的候选可能完好 ⇒ 记下后继续
            // （全程在读盘阶段，尚未写盘，故按 Infra 而不是"盘可能已改变"）
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Fail::infra(format!("checkpoint read failed: {}: {e}", path.display()))),
        };
        match (Checkpoint::deserialize(&bytes, &lim), RsCheckpoint::deserialize(&bytes, &lim)) {
            (Ok(c), _) => {
                if let Some(why) = scene_mismatch(src, &c.fp, &c.map) {
                    foreign.push((path, why));
                } else {
                    found.push((path, CheckpointSlot::Relocation(Box::new(c))));
                }
            }
            (_, Ok(c)) => {
                if let Some(why) = scene_mismatch(src, &c.fp, &c.map) {
                    foreign.push((path, why));
                } else {
                    found.push((path, CheckpointSlot::Resize(Box::new(c))));
                }
            }
            (Err(re), Err(_)) => damaged.push((path, re.to_string())),
        }
    }
    match found.len() {
        0 if foreign.is_empty() && damaged.is_empty() => Ok(CheckpointSlot::Empty),
        // 候选全在而全部不可读：现场存在但已无法判读——如实报 Infra 并列明各份的死因，
        // 出路是 abandon，而不是让续跑路径去猜
        0 if foreign.is_empty() => Err(Fail::infra(format!(
            "checkpoint files exist but none is readable: {}; release them with `diskedit abandon`",
            damaged.iter().map(|(p, e)| format!("{} ({e})", p.display())).collect::<Vec<_>>().join("; ")
        ))),
        // 全部"创建时事实"对不上：现场在，但不属于这个目标场景——点名各份的死因，
        // 拒绝必须发生在任何写盘之前
        0 => {
            for (p, e) in &damaged {
                eprintln!("warning: unreadable checkpoint {}: {e}", p.display());
            }
            Err(Fail::refused(format!(
                "checkpoint does not match this target scene — refusing: {}; release it with `diskedit abandon` if that scene is no longer wanted",
                foreign.iter().map(|(p, e)| format!("{} ({e})", p.display())).collect::<Vec<_>>().join("; ")
            )))
        }
        // 唯一可读者即取信；损坏/外来的那份要报出来——它本可能构成歧义判定，
        // 用户需要知道盘上不止一份落点文件
        1 => {
            for (p, e) in damaged.iter().chain(foreign.iter()) {
                eprintln!("warning: unusable checkpoint {}: {e}", p.display());
            }
            Ok(found.swap_remove(0).1)
        }
        _ => Ok(CheckpointSlot::Ambiguous(found.into_iter().map(|(p, _)| p).collect())),
    }
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
    // 相对路径（如 "a.ckpt"）的 parent() 是空而非 None：空父目录拼出的临时路径会带根
    // 组件，落进当前盘的根目录
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
    // 落点目录归文件自己保证：路径由身份派生，身份不知道目录是否存在
    crate::dev::best_effort_mkdir(parent);
    let tmp = parent.join(format!(
        ".diskedit.ckpt.tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
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
    /// copy 的数据复制全部完成、表项提交前 abort（与 `rs-before-commit` 同形：
    /// 不可逆的一步已经迈出，而那份"发生过什么"还没写进表）
    fault_copy_before_commit() = "copy-before-commit";
    /// 事务入口：本次调用的**任何判定与写入之前**。
    /// 与下面的 `chunk`/`rs-chunk` 成对使用，"边界前 abort 必须留下完全未改动的盘"、
    /// "边界后 abort 已在盘上留下可续传的现场"这两句话才有可自动断言的落点
    fault_before_any_write() = "before-any-write";
}

// 本模块内的调用不需要它；额外暴露给在线路径（online 的 sfdisk 分界）布点用。
// 在线路径仅 Linux，故非 Linux 构建下这个再导出没有使用者
#[cfg(target_os = "linux")]
pub(crate) use fault_points;

/// 扩容终点：`moves` 是按"末→首"的尾部紧凑打包序，各分区新起点中最小者即本次腾出空间的
/// 左边界，故 grow_end = min(new_first) − 1；无 movable 时扩到 last_usable_lba。
/// 加法一律 checked：越界/损坏表下报错而非回绕（回绕会把荒谬的 LBA 写进表再交给 resize）。
/// `target_start`：moves 可整份来自 ckpt，其 first+delta 的下界只到 LBA 1——终点低于
/// 目标分区自身起点就是端点倒挂的表项。上、下两个边界在此收口成唯一判据
pub fn grow_end_for(plan: &Plan, target_start: u64) -> io::Result<u64> {
    let mut leftmost: Option<u64> = None;
    for m in &plan.moves {
        let new_first = m.first_lba.checked_add(m.delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        leftmost = Some(leftmost.map_or(new_first, |f| f.min(new_first)));
    }
    let grow_end = match leftmost {
        // moves 可整份来自 ckpt，减法同样 checked：不依赖"正常输入不会产生这种值"
        Some(f) => f.checked_sub(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target below LBA 1 — refusing")
        })?,
        None => plan.last_usable_lba,
    };
    if grow_end > plan.last_usable_lba {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "relocation target beyond last usable LBA — refusing"));
    }
    if grow_end < target_start {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "grow target below its own start — refusing"));
    }
    Ok(grow_end)
}

/// 分区已扩、swap 未重建：创建于 32K 页的 swap 在本机（4K 页）激活不了，identify 按
/// swapon 口径认不出它，但元数据确实是 swap，头里的页数仍是扩前的值——这是一条真实的
/// 未完成后置条件，报成功就是静默成功。判据、措辞、补救命令同处一处；探测区间由调用点
/// 决定（扩前 / 扩后 / 提交后各不相同，那是调用点的知识）
pub(crate) fn swap_rebuild_pending(
    src: &FileSource,
    part: u32,
    base: u64,
    len_bytes: u64,
) -> io::Result<Option<Pending>> {
    // 读失败≠"确认存在待办"：Pending 文案断言"本机激活不了"只在探测成功且命中时成立，
    // 设备 I/O 故障属于另一码事，按故障上抛（调用点都在写盘之后，Failed 的
    // "盘可能已改变"警示才是对状态的如实表述）
    if !crate::fsid::unactivatable_swap(src, base, len_bytes)? {
        return Ok(None);
    }
    Ok(Some(Pending::new(
        part,
        PendingKind::Swap,
        "partition grown but the swap area was not rebuilt (its page format is not activatable on this host)",
        crate::fsops::rescue_hint("swap", &crate::dev::part_dev_hint(src, part, base)),
    )))
}

/// 一次搬移事务 = 事前判定（只读）+ 执行（写盘）。
/// 两半的返回类型不同，这就是 durable boundary 的类型表达：`prepare_apply` 只可能给出
/// `Refused` / `Infra`（此刻确定未写盘），`execute_apply` 的返回类型里没有 `Fail`，
/// 因此"写盘之后仍返回 Refused"在那半边**写不出来**。
/// 几何由调用方解析（一条命令解析一次，往下传），本函数与执行半程都不读表
pub fn apply(
    src: &mut FileSource,
    g: &ValidatedGeometry,
    plan: &Plan,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
) -> Outcome {
    // 事务入口的注入点：此刻 abort 必须与"从未运行过"不可区分
    fault_before_any_write();
    // 后置条件中未完成的部分：布局写完后才可能产生，交调用方统一换算退出码
    let mut pending: Vec<Pending> = Vec::new();
    let r = (|| -> Result<(), Fail> {
        let d = prepare_apply(src, g, plan, chunk_len, no_fs, log)?;
        // 修复虽是写盘，但先于任何目标写入执行；放在 Fail 域是为了保住分类：
        // 其读表阶段尚未写一个字节（Infra），一旦进入 io 域就只剩 Failed 一条路
        if let Some(what) = plan.repair.describe() {
            gpt_policy::apply_repair(src, &plan.repair)?;
            log(&format!("[repair] {what}"));
        }
        fault_after_repair();
        // 越界之后的 io 失败一律归 Failed（"盘可能已改变"）
        execute_apply(src, plan, d, log, &mut pending).map_err(Fail::from)
    })();
    crate::outcome::finish(r, pending)
}

/// 事前判定的产物，也是 execute 的**唯一输入源**：凡是执行阶段要用的都在这里带过去。
/// `chunk_len` / `no_fs` 若同时留在 execute 的参数位上，同一个值就有两个来源、谁赢没有定义。
/// `g` 随行：执行半程对表项的全部落位都基于它（不重读盘上的表）
struct ApplyDecision {
    g: ValidatedGeometry,
    ckpt: Checkpoint,
    ckpt_path: PathBuf,
    chunk_len: u64,
    no_fs: bool,
}

/// 事前判定（只读）：FS preflight → 槽位判定。
/// 返回 `Err` 一律发生在任何写盘之前；本函数签名上就没有 `&mut FileSource`
fn prepare_apply(
    src: &FileSource,
    g0: &ValidatedGeometry,
    plan: &Plan,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
) -> Result<ApplyDecision, Fail> {
    // 占用闸（与 prepare_resize 同一判据）：被搬移分区的数据区会被整体重写，grow 目标
    // 随后要接 FS 步——挂载中或活动 swap 的分区必须在任何写盘前拒绝。镜像无块设备
    // 占用语义，闸内自会放行
    for m in &plan.moves {
        crate::fsops::ensure_idle_before_write(src, m.part_num, m.first_lba.checked_mul(plan.ss)
            .ok_or_else(|| Fail::infra("move source start overflows byte offset"))?)?;
    }
    if let Some(ge) = g0.entry_index(plan.grow_part).and_then(|i| g0.entries.get(i))
        && ge.ending_lba != 0
    {
        crate::fsops::ensure_idle_before_write(src, plan.grow_part, ge.starting_lba.checked_mul(plan.ss)
            .ok_or_else(|| Fail::infra("grow target start overflows byte offset"))?)?;
    }
    // 几何由调用方解析并下传（构造点即拒绝条目重叠），FS preflight 与 ckpt 判定共用它；
    // 几何里的上界是"修复生效后"的值，ckpt 的边界校验依赖它。盘在 plan 生成之后变了的话，
    // 由下面的恢复校验拒绝
    // 写盘前的 preflight：FS 扩展属本次操作的后置条件，工具缺失必须现在拒绝——
    // 一旦开始写盘才发现，就会留下"分区已改、FS 未扩"的中间态。
    // 不依赖命令层是否检查过：续传路径不经过 plan，本处才是唯一必经关口
    if !no_fs
        && let Some(ge) = g0.entry_index(plan.grow_part).and_then(|i| g0.entries.get(i))
        && ge.ending_lba != 0
    {
        // LBA 的单位是**表自身**的 ss（plan.ss）。此处位于 apply_repair / 搬移之前，
        // 本次调用尚未写目标盘：identify 的环境故障按 Infra 报，check_grow 的
        // "不支持 / 工具缺失"交 FsError 分流（10 / 30），不在此另判
        let ft = crate::fsid::identify(
            src,
            ge.starting_lba * plan.ss,
            (ge.ending_lba - ge.starting_lba + 1) * plan.ss,
        )
        .map_err(Fail::infra_io)?;
        crate::fsops::check_grow(ft)?;
    }
    // 恢复三态：有效 → 续传 / 槽位被另一族作业占用 → 拒绝 / 空 → 新建
    let slot = read_checkpoint(src, g0)?;
    let ckpt_path = src.identity.checkpoint_path().to_path_buf();

    let ckpt = match slot {
        CheckpointSlot::Ambiguous(paths) => return Err(ambiguous_checkpoint(&paths)),
        // 与 make_plan_resuming 共用同一句拒绝理由：同一事实的措辞只写一次
        CheckpointSlot::Resize(_) => return Err(foreign_resize_refusal()),
        CheckpointSlot::Relocation(c) => {
            let c = *c;
            // plan 参数与磁盘现状必须一致（盘大小/扇区/计划/chunk 全等），否则拒绝
            let fresh = Checkpoint {
                disk_size: src.size,
                ss: plan.ss,
                grow_part: plan.grow_part,
                last_usable_lba: plan.last_usable_lba,
                moves: plan.moves.clone(),
                kind: plan.kind,
                cur_index: c.cur_index.min(plan.moves.len() as u32),
                chunks_done: c.chunks_done,
                chunk_bytes: chunk_len,
                fp: src.fingerprint,
                map: src.loop_mapping,
            };
            // chunk_bytes 单独先查：它是命令行可控项，混在"disk/plan 全等"里报会让
            // 用户去查盘，而真实原因是这次换了 --chunk-size
            if c.chunk_bytes != chunk_len {
                return Err(crate::outcome::Fail::refused(format!(
                    "the checkpoint was written with --chunk-size {} MiB, this run uses {} — re-run with the original value",
                    c.chunk_bytes / (1024 * 1024),
                    chunk_len / (1024 * 1024),
                )));
            }
            if c.disk_size != fresh.disk_size || c.ss != fresh.ss || c.grow_part != fresh.grow_part
                || c.last_usable_lba != fresh.last_usable_lba || c.moves.len() != fresh.moves.len()
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
            kind: plan.kind,
            cur_index: 0,
            chunks_done: 0,
            chunk_bytes: chunk_len,
            fp: src.fingerprint,
            map: src.loop_mapping,
        },
    };
    Ok(ApplyDecision { g: g0.clone(), ckpt, ckpt_path, chunk_len, no_fs })
}

/// 数据 chunk 的 I/O 失败必须带着精确 LBA 区间上抛：errno 说不出"哪一段"，
/// 用户需要它区分瞬态错误（重跑续传）与永久坏块（先做救援镜像再对镜像操作）。
/// 本层只报告区间与出路，不判断块对文件系统是否已分配、可否弃——那是 fsck 的事
fn chunk_io_error(
    e: io::Error,
    action: &str,
    base_lba: u64,
    len_lba: u64,
    chunk: usize,
    total: usize,
    hint: &str,
) -> io::Error {
    // 诊断区间同样不回绕：chunk 搬移的算术全部 checked，报错的 LBA 没有理由例外
    let end = base_lba.saturating_add(len_lba);
    io::Error::other(format!(
        "{action} failed at LBA {base_lba}..{end} (chunk {chunk}/{total}): {e} — {hint}"
    ))
}

const RESUME_HINT: &str = "the checkpoint is kept, re-run the command to retry this block (transient errors resume from here); for a permanently unreadable block, image the source with ddrescue and run the operation on the image";
const COPY_HINT: &str = "copy has no resume — release the scene with `diskedit undo` (if rollbackable) or `diskedit abandon`, then re-run; a permanently unreadable block must be imaged with ddrescue first";

// ---- durable boundary：以上判定全部结束，以下开始写盘 ----

/// 执行（写盘）。入参 `&mut FileSource` + 返回 `io::Result` 共同构成边界：
/// 函数体内**构造不出** `Fail::Refused`（返回类型里放不下它），
/// 于是"写盘之后还能返回 Refused"在这里连编译都过不去
fn execute_apply(
    src: &mut FileSource,
    plan: &Plan,
    d: ApplyDecision,
    log: &mut dyn FnMut(&str),
    pending: &mut Vec<Pending>,
) -> io::Result<()> {
    // 决策按值收下：ckpt 就地推进；表项落位全部基于随行几何（不重读盘上的表——
    // 搬移循环内每条重解析一次正是"解析次数随 N 增长"的根源）。条目绝对赋值 +
    // 同一份几何连续提交，与逐条重读盘上表再提交逐字节等价（commit 幂等）
    let mut ckpt = d.ckpt;
    let (chunk_len, no_fs) = (d.chunk_len, d.no_fs);
    let ckpt_path = &d.ckpt_path;
    let mut g = d.g;
    // 修复已由 apply 的 Fail 域在调用本函数之前完成（备份头搬移 / 保护 MBR 重写），
    // 使后续所有写入都基于修复后的几何
    atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
    // 本操作含**不可回滚的写入**：搬移的数据块按设计不入 journal（前向恢复、无回滚），
    // 故留一条标记使 undo 拒绝回滚——否则表被回滚而数据未回滚，布局与盘上内容自相矛盾。
    // delta_lba 在 plan 生成处只产生非负值：目标起点 = first_lba + delta，直接 checked 相加
    let effective = plan.moves.iter().find(|m| m.delta_lba != 0);
    if let Some(m) = effective {
        let to_lba = m.first_lba.checked_add(m.delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        src.set_mutation(crate::dev::Mutation::RelocatePartition {
            partition: m.part_num,
            from_lba: m.first_lba,
            to_lba,
        });
    }
    // 位移全为 0（右邻已尾打包时重跑 resize grow --allow-move 的典型产物）⇒ 没有
    // 不可回滚的写入，不落屏障：undo 的可回滚性不受这次空转影响
    if effective.is_some() {
        src.mark_non_reversible()?;
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
        // δ=0：条目已在目标位（右邻尾打包后重跑的产物）。自拷贝、重写表、推进 ckpt
        // 之外的一切都是空转，跳过——表项绝对赋值下它本来就"已提交"
        if m.delta_lba == 0 {
            ckpt.chunks_done = 0;
            ckpt.cur_index = mi as u32 + 1;
            atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
            continue;
        }
        // 目标区端点一次算好、全链 checked：要原样写进表项并进入字节地址换算
        let new_first = m.first_lba.checked_add(m.delta_lba).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        let new_end = m.len_lba.checked_sub(1).and_then(|l| new_first.checked_add(l)).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "relocation target LBA overflows — refusing")
        })?;
        // durable 起点：只有"中断时正在搬的那一条"带 chunk 级进度，后续条目从 0 开始
        let entry_resume = if mi as u32 == resume_index { resume_chunks } else { 0 };
        ckpt.chunks_done = entry_resume;
        // swap：内容可弃 → 不搬数据，表项落位后 mkswap 重建（UUID/卷标保持）
        if m.is_swap {
            // PARTUUID/分区号由条目原样保留（unique guid 不动），只改位置。
            // 绝对赋值而非 += delta：commit 后、ckpt 更新前崩溃的重放幂等
            {
                let e = &mut g.entries[(m.part_num - 1) as usize];
                e.starting_lba = new_first;
                e.ending_lba = new_end;
            }
            g.commit(src)?;
            fault_after_swap_commit(mi);
            // 旧 swap 签名区在源位置（数据区未搬移），从那里读 UUID/卷标
            let ident = read_swap_identity(src, m.first_lba, m.len_lba, plan.ss);
            // mkswap 是外部工具的写盘，先落屏障（对齐 execute_resize 的 swap 分支）
            src.set_mutation(crate::dev::Mutation::ExternalFsTool);
            src.mark_non_reversible()?;
            match crate::fsops::recreate_swap(src, m.part_num, ident) {
                Ok(()) => log(&format!("swap {} recreated at new location (UUID preserved)", m.part_num)),
                // 表项已落位、内容可弃：swap 未重建属"后续步骤未完成"，不是本次操作失败
                Err(e) => pending.push(Pending::new(
                    m.part_num,
                    PendingKind::Swap,
                    e.to_string(),
                    crate::fsops::rescue_hint("swap", &crate::dev::part_dev_hint(src, m.part_num, m.first_lba * plan.ss)),
                )),
            }
            ckpt.chunks_done = 0;
            ckpt.cur_index = mi as u32 + 1;
            atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
            fault_after_swap_entry(mi);
            continue;
        }
        let src_off = m.first_lba * plan.ss;
        let dst_off = new_first * plan.ss;
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
            src.read_at(src_off + within, &mut buf).map_err(|e| {
                chunk_io_error(e, "reading source", (src_off + within) / plan.ss, len / plan.ss, i + 1, total_chunks as usize, RESUME_HINT)
            })?;
            src.write_data_at(dst_off + within, &buf).map_err(|e| {
                chunk_io_error(e, "writing target", (dst_off + within) / plan.ss, len / plan.ss, i + 1, total_chunks as usize, RESUME_HINT)
            })?;
            src.sync_data()?; // 数据 chunk 用 sync_data；表结构提交用 sync_all
            ckpt.chunks_done = i as u64 + 1; // 内存进度：递增发生在 sync_data 之后，永不超前于 durable 数据
            pending_chunks += 1;
            // 批量提交：攒满一批或到达末 chunk 才落盘。落后只导致重复执行
            // （重做读到的仍是原始源数据），超前才会跳过未落盘区间——单向不等式不可破坏
            if pending_chunks >= CKPT_BATCH_CHUNKS || i as u64 + 1 == total_chunks {
                atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
                pending_chunks = 0;
            }
            fault_chunk(i as u64 + 1);
        }
        fault_before_entry_commit();
        // 本分区完成 → 按崩溃安全四结构序列提交表项（整表重写）。
        // 绝对赋值（非 += delta）：commit 后 ckpt 更新前崩溃的重放幂等
        {
            let e = &mut g.entries[(m.part_num - 1) as usize];
            e.starting_lba = new_first;
            e.ending_lba = new_end;
        }
        g.commit(src)?;
        // 起始位置变化的 NTFS 分区需修 HiddenSectors（数据是字节拷贝，boot sector 带着旧值）
        if crate::fsid::identify(src, new_first * plan.ss, m.len_lba * plan.ss)? == "ntfs" {
            fix_ntfs_hidden_sectors(src, new_first, plan.ss, log)?;
        }
        log(&format!("partition {} relocated (delta {} sectors)", m.part_num, m.delta_lba));
        ckpt.chunks_done = 0;
        ckpt.cur_index = mi as u32 + 1;
        atomic_write_ckpt(ckpt_path, &ckpt.serialize())?;
        fault_after_entry_commit(mi);
    }
    fault_before_grow();

    // 扩容收尾：目标分区扩到 movable 新区域之前（防止与刚打包的分区重叠）；
    // 无 movable 时到 last_usable。commit → resize FS（同一份随行几何，不重读盘上的表）
    let (grow_start, grow_len, old_len);
    {
        let te = &mut g.entries[(plan.grow_part - 1) as usize];
        let grow_end = grow_end_for(plan, te.starting_lba)?;
        old_len = te.ending_lba.checked_sub(te.starting_lba)
            .map(|v| v + 1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "grow target entry inverted — refusing"))?;
        te.ending_lba = grow_end;
        grow_start = te.starting_lba;
        grow_len = grow_end - grow_start + 1;
    }
    g.commit(src)?;
    table::ensure_protective_mbr(src)?;
    log(&format!("partition {} extended (FS area ends before relocated partitions)", plan.grow_part));

    crate::dev::warn_if_remove_failed(ckpt_path);

    // FS resize 是契约的一部分：失败即后置条件未满足（由调用方换算为 PARTIAL）。
    // 这里不能只打日志当成功——脚本会据此认为空间已可用
    let fstype = crate::fsid::identify(src, grow_start * g.ss, grow_len * g.ss)?;
    if no_fs {
        log("partition extended (--no-fs: filesystem left untouched)");
    } else {
        finalize_growth(src, plan.grow_part, fstype, GrowthRegions {
            old: (grow_start, old_len),
            new: (grow_start, grow_len),
        }, g.ss, log, pending)?;
    }
    Ok(())
}

/// 扩容收尾的区间对（LBA，表自身 ss）：swap 重建读旧头的区间与探测新空间生效的区间
/// 是两个不同的事实，调用点各给一次
struct GrowthRegions {
    old: (u64, u64), // 搬移前 (start, len)：swap 旧头仍在此处（两族都拒绝搬 swap，start 不变）
    new: (u64, u64), // 提交后 (start, len)：探测按它进行
}

/// 扩容的统一 FS 收尾（apply 与 resize 的唯一实现）：表项已按新几何提交后，
/// 按 FS 家族给出唯一处置。未完成的后置条件进 `pending`，退出码由调用方统一换算；
/// 其余 `Err` 归 io 域（此刻已过 durable boundary，不得再伪装成拒绝）。
/// 各家族的可做性已在事前由 check_grow 给出，此处不判第二遍
fn finalize_growth(
    src: &mut FileSource,
    part: u32,
    fstype: &str,
    r: GrowthRegions,
    ss: u64,
    log: &mut dyn FnMut(&str),
    pending: &mut Vec<Pending>,
) -> io::Result<()> {
    match fstype {
        // unknown/LVM PV 无本工具可扩的文件系统；其中混着一类**真实的未完成后置条件**
        //（创建于 32K 页的 swap，本机激活不了），故先探测一遍。
        // 两侧都不在此落 ExternalFsTool 屏障：PV 的外部写入（pvresize/lvextend）在收尾
        // resize_done 里发生，屏障由它携带 journal 写句柄在 spawn 前落——"表被回滚、
        // PV 已扩"的自相矛盾同样被排除，且 undo 不再被"屏障已落、pvresize 未跑"的
        // 死亡窗口无谓锁死。unknown 这支只探测、不写盘
        "unknown" | "lvm2_pv" => {
            match swap_rebuild_pending(src, part, r.new.0 * ss, r.new.1 * ss)? {
                Some(missed) => pending.push(missed),
                None => log("partition extended (no resizable filesystem inside)"),
            }
        }
        // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持）。
        // grow_support 对 swap 承诺的就是 mkswap——漏掉它即违背承诺。外部工具写盘，先落屏障
        "swap" => {
            src.set_mutation(crate::dev::Mutation::ExternalFsTool);
            src.mark_non_reversible()?;
            let ident = read_swap_identity(src, r.old.0, r.old.1, ss);
            match crate::fsops::recreate_swap(src, part, ident) {
                Ok(()) => log("swap recreated (UUID preserved)"),
                Err(e) => pending.push(Pending::new(
                    part,
                    PendingKind::Swap,
                    e.to_string(),
                    crate::fsops::rescue_hint("swap", &crate::dev::part_dev_hint(src, part, r.old.0 * ss)),
                )),
            }
        }
        // 外部 FS 工具（resize2fs/xfs_growfs/…）的写入不可回滚：扩完后 FS 自述尺寸大于
        // 旧分区尺寸，此后回滚表项即"表与内容自相矛盾"。必须先于该写入落屏障
        _ => {
            src.set_mutation(crate::dev::Mutation::ExternalFsTool);
            src.mark_non_reversible()?;
            match crate::fsops::resize_fs(src, part, fstype) {
                Ok(()) => log("filesystem resized"),
                Err(e) => pending.push(Pending::new(
                    part,
                    PendingKind::Fs,
                    e.to_string(),
                    crate::fsops::rescue_hint(fstype, &crate::dev::part_dev_hint(src, part, r.old.0 * ss)),
                )),
            }
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
///
/// 卷标以**字节**返回：sws_volume 是 16 字节定长字段，mkswap 存入时不校验编码，
/// 任何 String 化（Latin-1 或替换非法序列）都会在写回时改变原值
pub(crate) fn read_swap_identity(src: &FileSource, first_lba: u64, len_lba: u64, ss: u64) -> (Option<[u8; 16]>, Option<Vec<u8>>) {
    let base = first_lba * ss;
    // 签名位置由唯一探测器判定；候选集取 libblkid 口径（含 32K）——本函数只读元数据，
    // 宽于"swapon 可激活"的范围是有意的：读得到就能保住 UUID/PARTUUID。
    // 探测读失败与未命中同出口：收尾时的尽力读取，保不住就按随机 UUID 走
    if !matches!(
        crate::fsid::probe_swap_header(src, base, len_lba * ss, &crate::fsid::blkid_known_pages()),
        Ok(Some(_))
    ) {
        return (None, None); // 未格式化、非 swap、或读不出来 → 随机 UUID
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
    let vol_bytes: Vec<u8> = vol.iter().take_while(|&&b| b != 0).copied().collect();
    let uuid_opt = if uuid == [0u8; 16] { None } else { Some(uuid) };
    let vol_opt = if vol_bytes.is_empty() { None } else { Some(vol_bytes) };
    (uuid_opt, vol_opt)
}

// ---------- 通用 resize-part：grow / shrink / move 三合一 ----------

const CKPT2_MAGIC: &[u8; 8] = b"DKECKPT2";
/// 字段变动即升版本：旧 ckpt 解析失败 → 当作"无 ckpt"重跑（搬移与 commit 都幂等，重跑安全）。
/// 不靠长度巧合兜底——CRC 覆盖长度随字段变化，旧布局必然对不上。
/// v4：追加目标指纹（3×u64）与 loop 映射（1+2×u64），语义同 CKPT1 v5
const CKPT2_VERSION: u32 = 4;

/// resize-part 的 checkpoint（v4）：搬移覆盖源区，中途断电后新旧两处都不完整，
/// 已完成位置只能由 checkpoint 判定。"表项是否已提交"不在这里存——盘上的分区几何
/// 就是那个事实，另存一份标记只会与它分叉（见 prepare_resize 的恢复分支）
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
    fp: crate::dev::TargetFingerprint, // 目标文件指纹（"同路径换文件"防线）
    map: Option<(u64, u64)>,           // 创建时的 loop 映射；None = 非 loop 进入
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
        b.extend_from_slice(&self.fp.dev.to_le_bytes());
        b.extend_from_slice(&self.fp.ino.to_le_bytes());
        b.extend_from_slice(&self.fp.size.to_le_bytes());
        b.push(self.map.is_some() as u8);
        if let Some((off, limit)) = self.map {
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&limit.to_le_bytes());
        }
        let crc = table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }
    fn deserialize(b: &[u8], lim: &GeometryLimits) -> io::Result<Self> {
        // 最短完整布局 = 8(magic)+4(ver)+6×u64+4(part)+1(fs_shrunk)+8(chunks_done)
        // +8(chunk_bytes)+24(fp)+1(map flag)+4(crc) = 110；CRC 4 字节必须计入：
        // 截断文件走 InvalidData 而非在尾部切片时 panic
        if b.len() < 8 + 4 + 8 * 6 + 4 + 1 + 8 + 8 + 24 + 1 + 4 || &b[0..8] != CKPT2_MAGIC {
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
        // 同 Checkpoint：ss 进入写地址换算，须与目标几何一致
        if ss != lim.sector_size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint sector size differs from target geometry"));
        }
        let part = u32::from_le_bytes(b.get(o..o + 4).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?.try_into().unwrap()); o += 4;
        let old_start = rd64(b, &mut o)?;
        let old_end = rd64(b, &mut o)?;
        let new_start = rd64(b, &mut o)?;
        let new_end = rd64(b, &mut o)?;
        let fs_shrunk = *b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))? != 0; o += 1;
        let chunks_done = rd64(b, &mut o)?;
        let chunk_bytes = rd64(b, &mut o)?;
        // 指纹与映射属于"创建时事实"：解析只负责取值，比对在 read_checkpoint
        let fp = crate::dev::TargetFingerprint {
            dev: rd64(b, &mut o)?,
            ino: rd64(b, &mut o)?,
            size: rd64(b, &mut o)?,
        };
        // 标志字节先行跨过，再按标志决定是否读它后面那两个 u64
        let flagged = *b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?;
        o += 1;
        let map = match flagged {
            0 => None,
            1 => Some((rd64(b, &mut o)?, rd64(b, &mut o)?)),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible resize checkpoint mapping flag")),
        };
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
        // 分区号与四个 LBA 的边界来自**已验证几何**：分区号上界是该表的条目数（不再是常量
        // 128），LBA 必须落在本容器的可用区内——越界值会在恢复时被当成合法目标写进表，
        // 或让长度算术在 debug 下溢出（old_end − old_start + 1）
        if !(1..=lim.entry_count).contains(&part) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint partition number invalid"));
        }
        if old_start < lim.first_usable_lba
            || old_end > lim.last_usable_lba
            || new_start < lim.first_usable_lba
            || new_end > lim.last_usable_lba
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint range outside usable area"));
        }
        // 进度上界同 Checkpoint：搬移区间的 chunk 总数（区间端点已验非倒挂，乘法仍 checked）
        let chunks_limit = (old_end - old_start + 1).checked_mul(ss)
            .and_then(|t| t.checked_add(chunk_bytes - 1))
            .map(|t| t / chunk_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint chunk count overflow"))?;
        if chunks_done > chunks_limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible resize checkpoint chunk progress"));
        }
        Ok(RsCheckpoint { disk_size, ss, part, old_start, old_end, new_start, new_end, fs_shrunk, chunks_done, chunk_bytes, fp, map })
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

/// 本次 resize 请求的目标形状——与 ckpt、盘上几何并列的第三个维度
struct Requested {
    part: u32,
    start: u64,
    end: u64,
    chunk_len: u64,
}

/// resize 的恢复判定结论。三个维度缺一不可：
///   requested = 本次请求的目标区间
///   ckpt      = 持久状态（搬移前/后区间、FS 是否已缩、chunk 进度）
///   on_disk   = 表里**当前**的分区区间
///
/// 判据不能只取其中两个。"盘上几何 == ckpt.new_"只说明表已提交，还要问本次请求是否就是那次
/// 作业的目标；少了后一问，崩溃后改了 --end 重跑会被判成"已提交，收尾即可"，
/// 于是表不动、FS 不动，却报成功——用户拿着一次什么都没做的成功去用那块空间
enum RestoreState<'a> {
    /// 槽位空：全新作业
    Fresh,
    /// 表未提交（盘上几何仍是搬移前），且本次请求与 ckpt 记的是同一次作业 → 从 chunks_done 续传
    Resume(&'a RsCheckpoint),
    /// 表已提交（盘上几何 == 请求 == ckpt 记的目标），只剩 FS 收尾
    Committed(&'a RsCheckpoint),
    /// 对不上：ckpt 属于另一次作业，或盘上几何既不是搬移前也不是搬移后
    Divergent(&'static str),
}

/// 恢复判定：三个维度一次比完，调用方 match 结果即不可能漏看任何一个
fn classify_restore<'a>(
    req: &Requested,
    ckpt: &'a RsCheckpoint,
    on_disk: (u64, u64),
    disk_size: u64,
    ss: u64,
) -> RestoreState<'a> {
    if ckpt.part != req.part || ckpt.disk_size != disk_size || ckpt.ss != ss {
        return RestoreState::Divergent("the checkpoint was written for a different target");
    }
    if (ckpt.new_start, ckpt.new_end) != (req.start, req.end) {
        return RestoreState::Divergent("this request is not the job recorded in the checkpoint");
    }
    if on_disk == (ckpt.new_start, ckpt.new_end) {
        return RestoreState::Committed(ckpt);
    }
    if on_disk == (ckpt.old_start, ckpt.old_end) && ckpt.chunk_bytes == req.chunk_len {
        return RestoreState::Resume(ckpt);
    }
    // 几何与旧端一致而仅 chunk_bytes 不同：resume_gate 不校验 chunk 大小，这里
    // 是唯一拦得住的地方——报"几何不匹配"会让人去查盘而不是查 --chunk-size
    if on_disk == (ckpt.old_start, ckpt.old_end) {
        return RestoreState::Divergent("on-disk geometry matches the checkpoint, but --chunk-size differs from the interrupted job");
    }
    RestoreState::Divergent("on-disk geometry matches neither end of the checkpoint")
}

// 执行期失败的类型化区分（写盘前拒绝 / 写盘后失败）定义在 outcome 模块

/// 通用分区重定位：new_start/new_end 任意（grow/shrink/move 组合）。
/// 顺序：缩容 FS（如有）→ 数据搬移（方向感知 + checkpoint 续传）→ 提交表项 → 扩容 FS（如有）。
/// 两半的返回类型就是 durable boundary 的类型表达（见 `prepare_resize` / `execute_resize`）。
/// 对外入口负责把内部失败分类换算为 Outcome（退出码只在这一层定）。
/// 几何与修复动作由调用方解析（一条命令解析一次，往下传），本函数不读表
#[allow(clippy::too_many_arguments)] // src/几何/修复动作/区间/chunk/开关/日志各司其职：合成参数对象只是给 lint 换一层间接
pub fn resize_part(
    src: &mut FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
) -> Outcome {
    // 事务入口的注入点（与 apply 同义）：此刻 abort 必须与"从未运行过"不可区分
    fault_before_any_write();
    let mut pending: Vec<Pending> = Vec::new();
    let r = (|| -> Result<(), Fail> {
        let p = prepare_resize(src, g, repair, part, new_start, new_end, chunk_len, no_fs, log)?;
        // 修复先于任何目标写入；放 Fail 域以保住分类（同 apply）
        if let Some(what) = p.repair.describe() {
            gpt_policy::apply_repair(src, &p.repair)?;
            log(&format!("[repair] {what}"));
        }
        fault_after_repair();
        // 越界之后的 io 失败一律归 Failed（"盘可能已改变"）
        execute_resize(src, p, log, &mut pending).map_err(Fail::from)
    })();
    crate::outcome::finish(r, pending)
}

/// 本次对 FS 的动作。prepare 算一次，execute 只 match——若两处各自从尺寸重新推导，
/// "要不要缩/扩"就有了两个来源（尺寸没变、缩容已完成、`--no-fs` 都落进 `Leave`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FsAction {
    /// 不碰 FS
    Leave,
    Shrink,
    Grow,
}

/// 事前判定的产物，也是 execute 的**唯一输入源**
struct ResizeDecision {
    ss: u64,
    part: u32,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    new_end: u64,
    fstype: &'static str,
    fs_shrunk: bool,
    resume_chunks: u64,
    committed_old: Option<(u64, u64)>,
    /// 本次要对 FS 做的动作（唯一决策键：preflight 与 execute 同键）
    fs_action: FsAction,
    repair: RepairAction,
    new_bytes: u64,
    chunk_len: u64,
    no_fs: bool,
    ckpt_path: PathBuf,
    /// 随行几何：执行半程的表项提交基于它（不重读盘上的表）
    g: ValidatedGeometry,
}

/// 事前判定（只读）：几何、范围、重叠、槽位、恢复三态、FS 能力与工具 preflight。
/// 返回 `Err` 一律发生在任何写盘之前；签名里没有 `&mut FileSource`，因此它**不可能**写盘
#[allow(clippy::too_many_arguments)]
fn prepare_resize(
    src: &FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    no_fs: bool,
    log: &mut dyn FnMut(&str),
) -> Result<ResizeDecision, Fail> {
    // ---- 事前判定：全部只读，且必须在首次写盘（apply_repair）之前结束 ----
    // 下面的每一条拒绝都承诺"本次未写盘"（退出码 10），故判定与写盘的先后不能颠倒
    // 几何由调用方解析并下传：ckpt 的合法边界来自它（分区号上界、可用区、容器末端）
    let g = g.clone();
    let slot = read_checkpoint(src, &g)?;
    let ss = g.ss;
    let e = gpt_policy::live_entry(&g, part)?;
    // 占用闸：搬移/改写一个被挂载或活动 swap 的分区会让现场失效。镜像无块设备占用
    // 语义，闸内自会放行（支持边界见 README）
    crate::fsops::ensure_idle_before_write(src, part, e.starting_lba.checked_mul(ss)
        .ok_or_else(|| Fail::infra("partition start overflows byte offset"))?)?;
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
    // 恢复：checkpoint 与当前参数一致才允许续传，否则拒绝
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
    // 恢复判定取三个维度：本次请求 / 持久状态 / 盘上几何（见 classify_restore）
    let requested = Requested { part, start: new_start, end: new_end, chunk_len };
    let state = match existing.as_ref() {
        None => RestoreState::Fresh,
        Some(c) => classify_restore(&requested, c, (old_start, old_end), src.size, ss),
    };
    let (fs_shrunk, resume_chunks, committed_old) = match state {
        RestoreState::Fresh => (false, 0, None),
        // Y = durable 恢复点（ckpt 文件里的值），不是本进程内存进度
        RestoreState::Resume(c) => {
            log(&format!("resuming at chunk {} (durable checkpoint)", c.chunks_done));
            (c.fs_shrunk, c.chunks_done, None)
        }
        RestoreState::Committed(c) => {
            log("resize already committed — finishing filesystem step");
            (c.fs_shrunk, c.chunks_done, Some((c.old_start, c.old_end)))
        }
        // 有 ckpt 而三个维度对不上：不猜是哪一次作业，也不当作"没有 ckpt"重做一遍
        RestoreState::Divergent(why) => {
            return Err(Fail::refused(format!("existing checkpoint does not match this resize — refusing: {why}")));
        }
    };

    // 阶段 0/3 的 FS 大小判据：已提交态下用 ckpt 记录的搬移前形状，否则本次请求的旧形状
    let (fb_start, fb_end) = committed_old.unwrap_or((old_start, old_end));
    let old_bytes = (fb_end - fb_start + 1) * ss;
    let new_bytes = (new_end - new_start + 1) * ss;
    // 表项 LBA 的单位是表自身的 ss。identify 只读目标内容，仍属事前判定
    let fstype = crate::fsid::identify(src, old_start * ss, (old_end - old_start + 1) * ss).map_err(Fail::infra_io)?;

    // preflight：本操作的后置条件含 FS 调整，工具缺失必须在阶段 0
    // （缩容时 FS shrink 即首次写盘）之前拒绝，否则会留下"表已改、FS 未改"的中间态。
    // --no-fs 与缩容不可共存：分区末端越过未缩的 FS 元数据即数据损坏，这不是"少做一步"
    // 而是另一种操作，必须在写盘前拒绝（已缩过 FS 的续传不在此列）
    let needs_shrink = new_bytes < old_bytes && !fs_shrunk;
    if no_fs && needs_shrink {
        return Err(Fail::refused(
            "--no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the new partition end would cut into filesystem metadata",
        ));
    }
    // FS 动作是**唯一决策键**：preflight 与 execute 都 match 它，不各自从尺寸重新推导
    let fs_action = if needs_shrink {
        FsAction::Shrink
    } else if new_bytes > old_bytes && !no_fs {
        FsAction::Grow
    } else {
        FsAction::Leave
    };
    match fs_action {
        FsAction::Shrink => crate::fsops::check_shrink(fstype)?,
        FsAction::Grow => crate::fsops::check_grow(fstype)?,
        FsAction::Leave => {}
    }
    // ext 预查 FS 最小尺寸（resize2fs -P × dumpe2fs -h 块大小），缩太小在写盘前拒绝。
    // 同样是只读探测
    if fs_action == FsAction::Shrink
        && let Some(min) = crate::fsops::fs_min_bytes(src, part, fstype)?
        && new_bytes < min
    {
        return Err(Fail::refused(format!(
            "target size {new_bytes} < minimum FS size {min} bytes (resize2fs -P)"
        )));
    }

    Ok(ResizeDecision {
        ss, part, old_start, old_end, new_start, new_end, fstype,
        fs_shrunk, resume_chunks, committed_old, fs_action, repair, new_bytes,
        chunk_len, no_fs,
        ckpt_path: src.identity.checkpoint_path().to_path_buf(),
        g,
    })
}

// ---- durable boundary：以上判定全部结束，以下开始写盘 ----

/// 执行（写盘）。入参 `&mut FileSource` + 返回 `io::Result` 共同构成边界：
/// 函数体内**构造不出** `Fail::Refused`（返回类型里放不下它），
/// 于是"写盘之后还能返回 Refused"在这里连编译都过不去
fn execute_resize(
    src: &mut FileSource,
    p: ResizeDecision,
    log: &mut dyn FnMut(&str),
    pending: &mut Vec<Pending>,
) -> io::Result<()> {
    let (ss, part) = (p.ss, p.part);
    let (old_start, old_end) = (p.old_start, p.old_end);
    let (new_start, new_end) = (p.new_start, p.new_end);
    let (chunk_len, no_fs) = (p.chunk_len, p.no_fs);
    let new_bytes = p.new_bytes;
    let fstype = p.fstype;
    let committed_old = p.committed_old;
    // 修复已由 resize 的 Fail 域在调用本函数之前完成，后续所有写入都基于修复后的几何
    let mut ckpt = RsCheckpoint {
        disk_size: src.size, ss, part,
        old_start, old_end, new_start, new_end,
        fs_shrunk: p.fs_shrunk, chunks_done: p.resume_chunks,
        chunk_bytes: chunk_len,
        fp: src.fingerprint,
        map: src.loop_mapping,
    };
    let save = |c: &RsCheckpoint| atomic_write_ckpt(&p.ckpt_path, &c.serialize());

    // ---- 阶段 0：缩容 FS 先缩（只做一次）----
    // 本块只在 !no_fs 时可达（no_fs + 缩容 + FS 未缩已在上面整体拒绝），因此
    // "能否缩 / 类型是否认得 / 工具是否齐备"三重判据已由上面的 check_shrink 一次性给出。
    // 此处不另设判据，以 check_shrink 的三重结果（能否缩 / 类型 / 工具）为准
    if p.fs_action == FsAction::Shrink {
        // 缩容**不**落不可回滚标记：缩小之后再回滚表项只会得到"FS 小于分区"这一自洽状态
        // （与扩容方向相反），undo 仍然安全可用。判据是可逆性，不是"有没有调外部工具"
        crate::fsops::shrink_fs(src, part, fstype, new_bytes)?;
        ckpt.fs_shrunk = true;
        save(&ckpt)?;
        log(&format!("fs shrunk to {new_bytes} bytes"));
        fault_rs_after_fs_shrink();
    }

    // ---- 阶段 1：数据搬移（方向感知 + chunk 续传）----
    // 方向与幅度分开算：abs_diff 不经 i64 cast，差值再大也不会翻转符号
    let forward = new_start > old_start;
    let moved = new_start.abs_diff(old_start);
    if moved != 0 && committed_old.is_none() {
        // 含数据搬移：字节不入 journal，落屏障令 undo 拒绝（前向恢复、无回滚）
        src.set_mutation(crate::dev::Mutation::RelocatePartition {
            partition: part,
            from_lba: old_start,
            to_lba: new_start,
        });
        src.mark_non_reversible()?;
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
            if forward { v.reverse(); }
            v
        };
        for (i, (within, len)) in order.iter().enumerate() {
            if (i as u64) < ckpt.chunks_done {
                continue;
            }
            let mut buf = vec![0u8; *len as usize];
            src.read_at(src_off + within, &mut buf).map_err(|e| {
                chunk_io_error(e, "reading source", (src_off + within) / ss, len / ss, i + 1, order.len(), RESUME_HINT)
            })?;
            src.write_data_at(dst_off + within, &buf).map_err(|e| {
                chunk_io_error(e, "writing target", (dst_off + within) / ss, len / ss, i + 1, order.len(), RESUME_HINT)
            })?;
            src.sync_data()?;
            ckpt.chunks_done = i as u64 + 1;
            save(&ckpt)?;
            fault_rs_chunk(i as u64 + 1);
        }
        log(&format!("data moved by {moved} sectors"));
    }
    // ---- 阶段 2：提交表项（幂等：重复执行结果相同；已提交态跳过）----
    if committed_old.is_none() {
        fault_rs_before_commit();
        // 表项提交基于 prepare 阶段随行而来的几何（同一持锁期间内它就是权威事实），
        // 绝对赋值保证中断重放幂等
        let mut g2 = p.g.clone();
        {
            let te = &mut g2.entries[(part - 1) as usize];
            te.starting_lba = new_start;
            te.ending_lba = new_end;
        }
        g2.commit(src)?;
        table::ensure_protective_mbr(src)?;
        // 提交后不写 ckpt：恢复分支以盘上几何判定"是否已提交"，不需要第二份标记。
        // 此处中断后的重放由表项的绝对赋值保证幂等 —— fault_rs_after_commit 正测这一点
        log(&format!("partition {part} committed at {new_start}..{new_end}"));
        fault_rs_after_commit();
    }

    // 起始 LBA 变了才需要修 NTFS HiddenSectors（扩缩不动 start 时跳过）
    if moved != 0 && fstype == "ntfs" {
        fix_ntfs_hidden_sectors(src, new_start, ss, log)?;
    }

    // ---- 阶段 3：扩容 FS ----
    crate::dev::warn_if_remove_failed(&p.ckpt_path);
    // 要不要扩由 prepare 的决策给出，此处不比尺寸；家族处置与 apply 共用同一实现
    if p.fs_action == FsAction::Grow {
        finalize_growth(src, part, fstype, GrowthRegions {
            old: (old_start, old_end - old_start + 1),
            new: (new_start, new_end - new_start + 1),
        }, ss, log, pending)?;
    } else if no_fs {
        log("partition resized (--no-fs: filesystem left untouched)");
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
/// 两半的返回类型就是 durable boundary 的类型表达（见 `prepare_copy` / `execute_copy`）。
/// 几何与修复动作由调用方解析并下传，本函数不读表
#[allow(clippy::too_many_arguments)] // 同 resize_part：参数即最小事实集
pub fn copy_part(
    src: &mut FileSource,
    g: &ValidatedGeometry,
    repair: RepairAction,
    part: u32,
    new_start: u64,
    name: &str,
    chunk_len: u64,
    log: &mut dyn FnMut(&str),
) -> Result<u32, Fail> {
    // 事务入口注入点与 apply / resize_part 同形：durable boundary 之前 abort 必须留下
    // 完全未改动的盘，这条不变量在 copy 上同样要可断言
    fault_before_any_write();
    let p = prepare_copy(g, repair, part, new_start)?;
    // 修复先于任何目标写入；放 Fail 域以保住分类（同 apply）
    if let Some(what) = p.repair.describe() {
        gpt_policy::apply_repair(src, &p.repair)?;
        log(&format!("[repair] {what}"));
    }
    // 越界之后的 io 失败一律归 Failed（复制阶段不改表，但数据区已动）
    execute_copy(src, &p, name, chunk_len, log).map_err(Fail::from)
}

/// 事前判定的产物：自有数据 + 从源条目取下的两个 GUID
struct PreparedCopy {
    ss: u64,
    part: u32,
    src_start: u64,
    new_start: u64,
    new_end: u64,
    len: u64,
    type_guid: [u8; 16],
    unique_guid: [u8; 16],
    repair: RepairAction,
}

/// 事前判定（只读）：源分区存在性、目标范围与重叠。返回 `Err` 一律在任何写盘之前。
/// 几何与修复动作由调用方解析并下传（本函数纯只读，不触盘）
fn prepare_copy(
    g: &ValidatedGeometry,
    repair: RepairAction,
    part: u32,
    new_start: u64,
) -> Result<PreparedCopy, Fail> {
    let ss = g.ss;
    let e = gpt_policy::live_entry(g, part)?;
    let len = e.ending_lba - e.starting_lba + 1;
    let src_start = e.starting_lba;
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
    Ok(PreparedCopy {
        ss, part, src_start, new_start, new_end, len,
        type_guid: e.partition_type_guid,
        unique_guid: e.unique_partition_guid,
        repair,
    })
}

/// 执行（写盘）。返回 `io::Result`：函数体内构造不出 `Fail::Refused`
fn execute_copy(
    src: &mut FileSource,
    p: &PreparedCopy,
    name: &str,
    chunk_len: u64,
    log: &mut dyn FnMut(&str),
) -> io::Result<u32> {
    let (ss, part, new_start, new_end, len) = (p.ss, p.part, p.new_start, p.new_end, p.len);
    let src_off = p.src_start * ss;
    let dst_off = new_start * ss;
    let total = len * ss;
    // 修复已由 copy 的 Fail 域在调用本函数之前完成，后续所有写入都基于修复后的几何
    // 含数据复制：字节不入 journal，留屏障令 undo 拒绝（前向恢复、无回滚）
    src.set_mutation(crate::dev::Mutation::CopyPartition {
        partition: p.part,
        from_lba: p.src_start,
        to_lba: new_start,
    });
    src.mark_non_reversible()?;
    let mut pos = 0u64;
    let total_n = total.div_ceil(chunk_len) as usize;
    while pos < total {
        let len_c = chunk_len.min(total - pos);
        let mut buf = vec![0u8; len_c as usize];
        src.read_at(src_off + pos, &mut buf).map_err(|e| {
            chunk_io_error(e, "reading source", (src_off + pos) / ss, len_c / ss, (pos / chunk_len + 1) as usize, total_n, COPY_HINT)
        })?;
        src.write_data_at(dst_off + pos, &buf).map_err(|e| {
            chunk_io_error(e, "writing target", (dst_off + pos) / ss, len_c / ss, (pos / chunk_len + 1) as usize, total_n, COPY_HINT)
        })?;
        src.sync_data()?;
        pos += len_c;
    }
    fault_copy_before_commit();
    // add_entry_at 自带"写盘前拒绝"语义，但它在本命令的边界之后：数据已经复制过去，
    // 此刻它报什么，对本次调用的结论都只能是"盘可能已改变"
    let num = gpt_policy::add_entry_at(
        src, new_start, new_end, name, p.type_guid, p.unique_guid,
    )
    .map_err(crate::outcome::into_io_error)?;
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

    /// 测试用的可操作几何：从夹具盘上解析后走生产同一构造点（含重叠拒绝）。
    /// checkpoint 的合法边界由它提供，故测试不再自带一套边界数字
    fn geom_of(src: &FileSource) -> ValidatedGeometry {
        let g = table::load_gpt(src).unwrap().expect("fixture must carry a GPT");
        ValidatedGeometry::new(&g, src.size / g.ss - 1, None).unwrap()
    }

    /// checkpoint 字段判据用的几何边界（合成值，形状与生产一致：分区号上界 = entry_count）
    fn lim(entry_count: u32) -> GeometryLimits {
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
    fn make_plan(src: &mut FileSource, grow_part: u32) -> Result<Plan, Fail> {
        let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
        plan_tail_packed(&g, repair, grow_part)
    }

    // ---- 测试便利入口：显式解析几何后走生产路径（生产调用方在锁下解析一次后传参） ----

    fn plan_resuming(src: &mut FileSource, grow_part: u32) -> Result<Plan, Fail> {
        let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
        make_plan_resuming(src, &g, repair, grow_part)
    }

    fn plan_shift(src: &mut FileSource, grow_part: u32, shift: Option<u64>) -> Result<Plan, Fail> {
        let (g, repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
        make_plan_shift_resuming(src, &g, repair, grow_part, shift)
    }

    fn pending_relocation(src: &FileSource, grow_part: u32) -> Result<bool, Fail> {
        let (g, _repair) = gpt_policy::resolve_geometry(src)?.ok_or_else(|| Fail::refused("no GPT on target"))?;
        has_pending_relocation(src, &g, grow_part)
    }

    fn rsize(src: &mut FileSource, part: u32, start: u64, end: u64, chunk: u64, no_fs: bool, log: &mut dyn FnMut(&str)) -> Outcome {
        let (g, repair) = gpt_policy::resolve_geometry(src).unwrap().unwrap();
        resize_part(src, &g, repair, part, start, end, chunk, no_fs, log)
    }

    fn ap(src: &mut FileSource, plan: &Plan, chunk: u64, no_fs: bool, log: &mut dyn FnMut(&str)) -> Outcome {
        let (g, _repair) = gpt_policy::resolve_geometry(src).unwrap().unwrap();
        apply(src, &g, plan, chunk, no_fs, log)
    }

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
            identity: crate::dev::TargetIdentity::resolve_image(&path),
            file: f,
            path: path.clone(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: Default::default(),
            loop_mapping: None,
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
            identity: crate::dev::TargetIdentity::resolve_image(&tmp),
            file: f,
            path: tmp.clone(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: Default::default(),
            loop_mapping: None,
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
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

        // 续传：跳过 chunk 0，补齐 chunk 1..3，提交表项
        let o = rsize(&mut src,1, 12288, 20479, chunk, false, &mut |_| {});
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
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

        let o = rsize(&mut src,1, new_start, new_end, chunk, true, &mut |_| {});
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

    /// 崩溃后改了 --end 重跑：ckpt 是那次作业的、盘上几何也停在它的终点，但**本次请求不是它**。
    /// 判据若只取"盘上几何 == ckpt.new_"，这一次会被判成"已提交，只剩 FS 收尾"：表不动、
    /// FS 不动，却报成功——用户拿着一次什么都没做的成功去用那块空间
    #[test]
    fn retargeted_resize_after_crash_is_refused() {
        let ss = 512u64;
        let chunk: u64 = 1024 * 1024;
        let (old_start, old_end) = (2048u64, 10239u64);
        let (new_start, new_end) = (12288u64, 20479u64);
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], old_start, old_end)]);

        // 现场：表已提交到 (new_start, new_end)，ckpt 记着这次作业（提交后、FS 收尾前中断）
        let mut g = table::load_gpt(&src).unwrap().unwrap();
        {
            let e = &mut g.entries[0];
            e.starting_lba = new_start;
            e.ending_lba = new_end;
        }
        let last_lba = src.size / ss - 1;
        table::commit_gpt(&mut src, &g, last_lba).unwrap();
        table::ensure_protective_mbr(&mut src).unwrap();
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        let ckpt = RsCheckpoint {
            disk_size: src.size, ss, part: 1,
            old_start, old_end, new_start, new_end,
            fs_shrunk: false, chunks_done: 4, chunk_bytes: chunk,
            fp: Default::default(),
            map: None,
        };

        // (a) 请求与 ckpt 记的目标不同 → 拒绝，且不写盘
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let o = rsize(&mut src,1, new_start, new_end + 2048, chunk, true, &mut |_| {});
        assert_eq!(o.exit_code(), crate::outcome::EXIT_REFUSED, "a retargeted run must be refused");
        assert_eq!(std::fs::read(&path).unwrap(), before, "a refused run must not write");
        assert!(ckpt_path.exists(), "the checkpoint must stay for the job it belongs to");

        // (b) 请求与 ckpt 一致，但盘上几何既不是搬移前也不是搬移后（第三方工具动过表）→ 同样拒绝
        let mut g = table::load_gpt(&src).unwrap().unwrap();
        {
            let e = &mut g.entries[0];
            e.starting_lba = 16384;
            e.ending_lba = 24575;
        }
        table::commit_gpt(&mut src, &g, last_lba).unwrap();
        table::ensure_protective_mbr(&mut src).unwrap();
        let before = std::fs::read(&path).unwrap();
        let o = rsize(&mut src,1, new_start, new_end, chunk, true, &mut |_| {});
        assert_eq!(o.exit_code(), crate::outcome::EXIT_REFUSED, "an unknown on-disk geometry must be refused");
        assert_eq!(std::fs::read(&path).unwrap(), before, "a refused run must not write");
        assert_eq!(
            table::load_gpt(&src).unwrap().unwrap().entries[0].starting_lba,
            16384,
            "the table must stay where the third party put it"
        );

        drop(src);
        crate::dev::warn_if_remove_failed(&ckpt_path);
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
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();
        let o = ap(&mut src, &plan, chunk, true, &mut |_| {});
        assert_eq!(o.exit_code(), crate::outcome::EXIT_REFUSED, "apply must refuse while a resize job owns the slot");
        assert!(matches!(read_checkpoint(&src, &geom_of(&src)).unwrap(), CheckpointSlot::Resize(_)), "the resize checkpoint must survive");
        assert_eq!(table::load_gpt(&src).unwrap().unwrap().entries[1].starting_lba, 8192, "nothing may be relocated");

        // (b) 槽位里是 plan 型搬移的 ckpt → resize_part 拒绝（不能按"无 ckpt"重做）
        std::fs::remove_file(&ckpt_path).unwrap();
        let ck = Checkpoint {
            disk_size: src.size, ss, grow_part: 1, last_usable_lba: plan.last_usable_lba,
            moves: plan.moves.clone(), kind: PlanKind::TailPacked, cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();
        let o = rsize(&mut src,1, 2048, 3071, chunk, false, &mut |_| {});
        assert!(
            matches!(&o, Outcome::Refused(m) if m.contains("relocation job")),
            "resize_part must refuse while a relocation job owns the slot: {o:?}"
        );
        assert!(matches!(read_checkpoint(&src, &geom_of(&src)).unwrap(), CheckpointSlot::Relocation(_)), "the relocation checkpoint must survive");

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

        assert!(matches!(read_checkpoint(&src, &geom_of(&src)).unwrap(), CheckpointSlot::Empty));

        // 头部魔数在、内容被截断：正是断电撕裂写下的样子。
        // 新契约：文件在而全部不可读 = Infra，且文案指向 abandon（不再吞成空槽）
        std::fs::write(&ckpt_path, CKPT_MAGIC).unwrap();
        let e = read_checkpoint(&src, &geom_of(&src)).err().expect("a corrupt checkpoint must not read as an empty slot");
        assert!(matches!(&e, Fail::Infra(m) if m.contains("none is readable")), "{e:?}");

        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&path);
    }

    /// 盘上可控的分区号必须在构造点拦掉：`Plan.moves` 可整份来自 ckpt，而下游拿它直接索引
    /// `entries[(n-1)]`，越界会 panic。四种畸形都重算 CRC——证明拦住它们的是字段校验，不是 CRC
    #[test]
    fn checkpoint_rejects_implausible_partition_numbers() {
        let ckpt = Checkpoint {
            disk_size: 64 * 1024 * 1024,
            ss: 512,
            grow_part: 3,
            last_usable_lba: 100_000,
            moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
            kind: PlanKind::TailPacked,
            cur_index: 0,
            chunks_done: 0,
            chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        assert!(Checkpoint::deserialize(&ckpt.serialize(), &lim(128)).is_ok(), "the baseline must be readable");

        // 字段偏移：magic(8) + ver(4) + disk_size(8) + ss(8) ⇒ grow_part @28；
        // 其后 last_usable_lba(8) + count(4) ⇒ 首条 move 的 part_num @44
        const GROW_PART: usize = 28;
        const MOVE_PART: usize = 44;
        for (off, value, what) in [
            (GROW_PART, 0u32, "grow_part = 0"),
            (GROW_PART, 129, "grow_part = 129"),
            (MOVE_PART, 0, "part_num = 0"),
            (MOVE_PART, 129, "part_num = 129"),
        ] {
            let mut b = ckpt.serialize();
            b[off..off + 4].copy_from_slice(&value.to_le_bytes());
            let crc_off = b.len() - 4;
            let crc = table::crc32(&b[..crc_off]);
            b[crc_off..].copy_from_slice(&crc.to_le_bytes());
            let e = match Checkpoint::deserialize(&b, &lim(128)) {
                Ok(_) => panic!("{what} must be rejected"),
                Err(e) => e,
            };
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{what}: {e}");
        }

        // 分区号上界来自**表的几何**，不是常量 128：同一份合法记录（grow_part = 200、
        // 首条 move 的 part_num = 3）在 256 槽位的几何下必须可读，在 128 槽位下被拒。
        // 少了这条，一块 256 槽位的表产生的 checkpoint 会被判成"损坏"，中断的搬移永久无法续传
        let wide = Checkpoint {
            grow_part: 200,
            last_usable_lba: 100_000,
            fp: Default::default(),
            map: None,
            ..ckpt
        };
        assert!(
            Checkpoint::deserialize(&wide.serialize(), &lim(256)).is_ok(),
            "a record written against a 256-slot table must stay readable"
        );
        assert!(
            Checkpoint::deserialize(&wide.serialize(), &lim(128)).is_err(),
            "the same record must be rejected when the table only has 128 slots"
        );
    }

    /// 原子写的临时文件必须落在目标同目录：相对路径的 parent() 是空而非 None，
    /// 历史实现把空父目录拼上根组件，临时文件会写到当前盘的根目录
    #[test]
    fn relative_checkpoint_path_writes_beside_the_target() {
        let name = format!(".diskedit.rel_ckpt_test.{}.tmp", std::process::id());
        let p = std::path::PathBuf::from(&name);
        atomic_write_ckpt(&p, b"payload").unwrap();
        assert!(p.exists(), "the ckpt must land beside its relative target");
        let _ = std::fs::remove_file(&p);
    }

    /// 进度字段同属盘上可控值：CRC 自洽但值荒谬的 ckpt 若被放行，续传会把该条目的
    /// 全部 chunk 跳过后仍提交表项——表将指向从未复制过的位置。重算 CRC 以证明
    /// 拦住它的是上界校验；两族 ckpt 各验一遍
    #[test]
    fn checkpoint_rejects_implausible_chunk_progress() {
        // 搬移型：len_lba=100 × ss=512 = 50 KiB < 1 MiB ⇒ 该条目只有 1 个 chunk
        let ckpt = Checkpoint {
            disk_size: 64 * 1024 * 1024,
            ss: 512,
            grow_part: 1,
            last_usable_lba: 100_000,
            moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
            kind: PlanKind::TailPacked,
            cur_index: 0,
            chunks_done: 1,
            chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        assert!(Checkpoint::deserialize(&ckpt.serialize(), &lim(128)).is_ok(), "baseline must stay readable");

        const CUR_INDEX: usize = 73;  // 头 73 字节（magic..moves[0]）之后
        const CHUNKS_DONE: usize = 77;
        let rewrite_crc = |b: &mut Vec<u8>| {
            let crc_off = b.len() - 4;
            let crc = table::crc32(&b[..crc_off]);
            b[crc_off..].copy_from_slice(&crc.to_le_bytes());
        };
        let mut b = ckpt.serialize();
        b[CHUNKS_DONE..CHUNKS_DONE + 8].copy_from_slice(&2u64.to_le_bytes());
        rewrite_crc(&mut b);
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("chunks_done beyond the entry's chunk count must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // 全部条目已提交（cur_index == moves.len()）时进度必须归零
        let mut b = ckpt.serialize();
        b[CUR_INDEX..CUR_INDEX + 4].copy_from_slice(&1u32.to_le_bytes());
        rewrite_crc(&mut b);
        assert!(Checkpoint::deserialize(&b, &lim(128)).is_err(), "residual progress after the last entry must be rejected");

        // cur_index 越过条目数（chunks_done 归零可绕过进度校验）：放行会在恢复比对里
        // 被夹回 len，续传循环空转、搬移全部跳过而表仍按新几何提交
        let mut b = ckpt.serialize();
        b[CUR_INDEX..CUR_INDEX + 4].copy_from_slice(&2u32.to_le_bytes());
        rewrite_crc(&mut b);
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("cur_index beyond the move list must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // 单分区 resize 型：区间 1024 LBA × 512 B = 512 KiB < 1 MiB ⇒ 同样只有 1 个 chunk
        let rs = RsCheckpoint {
            disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
            old_start: 2048, old_end: 3071, new_start: 2048, new_end: 3071,
            fs_shrunk: false, chunks_done: 1, chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        assert!(RsCheckpoint::deserialize(&rs.serialize(), &lim(128)).is_ok(), "baseline must stay readable");
        // 带 loop 映射与真实指纹的现场同样要原样回读：标志字节与它后面那两个 u64 是一组，
        // 偏移记账错位会在这里立刻现形
        let mapped = RsCheckpoint {
            disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
            old_start: 2048, old_end: 3071, new_start: 2048, new_end: 3071,
            fs_shrunk: false, chunks_done: 1, chunk_bytes: 1024 * 1024,
            fp: crate::dev::TargetFingerprint { dev: 7, ino: 42, size: 1 << 20 },
            map: Some((1024 * 1024, 0)),
        };
        let back = RsCheckpoint::deserialize(&mapped.serialize(), &lim(128)).expect("mapped checkpoint must round-trip");
        assert_eq!(back.fp, crate::dev::TargetFingerprint { dev: 7, ino: 42, size: 1 << 20 });
        assert_eq!(back.map, Some((1024 * 1024, 0)));
        let mut b = rs.serialize();
        b[65..73].copy_from_slice(&2u64.to_le_bytes()); // chunks_done @ 12+6×8+4+1
        rewrite_crc(&mut b);
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("chunks_done beyond the moved range must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }

    /// ss 是 LBA→字节换算的唯一单位，写地址全经它换算：与目标几何不一致的 ckpt 即使
    /// CRC 正确也必须拦下（重算 CRC——证明拦住它的是字段校验，不是 CRC），且两族
    /// decoder 同一防线
    #[test]
    fn checkpoint_rejects_sector_size_mismatch() {
        let base = Checkpoint {
            disk_size: 64 * 1024 * 1024,
            ss: 512,
            grow_part: 1,
            last_usable_lba: 100_000,
            moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
            kind: PlanKind::TailPacked,
            cur_index: 0,
            chunks_done: 0,
            chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        assert!(Checkpoint::deserialize(&base.serialize(), &lim(128)).is_ok(), "the baseline must be readable");

        // 字段偏移：magic(8) + ver(4) + disk_size(8) ⇒ ss @20（两族布局同前缀）
        const SS: usize = 20;
        let mut b = base.serialize();
        b[SS..SS + 8].copy_from_slice(&4096u64.to_le_bytes());
        let crc_off = b.len() - 4;
        let crc = table::crc32(&b[..crc_off]);
        b[crc_off..].copy_from_slice(&crc.to_le_bytes());
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a valid CRC must not smuggle a foreign sector size");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        let rs = RsCheckpoint {
            disk_size: 64 * 1024 * 1024,
            ss: 512,
            part: 1,
            old_start: 2048, old_end: 2147, new_start: 2048, new_end: 2147,
            fs_shrunk: false, chunks_done: 0, chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        assert!(RsCheckpoint::deserialize(&rs.serialize(), &lim(128)).is_ok(), "the baseline must be readable");
        let mut b = rs.serialize();
        b[SS..SS + 8].copy_from_slice(&4096u64.to_le_bytes());
        let crc_off = b.len() - 4;
        let crc = table::crc32(&b[..crc_off]);
        b[crc_off..].copy_from_slice(&crc.to_le_bytes());
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a valid CRC must not smuggle a foreign sector size");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }

    /// Checkpoint 头部与载荷判据的拒绝分支逐字段覆盖。CRC 之前的判据直接改字节即可
    /// 触发；CRC 之后的判据（chunk_bytes）须重算 CRC，证明拦住它的是字段校验本身。
    /// 布局推导沿用上方测试：moves 从 @44 起每条 29 字节，
    /// count=1 时 cur_index@73、chunks_done@77、chunk_bytes@85、kind@93
    #[test]
    fn checkpoint_rejects_malformed_header_and_payload() {
        let ckpt = Checkpoint {
            disk_size: 64 * 1024 * 1024,
            ss: 512,
            grow_part: 1,
            last_usable_lba: 100_000,
            moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 100, delta_lba: 200, is_swap: false }],
            kind: PlanKind::TailPacked,
            cur_index: 0,
            chunks_done: 0,
            chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        assert!(Checkpoint::deserialize(&ckpt.serialize(), &lim(128)).is_ok(), "the baseline must be readable");
        let rewrite_crc = |b: &mut Vec<u8>| {
            let crc_off = b.len() - 4;
            let crc = table::crc32(&b[..crc_off]);
            b[crc_off..].copy_from_slice(&crc.to_le_bytes());
        };

        // 魔数与版本在 CRC 之前判定：无需重算
        let mut b = ckpt.serialize();
        b[0..8].copy_from_slice(b"NOTMAGI!");
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("foreign magic must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        let mut b = ckpt.serialize();
        b[8..12].copy_from_slice(&99u32.to_le_bytes());
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a foreign version must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // last_usable_lba 越容器（@32）：放行会拿一个容器上并不存在的可用区去判区间
        const LAST_USABLE: usize = 32;
        let mut b = ckpt.serialize();
        b[LAST_USABLE..LAST_USABLE + 8].copy_from_slice(&200_001u64.to_le_bytes());
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("last_usable beyond the container must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // 条目数越上界（@40）：防巨型 with_capacity，上界 = 表条目数
        const COUNT: usize = 40;
        let mut b = ckpt.serialize();
        b[COUNT..COUNT + 4].copy_from_slice(&129u32.to_le_bytes());
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a move count beyond the table's entries must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // 首条 move 的字段：first_lba@48、len_lba@56、delta_lba@64（区间算术全 checked）
        const FIRST_LBA: usize = 48;
        const LEN_LBA: usize = 56;
        const DELTA_LBA: usize = 64;
        // 源区间溢出
        let mut b = ckpt.serialize();
        b[FIRST_LBA..FIRST_LBA + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        b[LEN_LBA..LEN_LBA + 8].copy_from_slice(&1u64.to_le_bytes());
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an overflowing entry range must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        // 目标区间溢出
        let mut b = ckpt.serialize();
        b[DELTA_LBA..DELTA_LBA + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an overflowing target range must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        // 出可用区（first_usable = 34）
        let mut b = ckpt.serialize();
        b[FIRST_LBA..FIRST_LBA + 8].copy_from_slice(&33u64.to_le_bytes());
        b[LEN_LBA..LEN_LBA + 8].copy_from_slice(&1u64.to_le_bytes());
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an entry outside the usable area must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // plan kind 只认 0/1（@93）
        const KIND: usize = 93;
        let mut b = ckpt.serialize();
        b[KIND] = 7;
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("an unknown plan kind must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // CRC 在字段判据之前：撕一位必须先撞 CRC
        let mut b = ckpt.serialize();
        let last = b.len() - 1;
        b[last] ^= 0xFF;
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("a torn tail must be rejected by CRC");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // chunk_bytes = 0 在 CRC 之后判定：重算 CRC 证明拦它的是判据本身
        const CHUNK_BYTES: usize = 85;
        let mut b = ckpt.serialize();
        b[CHUNK_BYTES..CHUNK_BYTES + 8].copy_from_slice(&0u64.to_le_bytes());
        rewrite_crc(&mut b);
        let e = Checkpoint::deserialize(&b, &lim(128)).err().expect("chunk_bytes = 0 must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }

    /// 单分区 resize 型的对应拒绝分支（与搬移型逐条对称）。
    /// 布局：magic(8)+ver(4)+disk_size(8)+ss(8)+part(4) ⇒ old_start@32、old_end@40、
    /// new_start@48、new_end@56、fs_shrunk@64、chunks_done@65、chunk_bytes@73
    #[test]
    fn resize_checkpoint_rejects_malformed_records() {
        let rs = RsCheckpoint {
            disk_size: 64 * 1024 * 1024, ss: 512, part: 1,
            old_start: 2048, old_end: 3071, new_start: 2048, new_end: 3071,
            fs_shrunk: false, chunks_done: 0, chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        assert!(RsCheckpoint::deserialize(&rs.serialize(), &lim(128)).is_ok(), "the baseline must be readable");
        let rewrite_crc = |b: &mut Vec<u8>| {
            let crc_off = b.len() - 4;
            let crc = table::crc32(&b[..crc_off]);
            b[crc_off..].copy_from_slice(&crc.to_le_bytes());
        };

        let mut b = rs.serialize();
        b[0..8].copy_from_slice(b"NOTMAGI!");
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("foreign magic must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        let mut b = rs.serialize();
        b[8..12].copy_from_slice(&99u32.to_le_bytes());
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a foreign version must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // 区间倒挂：放行会让 chunk 总数算式 old_end − old_start + 1 在 debug 下 panic
        const OLD_START: usize = 32;
        const OLD_END: usize = 40;
        let mut b = rs.serialize();
        b[OLD_START..OLD_START + 8].copy_from_slice(&3071u64.to_le_bytes());
        b[OLD_END..OLD_END + 8].copy_from_slice(&2048u64.to_le_bytes());
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("an inverted range must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // 分区号越上界
        const PART: usize = 28;
        let mut b = rs.serialize();
        b[PART..PART + 4].copy_from_slice(&129u32.to_le_bytes());
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("an implausible partition number must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // 出可用区（first_usable = 34）
        let mut b = rs.serialize();
        b[OLD_START..OLD_START + 8].copy_from_slice(&33u64.to_le_bytes());
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a range outside the usable area must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // CRC 先于字段判据：撕一位必须先撞 CRC
        let mut b = rs.serialize();
        let last = b.len() - 1;
        b[last] ^= 0xFF;
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("a torn tail must be rejected by CRC");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");

        // chunk_bytes = 0 在 CRC 之后：重算 CRC 证明拦它的是判据本身
        const CHUNK_BYTES: usize = 73;
        let mut b = rs.serialize();
        b[CHUNK_BYTES..CHUNK_BYTES + 8].copy_from_slice(&0u64.to_le_bytes());
        rewrite_crc(&mut b);
        let e = RsCheckpoint::deserialize(&b, &lim(128)).err().expect("chunk_bytes = 0 must be rejected");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    }

    /// `shift = None`（缩容请求）不产生新的搬移计划：无 ckpt 必须拒绝而不是现算一个；
    /// 有 ckpt 则以 ckpt 为准（那才是盘上真正在做的事）
    #[test]
    fn shift_plan_none_never_ghost_relocates() {
        let chunk: u64 = 1024 * 1024;
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
        let g = table::load_gpt(&src).unwrap().unwrap();
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();

        let e = match plan_shift(&mut src, 1, None) {
            Err(e) => e,
            Ok(_) => panic!("no checkpoint must mean no relocation plan for a shrink"),
        };
        assert!(matches!(&e, Fail::Refused(m) if m.contains("shrink")), "{e:?}");

        let plan = make_plan(&mut src, 1).unwrap();
        let ck = Checkpoint {
            disk_size: src.size, ss: g.ss, grow_part: 1, last_usable_lba: plan.last_usable_lba,
            moves: plan.moves.clone(), kind: PlanKind::Shift, cur_index: 0, chunks_done: 0, chunk_bytes: chunk,
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();
        let resumed = plan_shift(&mut src, 1, None).unwrap();
        assert_eq!(resumed.moves.len(), plan.moves.len(), "resume must return the checkpoint's plan");
        assert!(pending_relocation(&src, 1).unwrap());
        assert!(!pending_relocation(&src, 2).unwrap(), "another partition has no pending job");

        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&path);
    }

    /// 槽位被**另一族**作业（单分区 resize 的 RsCheckpoint）占着：plan 型搬移的两条生成
    /// 路径都必须拒绝：若与空槽同归 `Empty`，plan 会现算一份计划并报
    /// 成功，而 apply 在 prepare_apply 的分派处必然拒绝——同一条命令两个入口给出相反结论
    #[test]
    fn foreign_resize_ckpt_refuses_relocation_plans() {
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        // 单分区 resize 作业的现场：分区号与几何取 fixture 里分区 1 的值，判定只看族别
        let rs = RsCheckpoint {
            disk_size: src.size,
            ss: 512,
            part: 1,
            old_start: 2048,
            old_end: 6143,
            new_start: 2048,
            new_end: 6143,
            fs_shrunk: false,
            chunks_done: 0,
            chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();

        let e = plan_resuming(&mut src, 1).err().expect("a foreign resize job must refuse a fresh plan");
        assert!(matches!(&e, Fail::Refused(m) if m.contains("single-partition resize job")), "{e:?}");
        let e = plan_shift(&mut src, 1, Some(1024)).err().expect("the shift path must refuse it too");
        assert!(matches!(&e, Fail::Refused(m) if m.contains("single-partition resize job")), "{e:?}");
        // 归属仍判"本分区无搬移作业"：resize 自己要按这份 RsCheckpoint 续跑，不该被搬移的门挡住
        assert!(!pending_relocation(&src, 1).unwrap());

        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&path);
    }

    /// 槽位被别的分区的作业占着：现算路径必须拒绝——当空槽会覆盖别人的现场，
    /// 当自己的续跑点会执行与盘上几何不符的 plan；has_pending_relocation 对
    /// 请求分区仍是"无作业"
    #[test]
    fn foreign_checkpoint_slot_refuses_fresh_plans() {
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 10239)]);
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        // grow_part = 2 的作业记录，moves 内容任意：resume 判定只看归属
        let ck = Checkpoint {
            disk_size: src.size,
            ss: 512,
            grow_part: 2,
            last_usable_lba: 32734,
            moves: vec![PlanEntry { part_num: 1, first_lba: 2048, len_lba: 4096, delta_lba: 2048, is_swap: false }],
            kind: PlanKind::TailPacked,
            cur_index: 0,
            chunks_done: 0,
            chunk_bytes: 1024 * 1024,
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &ck.serialize()).unwrap();

        let e = plan_resuming(&mut src, 1).err().expect("a foreign job must refuse a fresh plan");
        assert!(matches!(&e, Fail::Refused(m) if m.contains("occupies the checkpoint slot")), "{e:?}");
        let e = plan_shift(&mut src, 1, Some(4096)).err().expect("a foreign job must refuse a fresh shift plan");
        assert!(matches!(&e, Fail::Refused(m) if m.contains("occupies the checkpoint slot")), "{e:?}");
        assert!(!pending_relocation(&src, 1).unwrap(), "the occupying job belongs to another partition");

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
            identity: crate::dev::TargetIdentity::resolve_image(&tmp),
            file: f,
            path: tmp.clone(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: Default::default(),
            loop_mapping: None,
        };
        // gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
        crate::table::ensure_protective_mbr(&mut src).unwrap();
        (src, tmp)
    }

    fn plan_open(path: &std::path::Path) -> FileSource {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        let size = std::fs::metadata(path).unwrap().len();
        FileSource {
            identity: crate::dev::TargetIdentity::resolve_image(path),
            file: f,
            path: path.into(),
            sector_size: 512,
            size,
            is_block: false,
            journal: None,
            ownership: None,
            fingerprint: Default::default(),
            loop_mapping: None,
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

        // 尾部容量不足：条目越过 last_usable（损坏表）→ 不得进入规划。
        // gptman 会拒绝写入越界条目，故先写合法表再手工修补原始字节 + 重算**该副本**的双 CRC
        let (src3, p3) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 30000, 32000)]);
        drop(src3);
        let last_lba = std::fs::metadata(&p3).unwrap().len() / 512 - 1;
        let backup_arr_lba = last_lba - 32; // 备份数组跨度 = 128×128/512 扇区，紧邻备份头之前
        // 条目 2（索引 1）的 ending_lba 改成 34000（越过 last_usable）
        let patch_entry = |raw: &mut Vec<u8>, arr_lba: u64, hdr_lba: u64| {
            let (a, h) = ((arr_lba * 512) as usize, (hdr_lba * 512) as usize);
            raw[a + 128 + 40..a + 128 + 48].copy_from_slice(&34000u64.to_le_bytes());
            let arr_crc = table::crc32(&raw[a..a + 128 * 128]);
            raw[h + 88..h + 92].copy_from_slice(&arr_crc.to_le_bytes());
            raw[h + 16..h + 20].fill(0);
            let hdr_crc = table::crc32(&raw[h..h + 92]);
            raw[h + 16..h + 20].copy_from_slice(&hdr_crc.to_le_bytes());
        };
        // 越界的只有主副本：那是这一份的事，盘尾备份给出合法表，规划照旧成立
        {
            let mut raw = std::fs::read(&p3).unwrap();
            patch_entry(&mut raw, 2, 1);
            std::fs::write(&p3, &raw).unwrap();
        }
        let mut src3 = plan_open(&p3);
        assert!(make_plan(&mut src3, 1).is_ok(), "the intact backup copy must still yield a plan");
        drop(src3);
        // 两份都越界 ⇒ 拒绝：越界条目不得被当成有效布局
        {
            let mut raw = std::fs::read(&p3).unwrap();
            patch_entry(&mut raw, 2, 1);
            patch_entry(&mut raw, backup_arr_lba, last_lba);
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
        let plan_of = |moves: Vec<PlanEntry>, last_usable: u64| Plan { ss: 512, last_usable_lba: last_usable, grow_part: 1, moves, kind: PlanKind::TailPacked, repair: RepairAction::None };
        let mv = |first_lba: u64, delta_lba: u64| PlanEntry { part_num: 1, first_lba, len_lba: 10, delta_lba, is_swap: false };

        // 无 movable → last_usable_lba
        assert_eq!(grow_end_for(&plan_of(vec![], 32734), 0).unwrap(), 32734);
        // last_usable_lba 取满值也不得溢出
        assert_eq!(grow_end_for(&plan_of(vec![], u64::MAX), 0).unwrap(), u64::MAX);
        // delta = 0（已贴合的 movable 同样进 moves）→ 其新起点 − 1
        assert_eq!(grow_end_for(&plan_of(vec![mv(1000, 0)], 32734), 0).unwrap(), 999);
        // 多个 movable → 取最小新起点 − 1（100→150、200→250）
        assert_eq!(grow_end_for(&plan_of(vec![mv(100, 50), mv(200, 50)], 32734), 0).unwrap(), 149);
        // first_lba + delta_lba 溢出 → 报错，不回绕
        assert!(grow_end_for(&plan_of(vec![mv(u64::MAX - 5, 10)], u64::MAX), 0).is_err());
        // 新起点越过 last_usable_lba → 拒绝（越界表不得写进几何）
        assert!(grow_end_for(&plan_of(vec![mv(40000, 0)], 32734), 0).is_err());
        // 终点低于目标分区自身起点 → 拒绝（端点倒挂的表项不得提交）
        assert!(grow_end_for(&plan_of(vec![mv(100, 0)], 32734), 200).is_err());
    }

    /// 右邻已尾打包时重跑 grow：规划只产出 δ=0 的空转 move，执行它不得写盘。
    /// 落屏障的条件是"moves 里存在非 0 δ"（apply 里的 `effective`），本测试同时锁住
    /// 该判定的输入（全 0 δ）与后果（邻居表项一字未动）
    #[test]
    fn tail_packed_rerun_only_yields_zero_delta_moves() {
        let data = vec![0u8; 4 * 1024 * 1024];
        let mut src = crate::support::src_from("tp0", &data);
        table::create_gpt(&mut src, 512, None).unwrap();
        // 4MiB = 8192 扇区，尾部留给备份头与备份数组 ⇒ last_usable = 8158：
        // p3 紧贴尾部，p1 与 p3 之间留 2000 扇区空隙
        gpt_policy::add_entry_at(&mut src, 2048, 3000, "p1", table::LINUX_FS_TYPE_GUID, [1; 16]).unwrap();
        gpt_policy::add_entry_at(&mut src, 5001, 8158, "p3", table::LINUX_FS_TYPE_GUID, [3; 16]).unwrap();
        let plan = make_plan(&mut src, 1).unwrap();
        assert!(!plan.moves.is_empty(), "the tail neighbour is a blocker and must appear as a move");
        assert!(plan.moves.iter().all(|m| m.delta_lba == 0), "an already tail-packed neighbour needs no relocation");
        let before = table::load_gpt(&src).unwrap().unwrap();
        let o = ap(&mut src, &plan, 1 << 20, true, &mut |_| {});
        assert!(o.is_complete(), "a zero-delta plan must not fail: {}", o.exit_code());
        let after = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(
            (before.entries[2].starting_lba, before.entries[2].ending_lba),
            (after.entries[2].starting_lba, after.entries[2].ending_lba),
            "a zero-delta move must leave the neighbour untouched"
        );
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
        let (_, action) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
        gpt_policy::apply_repair(&mut src, &action).unwrap();
        let g2 = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g2.state, table::GptState::Valid);
        assert_eq!(g2.pmbr, table::PmbrSize::Normal);
        assert_eq!(g2.header.backup_lba, new_file_last);
        assert_eq!(g2.header.last_usable_lba, new_file_last - 32 - 1); // span = 128×128/512
        assert_eq!(gpt_policy::classify_repair(&g2, new_file_last).unwrap(), RepairAction::None, "修复须幂等");
        drop(src);
        let _ = std::fs::remove_file(&p);
    }

    /// 把盘上主头的 AlternateLBA 指到一个早于容器末端的值（构造"需要修复"的表）
    fn make_stale(path: &std::path::Path) {
        let mut raw = std::fs::read(path).unwrap();
        raw[512 + 32..512 + 40].copy_from_slice(&20000u64.to_le_bytes());
        raw[512 + 16..512 + 20].fill(0);
        let hdr_crc = table::crc32(&raw[512..512 + 92]);
        raw[512 + 16..512 + 20].copy_from_slice(&hdr_crc.to_le_bytes());
        std::fs::write(path, &raw).unwrap();
    }

    /// 「Refused 承诺本次未写盘」的落地检验：需要修复的 stale 表遇到事前拒绝时，
    /// 盘上必须**仍是** stale。修复写盘若发生在事前判定之前，这条断言就会红
    #[test]
    fn refused_paths_do_not_write_the_disk() {
        for via_apply in [false, true] {
            let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 12287)]);
            drop(src);
            make_stale(&p);
            let mut src = plan_open(&p);
            let ckpt_path = src.identity.checkpoint_path().to_path_buf();
            assert!(
                matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::NeedsRepair { .. }),
                "夹具须是需要修复的表"
            );

            let chunk: u64 = 1024 * 1024;
            let o = if via_apply {
                // 槽位被另一族（单分区 resize）作业占着 ⇒ apply 须事前拒绝
                let rs = RsCheckpoint {
                    disk_size: src.size, ss: 512, part: 1, old_start: 2048, old_end: 6143,
                    new_start: 2048, new_end: 4095, fs_shrunk: false, chunks_done: 0, chunk_bytes: chunk,
                    fp: Default::default(),
                    map: None,
                };
                atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();
                let plan = make_plan(&mut src, 1).unwrap();
                ap(&mut src, &plan, chunk, true, &mut |_| {})
            } else {
                // 新范围与分区 2 重叠 ⇒ resize-part 须事前拒绝
                rsize(&mut src, 1, 2048, 9000, chunk, true, &mut |_| {})
            };
            assert_eq!(o.exit_code(), 10, "via_apply={via_apply}");

            let g = table::load_gpt(&src).unwrap().unwrap();
            assert!(
                matches!(g.state, table::GptState::NeedsRepair { .. }),
                "via_apply={via_apply}: 拒绝路径不得写盘（修复不能被提前执行）"
            );
            assert_eq!(g.header.backup_lba, 20000, "via_apply={via_apply}");
            drop(src);
            let _ = std::fs::remove_file(&ckpt_path);
            let _ = std::fs::remove_file(&p);
        }
    }

    /// 表本来就合法（无需修复）时，事前拒绝同样是 10 且一字未写：边界是否推进只取决于
    /// "本次调用有没有碰过目标盘"，与"有没有修复可做"无关。与上一条互补——那条用需要修复
    /// 的表，拦的是"修复写盘被提前执行"；这条稳住"无修复可做"时不得把拒绝误升成 30
    #[test]
    fn refusal_on_an_already_valid_table_is_still_ten() {
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143), (2, [0x22; 16], 8192, 12287)]);
        drop(src);
        let mut src = plan_open(&p);
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        assert!(
            matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::Valid),
            "夹具须是无需修复的表"
        );
        let before = std::fs::read(&p).unwrap();

        // 新范围与分区 2 重叠 ⇒ 事前拒绝
        let o = rsize(&mut src,1, 2048, 9000, 1024 * 1024, true, &mut |_| {});
        assert_eq!(o.exit_code(), 10, "无需修复的表上，事前拒绝仍是 Refused");
        drop(src);

        assert_eq!(std::fs::read(&p).unwrap(), before, "拒绝路径不得写盘");
        assert!(!ckpt_path.exists(), "拒绝早于 checkpoint 落盘");
        let _ = std::fs::remove_file(&p);
    }

    /// 边界另一侧：搬移中途失败只能报 Failed（30），不得报 Refused（10）——此刻数据区
    /// 已经动过，声称"本次未写盘"就是假的。注入一次读失败让它发生在前几个 chunk 之后
    #[test]
    fn failure_after_the_boundary_is_failed_not_refused() {
        let (mut src, path) = plan_fixture(&[(1, [0x11; 16], 2048, 14335)]); // 源 6 MiB
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        let chunk: u64 = 1024 * 1024;
        // 注入点落在搬移的第三轮读（右移时尾→头推进，此刻已有 3 个 chunk 写进目标区），
        // 且不在 prepare 阶段的任何探测区间里
        let _fault = crate::dev::ReadFaultGuard::at(2048 * 512 + 3 * chunk);
        let o = rsize(&mut src,1, 6144, 18431, chunk, true, &mut |_| {});
        assert_eq!(o.exit_code(), 30, "越界之后的失败须按 Failed 报（不得声称未写盘）");
        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&path);
    }

    /// 恢复判定对不上（ckpt 记的是另一次作业）仍是事前拒绝：与"槽位被别的作业占着"同码，
    /// 因为此刻确实一字未写。同理 `--no-fs` 与缩容不可共存的判定也必须在写盘之前
    #[test]
    fn divergent_and_no_fs_shrink_are_refused_before_any_write() {
        let chunk: u64 = 1024 * 1024;
        // (a) Divergent：ckpt 的目标区间与本次请求不同
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        drop(src);
        make_stale(&p);
        let mut src = plan_open(&p);
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        let rs = RsCheckpoint {
            disk_size: src.size, ss: 512, part: 1, old_start: 2048, old_end: 6143,
            new_start: 3072, new_end: 5119, fs_shrunk: false, chunks_done: 0, chunk_bytes: chunk,
            fp: Default::default(),
            map: None,
        };
        atomic_write_ckpt(&ckpt_path, &rs.serialize()).unwrap();
        let o = rsize(&mut src, 1, 2048, 4095, chunk, true, &mut |_| {});
        assert_eq!(o.exit_code(), 10, "Divergent 属事前拒绝");
        assert!(
            matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::NeedsRepair { .. }),
            "Divergent 拒绝不得写盘"
        );
        let _ = std::fs::remove_file(&ckpt_path);
        drop(src);
        let _ = std::fs::remove_file(&p);

        // (b) --no-fs + 缩容：分区末端会切进未缩的 FS 元数据，必须事前拒绝
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        drop(src);
        make_stale(&p);
        let mut src = plan_open(&p);
        let o = rsize(&mut src, 1, 2048, 4095, chunk, true, &mut |_| {});
        assert_eq!(o.exit_code(), 10, "--no-fs 缩容属事前拒绝");
        assert!(
            matches!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::NeedsRepair { .. }),
            "--no-fs 缩容拒绝不得写盘"
        );
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
        let (_, action) = gpt_policy::resolve_geometry(&src).unwrap().unwrap();
        gpt_policy::apply_repair(&mut src, &action).unwrap();
        let g2 = table::load_gpt(&src).unwrap().unwrap();
        assert_eq!(g2.state, table::GptState::Valid);
        assert_eq!(g2.pmbr, table::PmbrSize::Normal);
        assert_eq!(g2.header.backup_lba, file_last);
        assert_eq!(g2.header.last_usable_lba, file_last - 32 - 1); // span = 128×128/512
        assert_eq!(table::load_gpt(&src).unwrap().unwrap().state, table::GptState::Valid, "修复须幂等");
        drop(src);
        let _ = std::fs::remove_file(&p);
    }

    /// 几何自洽性在解析层强制：last_usable_lba 越过盘末端即拒绝（不必等下游算术兜底）。
    /// 该判定读的是**本副本**的字段，故单份越界只说明这一份不可用；两份都越界才拒绝
    #[test]
    fn geometry_rejects_last_usable_beyond_disk() {
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        drop(src);
        // 对照组：未打补丁时表可解析——证明后面的 Err 来自几何判定而非 CRC 写错
        let clean = plan_open(&p);
        assert!(table::load_gpt(&clean).unwrap().is_some());
        drop(clean);
        let last_lba = std::fs::metadata(&p).unwrap().len() / 512 - 1;
        // 手改 last_usable_lba 后重算该副本的头 CRC，才能穿过签名/CRC 抵达几何判定
        let patch = |raw: &mut Vec<u8>, hdr_lba: u64| {
            let h = (hdr_lba * 512) as usize;
            raw[h + 48..h + 56].copy_from_slice(&u64::MAX.to_le_bytes());
            raw[h + 16..h + 20].fill(0);
            let hdr_crc = table::crc32(&raw[h..h + 92]);
            raw[h + 16..h + 20].copy_from_slice(&hdr_crc.to_le_bytes());
        };

        // 只坏主头：盘尾备份完好 ⇒ 回退。单份越界不该否掉整块盘
        {
            let mut raw = std::fs::read(&p).unwrap();
            patch(&mut raw, 1);
            std::fs::write(&p, &raw).unwrap();
        }
        let src = plan_open(&p);
        let g = table::load_gpt(&src).expect("a single out-of-range copy must fall back").expect("the backup copy is intact");
        assert!(
            matches!(g.state, table::GptState::NeedsRepair { cause: table::HeaderIssue::PrimaryUnreadable }),
            "recovered-from-backup must be marked for repair"
        );
        drop(src);

        // 两份都越界 ⇒ 拒绝，且拒绝发生在解析层（所有消费者共享），resolve_geometry 只是透传
        {
            let mut raw = std::fs::read(&p).unwrap();
            patch(&mut raw, last_lba);
            std::fs::write(&p, &raw).unwrap();
        }
        let src = plan_open(&p);
        let err = match table::load_gpt(&src) {
            Err(e) => e,
            Ok(_) => panic!("beyond-container last_usable_lba must be refused at parse time"),
        };
        assert!(
            matches!(err, table::GptError::BeyondContainer { field: "last_usable_lba", .. }),
            "{err}"
        );
        assert!(gpt_policy::resolve_geometry(&src).is_err());
        drop(src);
        let _ = std::fs::remove_file(&p);
    }

    /// swap 身份读取的字节保真：sws_volume 是 16 字节定长字段，mkswap -L 存入时不校验编码，
    /// 解成 String 必改写非法序列（Latin-1 会把 0xFF 变成 U+00FF 的两个字节），
    /// 于是重建 swap 时 -L 写回的值与盘上原值不同
    #[test]
    fn swap_identity_label_is_raw_bytes() {
        let ss = 512u64;
        let ps = 4096usize;
        let mut data = vec![0u8; 4 * ps];
        data[ps - 10..ps].copy_from_slice(b"SWAPSPACE2");
        data[1036..1052].copy_from_slice(&[0x5A; 16]);
        data[1052..1068].copy_from_slice(&[b'A', 0xFF, b'B', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let src = crate::support::src_from("swaplabel", &data);
        let path = src.path.clone();
        let (uuid, label) = read_swap_identity(&src, 0, data.len() as u64 / ss, ss);
        assert_eq!(uuid, Some([0x5A; 16]));
        assert_eq!(label.as_deref(), Some(&[b'A', 0xFF, b'B'][..]));
        drop(src);
        let _ = std::fs::remove_file(&path);
    }

    /// 32K 页格式的 swap 在本机（4K 页）认不出类型：identify 走 swapon 口径给出 "unknown"，
    /// 扩容收尾因此整段跳过 FS 步骤。但它确实是"分区已扩、swap 未重建"这一未完成的后置条件，
    /// 必须报 Pending（20）而不是又一次静默成功。MBR 的 resize 与 apply 两条路径已有同一判据，
    /// 这条守住 resize_part 那条
    #[test]
    fn unactivatable_swap_growth_is_pending_not_silent_success() {
        let (src, p) = plan_fixture(&[(1, [0x11; 16], 2048, 6143)]);
        drop(src);
        // 只在 32K 候选位置放 swap 签名：swapon 口径（候选集不含 32K）看不到它 → identify 给 unknown
        let base = 2048 * 512;
        let mut raw = std::fs::read(&p).unwrap();
        raw[base + 32768 - 10..base + 32768].copy_from_slice(b"SWAPSPACE2");
        std::fs::write(&p, &raw).unwrap();

        let mut src = plan_open(&p);
        let ckpt_path = src.identity.checkpoint_path().to_path_buf();
        assert_eq!(crate::fsid::identify(&src, base as u64, 4096 * 512).unwrap(), "unknown");
        assert!(crate::fsid::unactivatable_swap(&src, base as u64, 4096 * 512).unwrap());

        // 起点不变、向右扩
        let o = rsize(&mut src,1, 2048, 8191, 1024 * 1024, false, &mut |_| {});
        match &o {
            crate::outcome::Outcome::Applied { pending, .. } => {
                assert_eq!(pending.len(), 1, "须报一条待办：{pending:?}");
                assert_eq!(pending[0].kind, crate::outcome::PendingKind::Swap);
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        assert!(!o.is_complete());
        assert_eq!(o.exit_code(), 20, "分区已扩而 swap 未重建，不能报完全成功");

        drop(src);
        let _ = std::fs::remove_file(&ckpt_path);
        let _ = std::fs::remove_file(&p);
    }

    /// scene_mismatch 三判据：指纹不符、映射不符、映射有无之别各自拦下；双零指纹
    /// （真块设备节点 / 非 Unix 平台）不构成证据，比对只对"双方都有意义"的值进行
    #[test]
    fn scene_mismatch_guards_foreign_scenes() {
        let fp = |dev: u64, ino: u64, size: u64| crate::dev::TargetFingerprint { dev, ino, size };
        let scene = |fp: crate::dev::TargetFingerprint, map: Option<(u64, u64)>| FileSource {
            fingerprint: fp,
            loop_mapping: map,
            ..crate::support::src_from("scene", &[])
        };

        // 指纹不符 → 拦
        let s = scene(fp(7, 42, 1024), None);
        assert!(scene_mismatch(&s, &fp(7, 43, 1024), &None).unwrap().contains("fingerprint"));
        // 指纹一致且双方都无映射 → 放行
        let s = scene(fp(7, 42, 1024), None);
        assert!(scene_mismatch(&s, &fp(7, 42, 1024), &None).is_none());
        // 映射值不符 → 拦
        let s = scene(fp(7, 42, 1024), Some((0, 0)));
        assert!(scene_mismatch(&s, &fp(7, 42, 1024), &Some((4096, 0))).unwrap().contains("loop mapping"));
        // 映射有无之别 → 拦：loop 写入的现场不能改从裸镜像续跑，反之亦然
        let s = scene(fp(7, 42, 1024), None);
        assert!(scene_mismatch(&s, &fp(7, 42, 1024), &Some((0, 0))).unwrap().contains("loop device"));
        // 双零指纹不构成证据（真块设备 / 非 Unix 平台）：指纹不同也放行
        let s = scene(fp(0, 0, 0), None);
        assert!(scene_mismatch(&s, &fp(7, 42, 1024), &None).is_none());
        let _ = std::fs::remove_file(&s.path);
    }
}
