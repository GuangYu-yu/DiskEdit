//! 恢复记录在磁盘上的固定头与载荷编解码。
//!
//! 头部只标识格式家族与载荷长度，两段 CRC 分别覆盖头与载荷。解码顺序固定为
//! 「magic → 头 CRC → 长度一致性 → 载荷 CRC → 内容」，每一步的失败由
//! [`DecodeFailure`] 分开表达。记录只有一种形状：载荷变化必须体现为长度变化，
//! 只改字段语义而不改长度不会通过内容解析。
//!
//! 每条恢复区间自带 CRC（覆盖该条的全部字段），载荷尾段被截断时完整的前缀仍可逐条
//! 验证并回放（见 [`decode_prefix`]）。
//!
//! 兼容约定：同一 `MAGIC` 下只有一种布局，尾段可选。**后向可读**——载荷在某个尾段
//! 之前结束，该段取缺省值（`entry_commits` 为空表、`repair` 为无、`table_fence` 为
//! `Pending`）。**前向必拒**——已知尾段之后出现无法识别的字节即按 `Format` 拒读，
//! 不跳过、不当缺省。不做前向兼容是因为记录决定"要不要再写一遍"：看不懂的尾段被跳过，
//! 等于让旧构建在不知道修复或表步已经落地的状态下重写第二遍。

use crate::dev::DiskGuidState;
use crate::outcome::RunOutcome;
use crate::platform::{IdentityMaterial, TargetId};
use crate::planner::chunk::move_chunk_layout;
use sha2::{Digest, Sha256};

use super::scene::{Mapping, SceneIdentity, SceneProof};

/// 格式家族的标识字节：此标识下的搬移位移是有符号值，别的标识一律解码失败
pub(crate) const MAGIC: [u8; 4] = *b"DSK3";
/// 旧格式家族的标识：只用于把拒读的原因说成「旧布局」，解码器不认识它
pub(crate) const LEGACY_MAGIC: [u8; 4] = *b"DSK2";
/// 头部的固定字节数
pub(crate) const HEADER_LEN: usize = 16;

const HEADER_CRC_RANGE: std::ops::Range<usize> = 0..12;
pub(crate) const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;

/// 认出旧格式家族的名字，供各入口把拒读原因说清楚；别的值都不是识别过的布局
pub(crate) fn legacy_layout(magic: [u8; 4]) -> Option<&'static str> {
    (magic == LEGACY_MAGIC).then_some("DSK2")
}

fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

/// 记录头。`header_crc` 覆盖 `0..12`，`payload_crc` 覆盖载荷全部字节
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecoveryHeader {
    pub(crate) payload_len: u32,
    pub(crate) header_crc: u32,
    pub(crate) payload_crc: u32,
}

/// 事务标识：128 位随机值
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TxId([u8; 16]);

impl TxId {
    pub(crate) const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub(crate) fn bytes(&self) -> [u8; 16] {
        self.0
    }
}

/// 计划标识：128 位随机值
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanId([u8; 16]);

impl PlanId {
    pub(crate) const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub(crate) fn bytes(&self) -> [u8; 16] {
        self.0
    }
}

/// 计划版本：每次持久化递增
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PlanVersion(u64);

impl PlanVersion {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

/// 计划内容摘要：只有操作类型不足以安全恢复
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanDigest([u8; 32]);

impl PlanDigest {
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn bytes(&self) -> [u8; 32] {
        self.0
    }

    /// 请求摘要：把决定"这是哪一次作业"的字段按固定顺序编码后取 sha256。
    ///
    /// 盘面上的旧起点/旧末端**不入摘要**：表提交之后重算必然对不上，而那一维由
    /// "盘上条目是否已停在新区间"单独判定。入摘要的是目标请求本身（操作、分区、
    /// 新区间、扇区大小、盘大小）；`--chunk` 是执行参数而非目标身份，不入摘要——
    /// 它决定游标到字节的映射，由续跑校验单独核对
    pub(crate) fn of_request(
        op: OpKind,
        part: u32,
        new_start: u64,
        new_end: u64,
        sector_size: u64,
        disk_size: u64,
    ) -> Self {
        let mut input = Vec::with_capacity(1 + 4 + 8 * 4);
        input.push(op.tag());
        input.extend_from_slice(&part.to_le_bytes());
        input.extend_from_slice(&new_start.to_le_bytes());
        input.extend_from_slice(&new_end.to_le_bytes());
        input.extend_from_slice(&sector_size.to_le_bytes());
        input.extend_from_slice(&disk_size.to_le_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&Sha256::digest(&input));
        Self(out)
    }

    /// 无搬移计划的写命令（new / mkfs / set / resizefs）的请求摘要：
    /// 操作码 + 固定顺序的数值 + 带长度前缀的字节串。同一请求每次运行必须得到同一摘要
    pub(crate) fn of_request_fields(op: OpKind, numbers: &[u64], blobs: &[&[u8]]) -> Self {
        let mut input = vec![op.tag()];
        for n in numbers {
            input.extend_from_slice(&n.to_le_bytes());
        }
        for b in blobs {
            input.extend_from_slice(&(b.len() as u64).to_le_bytes());
            input.extend_from_slice(b);
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&Sha256::digest(&input));
        Self(out)
    }
}

/// 记录对应的布局操作。
///
/// 码值固定：`New`=1、`Add`=2、`Delete`=3、
/// `ResizePart`=4、`Move`=5、`Copy`=6、`Mkfs`=7、`ResizeFs`=8、`Set`=9、`Resize`=10。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpKind {
    New,
    Add,
    Delete,
    ResizePart,
    Move,
    Copy,
    Mkfs,
    /// `resizefs`：只动文件系统的扩缩（不写分区表）
    ResizeFs,
    /// `set`：分区属性写入（表区的 name/type/flag，或文件系统的 label/uuid）
    Set,
    /// `resize` 的一键作业：分区层、文件系统与 LVM 收尾同属一份记录
    Resize,
}

impl OpKind {
    fn tag(self) -> u8 {
        match self {
            Self::New => 1,
            Self::Add => 2,
            Self::Delete => 3,
            Self::ResizePart => 4,
            Self::Move => 5,
            Self::Copy => 6,
            Self::Mkfs => 7,
            Self::ResizeFs => 8,
            Self::Set => 9,
            Self::Resize => 10,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::New),
            2 => Some(Self::Add),
            3 => Some(Self::Delete),
            4 => Some(Self::ResizePart),
            5 => Some(Self::Move),
            6 => Some(Self::Copy),
            7 => Some(Self::Mkfs),
            8 => Some(Self::ResizeFs),
            9 => Some(Self::Set),
            10 => Some(Self::Resize),
            _ => None,
        }
    }

    pub(crate) fn command_name(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Add => "add",
            Self::Delete => "delete",
            Self::ResizePart => "resize-part",
            Self::Move => "move",
            Self::Copy => "copy",
            Self::Mkfs => "mkfs",
            Self::ResizeFs => "resizefs",
            Self::Set => "set",
            Self::Resize => "resize",
        }
    }

    /// 重跑原命令能否接着这份记录：只有请求可重放、且重放不会产生第二次效果的操作为真。
    /// `add` / `delete` 的请求依赖当前表（重放会重复添加或删除不存在的条目），
    /// `copy` 不提供续跑
    pub(crate) fn resumable(self) -> bool {
        match self {
            Self::Add | Self::Delete | Self::Copy => false,
            Self::New | Self::ResizePart | Self::Move | Self::Mkfs | Self::ResizeFs | Self::Set | Self::Resize => true,
        }
    }
}

/// 已持久化的进度。内存中更超前的进度不得写入
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Progress {
    pub(crate) completed_steps: u32,
    pub(crate) transfer_cursor: u64,
    pub(crate) fs_phase: u8,
}

/// `resize` 分区层提交之后还要跑的 LVM 收尾基线。存在即表示本作业请求过
/// `--grow-lv`；三个数值都是写表前采到的量，续跑时 `pvresize` 之后据此把 LV
/// 补到目标长度。要扩哪个 LV 由续跑请求重新选定，记录不保存路径
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LvmPost {
    pub(crate) lv_size_before: u64,
    pub(crate) lv_target: u64,
    pub(crate) extent_bytes: u64,
}

/// 搬移作业的进入语义。续跑据此对号：按另一种语义执行同一份计划，就是把用户的请求
/// 静默放大或缩水
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelocKind {
    /// 吃满右侧空闲的尾部打包（`resize grow`）
    TailPacked,
    /// 精确落在请求新末端的最小位移（`resize SIZE --allow-move`）
    Shift,
}

impl RelocKind {
    fn tag(self) -> u8 {
        match self {
            Self::TailPacked => 0,
            Self::Shift => 1,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::TailPacked),
            1 => Some(Self::Shift),
            _ => None,
        }
    }
}

/// 一条搬移条目的表步提交标记。记录里第一次出现"表已开始写、游标未推进"这个事实：
/// 表步之前先落 `InFlight`，条目完成时与游标推进同一笔落 `Committed`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryCommit {
    /// 表步尚未开始
    Pending,
    /// 表步已开始，结果尚未记入记录
    InFlight,
    /// 条目完成，表已写入
    Committed,
}

