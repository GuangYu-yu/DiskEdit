//! 计划步骤的单步实现：搬移、复制、表提交、换区重建、文件系统扩缩与属性改写。
//!
//! 步骤的驱动、预扫描与放置分派在 [`crate::exec`]；本模块只提供每一步的写入实现。
//! 计划步骤之外的补写落在各自调用点，不写分区表项：
//! - `cmd/recover` 的回退方向回放；
//! - 已提交态续跑时的 [`fix_ntfs_hidden_sectors`]（经 [`crate::exec`] 的入口调用）；
//! - mkfs 之前的 `fsops::wipe_zero`；
//! - LVM 收尾链与换区签名重建。
//!
//! 每个步骤要么完成、要么给出"后置条件未达成"（由驱动层留下待办），要么按
//! "首写之前 / 之后"分类失败。预扫描在第一个目标字节写入之前跑完：执行器缺席、
//! 未装配的表步骤、不合法的搬移形状都在那里拒绝。

pub(crate) mod copy;
pub(crate) mod fs;
pub(crate) mod mkfs;
pub(crate) mod relocate;
pub(crate) mod set;
pub(crate) mod swap;
pub(crate) mod table;

use self::copy::CopyPartition;
use self::fs::FsResize;
use self::mkfs::FsCreate;
use self::relocate::Relocate;
use self::set::FsSet;
use self::swap::SwapRebuild;
use crate::dev::WriteSource;
use crate::outcome::RunOutcome;
use crate::planner::chunk::move_chunk_layout;
use crate::planner::{ConfirmedPlan, PlanStep};
use crate::recovery::Relocation;
use crate::table::TableStep;
use std::io;

/// 计划步骤里一段字节区间的归属：拒绝措辞的前缀。`Sole` 是计划中只有一段区间的
/// 步骤（`FsResize`、`FsCreate`）；复制步骤有源、目标两段。
#[derive(Clone, Copy)]
enum RangeRole {
    Sole,
    Source,
    Target,
}

impl RangeRole {
    fn prefix(self) -> &'static str {
        match self {
            Self::Sole => "",
            Self::Source => "source ",
            Self::Target => "target ",
        }
    }
}

/// 一段扇区数的字节长度。溢出说明计划没有经过装配：`FsResize`、`FsCreate` 与
/// `CopyPartition` 的几何字段相同的部分都从这里换算，措辞只在这里有一份。
fn region_bytes(part: u32, sectors: u64, sector_size: u64) -> Result<u64, String> {
    sectors.checked_mul(sector_size).ok_or_else(|| {
        format!("partition {part}: {sectors} sectors of {sector_size} bytes overflow the address space")
    })
}

/// 一段区间的起始字节：起点乘法与「起点 + 长度」都不允许溢出。`len` 由
/// [`region_bytes`] 算出，调用方在同一份长度上补自己的额外判据（目标字节数一致、
/// 盘内边界与重叠）。
fn region_at(
    part: u32,
    role: RangeRole,
    first_lba: u64,
    sector_size: u64,
    len: u64,
) -> Result<u64, String> {
    let base = first_lba.checked_mul(sector_size).ok_or_else(|| {
        format!(
            "partition {part}: {}start LBA {first_lba} overflows the address space",
            role.prefix()
        )
    })?;
    if base.checked_add(len).is_none() {
        return Err(format!(
            "partition {part}: {}byte range {base}+{len} overflows the address space",
            role.prefix()
        ));
    }
    Ok(base)
}

/// 搬移进度的内存载体：条目内块位置、全局块游标与已完成步骤数都从这里读写。
///
/// 续跑时由恢复记录注入（见 [`journal_state`]），执行过程中只前进。字段私有：读取只有
/// 两个用途——推进游标，以及把 durable 进度交给记录的 sink。判定层（作业是否完成）
/// 只看记录里的阶段，不读这里。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TransferState {
    entry_index: u32,
    chunks_in_entry: u64,
    completed_steps: u32,
    /// 全局累计块数，跨条目只增不减
    transfer_cursor: u64,
    /// 本条目已落盘目的前缀的执行顺序摘要；条目内块数归零时同步归零
    prefix_digest: [u8; 32],
}

impl TransferState {
    /// 由记录里的 durable 快照还原：续跑的起点，或载体步骤从宿主收到的那一份
    pub(crate) fn restore(
        entry_index: u32,
        chunks_in_entry: u64,
        completed_steps: u32,
        transfer_cursor: u64,
        prefix_digest: [u8; 32],
    ) -> Self {
        Self { entry_index, chunks_in_entry, completed_steps, transfer_cursor, prefix_digest }
    }

    /// 本条目已搬的块数：搬移序列从它之后继续
    fn chunks_done(&self) -> u64 {
        self.chunks_in_entry
    }

    /// 全局累计块数，供测试注入点定位
    fn cursor(&self) -> u64 {
        self.transfer_cursor
    }

    /// 一块搬完：条目内计数与全局游标同时前进
    fn advance_chunk(&mut self) {
        self.chunks_in_entry += 1; // audit-allow: counter
        self.transfer_cursor += 1; // audit-allow: counter
    }

