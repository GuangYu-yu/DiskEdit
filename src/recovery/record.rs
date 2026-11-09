//! 恢复记录在磁盘上的固定头与载荷编解码。
//!
//! 头部只标识格式家族与载荷长度，两段 CRC 分别覆盖头与载荷。解码顺序固定为
//! 「magic → 头 CRC → 长度一致性 → 载荷 CRC → 内容」，每一步的失败由
//! [`DecodeFailure`] 分开表达。记录只有一种形状：载荷变化必须体现为长度变化，
//! 只改字段语义而不改长度不会通过内容解析。
//!
//! 每条恢复区间自带 CRC（覆盖该条的全部字段），载荷尾段被截断时完整的前缀仍可逐条
//! 验证并回放（见 [`decode_prefix`]）。

use crate::dev::DiskGuidState;
use crate::platform::{IdentityMaterial, TargetId};
use sha2::{Digest, Sha256};

use super::scene::{Mapping, SceneIdentity, SceneProof};

/// 格式家族的标识字节
pub(crate) const MAGIC: [u8; 4] = *b"DSKR";
/// 头部的固定字节数
pub(crate) const HEADER_LEN: usize = 16;

const HEADER_CRC_RANGE: std::ops::Range<usize> = 0..12;
pub(crate) const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;

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
    /// "盘上条目是否已停在新区间"单独判定。入摘要的是用户请求本身，续跑据此拒绝
    /// 换了参数的请求（例如另换 `--chunk`，它决定游标到字节的映射）
    pub(crate) fn of_request(
        op: OpKind,
        part: u32,
        new_start: u64,
        new_end: u64,
        sector_size: u64,
        disk_size: u64,
        chunk_len: u64,
    ) -> Self {
        let mut input = Vec::with_capacity(1 + 4 + 8 * 5);
        input.push(op.tag());
        input.extend_from_slice(&part.to_le_bytes());
        input.extend_from_slice(&new_start.to_le_bytes());
        input.extend_from_slice(&new_end.to_le_bytes());
        input.extend_from_slice(&sector_size.to_le_bytes());
        input.extend_from_slice(&disk_size.to_le_bytes());
        input.extend_from_slice(&chunk_len.to_le_bytes());
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
/// 码值取契约的唯一映射：`New`=1、`Add`=2、`Delete`=3、
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
    /// `copy` 明确不提供续跑（见 `help copy` 的 "No resume"）
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

/// 一条搬移：分区号、源起点、长度、无符号位移、是否 swap。
///
/// 数组顺序即执行顺序，**必须原样持久化**：尾部打包与最小位移都按"末→头"处理，
/// 后一条的目的区可以复写前一条的旧位，重排会让拷贝读到已被覆写的数据
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RelocEntry {
    pub(crate) partition: u32,
    pub(crate) src_first: u64,
    pub(crate) len: u64,
    pub(crate) delta: u64,
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

/// 某阶段校验结果的种类
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StageKind {
    TableReadBack,
    ToolExit,
    KernelResync,
    TransferCursor,
}

impl StageKind {
    fn tag(self) -> u8 {
        match self {
            Self::TableReadBack => 1,
            Self::ToolExit => 2,
            Self::KernelResync => 3,
            Self::TransferCursor => 4,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::TableReadBack),
            2 => Some(Self::ToolExit),
            3 => Some(Self::KernelResync),
            4 => Some(Self::TransferCursor),
            _ => None,
        }
    }
}

/// 某阶段的校验事实
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StageFact {
    pub(crate) kind: StageKind,
    pub(crate) ok: bool,
    pub(crate) detail: String,
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
    /// 搬移作业的可恢复状态。只有 [`OpKind::Resize`] 可能非 `None`；非搬移的
    /// resize 记录保持为空
    pub(crate) relocation: Option<Relocation>,
    pub(crate) stages: Vec<StageFact>,
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
            stages: Vec::new(),
            action: RecoveryAction::InspectOnly,
            commit: CommitState::Prepared,
            retention: Retention::Active,
            disk_guid,
            restore_blocked: false,
            restore: Vec::new(),
            source: None,
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
        out.extend((u16::try_from(self.stages.len()).expect("stage count fits in 16 bits")).to_le_bytes());
        for stage in &self.stages {
            out.push(stage.kind.tag());
            out.push(u8::from(stage.ok));
            let detail = stage.detail.as_bytes();
            out.extend((u32::try_from(detail.len()).expect("stage detail fits in 32 bits")).to_le_bytes());
            out.extend_from_slice(detail);
        }
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
        // 源区间段在载荷最末，段的存在由操作类型决定：`copy` 必带，其余操作没有
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
    // 源区间段只属于 `copy`，且是它必带的末段。段按 `op` 直接解析：`copy` 缺段与
    // 其余操作多出字节都说明载荷与操作类型不符
    if record.op == OpKind::Copy {
        record.source = Some(read_source(&mut c)?);
    }
    if !c.is_empty() {
        return None;
    }
    Some(record)
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
    // 搬移计划只属于 `move` / `resize`；别的操作带它即与操作类型矛盾
    if relocation.is_some() && !matches!(op, OpKind::Resize | OpKind::Move) {
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
    let stage_count = c.u16()? as usize;
    let mut stages = Vec::with_capacity(stage_count.min(1024));
    for _ in 0..stage_count {
        let kind = StageKind::from_tag(c.u8()?)?;
        let ok = match c.u8()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let len = c.u32()? as usize;
        let detail = String::from_utf8(c.take(len)?.to_vec()).ok()?;
        stages.push(StageFact { kind, ok, detail });
    }
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
        let delta = c.u64()?;
        let is_swap = match c.u8()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        if partition == 0 || len == 0 {
            return None;
        }
        // 源区间与目标区间都要能算出端点，且目标起点不越过源起点（位移只减不增的
        // 搬移计划不会产出负位移）
        src_first.checked_add(len)?;
        let dst_first = src_first.checked_add(delta)?;
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
    if grow_part == 0 || entries.iter().any(|e| e.partition == grow_part) {
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
}

#[cfg(test)]
mod digest_golden {
    use super::{OpKind, PlanDigest};

    /// 请求摘要向量：操作码、数值与带长度前缀的字节串的编码一旦变化，这里先红。
    /// 向量按当前码值表计算（New = 1，Resize = 10）；码值或编码变更时重新生成。
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