impl EntryCommit {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::InFlight => 1,
            Self::Committed => 2,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Pending),
            1 => Some(Self::InFlight),
            2 => Some(Self::Committed),
            _ => None,
        }
    }
}

/// 表区修复的种类。码值固定：`RelocateBackup`=0、`RepairProtectiveMbr`=1、
/// `RelocateAndRepair`=2
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairKind {
    /// 只把备份头搬到容器末端
    RelocateBackup,
    /// 只重建保护 MBR 的 SizeInLBA
    RepairProtectiveMbr,
    /// 两者一起
    RelocateAndRepair,
}

impl RepairKind {
    fn tag(self) -> u8 {
        match self {
            Self::RelocateBackup => 0,
            Self::RepairProtectiveMbr => 1,
            Self::RelocateAndRepair => 2,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::RelocateBackup),
            1 => Some(Self::RepairProtectiveMbr),
            2 => Some(Self::RelocateAndRepair),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::RelocateBackup => "relocate the backup GPT",
            Self::RepairProtectiveMbr => "rewrite the protective MBR",
            Self::RelocateAndRepair => "relocate the backup GPT and rewrite the protective MBR",
        }
    }
}

/// 修复动作在记录里的状态：`InFlight` = 已开始、结果未记入；`Landed` = 收敛并已记入
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairState {
    InFlight,
    Landed,
}

impl RepairState {
    fn tag(self) -> u8 {
        match self {
            Self::InFlight => 0,
            Self::Landed => 1,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::InFlight),
            1 => Some(Self::Landed),
            _ => None,
        }
    }
}

/// 记录下来的表区修复：种类、搬移后的备份头位置与重建后的末端，以及它走到哪一步。
/// 只重建保护 MBR 的动作没有 LBA 值，两处写零
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepairFact {
    pub(crate) kind: RepairKind,
    pub(crate) new_backup_lba: u64,
    pub(crate) new_last_usable: u64,
    pub(crate) state: RepairState,
}

/// 表步围栏：没有搬移计划的记录（表类命令）用它表达"表可能已写"。
/// `Pending` = 表步未开始，`InFlight` = 已开始、结果未记入，`Landed` = 已写并记入
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TableFence {
    #[default]
    Pending,
    InFlight,
    Landed,
}

impl TableFence {
    fn tag(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::InFlight => 1,
            Self::Landed => 2,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Pending),
            1 => Some(Self::InFlight),
            2 => Some(Self::Landed),
            _ => None,
        }
    }
}

/// 一条搬移：分区号、源起点、长度、无符号位移、是否 swap。
///
/// 数组顺序即执行顺序，**必须原样持久化**：尾部打包与最小位移都按"末→头"处理，
/// 后一条的目的区可以复写前一条的旧位，重排会让拷贝读到已被覆写的数据
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RelocEntry {
    pub(crate) partition: u32,
    pub(crate) src_first: u64,
    pub(crate) len: u64,
    /// 目标起点相对源起点的位移（扇区）；负值表示向左移动
    pub(crate) delta: i64,
    pub(crate) is_swap: bool,
}

/// `resize --allow-move` 的可恢复状态。
///
/// 只存盘上推不出来的事实：有序计划、双游标、块字节、作业语义与几何守卫。
/// 扩容终点、每条目的 chunk 数、块大小收敛值、拷贝方向与各分区当前落点都由计划与
/// 锁下几何重推，不入记录。
///
/// swap 待重建由「[`Self::entry_index`] 指向 `is_swap` 的条目」表达：重建失败时不
/// 推进游标，重跑从同一条目补做，不靠盘面签名判断
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Relocation {
    pub(crate) entries: Vec<RelocEntry>,
    /// 当前条目下标；等于 `entries.len()` 表示全部条目已提交
    pub(crate) entry_index: u32,
    /// 当前条目已完成的 chunk 数
    pub(crate) chunks_in_entry: u64,
    pub(crate) chunk_bytes: u64,
    pub(crate) kind: RelocKind,
    pub(crate) grow_part: u32,
    /// 创建时的容器字节数与扇区大小：续跑换了目标尺寸/几何即拒绝
    pub(crate) disk_size: u64,
    pub(crate) ss: u64,
}

/// 恢复动作
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryAction {
    Continue(u64),
    Rollback(u64),
    InspectOnly,
}

impl RecoveryAction {
    fn write(self, out: &mut Vec<u8>) {
        match self {
            Self::Continue(cursor) => {
                out.push(1);
                out.extend(cursor.to_le_bytes());
            }
            Self::Rollback(point) => {
                out.push(2);
                out.extend(point.to_le_bytes());
            }
            Self::InspectOnly => {
                out.push(3);
                out.extend(0u64.to_le_bytes());
            }
        }
    }
}

/// 提交状态：越过 durable boundary 后固定为 `Committed`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitState {
    Prepared,
    Committed,
}

impl CommitState {
    fn tag(self) -> u8 {
        match self {
            Self::Prepared => 0,
            Self::Committed => 1,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Prepared),
            1 => Some(Self::Committed),
            _ => None,
        }
    }
}

/// 留存状态：清理判据的唯一来源
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retention {
    Active,
    CommittedPending,
    Unverified,
    Quarantined,
}

impl Retention {
    fn tag(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::CommittedPending => 1,
            Self::Unverified => 2,
            Self::Quarantined => 3,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Active),
            1 => Some(Self::CommittedPending),
            2 => Some(Self::Unverified),
            3 => Some(Self::Quarantined),
            _ => None,
        }
    }

    /// `Unverified` 与 `Quarantined` 不允许清理
    pub(crate) fn allows_cleanup(self) -> bool {
        match self {
            Self::Active | Self::CommittedPending => true,
            Self::Unverified | Self::Quarantined => false,
        }
    }
}

/// CRC 错误发生的那一段
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrcRegion {
    Header,
    Payload,
}

/// 解码失败的三类形态
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DecodeFailure {
    /// magic 或载荷形状不属于本构建；`recorded` 只在能解析到身份段时有值
    Format { magic: [u8; 4], recorded: Option<SceneIdentity> },
    /// 观察到的字节数与记录声明的长度不符
    Incomplete { observed: u64, expected: u64 },
    /// 完整长度下的 CRC 不符
    Crc { region: CrcRegion },
}

/// 回滚一个被覆盖的扇区区间：原始字节 + 写入后的字节 + LBA 区间 + 扇区大小。
/// 不含表语义差分，回滚只把字节写回原位；`after` 是写盘前核对"盘面仍是记录写后的样子"
/// 的唯一依据（与 `original` 等长，同样按扇区取整）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RestoreEntry {
    pub(crate) sector_size: u32,
    pub(crate) first_lba: u64,
    pub(crate) sectors: u32,
    pub(crate) original: Vec<u8>,
    pub(crate) after: Vec<u8>,
}

impl RestoreEntry {
    pub(crate) fn byte_offset(&self) -> Option<u64> {
        self.first_lba.checked_mul(u64::from(self.sector_size))
    }

    /// 字节数落在声明的扇区区间内（最后一扇区可以只存到文件末尾），写入后字节与原文等长，
    /// 且扇区大小是 2 的幂
    pub(crate) fn is_well_formed(&self) -> bool {
        if !self.sector_size.is_power_of_two() || self.sectors == 0 {
            return false;
        }
        if self.after.len() != self.original.len() {
            return false;
        }
        let len = self.original.len() as u64;
        let covered = u64::from(self.sectors) * u64::from(self.sector_size);
        let previous = covered - u64::from(self.sector_size);
        len > previous && len <= covered
    }
}

/// `copy` 的源区间：源分区号、首 LBA、扇区数与逻辑块大小，以及从这段字节算出的
/// sha256 全值。源与副本同属记录里的 `target`，源身份即 `target` 的身份，不另存一份
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CopySource {
    pub(crate) partition: u32,
    pub(crate) first_lba: u64,
    pub(crate) sectors: u64,
    pub(crate) sector_size: u64,
    pub(crate) digest: [u8; 32],
}

impl CopySource {
    pub(crate) fn byte_offset(&self) -> Option<u64> {
        self.first_lba.checked_mul(self.sector_size)
    }

    pub(crate) fn byte_len(&self) -> Option<u64> {
        self.sectors.checked_mul(self.sector_size)
    }

    /// 分区号非零、有扇区、逻辑块是 2 的幂，且端点都能算出
    fn is_well_formed(&self) -> bool {
        self.partition != 0
            && self.sectors != 0
            && self.sector_size.is_power_of_two()
            && self.first_lba.checked_add(self.sectors).is_some()
            && self.byte_offset().is_some()
            && self.byte_len().is_some()
    }
}

/// 记录侧字段派生的作业阶段。
///
/// 只有 [`Self::Undecidable`] 表示"组合合法但记录不足以判定"；记录能自证前后矛盾的
/// 组合不由它表达，[`Record::stage`] 直接返回内部错误。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    /// 尚未越过任何写前落盘点
    Planned,
    /// 数据搬移进行中：当前条目与全局块游标
    Relocating { entry: u32, cursor: u64 },
    /// 当前条目的块已全部搬完、条目游标尚未推进，且表步尚未开始。决策面把它与
    /// [`Self::Undecidable`] 同等处理；表步已开始（`InFlight`）时直接返回
    /// [`Self::Undecidable`]，盘面可能已经改过
    Relocated,
    /// 表已提交，作业的后续步骤未完成
    TableWritten,
    /// 分区层已完成，收尾步骤（扩容 / FS / LVM）未完成
    Finishing,
    /// 作业已提交，记录只等清理
    Complete,
    /// 字段组合合法（解码放行）但记录不足以判定阶段
    Undecidable,
}