    /// 一条目完成：条目游标前进、条目内块数归零、步数加一。
    ///
    /// 入参是 [`crate::exec::StepApplied`]：没有"这一步算数"的凭据就没有推进入口，
    /// 因此不存在"结论不是 `Applied` 却已经推进了游标"的次序
    pub(crate) fn advance_entry(&mut self, _: crate::exec::StepApplied) {
        self.entry_index += 1; // audit-allow: counter
        self.chunks_in_entry = 0;
        self.prefix_digest = [0u8; 32];
        self.completed_steps += 1; // audit-allow: counter
    }

    /// 已落盘前缀的执行顺序摘要；记录落盘与续跑比对共用这一份
    pub(crate) fn prefix_digest(&self) -> [u8; 32] {
        self.prefix_digest
    }

    fn set_prefix_digest(&mut self, digest: [u8; 32]) {
        self.prefix_digest = digest;
    }

    /// 一个不搬数据的步骤完成。同样要求 [`crate::exec::StepApplied`]：推进的理由必须
    /// 是"这一步算数"，不是一个布尔参数
    pub(crate) fn note_step(&mut self, _: crate::exec::StepApplied) {
        self.completed_steps += 1; // audit-allow: counter
    }

    /// 落盘用的 durable 快照：(条目游标, 条目内块数, 已完成步骤数, 全局块数)
    pub(crate) fn durable(&self) -> (u32, u64, u32, u64) {
        (self.entry_index, self.chunks_in_entry, self.completed_steps, self.transfer_cursor)
    }
}

/// 执行过程中的持久化回调。
///
/// 进度落盘的整份替换由 sink 实现方完成：[`crate::exec::run_plan`] 只在约定的时点调用
/// [`Self::transfer_checkpoint`]、[`Self::table_step_done`] 与
/// [`Self::fs_step_done`]，自身不写记录。
///
/// 实现方在方法内部用 `src.record_journal_mut()` 短借用记录并立即归还，不得跨调用
/// 持有。[`Self::table_step_done`] 与 [`Self::fs_step_done`] 收到的状态里，条目游标
/// 已经是推进后的值，实现方只落盘，不重复加一。
pub(crate) trait StepSink {
    fn transfer_checkpoint(
        &mut self,
        src: &mut WriteSource,
        state: &TransferState,
    ) -> Result<(), RunOutcome>;

    /// 条目的表步开始前的落盘时点：表步一旦写出，记录里必须已经有"这条的表可能已写"
    /// 的事实。返回错误时表步不得开始
    fn entry_table_started(
        &mut self,
        _src: &mut WriteSource,
        _entry: u32,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }

    /// 不带搬移条目的表步开始前的落盘时点：表步一旦写出，记录里必须已经有"表可能已写"
    /// 的事实。返回错误时表步不得开始
    fn table_step_started(&mut self, _src: &mut WriteSource) -> Result<(), RunOutcome> {
        Ok(())
    }

    fn table_step_done(
        &mut self,
        src: &mut WriteSource,
        step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome>;

    /// 文件系统步骤（`FsResize`/`FsCreate`/`FsSet`）完成后的落盘时点：
    /// 工具步骤在退出后再替换一次记录。进入工具之前的落盘由屏障承担
    fn fs_step_done(
        &mut self,
        src: &mut WriteSource,
        step: usize,
        state: &TransferState,
    ) -> Result<(), RunOutcome>;

    /// 过程提示：逐条目落位、块布局降级这类"用户该看到但不改变结论"的信息。
    /// 输出通道归实现方——命令层的日志格式（文本 / JSON）不能由执行器决定
    fn note(&mut self, _msg: &str) {}
}

/// 不落进度的 sink：没有搬移计划的步骤不需要记录任何游标。
pub(crate) struct NoProgress;

impl StepSink for NoProgress {
    fn transfer_checkpoint(
        &mut self,
        _src: &mut WriteSource,
        _state: &TransferState,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }

    fn table_step_done(
        &mut self,
        _src: &mut WriteSource,
        _step: usize,
        _state: &TransferState,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }

    fn fs_step_done(
        &mut self,
        _src: &mut WriteSource,
        _step: usize,
        _state: &TransferState,
    ) -> Result<(), RunOutcome> {
        Ok(())
    }
}

/// 一份计划的执行结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepFlow {
    /// 全部步骤执行完毕：命令级收尾（扩容、FS、LVM）可以继续
    Complete,
    /// 计划在某一步之后停下：那一步留下了待办，后续步骤没有执行。
    /// 记录必须保留——重跑从停下那一处继续
    Stopped,
}