/// 第 `entry` 条搬移的块数：swap 恒 0；布局无解即记录自相矛盾。
/// 与执行器共用 [`move_chunk_layout`]，不另算一遍
fn entry_chunks(reloc: &Relocation, entry: u32) -> Result<u64, RunOutcome> {
    let e = reloc.entries.get(entry as usize).ok_or_else(|| {
        RunOutcome::internal(format!("relocation entry {entry} is outside the recorded plan — the record contradicts itself"))
    })?;
    if e.is_swap {
        return Ok(0);
    }
    move_chunk_layout(e.len, reloc.ss, e.delta.unsigned_abs(), e.len, reloc.chunk_bytes)
        .map(|(_, count)| count)
        .ok_or_else(|| {
            RunOutcome::internal(format!(
                "the recovery record has no block layout for relocation entry {entry} \
                 (len {} sectors, delta {} sectors, {}-byte chunks) — it contradicts itself",
                e.len, e.delta, reloc.chunk_bytes
            ))
        })
}

/// 一份恢复记录的全部字段
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    /// 最近一次解码得到的头；新记录在首次 [`Record::encode`] 之前为零值
    pub(crate) header: RecoveryHeader,
    pub(crate) tx_id: TxId,
    pub(crate) target: SceneIdentity,
    pub(crate) plan_id: PlanId,
    pub(crate) plan_version: PlanVersion,
    pub(crate) op: OpKind,
    pub(crate) plan_digest: PlanDigest,
    pub(crate) progress: Progress,
    /// 分区层提交前的原区间 `(old_start, old_end)`。表提交之后盘上条目只剩新区间，
    /// FS 步要靠它判断该不该扩；只有 [`OpKind::Resize`] / [`OpKind::ResizePart`]
    /// 可能非 `None`
    pub(crate) old_range: Option<(u64, u64)>,
    /// 分区层提交后的 LVM 收尾基线；只有 [`OpKind::Resize`] 可能非 `None`
    pub(crate) lvm_post: Option<LvmPost>,
    /// 搬移作业的可恢复状态。只有 [`OpKind::Resize`] / [`OpKind::ResizePart`] /
    /// [`OpKind::Move`] 可能非 `None`；没有搬移步的记录保持为空
    pub(crate) relocation: Option<Relocation>,
    /// 逐条目的表步提交标记，与 `relocation.entries` 一一对应；没有搬移计划时为空
    pub(crate) entry_commits: Vec<EntryCommit>,
    /// 本作业执行过的表区修复；没有修复动作时为空
    pub(crate) repair: Option<RepairFact>,
    /// 表步围栏：没有搬移计划的记录用它表达表步走到哪一步
    pub(crate) table_fence: TableFence,
    /// 盘上 `stages[]` 段的原始字节（含两字节条数）。解码只校验定长头与取值，
    /// 不解释语义；重写时逐字节回写，条数不属于本类型的字段
    pub(crate) stages: Vec<u8>,
    pub(crate) action: RecoveryAction,
    pub(crate) commit: CommitState,
    pub(crate) retention: Retention,
    /// 写记录时盘上的 GPT Disk GUID。回放前与当前盘面比对的第二道凭据
    pub(crate) disk_guid: DiskGuidState,
    /// 只要发生不可逆写入（数据区搬运或工具落盘），回滚就不可执行
    pub(crate) restore_blocked: bool,
    /// 按写入顺序记录的被覆盖区间
    pub(crate) restore: Vec<RestoreEntry>,
    /// `copy` 的源区间与规划期摘要；其余操作保持为空
    pub(crate) source: Option<CopySource>,
}

impl Record {
    pub(crate) fn new(
        tx_id: TxId,
        target: SceneIdentity,
        plan_id: PlanId,
        plan_version: PlanVersion,
        op: OpKind,
        plan_digest: PlanDigest,
        disk_guid: DiskGuidState,
    ) -> Self {
        Self {
            header: RecoveryHeader::default(),
            tx_id,
            target,
            plan_id,
            plan_version,
            op,
            plan_digest,
            progress: Progress::default(),
            old_range: None,
            lvm_post: None,
            relocation: None,
            entry_commits: Vec::new(),
            repair: None,
            table_fence: TableFence::Pending,
            stages: vec![0, 0],
            action: RecoveryAction::InspectOnly,
            commit: CommitState::Prepared,
            retention: Retention::Active,
            disk_guid,
            restore_blocked: false,
            restore: Vec::new(),
            source: None,
        }
    }

    /// 记录是否越过提交点。完成与否只由 `commit` 回答；
    /// 穷尽匹配留在这里，新增 `CommitState` 变体时由编译器指出未处理的分支
    pub(crate) fn committed(&self) -> bool {
        match self.commit {
            CommitState::Prepared => false,
            CommitState::Committed => true,
        }
    }

    /// 记录里是否留着"目标可能已经被写过"的证据：表区的原文已登记，或已越过不可回滚点。
    /// 这是**持久侧的写事实**，用来回答"盘上是否可能已改过"；它不回答作业进行到哪
    /// （那是 [`Self::stage`] 的事），也不决定记录能否清理或能否报 `Applied`
    pub(crate) fn dirty(&self) -> bool {
        self.restore_blocked || !self.restore.is_empty()
    }

    /// 决策面的作业阶段：记录字段能不能支撑「继续」。
    ///
    /// 与报告面共用 [`Record::derive_stage`] 的派生，只在收尾窗口不同：
    /// [`Stage::TableWritten`] 与 [`Stage::Finishing`] 说明表步之后还有步骤没走完，但
    /// 「表步已写」只在盘上表项里，记录字段给不出这个答案，折成 [`Stage::Undecidable`]；
    /// [`Stage::Relocated`] 据实返回，调用方按不可继续处理。
    pub(crate) fn stage(&self) -> Result<Stage, RunOutcome> {
        Ok(match self.derive_stage()? {
            Stage::TableWritten | Stage::Finishing => Stage::Undecidable,
            other => other,
        })
    }

    /// 报告面的作业阶段：作业到哪一步，据实说。
    ///
    /// 只在 `inspect` 的展示里使用；决策路径走 [`Record::stage`]（收尾窗口折成
    /// [`Stage::Undecidable`]）或 [`Record::derive_stage`]（回滚准入按阶段分文案）。
    #[cfg(target_os = "linux")]
    pub(crate) fn progress_stage(&self) -> Result<Stage, RunOutcome> {
        self.derive_stage()
    }

    /// 由既有字段派生作业阶段：不读盘、不读执行期的 `TransferState`、不新增字段。
    ///
    /// 规则有序、先命中先用；`completed_steps` 的口径按 `op` 分家，不可跨 op 解释。
    /// 能自证前后矛盾的组合（块数超过布局上界、布局无解、条目已全部提交而步骤数落后）
    /// 按内部错误返回；其余无法判定的合法组合落到 [`Stage::Undecidable`]。
    pub(crate) fn derive_stage(&self) -> Result<Stage, RunOutcome> {
        if self.committed() {
            return Ok(Stage::Complete);
        }
        let progress = self.progress;
        let dirty = self.dirty();
        let clean = progress.completed_steps == 0 && progress.transfer_cursor == 0 && !dirty;
        let Some(reloc) = self.relocation.as_ref() else {
            // 表步围栏先于 op 规则：没有搬移计划的记录靠它说明表步走到哪
            match self.table_fence {
                TableFence::InFlight => return Ok(Stage::Undecidable),
                TableFence::Landed => return Ok(Stage::TableWritten),
                TableFence::Pending => {}
            }
            // 落盘窗口按 op 分家：move 先落旧区间再落搬移表，resize-part 的纯扩缩路径
            // 不落 relocation，resize 的 MBR 表项路径同样只落旧区间。进度全零且没有
            // 写事实时，记录自身就证明还没动手
            return Ok(match self.op {
                OpKind::Resize if clean => Stage::Planned,
                OpKind::Resize => Stage::Undecidable,
                OpKind::ResizePart if progress.completed_steps == 2 => Stage::Finishing,
                // 不含搬移步的操作派生不出搬移进度：进度全零且没有写事实时，
                // 记录自身就证明还没动手
                _ if clean => Stage::Planned,
                _ => Stage::Undecidable,
            });
        };
        if reloc.entries.is_empty() {
            // 只有 resize 的纯扩缩会绑一张空搬移表；move 至少搬自身条目，
            // resize-part 的纯扩缩路径不绑 relocation
            return Ok(match self.op {
                OpKind::Resize if progress.completed_steps == 0 => Stage::Planned,
                OpKind::Resize if progress.completed_steps >= 1 => Stage::Finishing,
                _ => Stage::Undecidable,
            });
        }
        let len = reloc.entries.len();
        if (reloc.entry_index as usize) < len {
            let chunks = entry_chunks(reloc, reloc.entry_index)?;
            if reloc.chunks_in_entry > chunks {
                return Err(RunOutcome::internal(format!(
                    "the recovery record records {} of {chunks} chunks for relocation entry {} — it contradicts itself",
                    reloc.chunks_in_entry, reloc.entry_index
                )));
            }
            // 表步已开始、结果未记入记录：盘面可能已经改过，记录给不出确定答案
            if self.entry_commits.get(reloc.entry_index as usize) == Some(&EntryCommit::InFlight) {
                return Ok(Stage::Undecidable);
            }
            if reloc.chunks_in_entry > 0 && reloc.chunks_in_entry < chunks {
                return Ok(Stage::Relocating { entry: reloc.entry_index, cursor: progress.transfer_cursor });
            }
            // 块已搬完、条目游标未推进：数据这一半是确定的事实，表那一半不在记录里。
            // 报告面据此说「已搬移」，决策面（`stage`）折成不可判定，盘面侧的问题由
            // 调用方按表项位置回答（move 的换区条目见 `committed_swap`）
            if reloc.chunks_in_entry > 0 && reloc.chunks_in_entry == chunks {
                return Ok(Stage::Relocated);
            }
            // 走到这里块进度必为 0：上面两支覆盖了大于 0 的全部组合
            if reloc.entry_index == 0
                && progress.completed_steps == 0
                && progress.transfer_cursor == 0
                && !dirty
            {
                return Ok(Stage::Planned);
            }
            return Ok(if self.op == OpKind::ResizePart {
                Stage::Undecidable
            } else {
                Stage::Relocating { entry: reloc.entry_index, cursor: progress.transfer_cursor }
            });
        }
        match self.op {
            OpKind::Move => Ok(Stage::TableWritten),
            OpKind::Resize => {
                let done = progress.completed_steps as usize;
                if done == len {
                    Ok(Stage::TableWritten)
                } else if done > len {
                    Ok(Stage::Finishing)
                } else {
                    Err(RunOutcome::internal(format!(
                        "the recovery record marks all {len} relocation entries as committed but only {done} plan steps as done — it contradicts itself"
                    )))
                }
            }
            _ => Ok(Stage::Undecidable),
        }
    }

    /// 整份编码：头 + 载荷一次生成，头 CRC 与载荷 CRC 都在这里写入
    pub(crate) fn encode(&mut self) -> std::io::Result<Vec<u8>> {
        let payload = self.encode_payload()?;
        let payload_crc = crc32(&payload);
        let payload_len = u32::try_from(payload.len()).expect("a recovery payload fits in 32 bits");

        let mut out = vec![0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&MAGIC);
        out[4..8].copy_from_slice(&payload_len.to_le_bytes());
        out[8..12].copy_from_slice(&payload_crc.to_le_bytes());
        let header_crc = crc32(&out[HEADER_CRC_RANGE]);
        out[12..16].copy_from_slice(&header_crc.to_le_bytes());
        out.extend_from_slice(&payload);

        self.header = RecoveryHeader { payload_len, header_crc, payload_crc };
        Ok(out)
    }

    fn encode_payload(&self) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.tx_id.bytes());
        out.extend_from_slice(&self.plan_id.bytes());
        out.extend(self.plan_version.get().to_le_bytes());
        out.extend(self.op.tag().to_le_bytes());
        out.extend_from_slice(&self.plan_digest.bytes());
        out.extend(self.progress.completed_steps.to_le_bytes());
        out.extend(self.progress.transfer_cursor.to_le_bytes());
        out.push(self.progress.fs_phase);
        match self.old_range {
            None => out.push(0),
            Some((start, end)) => {
                out.push(1);
                out.extend(start.to_le_bytes());
                out.extend(end.to_le_bytes());
            }
        }
        match self.lvm_post {
            None => out.push(0),
            Some(post) => {
                out.push(1);
                out.extend(post.lv_size_before.to_le_bytes());
                out.extend(post.lv_target.to_le_bytes());
                out.extend(post.extent_bytes.to_le_bytes());
            }
        }
        match &self.relocation {
            None => out.push(0),
            Some(r) => {
                out.push(1);
                out.extend((u32::try_from(r.entries.len()).expect("relocation entry count fits in 32 bits")).to_le_bytes());
                for e in &r.entries {
                    out.extend(e.partition.to_le_bytes());
                    out.extend(e.src_first.to_le_bytes());
                    out.extend(e.len.to_le_bytes());
                    out.extend(e.delta.to_le_bytes());
                    out.push(u8::from(e.is_swap));
                }
                out.extend(r.entry_index.to_le_bytes());
                out.extend(r.chunks_in_entry.to_le_bytes());
                out.extend(r.chunk_bytes.to_le_bytes());
                out.push(r.kind.tag());
                out.extend(r.grow_part.to_le_bytes());
                out.extend(r.disk_size.to_le_bytes());
                out.extend(r.ss.to_le_bytes());
            }
        }
        self.action.write(&mut out);
        out.push(self.commit.tag());
        out.push(self.retention.tag());
        // `stages` 段原字节回写：条数、各条定长头与正文都不重新生成
        out.extend_from_slice(&self.stages);
        write_scene(&self.target, &mut out);
        out.extend(self.disk_guid.encode());
        out.push(u8::from(self.restore_blocked));
        out.extend(
            (u32::try_from(self.restore.len()).expect("restore entry count fits in 32 bits"))
                .to_le_bytes(),
        );
        for entry in &self.restore {
            write_entry(entry, &mut out);
        }
        // 源区间段的存在由操作类型决定：`copy` 必带，其余操作没有
        if self.op == OpKind::Copy {
            let s = self.source.ok_or_else(|| {
                std::io::Error::other("a `copy` record carries its source range")
            })?;
            out.extend(s.partition.to_le_bytes());
            out.extend(s.first_lba.to_le_bytes());
            out.extend(s.sectors.to_le_bytes());
            out.extend(s.sector_size.to_le_bytes());
            out.extend_from_slice(&s.digest);
        }
        // 尾段：逐条目的表步提交标记。没有搬移计划时是零条，空段也照常写出
        out.extend(
            (u32::try_from(self.entry_commits.len()).expect("entry commit count fits in 32 bits"))
                .to_le_bytes(),
        );
        for state in &self.entry_commits {
            out.push(state.tag());
        }
        // 尾段：表区修复。无动作写一个零 tag，动作的取值按固定宽度写出
        match self.repair {
            None => out.push(0),
            Some(fact) => {
                out.push(1);
                out.push(fact.kind.tag());
                out.extend(fact.new_backup_lba.to_le_bytes());
                out.extend(fact.new_last_usable.to_le_bytes());
                out.push(fact.state.tag());
            }
        }
        // 尾段：表步围栏，固定一字节
        out.push(self.table_fence.tag());
        Ok(out)
    }
}

/// 单条恢复区间：字段 + 原文 + 写入后字节 + 该条自身的 CRC。
/// 逐条 CRC 让"载荷尾部撕裂"仍能保留并验证完整前缀
fn write_entry(entry: &RestoreEntry, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend(entry.sector_size.to_le_bytes());
    out.extend(entry.first_lba.to_le_bytes());
    out.extend(entry.sectors.to_le_bytes());
    let len = u32::try_from(entry.original.len()).expect("restore bytes fit in 32 bits");
    out.extend(len.to_le_bytes());
    out.extend_from_slice(&entry.original);
    out.extend_from_slice(&entry.after);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_le_bytes());
}

/// 解码一份完整记录。magic 不符或内容与格式定义不符都返回 [`DecodeFailure::Format`]，
/// CRC 失败单独成类
pub(crate) fn decode(bytes: &[u8]) -> Result<Record, DecodeFailure> {
    let mut magic = [0u8; 4];
    for (slot, byte) in magic.iter_mut().zip(bytes.iter()) {
        *slot = *byte;
    }
    if bytes.len() < 4 || magic != MAGIC {
        return Err(DecodeFailure::Format { magic, recorded: None });
    }
    if bytes.len() < HEADER_LEN {
        return Err(DecodeFailure::Incomplete {
            observed: bytes.len() as u64,
            expected: HEADER_LEN as u64,
        });
    }

    let payload_len = u32::from_le_bytes(bytes[4..8].try_into().expect("fixed header slice"));
    let payload_crc = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed header slice"));
    let header_crc = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed header slice"));

    if crc32(&bytes[HEADER_CRC_RANGE]) != header_crc {
        return Err(DecodeFailure::Crc { region: CrcRegion::Header });
    }
    let total_len = HEADER_LEN as u64 + u64::from(payload_len);
    if total_len > MAX_RECORD_BYTES {
        return Err(DecodeFailure::Format { magic, recorded: None });
    }
    if bytes.len() as u64 != total_len {
        return Err(DecodeFailure::Incomplete { observed: bytes.len() as u64, expected: total_len });
    }
    let payload = &bytes[HEADER_LEN..];
    if crc32(payload) != payload_crc {
        return Err(DecodeFailure::Crc { region: CrcRegion::Payload });
    }
    let mut record =
        decode_payload(payload).ok_or(DecodeFailure::Format { magic, recorded: None })?;
    record.header = RecoveryHeader { payload_len, header_crc, payload_crc };
    Ok(record)
}