/// 预扫描后的执行单元：`Transfer` 是「搬数据 + 提交表」的一个条目组，`Copy` 是
/// 「复制数据 + 提交副本条目」的一对，`TableOnly` 是不带搬移的表步骤，`FsResize` 是
/// 写表之后的文件系统步骤，`FsCreate` 是在分区上建立文件系统，`FsSet` 是改写文件系统
/// 内部属性。
pub(crate) enum PreparedStep<'a> {
    /// 一条搬移 + 提交它的表步骤；换区条目在表提交之后多一步重建（`swap`）
    Transfer { reloc: Relocate, commit: &'a TableStep, swap: Option<SwapRebuild> },
    Copy { copy: CopyPartition, commit: &'a TableStep },
    TableOnly(&'a TableStep),
    FsResize(FsResize),
    FsCreate(FsCreate),
    FsSet(FsSet),
}

/// 首次目标写入之前的只读核对：计划里第一条表步骤的比对区间必须与盘上表区逐段相符。
///
/// 多表步骤的计划按前后表戳串成一条链（装配期已核对），只有第一条的写前区间对应
/// 盘上现状；核对它即核对了整张表。返回 3：表在计划读出之后被外部改动，
/// 属于"请求与现状不符"，此时本次运行还没有写下任何字节。
pub(crate) fn verify_tables_before_write(src: &WriteSource, steps: &[PreparedStep<'_>]) -> Result<(), RunOutcome> {
    let first = steps.iter().find_map(|step| match step {
        PreparedStep::Transfer { commit, .. } | PreparedStep::Copy { commit, .. } => Some(*commit),
        PreparedStep::TableOnly(commit) => Some(*commit),
        PreparedStep::FsResize(_) | PreparedStep::FsCreate(_) | PreparedStep::FsSet(_) => None,
    });
    match first {
        Some(table) => crate::table::verify_regions_before(src, &table.expect_regions),
        None => Ok(()),
    }
}


pub(crate) fn ntfs_hidden_value(start_lba: u64) -> (u32, bool) {
    if start_lba >> 32 == 0 { (start_lba as u32, false) } else { (0u32, true) }
}

/// 定位 HiddenSectors 并读出它所在的扇区。偏移按调用方传入的 LBA 与扇区大小换算
/// （裸乘回绕会把字段写到盘上另一处）；签名不是 NTFS 引导扇区即拒绝——把
/// HiddenSectors 写进非引导扇区比不写更糟。"值是否已经正确"的判定与写入前的
/// 复核共用这一份读取
fn ntfs_hidden_field(src: &WriteSource, new_first_lba: u64, ss: u64) -> io::Result<(u64, Vec<u8>)> {
    let sector_off = new_first_lba
        .checked_mul(ss)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("NTFS HiddenSectors offset overflows ({new_first_lba} × {ss})"),
            )
        })?;
    let off = sector_off.checked_add(0x1C).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("NTFS HiddenSectors offset overflows ({new_first_lba} × {ss} + 0x1C)"),
        )
    })?;
    let mut probe = vec![0u8; ss as usize];
    src.read_at(sector_off, &mut probe)?;
    if probe.len() < 11 || probe[3..11] != *b"NTFS    " {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the first sector at LBA {new_first_lba} is not an NTFS boot sector; refusing to write HiddenSectors"),
        ));
    }
    Ok((off, probe))
}

/// 定位备份引导扇区的 HiddenSectors 并读出它所在的扇区。NTFS 在卷的**最后一个
/// 设备扇区**留一份引导扇区副本，HiddenSectors 与主扇区同义；只修主扇区会让
/// `ntfsfix -n`（逐字节比对主备）判定备份损坏。位置由分区尾扇区决定，不读 BPB：
/// 分区长度在搬移中不变，ntfsfix 与 Windows 都按分区/设备长度定位这一备份
fn ntfs_backup_field(
    src: &WriteSource,
    new_first_lba: u64,
    ss: u64,
    part_sectors: u64,
) -> io::Result<(u64, Vec<u8>)> {
    let last_lba = new_first_lba
        .checked_add(part_sectors)
        .and_then(|n| n.checked_sub(1))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("NTFS backup boot sector LBA overflows ({new_first_lba} + {part_sectors} - 1)"),
            )
        })?;
    let sector_off = last_lba.checked_mul(ss).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("NTFS backup boot sector offset overflows ({last_lba} × {ss})"),
        )
    })?;
    let off = sector_off.checked_add(0x1C).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("NTFS backup HiddenSectors offset overflows ({last_lba} × {ss} + 0x1C)"),
        )
    })?;
    let mut probe = vec![0u8; ss as usize];
    src.read_at(sector_off, &mut probe)?;
    if probe.len() < 11 || probe[3..11] != *b"NTFS    " {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the backup boot sector at LBA {last_lba} is not an NTFS boot sector; refusing to write HiddenSectors"),
        ));
    }
    Ok((off, probe))
}

fn hidden_sectors_at(probe: &[u8], new_first_lba: u64) -> io::Result<&[u8]> {
    probe.get(0x1C..0x20).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the boot sector at LBA {new_first_lba} is shorter than the HiddenSectors field"),
        )
    })
}