fn decode_payload(bytes: &[u8]) -> Option<Record> {
    let mut c = Cursor::new(bytes);
    let (mut record, restore_count) = read_payload_prefix(&mut c)?;
    let mut restore = Vec::with_capacity(restore_count.min(1024));
    for _ in 0..restore_count {
        let entry = read_entry(&mut c)?;
        restore.push(entry);
    }
    record.restore = restore;
    // 源区间段只属于 `copy`，且是它必带的段。段按 `op` 直接解析：`copy` 缺段与
    // 其余操作多出字节都说明载荷与操作类型不符
    if record.op == OpKind::Copy {
        record.source = Some(read_source(&mut c)?);
    }
    // 尾段一律可缺省：载荷在这里结束即取默认值。出现字节就必须是完整的段，
    // 段后的剩余字节必须是空，多出任何无法识别的内容都拒读
    record.entry_commits = read_entry_commits(&mut c, &record)?;
    record.repair = read_repair(&mut c)?;
    record.table_fence = read_table_fence(&mut c, &record)?;
    if !c.is_empty() {
        return None;
    }
    Some(record)
}

/// 尾段：表区修复。`None` = 段不完整或 tag 不认识；载荷在段首结束返回"无修复"
fn read_repair(c: &mut Cursor<'_>) -> Option<Option<RepairFact>> {
    if c.is_empty() {
        return Some(None);
    }
    match c.u8()? {
        0 => Some(None),
        1 => Some(Some(RepairFact {
            kind: RepairKind::from_tag(c.u8()?)?,
            new_backup_lba: c.u64()?,
            new_last_usable: c.u64()?,
            state: RepairState::from_tag(c.u8()?)?,
        })),
        _ => None,
    }
}

/// 尾段：表步围栏。载荷在段首结束取 `Pending`；出现字节必须是已知取值。
/// 有搬移计划的记录由逐条目事实承载表步，围栏必须停在 `Pending`；
/// 表步确认必然伴随步数前进，`Landed` 与零步数不能同时成立
fn read_table_fence(c: &mut Cursor<'_>, record: &Record) -> Option<TableFence> {
    let fence = if c.is_empty() {
        TableFence::Pending
    } else {
        TableFence::from_tag(c.u8()?)?
    };
    if record.relocation.is_some() && fence != TableFence::Pending {
        return None;
    }
    if fence == TableFence::Landed && record.progress.completed_steps == 0 {
        return None;
    }
    Some(fence)
}

/// 尾段：逐条目的表步提交标记。`None` = 段不完整、取值不认识或与搬移进度矛盾。
/// 载荷在段首结束返回空表（缺省），此时不做与进度的一致性校验
fn read_entry_commits(c: &mut Cursor<'_>, record: &Record) -> Option<Vec<EntryCommit>> {
    if c.is_empty() {
        return Some(Vec::new());
    }
    let count = c.u32()? as usize;
    let mut commits = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        commits.push(EntryCommit::from_tag(c.u8()?)?);
    }
    // 零条与缺段同义：没有逐条目事实可读，此时不与搬移进度做一致性校验
    if count == 0 {
        return Some(commits);
    }
    // 有逐条目事实就必须有对应的搬移计划，条数必须与计划一致
    let reloc = record.relocation.as_ref()?;
    if count != reloc.entries.len() {
        return None;
    }
    let cursor = reloc.entry_index as usize;
    if cursor > count {
        return None;
    }
    for (i, state) in commits.iter().enumerate() {
        let consistent = match state {
            EntryCommit::Committed => i < cursor,
            EntryCommit::InFlight => i == cursor,
            EntryCommit::Pending => i >= cursor,
        };
        if !consistent {
            return None;
        }
    }
    Some(commits)
}

/// `copy` 载荷末段的源区间：字段固定，没有标签字节。`None` = 段不完整或形状不成立
fn read_source(c: &mut Cursor<'_>) -> Option<CopySource> {
    let source = CopySource {
        partition: c.u32()?,
        first_lba: c.u64()?,
        sectors: c.u64()?,
        sector_size: c.u64()?,
        digest: c.array::<32>()?,
    };
    source.is_well_formed().then_some(source)
}

/// 载荷的固定前缀（到 `restore_count` 为止）。前缀里的每条区间由 [`read_entry`] 单独解析
fn read_payload_prefix(c: &mut Cursor<'_>) -> Option<(Record, usize)> {
    let tx_id = TxId::from_bytes(c.array::<16>()?);
    let plan_id = PlanId::from_bytes(c.array::<16>()?);
    let plan_version = PlanVersion::new(c.u64()?);
    let op = OpKind::from_tag(c.u8()?)?;
    let plan_digest = PlanDigest::from_bytes(c.array::<32>()?);
    let progress = Progress {
        completed_steps: c.u32()?,
        transfer_cursor: c.u64()?,
        fs_phase: c.u8()?,
    };
    let old_range = match c.u8()? {
        0 => None,
        1 => Some((c.u64()?, c.u64()?)),
        _ => return None,
    };
    // `resize`、`resize-part` 与 `move` 会改动分区落位，需要搬移前区间；其余操作带它即与操作类型矛盾
    if old_range.is_some() && !matches!(op, OpKind::Resize | OpKind::ResizePart | OpKind::Move) {
        return None;
    }
    let lvm_post = match c.u8()? {
        0 => None,
        1 => Some(LvmPost {
            lv_size_before: c.u64()?,
            lv_target: c.u64()?,
            extent_bytes: c.u64()?,
        }),
        _ => return None,
    };
    // 其余操作没有 LVM 收尾步骤；带基线即与操作类型矛盾
    if lvm_post.is_some() && op != OpKind::Resize {
        return None;
    }
    let relocation = match c.u8()? {
        0 => None,
        1 => Some(read_relocation(c)?),
        _ => return None,
    };
    // 搬移计划只属于 `move` / `resize` / `resize-part`；别的操作带它即与操作类型矛盾
    if relocation.is_some() && !matches!(op, OpKind::Resize | OpKind::ResizePart | OpKind::Move) {
        return None;
    }
    let action = match c.u8()? {
        1 => RecoveryAction::Continue(c.u64()?),
        2 => RecoveryAction::Rollback(c.u64()?),
        3 => {
            c.u64()?;
            RecoveryAction::InspectOnly
        }
        _ => return None,
    };
    let commit = CommitState::from_tag(c.u8()?)?;
    let retention = Retention::from_tag(c.u8()?)?;
    // `stages` 段：条数之后逐条是 1 字节 tag、1 字节 ok、4 字节正文长度与正文。
    // 只校验结构（tag 属于已知集合、ok 为 0/1、正文是 UTF-8、长度不越界），
    // 不做语义映射；整段原始字节留给重写回放
    let stages_start = c.pos;
    let stage_count = c.u16()? as usize;
    for _ in 0..stage_count {
        match c.u8()? {
            1..=4 => {}
            _ => return None,
        }
        match c.u8()? {
            0 | 1 => {}
            _ => return None,
        }
        let len = c.u32()? as usize;
        std::str::from_utf8(c.take(len)?).ok()?;
    }
    let stages = c.bytes_between(stages_start, c.pos).to_vec();
    let target = read_scene(c)?;
    let disk_guid = DiskGuidState::decode(c.take(crate::dev::CREDENTIAL_LEN)?).ok()?;
    let restore_blocked = match c.u8()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let restore_count = c.u32()? as usize;
    let record = Record {
        header: RecoveryHeader::default(),
        tx_id,
        target,
        plan_id,
        plan_version,
        op,
        plan_digest,
        progress,
        old_range,
        lvm_post,
        relocation,
        entry_commits: Vec::new(),
        repair: None,
        table_fence: TableFence::Pending,
        stages,
        action,
        commit,
        retention,
        disk_guid,
        restore_blocked,
        restore: Vec::new(),
        source: None,
    };
    Some((record, restore_count))
}

/// 解析搬移状态。条数按载荷剩余字节自然收敛：每条 29 字节，读不满即返回 `None`，
/// 不做巨型预分配。区间算术全程 checked——这些值直接进入拷贝地址换算
fn read_relocation(c: &mut Cursor<'_>) -> Option<Relocation> {
    let count = c.u32()? as usize;
    let mut entries: Vec<RelocEntry> = Vec::new();
    for _ in 0..count {
        let partition = c.u32()?;
        let src_first = c.u64()?;
        let len = c.u64()?;
        let delta = c.i64()?;
        let is_swap = match c.u8()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        if partition == 0 || len == 0 {
            return None;
        }
        // 源区间与目标区间都要能算出端点，位移可正可负
        src_first.checked_add(len)?;
        let dst_first = src_first.checked_add_signed(delta)?;
        dst_first.checked_add(len)?;
        if entries.iter().any(|e| e.partition == partition) {
            return None;
        }
        entries.push(RelocEntry { partition, src_first, len, delta, is_swap });
    }
    let entry_index = c.u32()?;
    let chunks_in_entry = c.u64()?;
    let chunk_bytes = c.u64()?;
    let kind = RelocKind::from_tag(c.u8()?)?;
    let grow_part = c.u32()?;
    let disk_size = c.u64()?;
    let ss = c.u64()?;
    if chunk_bytes == 0 || ss == 0 || entry_index as usize > entries.len() {
        return None;
    }
    // 全部条目已提交时进度归零；被扩容的分区不参与搬移（同一分区被搬走即与扩容矛盾）
    if entry_index as usize == entries.len() && chunks_in_entry != 0 {
        return None;
    }
    // grow_part 为 0 表示本次作业没有扩容步（纯搬移）；有扩容步时它不得是被搬走的分区
    if entries.iter().any(|e| e.partition == grow_part) {
        return None;
    }
    Some(Relocation {
        entries,
        entry_index,
        chunks_in_entry,
        chunk_bytes,
        kind,
        grow_part,
        disk_size,
        ss,
    })
}

/// 解析一条恢复区间；`None` = 该条不完整或自身 CRC 不符
fn read_entry(c: &mut Cursor<'_>) -> Option<RestoreEntry> {
    let start = c.pos();
    let sector_size = c.u32()?;
    let first_lba = c.u64()?;
    let sectors = c.u32()?;
    let len = c.u32()? as usize;
    let original = c.take(len)?.to_vec();
    let after = c.take(len)?.to_vec();
    let covered = c.pos();
    let crc = c.u32()?;
    if crc32(c.bytes_between(start, covered)) != crc {
        return None;
    }
    let entry = RestoreEntry { sector_size, first_lba, sectors, original, after };
    if !entry.is_well_formed() || entry.byte_offset().is_none() {
        return None;
    }
    Some(entry)
}

/// 载荷完整前缀：头与固定前缀都可信，且前 `kept` 条区间通过了各自的 CRC。
/// `record.restore` 只含这 `kept` 条；`total` 是载荷声明的条数
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Prefix {
    pub(crate) record: Record,
    pub(crate) kept: usize,
    pub(crate) total: usize,
}

/// 撕裂 / 截断载荷里仍可验证的前缀。头必须完整且 CRC 相符（否则连"记录声明了什么"
/// 都不可信）；随后逐条解析区间，遇到第一条不完整或 CRC 不符即停止——它后面的字节
/// 没有可验证的边界，不能猜。
pub(crate) fn decode_prefix(bytes: &[u8]) -> Option<Prefix> {
    let mut magic = [0u8; 4];
    for (slot, byte) in magic.iter_mut().zip(bytes.iter()) {
        *slot = *byte;
    }
    if bytes.len() < HEADER_LEN || magic != MAGIC {
        return None;
    }
    let payload_len = u32::from_le_bytes(bytes[4..8].try_into().expect("fixed header slice"));
    let payload_crc = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed header slice"));
    let header_crc = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed header slice"));
    let total_len = HEADER_LEN as u64 + u64::from(payload_len);
    if crc32(&bytes[HEADER_CRC_RANGE]) != header_crc || total_len > MAX_RECORD_BYTES {
        return None;
    }
    // 完整载荷由 [`decode`] 处理；这里只接手尾段已经无法整份验证的形态
    let payload = bytes.get(HEADER_LEN..)?;
    if bytes.len() as u64 == total_len && crc32(payload) == payload_crc {
        return None;
    }
    let mut c = Cursor::new(payload);
    let (mut record, restore_count) = read_payload_prefix(&mut c)?;
    let mut kept = 0usize;
    while kept < restore_count {
        let Some(entry) = read_entry(&mut c) else { break };
        record.restore.push(entry);
        kept += 1;
    }
    if kept == 0 {
        return None;
    }
    if let Some(record) = record.restore.get(kept - 1) {
        debug_assert!(record.is_well_formed(), "read_entry validates each entry");
    }
    Some(Prefix { record, kept, total: restore_count })
}

fn write_scene(scene: &SceneIdentity, out: &mut Vec<u8>) {
    out.push(match scene.proof() {
        SceneProof::DeviceId => 1,
        SceneProof::FileId => 2,
        SceneProof::Unavailable => 3,
    });
    match scene.mapping() {
        Some(m) => {
            out.push(1);
            out.extend(m.major().to_le_bytes());
            out.extend(m.minor().to_le_bytes());
        }
        None => out.push(0),
    }
    match scene.id().material() {
        IdentityMaterial::Block { major, minor, device_id } => {
            out.push(1);
            out.extend(major.to_le_bytes());
            out.extend(minor.to_le_bytes());
            match device_id {
                Some(value) => {
                    out.push(1);
                    let bytes = value.as_bytes();
                    out.extend(
                        u32::try_from(bytes.len()).expect("device ID fits in 32 bits").to_le_bytes(),
                    );
                    out.extend_from_slice(bytes);
                }
                None => out.push(0),
            }
        }
        IdentityMaterial::Image { volume_serial, file_id } => {
            out.push(2);
            match volume_serial {
                Some(value) => {
                    out.push(1);
                    out.extend(value.to_le_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(file_id);
        }
    }
    match scene.size() {
        Some(size) => {
            out.push(1);
            out.extend(size.to_le_bytes());
        }
        None => out.push(0),
    }
    match scene.file_id() {
        Some(file_id) => {
            out.push(1);
            out.extend_from_slice(&file_id);
        }
        None => out.push(0),
    }
}

fn read_scene(c: &mut Cursor<'_>) -> Option<SceneIdentity> {
    let proof = match c.u8()? {
        1 => SceneProof::DeviceId,
        2 => SceneProof::FileId,
        3 => SceneProof::Unavailable,
        _ => return None,
    };
    let mapping = match c.u8()? {
        1 => Some(Mapping::new(c.u64()?, c.u64()?)),
        0 => None,
        _ => return None,
    };
    let id = match c.u8()? {
        1 => {
            let major = c.u32()?;
            let minor = c.u32()?;
            let device_id = match c.u8()? {
                1 => {
                    let len = c.u32()? as usize;
                    Some(String::from_utf8(c.take(len)?.to_vec()).ok()?)
                }
                0 => None,
                _ => return None,
            };
            TargetId::from_material(IdentityMaterial::block(major, minor, device_id.as_deref()))
        }
        2 => {
            let volume_serial = match c.u8()? {
                1 => Some(c.u32()?),
                0 => None,
                _ => return None,
            };
            let file_id = c.array::<16>()?;
            TargetId::from_material(IdentityMaterial::image(volume_serial, Some(file_id)).ok()?)
        }
        _ => return None,
    };
    let size = match c.u8()? {
        1 => Some(c.u64()?),
        0 => None,
        _ => return None,
    };
    let file_id = match c.u8()? {
        1 => Some(c.array::<16>()?),
        0 => None,
        _ => return None,
    };
    Some(SceneIdentity::with_details(id, mapping, proof, size, file_id))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn pos(&self) -> usize {
        self.pos
    }

    fn is_empty(&self) -> bool {
        self.pos == self.bytes.len()
    }

    fn bytes_between(&self, start: usize, end: usize) -> &'a [u8] {
        &self.bytes[start..end]
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let slice = self.take(N)?;
        Some(slice.try_into().expect("slice length checked"))
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.array::<2>()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array::<4>()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.array::<8>()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.array::<8>()?))
    }
}

#[cfg(test)]
mod digest_golden {
    use super::{OpKind, PlanDigest};

    /// 请求摘要向量：操作码、数值与带长度前缀的字节串的编码一旦变化，这里先红。
    /// 码值或编码变更时重新生成。
    fn hex(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, pair) in s.as_bytes().chunks(2).enumerate() {
            out[i] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        out
    }

    #[test]
    fn request_digests_are_the_pinned_sha256_of_the_encoding() {
        assert_eq!(
            PlanDigest::of_request_fields(OpKind::New, &[], &[]).0,
            hex("4bf5122f344554c53bde2ebb8cd2b7e3d1600ad631c385a5d7cce23c7785459a")
        );
        assert_eq!(
            PlanDigest::of_request_fields(OpKind::New, &[7], &[b"xy"]).0,
            hex("3abc1ab05533dbbafdca4c977603955022c173f09f630d8a8bdd765af8c1448b")
        );
        assert_eq!(
            PlanDigest::of_request_fields(OpKind::Resize, &[4096], &[]).0,
            hex("ebeb48d4c8e9c961ed68fc134a3c0ca043b7052d0a00b855148f85c71367e375")
        );
    }
}

/// encode 合法产出的记录形态必须能整份读回；清理前的回读走的就是这套 decode
#[cfg(test)]
mod relocation_round_trip {
    use super::*;
    use crate::platform::{IdentityMaterial, TargetId};

    fn image_target() -> SceneIdentity {
        SceneIdentity::from_target(
            TargetId::from_material(IdentityMaterial::image(Some(7), Some([3; 16])).unwrap()),
        )
    }