/// 盘上的 HiddenSectors 是否已是 `new_first_lba` 对应的值。起点未变的续跑据此决定
/// 跳过修正：只有主、备两份引导扇区都读出来确实相等才可跳过，任一处不等说明字段
/// 没跟上分区（例如分区被别的工具搬过，或只修了主扇区），仍要照修
pub(crate) fn ntfs_hidden_sectors_match(
    src: &WriteSource,
    new_first_lba: u64,
    ss: u64,
    part_sectors: u64,
) -> io::Result<bool> {
    let (_, probe) = ntfs_hidden_field(src, new_first_lba, ss)?;
    let (_, backup) = ntfs_backup_field(src, new_first_lba, ss, part_sectors)?;
    Ok(hidden_sector_matches(&probe, new_first_lba)?
        && hidden_sector_matches(&backup, new_first_lba)?)
}

/// 纯判定：给定引导扇区内容，盘上的 HiddenSectors 是否已等于期望值
fn hidden_sector_matches(probe: &[u8], new_first_lba: u64) -> io::Result<bool> {
    let current = hidden_sectors_at(probe, new_first_lba)?;
    let (val, _) = ntfs_hidden_value(new_first_lba);
    Ok(current == val.to_le_bytes().as_slice())
}

/// NTFS boot sector BPB HiddenSectors（偏移 0x1C，u32 LE，值 = 分区起始 LBA；
/// NTFS 引导扇区布局，ntfs-3g bootsect.c 与微软 NTFS 规范同此定义）。
/// 单位是设备逻辑扇区（4Kn 原生盘上即 4Kn LBA，不折算 512）：bytes_per_sector 记录
/// 各自的扇区大小，hidden_sectors 语义随它计。字段只有 32 位：start ≥ 2^32 时写 0 并
/// 警告（Windows 将无法从该分区引导，数据不受影响）
pub(crate) fn fix_ntfs_hidden_sectors(
    src: &mut WriteSource,
    new_first_lba: u64,
    ss: u64,
    part_sectors: u64,
    part: u32,
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    let (val, warn) = ntfs_hidden_value(new_first_lba);
    let (off, probe) = ntfs_hidden_field(src, new_first_lba, ss)?;
    // 备份扇区先定位再写：任一份不是 NTFS 引导扇区都在落盘前拒绝，不留半修现场
    let (backup_off, backup_probe) = ntfs_backup_field(src, new_first_lba, ss, part_sectors)?;
    // 写前比对与写入都在写入口内完成：比较区间就是这 4 个字节，与探测读到的原文一致
    // 才写；不一致说明扇区在这中间变了，拒绝覆盖
    let current = hidden_sectors_at(&probe, new_first_lba)?;
    src.write_guarded(
        off,
        &val.to_le_bytes(),
        crate::dev::RegionStamp::of_bytes(off, current),
        crate::outcome::WriteStep::Fs { part },
    )
    .map_err(crate::dev::WriteConflict::into_io_error)?;
    // 备份引导扇区同步修正：ntfsfix 等工具逐字节比对主备，只改主扇区会被判损坏
    let backup_current = hidden_sectors_at(&backup_probe, new_first_lba)?;
    src.write_guarded(
        backup_off,
        &val.to_le_bytes(),
        crate::dev::RegionStamp::of_bytes(backup_off, backup_current),
        crate::outcome::WriteStep::Fs { part },
    )
    .map_err(crate::dev::WriteConflict::into_io_error)?;
    src.sync_data()?;
    if warn {
        log("warning: partition starts beyond 2^32; HiddenSectors zeroed — Windows cannot boot from it");
    }
    Ok(())
}

/// 计划步骤的 HiddenSectors 修正：自己识别内容类型，位移为 0 的条目没有可修的东西
/// （数据没动，位置也没动）。
///
/// 与命令层的 [`crate::exec::backfill_hidden_sectors`] 分工明确：这里走计划、自己做
/// 判定；那一侧由调用方先判定"已提交、起点变了"，只补一次。写入实现两者共用
pub(crate) fn step_hidden_sectors(
    src: &mut WriteSource,
    reloc: &Relocate,
    sector_size: u64,
    sink: &mut dyn StepSink,
) -> Result<(), RunOutcome> {
    if reloc.dst_first == reloc.src_first {
        return Ok(());
    }
    let overflow = || {
        RunOutcome::partial(format!(
            "partition {}: the relocated range overflows the address space — the transfer may be incomplete",
            reloc.part
        ))
    };
    let dst_off = reloc.dst_first.checked_mul(sector_size).ok_or_else(overflow)?;
    let total = reloc.sectors.checked_mul(sector_size).ok_or_else(overflow)?;
    let kind = crate::fsid::identify(src, dst_off, total)
        .map_err(|e| RunOutcome::partial(e.to_string()))?;
    if kind != crate::domain::ContentKind::Filesystem(crate::fsid::FsKind::Ntfs) {
        return Ok(());
    }
    crate::movepart::fault_before_hidden_fix();
    let mut emit = |m: &str| sink.note(m);
    fix_ntfs_hidden_sectors(src, reloc.dst_first, sector_size, reloc.sectors, reloc.part, &mut emit)
        .map_err(|e| RunOutcome::partial(e.to_string()))
}