    /// 完成态的纯搬移记录：游标越过末条目、块进度归零、没有扩容步（grow_part 为 0）
    #[test]
    fn a_completed_move_record_round_trips() {
        let mut record = Record::new(
            TxId::from_bytes([1; 16]),
            image_target(),
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(3),
            OpKind::Move,
            PlanDigest::from_bytes([4; 32]),
            DiskGuidState::Present([5; 16]),
        );
        record.old_range = Some((2048, 4096));
        record.progress = Progress { completed_steps: 1, transfer_cursor: 96, fs_phase: 0 };
        record.action = RecoveryAction::Rollback(0);
        record.commit = CommitState::Committed;
        record.restore_blocked = true;
        record.relocation = Some(Relocation {
            entries: vec![RelocEntry {
                partition: 1,
                src_first: 2048,
                len: 2048,
                delta: 512,
                is_swap: false,
            }],
            entry_index: 1,
            chunks_in_entry: 0,
            chunk_bytes: 96,
            kind: RelocKind::Shift,
            grow_part: 0,
            disk_size: 16 * 1024 * 1024,
            ss: 512,
        });

        let bytes = record.encode().unwrap();
        let mut decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.relocation.as_ref().unwrap().grow_part, 0);
        // Record 的 PartialEq 含 encode 才填的头，不直接比；重编码逐字节复原即整份回读
        assert_eq!(decoded.encode().unwrap(), bytes);
    }

    /// 起点变更缩容的 resize-part 记录同样携带搬移计划，回读不得因操作类型被拒
    #[test]
    fn a_resize_part_record_round_trips_with_a_relocation_plan() {
        let mut record = Record::new(
            TxId::from_bytes([6; 16]),
            image_target(),
            PlanId::from_bytes([8; 16]),
            PlanVersion::new(1),
            OpKind::ResizePart,
            PlanDigest::from_bytes([9; 32]),
            DiskGuidState::Present([10; 16]),
        );
        record.old_range = Some((2048, 16384));
        record.relocation = Some(Relocation {
            entries: vec![RelocEntry {
                partition: 1,
                src_first: 2048,
                len: 4096,
                delta: 512,
                is_swap: false,
            }],
            entry_index: 0,
            chunks_in_entry: 2,
            chunk_bytes: 96,
            kind: RelocKind::Shift,
            grow_part: 0,
            disk_size: 16 * 1024 * 1024,
            ss: 512,
        });

        let bytes = record.encode().unwrap();
        let mut decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.op, OpKind::ResizePart);
        assert_eq!(decoded.encode().unwrap(), bytes);
    }
}

/// `stages` 段：解码保留原始字节，重写时逐字节回写；非法 tag / ok / UTF-8 仍被拒
#[cfg(test)]
mod stages_segment {
    use super::*;

    /// `stages` 段在载荷内的起点：条数之前的定长字段
    /// （tx_id、plan_id、版本、操作、摘要、进度、三个可选段标记、动作、提交、保留）
    const STAGES_AT: usize = 100;

    fn image_target() -> SceneIdentity {
        SceneIdentity::from_target(
            TargetId::from_material(IdentityMaterial::image(None, Some([3; 16])).unwrap()),
        )
    }

    fn fresh() -> Vec<u8> {
        let mut record = Record::new(
            TxId::from_bytes([1; 16]),
            image_target(),
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(1),
            OpKind::ResizePart,
            PlanDigest::from_bytes([4; 32]),
            DiskGuidState::Unavailable,
        );
        record.encode().unwrap()
    }

    fn stages(entries: &[(u8, u8, &[u8])]) -> Vec<u8> {
        let mut out = u16::try_from(entries.len()).unwrap().to_le_bytes().to_vec();
        for (tag, ok, detail) in entries {
            out.push(*tag);
            out.push(*ok);
            out.extend(u32::try_from(detail.len()).unwrap().to_le_bytes());
            out.extend_from_slice(detail);
        }
        out
    }