/// 首次写入前的全量扫描：类型、装配完整性、表写入范围、计划形状、swap、块参数、
/// 块布局与地址区间，以及记录里的搬移计划与本次计划是否一致。
pub(crate) fn prepare_steps<'a>(
    src: &WriteSource,
    plan: &'a ConfirmedPlan,
    chunk_bytes: Option<u64>,
    sector_size: u64,
    record: Option<&Relocation>,
) -> Result<Vec<PreparedStep<'a>>, RunOutcome> {
    let raw = plan.plan().steps();
    let mut has_data = false;
    for (index, step) in raw.iter().enumerate() {
        match step {
            PlanStep::WriteTable(table) => {
                if table.expect_before.is_none() || table.expect_after.is_none() {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} writes the table but carries no expected stamp — the plan was not assembled",
                        index + 1 // audit-allow: index-in-bounds
                    )));
                }
                // 写前比对要求写入段落在比对区间内且长度一致；放到执行期才判，
                // 前面的条目可能已经写过目标字节
                let aligned = table.writes.iter().all(|w| {
                    w.bytes.len() as u64 == w.region.len && table.regions.contains(&w.region)
                });
                if !aligned {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} writes table bytes outside its comparison regions",
                        index + 1 // audit-allow: index-in-bounds
                    )));
                }
            }
            PlanStep::Relocate { src_first, dst_first, sectors, is_swap, .. } => {
                // 与记录侧同一判据（`relocation_copies`）：换区、零位移都不搬数据，
                // 那两类条目在记录里也没有块进度
                has_data |= crate::planner::chunk::relocation_copies(
                    *is_swap,
                    i64::from(*dst_first != *src_first),
                    *sectors,
                );
                if *sectors == 0 {
                    return Err(RunOutcome::internal(format!(
                        "plan step {} relocates zero sectors",
                        index + 1 // audit-allow: index-in-bounds
                    )));
                }
                if !matches!(raw.get(index + 1), Some(PlanStep::WriteTable(_))) { // audit-allow: index-in-bounds
                    return Err(RunOutcome::internal(format!(
                        "plan step {} relocates data but is not followed by the table step that commits it",
                        index + 1 // audit-allow: index-in-bounds
                    )));
                }
            }
            // 复制步骤与搬移步骤同形：数据在前、提交条目的表步骤紧随其后。载荷自洽与
            // 地址区间都在首写前判定，重复的防御判定在执行期只能按"盘可能已改变"上报
            PlanStep::CopyPartition { part, src_first, dst_first, sectors } => {
                has_data = true;
                if !matches!(raw.get(index + 1), Some(PlanStep::WriteTable(_))) { // audit-allow: index-in-bounds
                    return Err(RunOutcome::internal(format!(
                        "plan step {} copies data but is not followed by the table step that commits it",
                        index + 1 // audit-allow: index-in-bounds
                    )));
                }
                copy::preflight(
                    src,
                    &CopyPartition {
                        part: *part,
                        src_first: *src_first,
                        dst_first: *dst_first,
                        sectors: *sectors,
                    },
                    sector_size,
                )
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?; // audit-allow: display-only
            }
            // FS 步骤的能力与区间都在首写前判定：缺工具若等到写表之后才暴露，
            // 就只能按"盘可能已改变"上报
            PlanStep::FsResize { part, first_lba, sectors, target_bytes, fstype } => {
                fs::preflight(
                    src,
                    &FsResize {
                        part: *part,
                        first_lba: *first_lba,
                        sectors: *sectors,
                        target_bytes: *target_bytes,
                        fstype: *fstype,
                    },
                    sector_size,
                )
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?; // audit-allow: display-only
            }
            // 建立步骤同理：区间自洽与类型可得都在擦除第一个字节之前判定
            PlanStep::FsCreate { part, first_lba, sectors, fstype, label } => {
                mkfs::preflight(
                    src,
                    &FsCreate {
                        part: *part,
                        first_lba: *first_lba,
                        sectors: *sectors,
                        fstype: *fstype,
                        label: label.clone(),
                    },
                    sector_size,
                )
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?; // audit-allow: display-only
            }
            // 换区重建不在这里判定：它紧跟在自己的表提交之后，能力与区间都由那一步保证。
            // 形状（前后必须是搬移与表提交）由计划的构造点校验
            PlanStep::RebuildSwap { .. } => {}
            // 属性步骤：载荷自洽（分区号、卷标容量、UUID 形态）在写盘之前判定
            PlanStep::FsSet { part, attr, fstype } => {
                set::preflight(&FsSet {
                    part: *part,
                    attr: attr.clone(),
                    fstype: *fstype,
                })
                .map_err(|e| e.context(&format!("plan step {}", index + 1)))?; // audit-allow: display-only
            }
        }
    }

    let chunk = if has_data {
        let Some(chunk) = chunk_bytes else {
            return Err(RunOutcome::refused(
                "this plan relocates data but no chunk size was given",
            ));
        };
        const MIN_CHUNK: u64 = crate::support::MIB_BYTES;
        const MAX_CHUNK: u64 = 64 * 1024 * 1024; // audit-allow: const-expr
        if !(MIN_CHUNK..=MAX_CHUNK).contains(&chunk) || chunk % sector_size != 0 {
            return Err(RunOutcome::refused(format!(
                "chunk size {chunk} bytes must be within 1 MiB..=64 MiB and a multiple of the {sector_size}-byte sector"
            )));
        }
        Some(chunk)
    } else {
        None
    };

    // 记录与本次计划的搬移形状必须一致：按别人的条目数或块大小续跑会把块进度落到错误的位置。
    // 只有带搬移步骤的计划才是记录里那份作业本身；repair 一类的表子计划在记录绑定之后执行，
    // 它没有块布局，块进度也就不属于它——拿它和记录的条目数比较只会把"还没开始搬"误判成"搬丢了"
    let planned = raw
        .iter()
        .filter(|s| matches!(s, PlanStep::Relocate { .. }))
        .count();
    if let Some(reloc) = record.filter(|_| planned > 0) {
        let chunk_matches = match chunk {
            Some(chunk) => chunk == reloc.chunk_bytes,
            None => true,
        };
        if reloc.entries.len() != planned || !chunk_matches {
            return Err(RunOutcome::refused(format!(
                "the recovery record holds {} relocation entries of {}-byte chunks, this plan holds {planned} — \
                 the record and the requested transfer describe different jobs",
                reloc.entries.len(),
                reloc.chunk_bytes
            )));
        }
        // 布局等式的块数比较要求两处扇区大小相同：同一 LBA 长度在不同扇区下换算出不同的块数
        if reloc.ss != sector_size {
            return Err(RunOutcome::refused(format!(
                "the recovery record was written with a {}-byte sector, this target has {sector_size} — \
                 the record and the requested transfer describe different jobs",
                reloc.ss
            )));
        }
    }

    let mut steps = Vec::with_capacity(raw.len());
    let mut reloc_index = 0u32;
    let mut index = 0;
    while index < raw.len() {
        match &raw[index] {
            PlanStep::Relocate { part, src_first, dst_first, sectors, is_swap } => {
                // 块布局在此算一次并随步骤交出：执行循环与记录游标都读这份结果，
                // 不按同一条几何各自重算
                let shift = dst_first.abs_diff(*src_first);
                // 与记录侧同一判据（`relocation_copies`，delta 只用于判零）
                let copies = crate::planner::chunk::relocation_copies(
                    *is_swap,
                    i64::from(shift != 0),
                    *sectors,
                );
                let (chunk_bytes, chunk, chunks) = if !copies {
                    // 不拷数据的条目（换区、零位移）没有块布局，块进度恒为 0（记录里也是 0）
                    (chunk.unwrap_or(0), 0, 0)
                } else {
                    let requested = chunk.expect("a moving entry needs a chunk size");
                    let Some((chunk, count)) =
                        move_chunk_layout(*sectors, sector_size, shift, *sectors, requested)
                    else {
                        return Err(RunOutcome::internal(format!(
                            "plan step {} has no usable relocation block layout",
                            index + 1 // audit-allow: index-in-bounds
                        )));
                    };
                    // 地址区间的溢出判定与布局同源：执行期再遇到时前面的条目可能已写过
                    let src_base = src_first.checked_mul(sector_size).ok_or_else(|| {
                        RunOutcome::internal(format!(
                            "plan step {}: the source offset of partition {part} overflows the address space",
                            index + 1 // audit-allow: index-in-bounds
                        ))
                    })?;
                    let dst_base = dst_first.checked_mul(sector_size).ok_or_else(|| {
                        RunOutcome::internal(format!(
                            "plan step {}: the target offset of partition {part} overflows the address space",
                            index + 1 // audit-allow: index-in-bounds
                        ))
                    })?;
                    let total = sectors.checked_mul(sector_size).ok_or_else(|| {
                        RunOutcome::internal(format!(
                            "plan step {}: the relocation length of partition {part} overflows the address space",
                            index + 1 // audit-allow: index-in-bounds
                        ))
                    })?;
                    if src_base.checked_add(total).is_none() || dst_base.checked_add(total).is_none() {
                        return Err(RunOutcome::internal(format!(
                            "plan step {}: the relocation byte range of partition {part} overflows the address space",
                            index + 1 // audit-allow: index-in-bounds
                        )));
                    }
                    // 记录里当前条目已完成的块数不能超过该条目的块数
                    if let Some(reloc) = record
                        && reloc.entry_index == reloc_index
                        && reloc.chunks_in_entry > count
                    {
                        return Err(RunOutcome::internal(format!(
                            "plan step {}: {} chunks are recorded as done but the step only has {count}",
                            index + 1, // audit-allow: index-in-bounds
                            reloc.chunks_in_entry
                        )));
                    }
                    (requested, chunk, count)
                };
                reloc_index += 1; // audit-allow: counter
                // 形状校验在装配层，但计划是外部输入：越界必须是错误而不是 panic
                let Some(PlanStep::WriteTable(commit)) = raw.get(index + 1) else { // audit-allow: index-in-bounds
                    return Err(RunOutcome::internal(
                        "the relocation step is not followed by a table step",
                    ));
                };
                // 换区条目在表提交之后多一步重建；两处对"这是不是换区"必须同答，
                // 否则表会落位而内容不重建（或反之）
                let swap = match raw.get(index + 2) { // audit-allow: index-in-bounds
                    Some(PlanStep::RebuildSwap { part: p, src_first: s0, dst_first: d0, sectors: n }) => {
                        Some(SwapRebuild { part: *p, src_first: *s0, dst_first: *d0, sectors: *n })
                    }
                    _ => None,
                };
                if swap.is_some() != *is_swap {
                    return Err(RunOutcome::internal(format!(
                        "plan step {}: the swap-rebuild step and the relocation entry disagree on the partition type",
                        index + 1 // audit-allow: index-in-bounds
                    )));
                }
                index += 2 + usize::from(swap.is_some()); // audit-allow: counter
                steps.push(PreparedStep::Transfer {
                    reloc: Relocate {
                        part: *part,
                        src_first: *src_first,
                        dst_first: *dst_first,
                        sectors: *sectors,
                        chunk_bytes,
                        chunk,
                        chunks,
                    },
                    commit,
                    swap,
                });
            }
            PlanStep::RebuildSwap { .. } => {
                return Err(RunOutcome::internal(
                    "a swap-rebuild step is not attached to the relocation entry that owns it",
                ));
            }
            PlanStep::CopyPartition { part, src_first, dst_first, sectors } => {
                let Some(PlanStep::WriteTable(commit)) = raw.get(index + 1) else { // audit-allow: index-in-bounds
                    return Err(RunOutcome::internal(
                        "the copy step is not followed by a table step",
                    ));
                };
                steps.push(PreparedStep::Copy {
                    copy: CopyPartition {
                        part: *part,
                        src_first: *src_first,
                        dst_first: *dst_first,
                        sectors: *sectors,
                    },
                    commit,
                });
                index += 2; // audit-allow: counter
            }
            PlanStep::WriteTable(commit) => {
                steps.push(PreparedStep::TableOnly(commit));
                index += 1; // audit-allow: counter
            }
            PlanStep::FsResize { part, first_lba, sectors, target_bytes, fstype } => {
                steps.push(PreparedStep::FsResize(FsResize {
                    part: *part,
                    first_lba: *first_lba,
                    sectors: *sectors,
                    target_bytes: *target_bytes,
                    fstype: *fstype,
                }));
                index += 1; // audit-allow: counter
            }
            PlanStep::FsCreate { part, first_lba, sectors, fstype, label } => {
                steps.push(PreparedStep::FsCreate(FsCreate {
                    part: *part,
                    first_lba: *first_lba,
                    sectors: *sectors,
                    fstype: *fstype,
                    label: label.clone(),
                }));
                index += 1; // audit-allow: counter
            }
            PlanStep::FsSet { part, attr, fstype } => {
                steps.push(PreparedStep::FsSet(FsSet {
                    part: *part,
                    attr: attr.clone(),
                    fstype: *fstype,
                }));
                index += 1; // audit-allow: counter
            }
        }
    }
    Ok(steps)
}

/// 执行状态从记录纳入：游标跨运行累计，续跑必须从已落盘的值继续。
///
/// 记录里存在搬移计划时，全局块游标由步骤上的块布局累计：已提交条目的块数之和加
/// 当前条目已完成的块数。块数取自 [`prepare_steps`] 的布局结果，不按记录重新计算。
/// 记录里的游标只能落后于这个累计值——块写入与游标推进是两次落盘——领先即视为
/// 记录自相矛盾并拒绝
pub(crate) fn journal_state(src: &WriteSource, steps: &[PreparedStep]) -> Result<TransferState, RunOutcome> {    let Some(journal) = src.record_journal() else {
        return Ok(TransferState::default());
    };
    let progress = journal.progress();
    let mut state = TransferState {
        entry_index: 0,
        chunks_in_entry: 0,
        completed_steps: progress.completed_steps,
        transfer_cursor: progress.transfer_cursor,
        prefix_digest: [0u8; 32],
    };
    let Some(reloc) = journal.relocation() else {
        return Ok(state);
    };
    state.entry_index = reloc.entry_index;
    state.chunks_in_entry = reloc.chunks_in_entry;
    state.prefix_digest = reloc.prefix_digest;
    let transfers = steps
        .iter()
        .filter(|s| matches!(s, PreparedStep::Transfer { .. }))
        .count();
    let mut recomputed = 0u64;
    let counts = steps.iter().filter_map(|s| match s {
        PreparedStep::Transfer { reloc, .. } => Some(reloc.chunks),
        _ => None,
    });
    for count in counts.take(reloc.entry_index as usize) {
        recomputed = recomputed.checked_add(count).ok_or_else(|| {
            RunOutcome::internal(
                "the recorded move cursor overflows the address space",
            )
        })?;
    }
    recomputed = recomputed.checked_add(reloc.chunks_in_entry).ok_or_else(|| {
        RunOutcome::internal(
            "the recorded move cursor overflows the address space",
        )
    })?;
    // 全局块游标只是"已落盘进度"的滞后表示：块写入与游标推进是两次落盘，游标可以
    // 落后，但不可能领先于条目游标能解释的块数。本计划没有搬移步骤（表已提交后的
    // FS 收尾）时这条分解不成立；有搬移步骤则条目表必然非空（条目就是一串 Transfer）
    if transfers > 0 && state.transfer_cursor > recomputed {
        return Err(RunOutcome::refused(format!(
            "the recovery record's transfer cursor ({}) exceeds the progress its entry cursors account for \
             ({recomputed}) — the record contradicts itself",
            state.transfer_cursor
        )));
    }
    state.transfer_cursor = state.transfer_cursor.max(recomputed);
    Ok(state)
}