    /// 把构造好的段替进一份新记录，并重算载荷长度、载荷 CRC 与头 CRC
    fn with_stages(segment: &[u8]) -> Vec<u8> {
        let bytes = fresh();
        let mut payload = bytes[HEADER_LEN..].to_vec();
        payload.splice(STAGES_AT..STAGES_AT + 2, segment.iter().copied());
        let mut out = bytes[..HEADER_LEN].to_vec();
        out[4..8].copy_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        out[8..12].copy_from_slice(&crc32(&payload).to_le_bytes());
        let header_crc = crc32(&out[HEADER_CRC_RANGE]);
        out[12..16].copy_from_slice(&header_crc.to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    #[test]
    fn a_non_empty_stages_segment_is_written_back_byte_for_byte() {
        let segment = stages(&[(1, 1, b"ok"), (4, 0, b"cursor short")]);
        let bytes = with_stages(&segment);
        let mut decoded = decode(&bytes).expect("a non-empty stages segment still decodes");
        let reencoded = decoded.encode().unwrap();
        assert_eq!(reencoded, bytes);
        let at = HEADER_LEN + STAGES_AT;
        assert_eq!(&reencoded[at..at + segment.len()], segment.as_slice());
    }

    #[test]
    fn unknown_tag_bad_ok_and_non_utf8_detail_are_rejected() {
        for (label, segment) in [
            ("unknown tag", stages(&[(5, 1, b"x")])),
            ("zero tag", stages(&[(0, 1, b"x")])),
            ("ok is not a bit", stages(&[(1, 2, b"x")])),
            ("detail is not utf-8", stages(&[(1, 1, &[0x80])])),
        ] {
            assert!(decode(&with_stages(&segment)).is_err(), "{label} must be rejected");
        }
    }

    #[test]
    fn a_zero_count_segment_keeps_the_scene_at_its_offset() {
        let bytes = fresh();
        let payload = &bytes[HEADER_LEN..];
        let mut scene = Vec::new();
        write_scene(&image_target(), &mut scene);
        assert_eq!(&payload[STAGES_AT..STAGES_AT + 2], &0u16.to_le_bytes());
        let scene_at = STAGES_AT + 2;
        assert_eq!(&payload[scene_at..scene_at + scene.len()], scene.as_slice());
    }
}

/// 尾段：逐条目的表步提交标记。段可缺省，出现即整段校验，段后不得有多余字节
#[cfg(test)]
mod entry_commit_ledger {
    use super::*;

    fn image_target() -> SceneIdentity {
        SceneIdentity::from_target(
            TargetId::from_material(IdentityMaterial::image(None, Some([3; 16])).unwrap()),
        )
    }

    /// 一条搬移计划上的记录：`entries` 条目的计划，游标停在 `cursor`
    fn moving_record(entries: u32, cursor: u32) -> Record {
        let mut record = Record::new(
            TxId::from_bytes([1; 16]),
            image_target(),
            PlanId::from_bytes([2; 16]),
            PlanVersion::new(1),
            OpKind::Move,
            PlanDigest::from_bytes([4; 32]),
            DiskGuidState::Unavailable,
        );
        record.old_range = Some((2048, 4096));
        record.relocation = Some(Relocation {
            entries: (0..entries)
                .map(|i| RelocEntry {
                    partition: i + 1,
                    src_first: 2048,
                    len: 2048,
                    delta: 512,
                    is_swap: false,
                })
                .collect(),
            entry_index: cursor,
            chunks_in_entry: 0,
            chunk_bytes: 96,
            kind: RelocKind::Shift,
            grow_part: 0,
            disk_size: 16 * 1024 * 1024,
            ss: 512,
        });
        record
    }

    /// 没有搬移计划的一条表步记录（`delete`）：表步围栏是它唯一的表步事实
    fn table_only_record() -> Record {
        Record::new(
            TxId::from_bytes([7; 16]),
            image_target(),
            PlanId::from_bytes([8; 16]),
            PlanVersion::new(1),
            OpKind::Delete,
            PlanDigest::from_bytes([9; 32]),
            DiskGuidState::Unavailable,
        )
    }

    /// 改完字节后重算载荷长度、载荷 CRC 与头 CRC
    fn rehash(bytes: &mut [u8]) {
        let payload_len = u32::try_from(bytes.len() - HEADER_LEN).unwrap();
        bytes[4..8].copy_from_slice(&payload_len.to_le_bytes());
        let payload_crc = crc32(&bytes[HEADER_LEN..]);
        bytes[8..12].copy_from_slice(&payload_crc.to_le_bytes());
        let header_crc = crc32(&bytes[HEADER_CRC_RANGE]);
        bytes[12..16].copy_from_slice(&header_crc.to_le_bytes());
    }

    #[test]
    fn the_ledger_round_trips() {
        let mut record = moving_record(2, 1);
        record.entry_commits = vec![EntryCommit::Committed, EntryCommit::InFlight];
        record.repair = Some(RepairFact {
            kind: RepairKind::RelocateAndRepair,
            new_backup_lba: 97_728,
            new_last_usable: 97_727,
            state: RepairState::Landed,
        });
        record.table_fence = TableFence::Pending;
        let bytes = record.encode().unwrap();

        let mut decoded = decode(&bytes).expect("the tail segments are part of the current layout");
        assert_eq!(
            decoded.entry_commits,
            vec![EntryCommit::Committed, EntryCommit::InFlight]
        );
        assert_eq!(decoded.repair, record.repair);
        assert_eq!(decoded.encode().unwrap(), bytes);
    }

    /// 没有搬移计划的记录：围栏 `Landed` 整份回读，阶段随之落到表步已写
    #[test]
    fn a_fence_lands_on_a_record_without_a_relocation_plan() {
        let mut record = table_only_record();
        record.table_fence = TableFence::Landed;
        record.progress.completed_steps = 1;
        let bytes = record.encode().unwrap();

        let decoded = decode(&bytes).expect("a fence is part of the current layout");
        assert_eq!(decoded.table_fence, TableFence::Landed);
        assert_eq!(decoded.derive_stage().unwrap(), Stage::TableWritten);
    }

    /// 围栏 `Landed` 而步数还是零：表步已确认与进度未前进互相矛盾，整份拒读
    #[test]
    fn a_landed_fence_without_a_completed_step_is_refused() {
        let mut record = table_only_record();
        record.table_fence = TableFence::Landed;
        record.progress.completed_steps = 0;
        let bytes = record.encode().unwrap();

        assert!(matches!(decode(&bytes), Err(DecodeFailure::Format { .. })));
    }

    /// 有搬移计划的记录不接受已落定的围栏：表步由逐条目事实承载
    #[test]
    fn a_moved_fence_on_a_relocation_record_is_refused() {
        let mut record = moving_record(1, 0);
        record.table_fence = TableFence::Landed;
        let bytes = record.encode().unwrap();

        assert!(matches!(decode(&bytes), Err(DecodeFailure::Format { .. })));
    }

    /// 围栏已开始而表步结果未记入：盘面可能已经改过，阶段给不出确定答案
    #[test]
    fn an_in_flight_fence_makes_the_stage_undecidable() {
        let mut record = table_only_record();
        record.table_fence = TableFence::InFlight;
        assert_eq!(record.derive_stage().unwrap(), Stage::Undecidable);

        record.table_fence = TableFence::Pending;
        assert_eq!(record.derive_stage().unwrap(), Stage::Planned);
    }

    /// 旧格式的记录被拒读：同样的载荷只换头部的格式标识即被拒读，前缀回放也不接手
    #[test]
    fn the_previous_layout_is_refused_by_magic() {
        let mut record = moving_record(1, 0);
        record.entry_commits = vec![EntryCommit::Pending];
        let mut bytes = record.encode().unwrap();
        bytes[0..4].copy_from_slice(&LEGACY_MAGIC);
        rehash(&mut bytes);

        assert_eq!(
            decode(&bytes),
            Err(DecodeFailure::Format { magic: LEGACY_MAGIC, recorded: None })
        );
        assert!(decode_prefix(&bytes).is_none());
        assert_eq!(legacy_layout(LEGACY_MAGIC), Some("DSK2"));
        assert_eq!(legacy_layout(MAGIC), None);
    }

    #[test]
    fn an_unknown_state_tag_is_refused() {
        let mut record = moving_record(2, 1);
        record.entry_commits = vec![EntryCommit::Committed, EntryCommit::InFlight];
        let mut bytes = record.encode().unwrap();
        // 尾部是"无修复"tag 与围栏各一字节，逐条目段的末字节在它们之前
        let last_state = bytes.len() - 3;
        bytes[last_state] = 3;
        rehash(&mut bytes);

        assert!(matches!(decode(&bytes), Err(DecodeFailure::Format { .. })));
    }

    #[test]
    fn an_unknown_repair_tag_is_refused() {
        let mut record = moving_record(1, 0);
        record.entry_commits = vec![EntryCommit::Pending];
        record.repair = Some(RepairFact {
            kind: RepairKind::RelocateBackup,
            new_backup_lba: 97_728,
            new_last_usable: 97_727,
            state: RepairState::InFlight,
        });
        let mut bytes = record.encode().unwrap();
        // 修复段占 tag + kind + 两个 u64 + state 共 19 字节，其 tag 在围栏之前
        const REPAIR_WIDTH: usize = 19;
        let repair_tag = bytes.len() - 1 - REPAIR_WIDTH;
        bytes[repair_tag] = 2;
        rehash(&mut bytes);

        assert!(matches!(decode(&bytes), Err(DecodeFailure::Format { .. })));
    }

    #[test]
    fn an_unknown_fence_tag_is_refused() {
        let mut bytes = moving_record(1, 0).encode().unwrap();
        let last = bytes.len() - 1;
        bytes[last] = 3;
        rehash(&mut bytes);

        assert!(matches!(decode(&bytes), Err(DecodeFailure::Format { .. })));
    }

    /// 有搬移计划而逐条目事实写成零条：解码按缺省接纳，阶段退回进度判据
    #[test]
    fn a_zero_count_ledger_on_a_relocation_record_falls_back_to_progress() {
        let mut record = moving_record(2, 1);
        let bytes = record.encode().unwrap();

        let decoded = decode(&bytes).expect("a zero-count ledger takes its default");
        assert!(decoded.entry_commits.is_empty());
        assert_eq!(
            decoded.derive_stage().unwrap(),
            Stage::Relocating { entry: 1, cursor: 0 }
        );
    }

    /// 逐条目事实必须与游标一致，条数必须与计划一致
    #[test]
    fn a_ledger_that_contradicts_the_cursor_is_refused() {
        for (label, entries, cursor, commits) in [
            (
                "a committed entry at the cursor",
                2,
                1,
                vec![EntryCommit::Pending, EntryCommit::Committed],
            ),
            (
                "a pending entry before the cursor",
                2,
                1,
                vec![EntryCommit::Pending, EntryCommit::Pending],
            ),
            (
                "an in-flight entry before the cursor",
                3,
                2,
                vec![EntryCommit::Pending, EntryCommit::InFlight, EntryCommit::Pending],
            ),
            ("fewer states than entries", 2, 1, vec![EntryCommit::Committed]),
            (
                "more states than entries",
                2,
                1,
                vec![EntryCommit::Pending, EntryCommit::Pending, EntryCommit::Pending],
            ),
        ] {
            let mut record = moving_record(entries, cursor);
            record.entry_commits = commits;
            let bytes = record.encode().unwrap();
            assert!(matches!(decode(&bytes), Err(DecodeFailure::Format { .. })), "{label}");
        }
    }

    #[test]
    fn an_in_flight_entry_makes_the_stage_undecidable() {
        let mut record = moving_record(1, 0);
        record.entry_commits = vec![EntryCommit::InFlight];
        assert_eq!(record.derive_stage().unwrap(), Stage::Undecidable);

        // 同一条记录去掉标记就是一个干净的起点
        let mut plain = moving_record(1, 0);
        plain.entry_commits = vec![EntryCommit::Pending];
        assert_eq!(plain.derive_stage().unwrap(), Stage::Planned);
    }

    /// 载荷在段首结束即取缺省：没有逐条目事实，搬移进度照常保留
    #[test]
    fn a_payload_ending_before_the_ledger_decodes_with_no_entry_facts() {
        let mut record = moving_record(2, 1);
        record.entry_commits = vec![EntryCommit::Committed, EntryCommit::InFlight];
        let mut bytes = record.encode().unwrap();
        // 逐条目段（4 字节条数 + 2 字节状态）之后还有"无修复"tag 与围栏各一字节
        bytes.truncate(bytes.len() - (4 + 2 + 2));
        rehash(&mut bytes);

        let decoded = decode(&bytes).expect("a payload ending before the ledger takes its defaults");
        assert!(decoded.entry_commits.is_empty());
        assert_eq!(decoded.relocation.as_ref().unwrap().entry_index, 1);
        assert_eq!(decoded.repair, None);
        assert_eq!(decoded.table_fence, TableFence::Pending);
    }

    /// 缺修复段：取"没有修复动作"，其余照常
    #[test]
    fn a_payload_ending_before_the_repair_segment_decodes_with_no_repair() {
        let mut record = moving_record(1, 0);
        record.entry_commits = vec![EntryCommit::Pending];
        let mut bytes = record.encode().unwrap();
        // 只去掉"无修复"tag 与围栏
        bytes.truncate(bytes.len() - 2);
        rehash(&mut bytes);

        let decoded = decode(&bytes).expect("a payload ending before the repair segment takes its defaults");
        assert_eq!(decoded.repair, None);
        assert_eq!(decoded.table_fence, TableFence::Pending);
        assert_eq!(decoded.entry_commits, vec![EntryCommit::Pending]);
    }

    /// 缺围栏段：取 `Pending`，已有的修复事实照常保留
    #[test]
    fn a_payload_ending_before_the_fence_segment_decodes_with_a_pending_fence() {
        let mut record = moving_record(1, 0);
        record.entry_commits = vec![EntryCommit::Pending];
        record.repair = Some(RepairFact {
            kind: RepairKind::RepairProtectiveMbr,
            new_backup_lba: 0,
            new_last_usable: 0,
            state: RepairState::Landed,
        });
        let mut bytes = record.encode().unwrap();
        bytes.truncate(bytes.len() - 1);
        rehash(&mut bytes);

        let decoded = decode(&bytes).expect("a payload ending before the fence takes its default");
        assert_eq!(decoded.table_fence, TableFence::Pending);
        assert_eq!(decoded.repair, record.repair);
    }

    /// 缺段不算错，段后的未知字节才算错
    #[test]
    fn trailing_bytes_after_the_known_tail_are_refused() {
        let mut bytes = moving_record(1, 0).encode().unwrap();
        bytes.push(0);
        rehash(&mut bytes);

        assert!(matches!(decode(&bytes), Err(DecodeFailure::Format { .. })));
    }
}