/// `resize` 内部使用的块字节数：8 MiB。与用户可见的 `--chunk` 默认值
/// （[`crate::support::DEFAULT_CHUNK_BYTES`]，4 MiB）是两件事，故不共用一个名字：
/// 这个值同时是恢复记录的步长下限——`Transfer` 每前进 `max(ChunkSize, 8 MiB)`
/// 替换一次记录并只更新 `cursor`，而 `resize` 不登记 `--chunk`，恢复粒度取该下限
pub(crate) const RESIZE_CHUNK_BYTES: u64 = 8 * crate::support::MIB_BYTES; // audit-allow: const-expr

#[cfg(test)]
mod tests {
    use super::*;

    /// 512 字节 NTFS 引导扇区：签名在 3..11，HiddenSectors 在 0x1C
    fn boot_sector(hidden: u32) -> Vec<u8> {
        let mut sector = vec![0u8; 512];
        sector[3..11].copy_from_slice(b"NTFS    ");
        sector[0x1C..0x20].copy_from_slice(&hidden.to_le_bytes());
        sector
    }

    /// 两扇区的 NTFS 卷：主引导扇区 + 卷尾的备份扇区。BPB 里写明
    /// bytes_per_sector=512、number_of_sectors=1（卷扇区数减一，mkntfs 同此），
    /// 备份位置由分区长度而非 BPB 决定，这里只求结构合法
    fn boot_volume(hidden: u32) -> Vec<u8> {
        let mut first = boot_sector(hidden);
        first[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes());
        first[0x28..0x30].copy_from_slice(&1u64.to_le_bytes());
        let mut vol = first.clone();
        vol.extend_from_slice(&first);
        vol
    }

    /// 跳过与照修的分岔：只有主备两份都确实等于期望值才跳过
    #[test]
    fn hidden_sectors_match_decides_skip_and_fix() {
        let ok = crate::support::src_from("ntfs_volume", &boot_volume(0));
        assert!(ntfs_hidden_sectors_match(&ok, 0, 512, 2).unwrap(), "主备都已正确时跳过");

        // 主扇区对、备份扇区不对：只修过主扇区的现场必须判不等，仍要照修
        let mut half = boot_volume(0);
        half[512 + 0x1C..512 + 0x20].copy_from_slice(&7u32.to_le_bytes());
        let half = crate::support::src_from("ntfs_half_fixed", &half);
        assert!(
            !ntfs_hidden_sectors_match(&half, 0, 512, 2).unwrap(),
            "备份扇区没跟上时必须判不等"
        );

        // 期望值超过 32 位时字段按 0 记：此时盘上的 0 也算对
        assert!(hidden_sector_matches(&boot_sector(0), 1 << 32).unwrap());
    }

    /// 字段缺失不是"值不对"，而是读不出。签名的判据不在这条纯判定里，见下一条用例
    #[test]
    fn hidden_sector_matches_refuses_a_sector_shorter_than_the_field() {
        assert!(hidden_sector_matches(&[0u8; 16], 0).is_err());
    }

    /// 非 NTFS 扇区不按 NTFS 的字段去比：签名判据在读取端 `ntfs_hidden_field`，
    /// 纯判定只比 `0x1C..0x20` 四字节。这里让字段值本身已经对上，拒绝仍必须发生
    #[test]
    fn a_non_ntfs_sector_is_refused_before_the_field_is_compared() {
        let mut sector = boot_sector(0);
        sector[3] = b'X';
        assert!(hidden_sector_matches(&sector, 0).unwrap(), "纯判定只比四字节，签名不归它管");
        let mut tampered = boot_volume(0);
        tampered[3] = b'X';
        let src = crate::support::src_from("ntfs_signature", &tampered);
        let err = ntfs_hidden_sectors_match(&src, 0, 512, 2).expect_err("签名不符必须拒绝");
        assert!(err.to_string().contains("not an NTFS boot sector"), "got {err}");

        // 同一份现场换成正确签名：路径通，读到的是同一个字段（排除夹具写错的假通过）
        let ok = crate::support::src_from("ntfs_signature_ok", &boot_volume(0));
        assert!(ntfs_hidden_sectors_match(&ok, 0, 512, 2).unwrap());
    }
}
