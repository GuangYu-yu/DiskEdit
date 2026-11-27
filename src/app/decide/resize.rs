//! `resize` 的判定：把分区（与其中的文件系统）扩到或缩到请求的终点。
//!
//! 判定层先按锁前现场算出新终点与方向，再决定是否需要文件系统步骤：
//! 扩容时先写表、后扩文件系统；收缩时先缩文件系统、后写表（`B13`）。
//! 表字节由纯核的编辑判据生成，执行层只消费由此得到的写段。

use diskedit_core::fs::{ContentKind, FsKind, Program, Support, Verb, catalog};
use diskedit_core::table::create::Segment;
use diskedit_core::table::edit::{self, GptTable};

use crate::app::observe::resize::ResizeSnapshot;
use crate::app::observe::table::TableView;
use crate::app::observe::{
    Command, FilesystemSnapshot, PartitionSelector, PriorState, Request, ResizeRequest, SizeSpec,
    TableFacts, TableFormat,
};
use crate::platform::Occupancy;

use super::filesystem as fsops;
use super::report::{Report, ResizePreview};
use super::step::{Step, StepKind, TableSegment};
use super::{Decision, Plan, Refusal};

/// 1 MiB 的字节数：百分比增量与 `--align mib` 的对齐单位。
const MIB: u64 = 1 << 20;

/// 搬移挡路分区时的默认块大小，与 `move` 命令一致。
const DEFAULT_CHUNK: u64 = 4 << 20;

/// 判定一次 `resize`。
pub fn decide(snapshot: &ResizeSnapshot, request: &Request) -> Decision {
    let Command::Resize(resize) = request.command() else {
        return refuse("resize reached with a different command");
    };
    if resize.fs_only {
        return refuse("resize --fs-only reached the partition-geometry path");
    }
    if matches!(snapshot.view, TableView::None) {
        return superfloppy(snapshot, resize);
    }
    if let Some(refusal) = online_precondition(snapshot) {
        return Decision::Refuse(refusal);
    }
    if let Some(decision) = pv_online(snapshot, resize) {
        return decision;
    }
    if snapshot.online.is_some() {
        return online(snapshot, resize);
    }
    if let Some(refusal) = admission(snapshot) {
        return Decision::Refuse(refusal);
    }
    let Some(partition) = &snapshot.selected else {
        return Decision::Refuse(match snapshot.selector {
            Some(PartitionSelector::Number(number)) => Refusal::PartitionMissing { number },
            Some(PartitionSelector::Last) => refuse_selector(),
            None => Refusal::TableMissing,
        });
    };
    let pv = partition.content == ContentKind::Lvm2Pv;
    if (resize.grow_lv || resize.lv.is_some()) && !pv {
        return Decision::Refuse(Refusal::TableEdit {
            why: format!(
                "--grow-lv/--lv needs an LVM2 PV — partition #{0} holds {1}",
                partition.number,
                partition.content.as_str()
            ),
        });
    }
    if pv && resize.no_fs {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--no-fs cannot resize an LVM2 PV — the PV has to be resized together with the \
                  partition; omit --no-fs"
                .to_owned(),
        });
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let sector = snapshot.target.sector_size;
    if sector == 0 {
        return Decision::Refuse(Refusal::GeometryUnknown {
            why: "the logical sector size is unknown".to_owned(),
        });
    }

    let (table, protective_mbr) = match editable(snapshot) {
        Ok(editable) => editable,
        Err(refusal) => return Decision::Refuse(refusal),
    };

    // MBR 主表没有可搬移的数据通道：`--allow-move` 与 `--chunk` 在 MBR 上无从执行，
    // 与基线一致地拒绝（退出码 3），而不是静默忽略后按普通扩缩继续。
    if matches!(table, Resizable::Mbr { .. }) {
        if resize.allow_move {
            return Decision::Refuse(Refusal::TableEdit {
                why: "--allow-move is not supported for MBR resize — MBR cannot relocate \
                      blocking partitions"
                    .to_owned(),
            });
        }
        if resize.chunk.is_some() {
            return Decision::Refuse(Refusal::TableEdit {
                why: "--chunk has no effect for MBR resize — MBR cannot relocate blocking \
                      partitions"
                    .to_owned(),
            });
        }
    }

    let number = partition.number;
    let start = partition.first_lba;
    let old_end = partition.last_lba;
    let current = partition.bytes;
    let last_usable = table.last_usable();
    let free = free_right(&table.used_ranges(), old_end, last_usable);

    // `--allow-move` 只在 GPT 上成立（MBR 已在上面拒绝）：让 `grow` 能吃掉右侧被
    // 邻接分区占住的空间，并把这些分区整体右移。
    let relocatable = matches!(table, Resizable::Gpt(_)) && resize.allow_move;
    // `grow --allow-move` 的天花板：右侧分区整体右移后仍要落在 last usable 之内，
    // 因此可扩的量 = 右侧空隙 + 盘尾余量。
    // 判定层禁止 `saturating_*`（见 tests/arithmetic.rs）：这里用显式 checked 减法，
    // 不允许 clippy 反向改回折中写法。
    #[allow(clippy::manual_saturating_arithmetic)]
    let grow_ceiling = if relocatable && resize.size.as_ref() == Some(&SizeSpec::Grow) {
        let tail = table
            .used_ranges()
            .iter()
            .map(|&(_, end)| end)
            .filter(|&end| end > old_end)
            .max()
            .map_or(last_usable, |end| last_usable.checked_sub(end).unwrap_or(0));
        Some(old_end + free + tail)
    } else {
        None
    };
    let new_end = match resolve_end(
        resize,
        EndContext {
            start,
            old_end,
            current,
            free,
            sector,
            last_usable,
            grow_ceiling,
        },
    ) {
        Ok(end) => end,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    if new_end < start {
        return Decision::Refuse(Refusal::TableEdit {
            why: "the requested end lands before the partition start".to_owned(),
        });
    }
    let grow = new_end > old_end;
    let shrink = new_end < old_end;

    // `grow` 请求无空间可扩不是"原样重跑"：顶到 last usable 与右侧被占是两种拒绝，
    // 退出码 3（基线 cmd/resize.rs:491、:500）。显式 SIZE/--end 等于现状才是空操作。
    if resize.size.as_ref() == Some(&SizeSpec::Grow) && !grow && !shrink {
        let why = if old_end >= last_usable {
            format!("partition already ends at last_usable_lba {last_usable} — nothing to grow")
        } else {
            "right side is occupied — pass --allow-move to relocate the blocking partitions \
             (plan will be printed; --yes confirms)"
                .to_owned()
        };
        return Decision::Refuse(Refusal::TableEdit { why });
    }

    // 右侧被挡时的搬移计划：把 `old_end` 之后的分区整体右移，腾出的空间恰好够
    // 目标终点。没有 `--allow-move` 时保持基线的拒绝。
    let mut moves: Vec<Relocation> = Vec::new();
    if grow && new_end - old_end > free {
        if !resize.allow_move {
            return Decision::Refuse(Refusal::TableEdit {
                why: "the right side is occupied by another partition — move it first, then rerun"
                    .to_owned(),
            });
        }
        let Resizable::Gpt(gpt) = &table else {
            return Decision::Refuse(Refusal::TableEdit {
                why: "--allow-move is not supported for MBR resize — MBR cannot relocate \
                      blocking partitions"
                    .to_owned(),
            });
        };
        let delta = (new_end - old_end) - free;
        match plan_relocations(gpt, old_end, delta, last_usable) {
            Ok(planned) => moves = planned,
            Err(refusal) => return Decision::Refuse(refusal),
        }
    } else if resize.allow_move && grow {
        // 右侧本来就空：`--allow-move` 没有可搬的对象，按基线拒绝而不是静默忽略。
        return Decision::Refuse(Refusal::TableEdit {
            why: "no partition to relocate (right side is free — no --allow-move needed)"
                .to_owned(),
        });
    }

    let Some(new_bytes) = (new_end - start + 1).checked_mul(sector) else {
        return Decision::Refuse(Refusal::TableEdit {
            why: "the new partition size overflows a 64-bit byte count".to_owned(),
        });
    };
    let geometry = FsGeometry {
        number,
        start_lba: start,
        old_bytes: current,
        new_bytes,
        sector,
    };
    // ext 收缩写前闸：最小尺寸在离线采集阶段由只读探针取到。
    // 目标小于最小值即拒绝，探针本身失败按"最小值未知"拒绝；两者都发生在任何
    // 写盘之前。基线在 layout/dry_run/MBR 三处各有一份，判据相同（R0211/R0382/R0416）。
    if shrink && !resize.no_fs && partition.content == ContentKind::Filesystem(FsKind::Ext) {
        match &snapshot.min_bytes {
            Some(Ok(minimum)) if geometry.new_bytes < *minimum => {
                return Decision::Refuse(Refusal::BelowMinimumSize {
                    number,
                    minimum: *minimum,
                    requested: geometry.new_bytes,
                });
            }
            Some(Err(why)) => {
                return Decision::Refuse(Refusal::MinimumSizeUnknown {
                    number,
                    why: why.clone(),
                });
            }
            Some(Ok(_)) | None => {}
        }
    }
    let lvm_step = if pv {
        match pv_chain_step(snapshot, resize, grow, shrink, &geometry) {
            Ok(step) => step,
            Err(refusal) => return Decision::Refuse(refusal),
        }
    } else {
        None
    };
    let filesystem_step = if pv {
        None
    } else {
        match filesystem_step(
            partition.content,
            resize.no_fs,
            grow,
            shrink,
            &geometry,
            &snapshot.tools,
        ) {
            Ok(step) => step,
            Err(refusal) => return Decision::Refuse(refusal),
        }
    };

    if resize.dry_run {
        return super::report::decision(
            Report::Resize(ResizePreview {
                partition: number,
                start_lba: start,
                old_end_lba: old_end,
                new_end_lba: new_end,
                sector_size: sector,
                filesystem: filesystem_step
                    .as_ref()
                    .map(|_| partition.content)
                    .or_else(|| lvm_step.as_ref().map(|_| partition.content)),
                grow,
            }),
            vec![Step::new(StepKind::Inspect, Vec::new())],
        );
    }
    if !grow && !shrink {
        return Decision::Unchanged {
            note: format!("partition #{number} already ends at LBA {old_end} — nothing to resize"),
        };
    }

    let mut segments = match table.resized(number, start, new_end, &moves) {
        Ok(segments) => segments,
        Err(why) => return Decision::Refuse(Refusal::TableDamaged { why }),
    };
    super::repair::append_protective(&mut segments, protective_mbr);
    let table_step = table_step(table.format(), sector, segments, snapshot.stamp);

    let mut steps = Vec::new();
    if shrink {
        // 收缩：先缩 PV/文件系统，再写表。
        if let Some(step) = lvm_step {
            steps.push(step);
        }
        if let Some(step) = filesystem_step {
            steps.push(step);
        }
        steps.push(table_step);
    } else {
        // 扩容：先把挡路分区搬到新位置，再写表，最后扩 PV/文件系统。
        for relocation in &moves {
            steps.push(relocation.step(sector));
        }
        steps.push(table_step);
        if let Some(step) = filesystem_step {
            steps.push(step);
        }
        if let Some(step) = lvm_step {
            steps.push(step);
        }
    }
    let plan = match Plan::write(steps, identity) {
        Ok(plan) => plan,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let relocated = if moves.is_empty() {
        String::new()
    } else {
        format!(", relocated {} blocking partition(s)", moves.len())
    };
    finish(
        snapshot,
        plan,
        format!(
            "resized partition #{number} to LBA {start}..{new_end}{relocated} (verify with: diskedit layout {})",
            snapshot.target.display
        ),
    )
}

/// 可编辑的表：GPT 与 MBR 主表。
enum Resizable {
    Gpt(GptTable),
    Mbr { lba0: Vec<u8>, total_lba: u64 },
}

impl Resizable {
    fn format(&self) -> TableFormat {
        match self {
            Self::Gpt(_) => TableFormat::Gpt,
            Self::Mbr { .. } => TableFormat::Mbr,
        }
    }

    fn last_usable(&self) -> u64 {
        match self {
            Self::Gpt(table) => table.header.last_usable_lba,
            Self::Mbr { total_lba, .. } => total_lba.checked_sub(1).unwrap_or(0),
        }
    }

    fn used_ranges(&self) -> Vec<(u64, u64)> {
        match self {
            Self::Gpt(table) => table.used_ranges(),
            // MBR 解析失败时不能当作"没有任何占用"：那会把整条盘尾算成空闲。这里
            // 按"整盘已用"处理，宁可不给扩展空间，也不越过未知占用。
            Self::Mbr { lba0, total_lba } => match edit::mbr_used_ranges(lba0, *total_lba) {
                Ok(used) => used,
                Err(_) => vec![(0, (*total_lba).max(1) - 1)],
            },
        }
    }

    /// 把指定分区的新终点落进表字节，返回写段；`moves` 里的分区同时改到新起点。
    fn resized(
        &self,
        number: u32,
        start: u64,
        new_end: u64,
        moves: &[Relocation],
    ) -> Result<Vec<Segment>, String> {
        match self {
            Self::Gpt(table) => {
                let index = table
                    .live_index(number)
                    .map_err(|error| error.to_string())?;
                let mut entries = table.entries.clone();
                entries[index].ending_lba = new_end;
                for relocation in moves {
                    entries[relocation.index].starting_lba = relocation.new_start;
                    entries[relocation.index].ending_lba = relocation.new_end();
                }
                table.segments(&entries).map_err(|error| error.to_string())
            }
            Self::Mbr { lba0, total_lba } => {
                let bytes = edit::resize_mbr(lba0, number, start, new_end, *total_lba)
                    .map_err(|error| error.to_string())?;
                Ok(vec![Segment {
                    region: region(0, bytes.len() as u64),
                    bytes,
                }])
            }
        }
    }
}

/// `resolve_end` 的输入：当前几何、右侧空隙与（`--allow-move` 时的）增长天花板。
struct EndContext {
    start: u64,
    old_end: u64,
    current: u64,
    free: u64,
    sector: u64,
    last_usable: u64,
    grow_ceiling: Option<u64>,
}

/// 计算新终点：SIZE、`--end`、`--grow-to-end` 三种入口，逐项 checked。
fn resolve_end(resize: &ResizeRequest, context: EndContext) -> Result<u64, Refusal> {
    let EndContext {
        start,
        old_end,
        current,
        free,
        sector,
        last_usable,
        grow_ceiling,
    } = context;
    let end = if resize.grow_to_end {
        last_usable
    } else if let Some(end) = resize.end {
        end
    } else if let Some(size) = &resize.size {
        if *size == SizeSpec::Grow {
            grow_ceiling.unwrap_or(old_end + free)
        } else {
            let bytes = resolve_bytes(*size, current)?;
            if bytes < sector {
                return Err(beyond_table(format!(
                    "size {bytes} < one sector ({sector})"
                )));
            }
            let sectors = bytes / sector;
            start
                .checked_add(sectors)
                .and_then(|end| end.checked_sub(1))
                .ok_or_else(|| beyond_table(format!("size {bytes} overflows the LBA range")))?
        }
    } else {
        return Err(beyond_table(
            "`resize` needs a size, --end or --grow-to-end",
        ));
    };
    let end = align_down(end, resize.align, sector);
    if end > last_usable {
        return Err(Refusal::TableEdit {
            why: format!("the requested end {end} is past the last usable LBA {last_usable}"),
        });
    }
    if end < start {
        return Err(Refusal::TableEdit {
            why: format!("the requested end {end} is before the partition start {start}"),
        });
    }
    Ok(end)
}

/// SIZE → 目标字节数（绝对口径）。
fn resolve_bytes(size: SizeSpec, current: u64) -> Result<u64, Refusal> {
    match size {
        SizeSpec::Grow => Ok(current),
        SizeSpec::Absolute(bytes) => Ok(bytes),
        SizeSpec::Delta { grow, bytes } => {
            if grow {
                current
                    .checked_add(bytes)
                    .ok_or_else(|| beyond_table("SIZE overflows the partition size"))
            } else {
                current
                    .checked_sub(bytes)
                    .ok_or_else(|| beyond_table(format!("SIZE exceeds the current size {current}")))
            }
        }
        SizeSpec::Percent { grow, num, denom } => {
            let raw = (current as u128)
                .checked_mul(num as u128)
                .and_then(|value| value.checked_div(100u128 * denom as u128))
                .filter(|value| *value <= u64::MAX as u128)
                .ok_or_else(|| beyond_table("SIZE overflows the partition size"))?
                as u64;
            let delta = raw / MIB * MIB;
            if grow {
                current
                    .checked_add(delta)
                    .ok_or_else(|| beyond_table("SIZE overflows the partition size"))
            } else {
                current
                    .checked_sub(delta)
                    .ok_or_else(|| beyond_table(format!("SIZE exceeds the current size {current}")))
            }
        }
    }
}

/// 按 `--align` 下取整新终点；未给出对齐时原样返回。对齐单位以字节给出，按扇区换算。
fn align_down(end: u64, align: Option<u64>, sector: u64) -> u64 {
    let Some(bytes) = align.filter(|bytes| *bytes > 1) else {
        return end;
    };
    let unit = (bytes / sector).max(1);
    end / unit * unit
}

/// 文件系统步骤的目标几何。
struct FsGeometry {
    number: u32,
    start_lba: u64,
    old_bytes: u64,
    new_bytes: u64,
    sector: u64,
}

/// 目标分区是 LVM2 PV 时的链条步骤。
///
/// 扩容在表写之后让 PV 吸收新空间（必要时再扩 LV 与其文件系统）；缩容在表写之前
/// 把 PV 元数据缩到新长度，但只在已分配 extent 都落在新长度之内时才允许。
fn pv_chain_step(
    snapshot: &ResizeSnapshot,
    resize: &ResizeRequest,
    grow: bool,
    shrink: bool,
    geometry: &FsGeometry,
) -> Result<Option<Step>, Refusal> {
    if !grow && !shrink {
        return Ok(None);
    }
    require_lvm_tools(snapshot, resize.grow_lv)?;
    let Some(facts) = &snapshot.lvm else {
        return Err(Refusal::FactUnknown {
            what: "the LVM PV facts were not captured".to_owned(),
        });
    };
    let start_bytes = geometry
        .start_lba
        .checked_mul(geometry.sector)
        .ok_or_else(|| Refusal::TableEdit {
            why: "the partition start overflows the byte offset".to_owned(),
        })?;
    if shrink {
        if resize.grow_lv {
            return Err(Refusal::TableEdit {
                why: "--grow-lv cannot be combined with shrinking the PV".to_owned(),
            });
        }
        let allocated = match &facts.allocated_end {
            Ok(bytes) => *bytes,
            Err(error) => return Err(lvm_refusal(error)),
        };
        if allocated > geometry.new_bytes {
            return Err(Refusal::TableEdit {
                why: format!(
                    "the PV's allocated extents end at {allocated} bytes, past the requested {} \
                     bytes — shrink the LV/filesystem inside it first",
                    geometry.new_bytes
                ),
            });
        }
        return Ok(Some(Step::new(
            StepKind::LvmPvShrink {
                partition: geometry.number,
                start_bytes,
                bytes: geometry.new_bytes,
            },
            vec![region(start_bytes, geometry.old_bytes)],
        )));
    }
    let lv = if resize.grow_lv {
        match &facts.selection {
            Some(Ok(selection)) => {
                // LV 级增量在判定期算清：不足一个 extent、LV 尺寸为零或目标溢出都不能
                // 等到表写之后才发现，否则会留下"表已扩、LV 未扩"的半成品。
                if selection.size == 0 {
                    return Err(Refusal::TableEdit {
                        why: format!("LV {} reports 0 bytes — nothing to extend", selection.path),
                    });
                }
                let delta = geometry
                    .new_bytes
                    .checked_sub(geometry.old_bytes)
                    .ok_or_else(|| Refusal::TableEdit {
                        why: "the new size is smaller than the current size".to_owned(),
                    })?;
                let extents = delta / selection.extent_bytes;
                if extents == 0 {
                    return Err(Refusal::TableEdit {
                        why: format!(
                            "added {delta} bytes is less than one extent ({} bytes) — the LV cannot \
                             take any of the added space",
                            selection.extent_bytes
                        ),
                    });
                }
                let grown = extents.checked_mul(selection.extent_bytes).ok_or_else(|| {
                    Refusal::TableEdit {
                        why: "the requested LV growth overflows the address space".to_owned(),
                    }
                })?;
                selection
                    .size
                    .checked_add(grown)
                    .ok_or_else(|| Refusal::TableEdit {
                        why: "the LV target size overflows the address space".to_owned(),
                    })?;
                Some(selection.path.clone())
            }
            Some(Err(error)) => return Err(lvm_refusal(error)),
            None => {
                return Err(Refusal::FactUnknown {
                    what: "the target LV was not selected before the plan".to_owned(),
                });
            }
        }
    } else {
        None
    };
    Ok(Some(Step::new(
        StepKind::LvmGrowChain {
            partition: geometry.number,
            start_bytes,
            old_bytes: geometry.old_bytes,
            new_bytes: geometry.new_bytes,
            grow_lv: resize.grow_lv,
            lv,
        },
        vec![region(start_bytes, geometry.new_bytes)],
    )))
}

/// PV 链所需的外部程序必须在现场采集时都可用。
fn require_lvm_tools(snapshot: &ResizeSnapshot, grow_lv: bool) -> Result<(), Refusal> {
    let mut required = vec![Program::Pvs, Program::Pvresize];
    if grow_lv {
        required.push(Program::Lvs);
        required.push(Program::Lvextend);
    }
    if !snapshot.target.kind.is_block_device() {
        required.push(Program::Losetup);
    }
    for program in required {
        if !snapshot
            .tools
            .iter()
            .any(|(known, available)| *known == program && *available)
        {
            return Err(Refusal::ToolMissing { program });
        }
    }
    Ok(())
}

/// LVM 查询失败 → 拒绝成因：请求不符归 3，环境失败归 4，缺工具归 6。
fn lvm_refusal(error: &crate::app::lvm::LvmError) -> Refusal {
    use crate::app::lvm::LvmError;
    match error {
        LvmError::Refused(message) | LvmError::Inactive(message) => Refusal::TableEdit {
            why: format!("{message} — {}", error.hint()),
        },
        LvmError::Failed(message) => Refusal::LvmFailed {
            why: message.clone(),
        },
        LvmError::ToolMissing(message) => Refusal::PlatformCannotRunTools {
            why: message.clone(),
        },
    }
}

/// 文件系统步骤；`--no-fs` 或只改表时为 `None`。
fn filesystem_step(
    content: ContentKind,
    no_fs: bool,
    grow: bool,
    shrink: bool,
    geometry: &FsGeometry,
    tools: &[(Program, bool)],
) -> Result<Option<Step>, Refusal> {
    if no_fs {
        if shrink {
            return Err(Refusal::TableEdit {
                why: "--no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the \
                      new partition end would cut into filesystem metadata"
                    .to_owned(),
            });
        }
        return Ok(None);
    }
    if !grow && !shrink {
        return Ok(None);
    }
    // swap 没有原地 resize：新尺寸只有按盘上的新分区重建才生效（旧 `resize` 收尾
    // 的换区重建）。收缩没有对应能力，按目录里的不支持理由拒绝。
    if content == ContentKind::Swap {
        let verb = if shrink { Verb::Shrink } else { Verb::Grow };
        match catalog::profile(content).support(verb) {
            Support::Tools(required) => {
                for program in required {
                    if !tools
                        .iter()
                        .any(|(known, available)| known == program && *available)
                    {
                        return Err(Refusal::ToolMissing { program: *program });
                    }
                }
            }
            Support::Unsupported(why) => {
                return Err(Refusal::ContentUnsupported {
                    what: format!("resize {}", content.as_str()),
                    why: why.to_owned(),
                });
            }
            Support::NotApplicable => {
                return Err(Refusal::ContentUnsupported {
                    what: format!("resize {}", content.as_str()),
                    why: "this content kind carries no filesystem to resize".to_owned(),
                });
            }
        }
        let base = geometry
            .start_lba
            .checked_mul(geometry.sector)
            .ok_or_else(|| Refusal::TableEdit {
                why: "the partition start overflows the byte offset".to_owned(),
            })?;
        let region =
            diskedit_core::region::Region::sized(base, geometry.new_bytes).map_err(|_| {
                Refusal::TableEdit {
                    why: "the swap region does not fit the target".to_owned(),
                }
            })?;
        return Ok(Some(Step::new(
            StepKind::RebuildSwap {
                partition: geometry.number,
                start_bytes: base,
                bytes: geometry.new_bytes,
            },
            vec![region],
        )));
    }
    let Some(filesystem) = content.filesystem() else {
        return match content {
            ContentKind::Unknown => Err(Refusal::FilesystemUnknown {
                number: geometry.number,
            }),
            ContentKind::Filesystem(_) => Ok(None),
            ContentKind::Swap
            | ContentKind::Lvm2Pv
            | ContentKind::Luks
            | ContentKind::BitLocker => Err(Refusal::NoFilesystem {
                number: geometry.number,
                what: content.as_str().to_owned(),
            }),
        };
    };
    let verb = if shrink { Verb::Shrink } else { Verb::Grow };
    match catalog::profile(content).support(verb) {
        Support::Tools(required) => {
            for program in required {
                if !tools
                    .iter()
                    .any(|(known, available)| known == program && *available)
                {
                    return Err(Refusal::ToolMissing { program: *program });
                }
            }
        }
        Support::Unsupported(why) => {
            return Err(Refusal::ContentUnsupported {
                what: format!("resize {}", content.as_str()),
                why: why.to_owned(),
            });
        }
        Support::NotApplicable => {
            return Err(Refusal::ContentUnsupported {
                what: format!("resize {}", content.as_str()),
                why: "this content kind carries no filesystem to resize".to_owned(),
            });
        }
    }
    let bytes = geometry.new_bytes;
    // 扩容时设备要暴露新的（更大的）区间；收缩时仍暴露原区间，工具按显式尺寸收缩。
    let device_len = if shrink { geometry.old_bytes } else { bytes };
    let base = geometry
        .start_lba
        .checked_mul(geometry.sector)
        .ok_or_else(|| Refusal::TableEdit {
            why: "the partition start overflows the byte offset".to_owned(),
        })?;
    let region =
        diskedit_core::region::Region::sized(base, device_len).map_err(|_| Refusal::TableEdit {
            why: "the filesystem region does not fit the target".to_owned(),
        })?;
    let kind = if shrink {
        StepKind::ShrinkFilesystem {
            partition: geometry.number,
            filesystem,
            bytes,
        }
    } else {
        StepKind::GrowFilesystem {
            partition: geometry.number,
            filesystem,
            bytes,
        }
    };
    Ok(Some(Step::new(kind, vec![region])))
}

/// 右侧连续空闲的扇区数。
fn free_right(used: &[(u64, u64)], old_end: u64, last_usable: u64) -> u64 {
    if old_end >= last_usable {
        return 0;
    }
    let mut next = last_usable;
    for &(start, _end) in used {
        if start > old_end && start - 1 < next {
            next = start - 1;
        }
    }
    // 循环条件保证 `next` 不低于 `old_end`：初值是更大的 `last_usable`，
    // 更新值是 `start - 1` 且 `start > old_end`。
    debug_assert!(next >= old_end);
    next - old_end
}

fn table_step(
    format: TableFormat,
    segment_size: u64,
    segments: Vec<Segment>,
    stamp: [u8; 32],
) -> Step {
    let regions = segments.iter().map(|segment| segment.region).collect();
    let segments: Vec<TableSegment> = segments
        .into_iter()
        .map(|segment| TableSegment {
            region: segment.region,
            bytes: segment.bytes,
        })
        .collect();
    Step::new(
        StepKind::WriteTable {
            format,
            segment_size,
            stamp,
            segments,
        },
        regions,
    )
}

/// 公共前置：分区节点、表形态、占用。
/// 无分区表（superfloppy）的 resize：文件系统占整盘，唯一有意义的动作是把它撑到
/// 容器末端；缩与"超过容器"都无分区边界可依，按基线拒绝。
fn superfloppy(snapshot: &ResizeSnapshot, resize: &ResizeRequest) -> Decision {
    match &snapshot.occupancy {
        Occupancy::Idle => {}
        Occupancy::Used { evidence } => {
            return Decision::Refuse(Refusal::Occupied {
                evidence: evidence.clone(),
            });
        }
        Occupancy::Unknown { why } => {
            return Decision::Refuse(Refusal::OccupancyUnknown { why: why.clone() });
        }
    }
    if let Some(selector) = resize.partition {
        let shown = match selector {
            PartitionSelector::Number(number) => number.to_string(),
            PartitionSelector::Last => "last".to_owned(),
        };
        return Decision::Refuse(Refusal::TableEdit {
            why: format!(
                "target has no partition table — drop :{shown} (the FS occupies the whole device)"
            ),
        });
    }
    if resize.grow_lv || resize.lv.is_some() {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--grow-lv/--lv needs an LVM PV — a superfloppy has no partitions".to_owned(),
        });
    }
    if resize.no_fs {
        return Decision::Refuse(Refusal::TableEdit {
            why:
                "--no-fs leaves nothing to do on a superfloppy — there is no partition to change, \
                  the only action is the filesystem grow"
                    .to_owned(),
        });
    }
    if resize.allow_move {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--allow-move has nothing to do on a superfloppy — there are no partitions to \
                  relocate"
                .to_owned(),
        });
    }
    if resize.chunk.is_some() {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--chunk has no effect on a superfloppy — there is nothing to relocate".to_owned(),
        });
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let current = snapshot.target.bytes;
    let Some(size) = &resize.size else {
        return Decision::Refuse(Refusal::TableEdit {
            why: "`resize` needs a size or `grow`".to_owned(),
        });
    };
    let requested = match resolve_bytes(*size, current) {
        Ok(bytes) => bytes,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    if requested < current {
        return Decision::Refuse(Refusal::TableEdit {
            why: "superfloppy cannot shrink — the FS occupies the whole device, there is no \
                  partition boundary to shrink to"
                .to_owned(),
        });
    }
    if requested > current {
        return Decision::Refuse(Refusal::TableEdit {
            why: "target exceeds device/image size — extend the image or replace the disk first \
                  (this tool does not resize the container)"
                .to_owned(),
        });
    }
    let Some(content) = snapshot.whole_content else {
        return Decision::Refuse(Refusal::TableEdit {
            why: "the whole-device content could not be identified".to_owned(),
        });
    };
    let Some(filesystem) = content.filesystem() else {
        return match content {
            ContentKind::Filesystem(_) => Decision::Refuse(Refusal::TableEdit {
                why: "this filesystem carries no grow step".to_owned(),
            }),
            ContentKind::Unknown
            | ContentKind::Swap
            | ContentKind::Lvm2Pv
            | ContentKind::Luks
            | ContentKind::BitLocker => Decision::Refuse(Refusal::NoFilesystem {
                number: 0,
                what: content.as_str().to_owned(),
            }),
        };
    };
    match catalog::profile(content).support(Verb::Grow) {
        Support::Tools(required) => {
            for program in required {
                if !snapshot
                    .tools
                    .iter()
                    .any(|(known, available)| known == program && *available)
                {
                    return Decision::Refuse(Refusal::ToolMissing { program: *program });
                }
            }
        }
        Support::Unsupported(why) => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resize {}", content.as_str()),
                why: why.to_owned(),
            });
        }
        Support::NotApplicable => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resize {}", content.as_str()),
                why: "this content kind carries no filesystem to resize".to_owned(),
            });
        }
    }
    let Ok(region) = diskedit_core::region::Region::sized(0, current) else {
        return Decision::Refuse(Refusal::TableEdit {
            why: "the device region is not addressable".to_owned(),
        });
    };
    let step = Step::new(
        StepKind::GrowFilesystem {
            partition: 0,
            filesystem,
            bytes: current,
        },
        vec![region],
    );
    let plan = match Plan::write(vec![step], identity) {
        Ok(plan) => plan,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    finish(
        snapshot,
        plan,
        format!(
            "superfloppy: {} grown to full device ({current} bytes) — verify with: diskedit layout {}",
            content.as_str(),
            snapshot.target.display
        ),
    )
}

/// 把快照里的表事实变成可编辑的表；GPT 先按修复判据落新上界与保护 MBR。
fn editable(snapshot: &ResizeSnapshot) -> Result<(Resizable, Option<Vec<u8>>), Refusal> {
    let Some(facts) = &snapshot.facts else {
        return Err(unusable(snapshot));
    };
    match facts {
        TableFacts::Gpt {
            header,
            entries,
            sector_size,
            total_bytes,
            state,
            pmbr,
            lba0,
        } => {
            let mut gpt = GptTable {
                header: header.clone(),
                entries: entries.clone(),
                sector_size: *sector_size,
                total_bytes: *total_bytes,
            };
            let repaired = super::repair::adopt(&mut gpt, *state, *pmbr, lba0)?;
            Ok((Resizable::Gpt(gpt), repaired.protective_mbr))
        }
        TableFacts::Mbr { lba0, total_lba } => Ok((
            Resizable::Mbr {
                lba0: lba0.clone(),
                total_lba: *total_lba,
            },
            None,
        )),
    }
}

/// 在线路径之前的使用事实：别的命名空间里的挂载、活动 swap 都不能在本次处理。
fn online_precondition(snapshot: &ResizeSnapshot) -> Option<Refusal> {
    if let Some(partition) = &snapshot.selected {
        let in_use = matches!(
            snapshot.partition_mount,
            Some(
                crate::platform::PartitionMount::Here { .. }
                    | crate::platform::PartitionMount::Elsewhere { .. }
                    | crate::platform::PartitionMount::Ambiguous
            )
        );
        if partition.content == ContentKind::Lvm2Pv && (snapshot.online.is_some() || in_use) {
            return Some(Refusal::TableEdit {
                why: "an LVM2 PV cannot be resized while its partition may be held by active \
                      volumes — the online PV geometry path is not available in this build; \
                      deactivate the volume group and use the offline PV path (or --grow-lv on an \
                      unmounted PV)"
                    .to_owned(),
            });
        }
    }
    if let Some(mount) = &snapshot.partition_mount {
        match mount {
            crate::platform::PartitionMount::Here { .. }
            | crate::platform::PartitionMount::NotMounted => {}
            crate::platform::PartitionMount::Elsewhere { mount_point, pid } => {
                return Some(Refusal::TableEdit {
                    why: format!(
                        "partition {0} is mounted at {1} in another mount namespace{2} — it cannot \
                         be resized from this one; unmount it there or run inside that namespace",
                        snapshot
                            .selected
                            .as_ref()
                            .map(|partition| partition.number)
                            .unwrap_or(0),
                        mount_point.display(),
                        match pid {
                            Some(pid) => format!(" (pid {pid})"),
                            None => " (the namespace id could not be determined)".to_owned(),
                        }
                    ),
                });
            }
            crate::platform::PartitionMount::Ambiguous => {
                return Some(Refusal::TableEdit {
                    why: "the partition has more than one mount record in this namespace — \
                          unmount the stacked layers and retry"
                        .to_owned(),
                });
            }
        }
    }
    if snapshot.swap_active == Some(true) {
        return Some(Refusal::TableEdit {
            why: format!(
                "partition {} is active swap — run swapoff first",
                snapshot
                    .selected
                    .as_ref()
                    .map(|partition| partition.number)
                    .unwrap_or(0)
            ),
        });
    }
    None
}

/// 在线 PV 几何：块设备上的 LVM2 PV 分区被 device-mapper 持有（占用为 `Used`）但未挂载时，
/// 旧版走在线路径——写表、按分区同步内核视图、再让 PV 吸收新空间。只扩不缩，与基线一致。
///
/// 返回 `None` 表示这不是在线 PV 现场，交回常规判定：未持有的 PV 走离线链，未判定的占用
/// 交给占用闸，别的主机形态维持既有拒绝。
fn pv_online(snapshot: &ResizeSnapshot, resize: &ResizeRequest) -> Option<Decision> {
    let partition = snapshot.selected.as_ref()?;
    if partition.content != ContentKind::Lvm2Pv || snapshot.is_partition_node {
        return None;
    }
    // 只处理整盘块设备上的 PV；镜像与挂载形态各走自己的路径。
    if !snapshot.target.kind.is_block_device() || snapshot.online.is_some() {
        return None;
    }
    if snapshot.swap_active == Some(true) {
        return None;
    }
    // 只有被 dm 持有（`Used`）才是在线；`Idle` 走离线 PV 链，`Unknown` 交给占用闸。
    if !matches!(snapshot.occupancy, Occupancy::Used { .. }) {
        return None;
    }
    Some(pv_online_decide(snapshot, resize, partition))
}

/// 在线 PV 几何的判定：只扩、只吃右侧连续空闲、必须能扩 PV。
fn pv_online_decide(
    snapshot: &ResizeSnapshot,
    resize: &ResizeRequest,
    partition: &crate::app::observe::table::PartitionView,
) -> Decision {
    if resize.no_fs {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--no-fs cannot resize an LVM2 PV — the PV has to be resized together with the \
                  partition; omit --no-fs"
                .to_owned(),
        });
    }
    if resize.allow_move {
        return Decision::Refuse(Refusal::TableEdit {
            why:
                "--allow-move is not available while the PV is held by active volumes — the online \
                  PV path cannot relocate blocking partitions"
                    .to_owned(),
        });
    }
    if resize.chunk.is_some() {
        return Decision::Refuse(Refusal::TableEdit {
            why:
                "--chunk has no effect while the PV is held by active volumes — the online PV path \
                  does not relocate data"
                    .to_owned(),
        });
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let sector = snapshot.target.sector_size;
    if sector == 0 {
        return Decision::Refuse(Refusal::GeometryUnknown {
            why: "the logical sector size is unknown".to_owned(),
        });
    }
    let (table, protective_mbr) = match editable(snapshot) {
        Ok(editable) => editable,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let number = partition.number;
    let start = partition.first_lba;
    let old_end = partition.last_lba;
    let current = partition.bytes;
    let last_usable = table.last_usable();
    let free = free_right(&table.used_ranges(), old_end, last_usable);
    let new_end = match resolve_end(
        resize,
        EndContext {
            start,
            old_end,
            current,
            free,
            sector,
            last_usable,
            grow_ceiling: None,
        },
    ) {
        Ok(end) => end,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    if new_end < old_end {
        return Decision::Refuse(Refusal::TableEdit {
            why: "the online PV path can only grow the partition — shrinking a PV held by active \
                  volumes needs the LV/PV shrink chain; deactivate the volume group and use the \
                  offline path"
                .to_owned(),
        });
    }
    if new_end == old_end {
        if resize.size.as_ref() == Some(&SizeSpec::Grow) {
            return Decision::Refuse(Refusal::TableEdit {
                why: "grow would not extend the PV partition — no usable free space to its right; \
                      the online PV path cannot relocate blocking partitions"
                    .to_owned(),
            });
        }
        return Decision::Unchanged {
            note: format!("partition #{number} is already {current} bytes — nothing to do"),
        };
    }
    if new_end - old_end > free {
        return Decision::Refuse(Refusal::TableEdit {
            why: "grow would not extend the PV partition — no usable free space to its right; the \
                  online PV path cannot relocate blocking partitions"
                .to_owned(),
        });
    }
    let bytes = match (new_end - start + 1).checked_mul(sector) {
        Some(bytes) => bytes,
        None => {
            return Decision::Refuse(Refusal::GeometryUnknown {
                why: "the grown partition length overflows the byte range".to_owned(),
            });
        }
    };
    let start_bytes = match start.checked_mul(sector) {
        Some(bytes) => bytes,
        None => {
            return Decision::Refuse(Refusal::GeometryUnknown {
                why: "the partition start overflows the byte offset".to_owned(),
            });
        }
    };
    let geometry = FsGeometry {
        number,
        start_lba: start,
        old_bytes: current,
        new_bytes: bytes,
        sector,
    };
    let lvm_step = match pv_chain_step(snapshot, resize, true, false, &geometry) {
        Ok(Some(step)) => step,
        Ok(None) => return refuse("the online PV chain produced no step"),
        Err(refusal) => return Decision::Refuse(refusal),
    };
    if resize.dry_run {
        return super::report::decision(
            Report::Resize(ResizePreview {
                partition: number,
                start_lba: start,
                old_end_lba: old_end,
                new_end_lba: new_end,
                sector_size: sector,
                filesystem: Some(partition.content),
                grow: true,
            }),
            vec![Step::new(StepKind::Inspect, Vec::new())],
        );
    }
    let mut segments = match table.resized(number, start, new_end, &[]) {
        Ok(segments) => segments,
        Err(why) => return Decision::Refuse(Refusal::TableDamaged { why }),
    };
    super::repair::append_protective(&mut segments, protective_mbr);
    let table_step = table_step(table.format(), sector, segments, snapshot.stamp);
    let refresh = Step::new(
        StepKind::RefreshKernelView {
            disk: snapshot.target.path.clone(),
            partition: number,
            start_bytes,
            bytes,
        },
        Vec::new(),
    );
    // 扩容：先写表、再同步内核视图、最后让 PV 吸收新空间（B13、B28）。
    let plan = match Plan::write(vec![table_step, refresh, lvm_step], identity) {
        Ok(plan) => plan,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    finish(
        snapshot,
        plan,
        format!(
            "resized LVM2 PV partition #{number} online to LBA {start}..{new_end} (verify with: diskedit layout {})",
            snapshot.target.display
        ),
    )
}

/// 已挂载分区的几何调整：只扩、只吃右侧连续空闲、必须带文件系统步。
fn online(snapshot: &ResizeSnapshot, resize: &ResizeRequest) -> Decision {
    let Some(online) = &snapshot.online else {
        return refuse("resize reached the online path without mount facts");
    };
    if resize.no_fs {
        return Decision::Refuse(Refusal::TableEdit {
            why:
                "--no-fs is not available online — the online path always includes the filesystem \
                  step; unmount the partition to skip it"
                    .to_owned(),
        });
    }
    if resize.allow_move {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--allow-move is not available online — the online path cannot relocate blocking \
                  partitions"
                .to_owned(),
        });
    }
    if resize.chunk.is_some() {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--chunk has no effect online — the online path does not relocate data".to_owned(),
        });
    }
    if resize.grow_lv || resize.lv.is_some() {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--grow-lv/--lv is not available online — resizing a mounted PV needs the online \
                  PV geometry path, which this build does not provide"
                .to_owned(),
        });
    }
    let Some(partition) = &snapshot.selected else {
        return Decision::Refuse(Refusal::PartitionMissing {
            number: online.partition,
        });
    };
    if partition.number != online.partition {
        return Decision::Refuse(Refusal::FactUnknown {
            what: "the mounted partition does not match the selected partition".to_owned(),
        });
    }
    let Some(identity) = snapshot.target.identity else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let sector = snapshot.target.sector_size;
    if sector == 0 {
        return Decision::Refuse(Refusal::GeometryUnknown {
            why: "the logical sector size is unknown".to_owned(),
        });
    }
    let (table, protective_mbr) = match editable(snapshot) {
        Ok(editable) => editable,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let number = partition.number;
    let start = partition.first_lba;
    let old_end = partition.last_lba;
    let current = partition.bytes;
    let last_usable = table.last_usable();
    let free = free_right(&table.used_ranges(), old_end, last_usable);
    let new_end = match resolve_end(
        resize,
        EndContext {
            start,
            old_end,
            current,
            free,
            sector,
            last_usable,
            grow_ceiling: None,
        },
    ) {
        Ok(end) => end,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let shrink = new_end < old_end;
    if new_end == old_end {
        if resize.size.as_ref() == Some(&SizeSpec::Grow) {
            return Decision::Refuse(Refusal::TableEdit {
                why:
                    "grow would not extend the partition — no usable free space to its right; the \
                      online path cannot relocate blocking partitions (unmount to grow offline, or \
                      fill the filesystem with `resize <mountpoint> --fs-only`)"
                        .to_owned(),
            });
        }
        return Decision::Unchanged {
            note: format!("partition #{number} is already {current} bytes — nothing to do"),
        };
    }
    if !shrink && new_end - old_end > free {
        return Decision::Refuse(Refusal::TableEdit {
            why: "grow would not extend the partition — no usable free space to its right; the \
                  online path cannot relocate blocking partitions (unmount to grow offline, or \
                  fill the filesystem with `resize <mountpoint> --fs-only`)"
                .to_owned(),
        });
    }
    let filesystem = if shrink {
        // 在线收缩只有 btrfs 支持；其余族仍按基线拒绝。
        if !matches!(partition.content, ContentKind::Filesystem(FsKind::Btrfs)) {
            return Decision::Refuse(Refusal::OnlineShrink {
                why: format!(
                    "{} cannot shrink while mounted; unmount it and shrink through the offline \
                     path where supported",
                    partition.content.as_str()
                ),
            });
        }
        if !snapshot
            .tools
            .iter()
            .any(|(known, ok)| *known == Program::Btrfs && *ok)
        {
            return Decision::Refuse(Refusal::ToolMissing {
                program: Program::Btrfs,
            });
        }
        FsKind::Btrfs
    } else {
        match online_filesystem(partition.content, number, &snapshot.tools) {
            Ok(filesystem) => filesystem,
            Err(refusal) => return Decision::Refuse(refusal),
        }
    };
    let bytes = (new_end - start + 1)
        .checked_mul(sector)
        .ok_or_else(|| Refusal::GeometryUnknown {
            why: "the grown partition length overflows the byte range".to_owned(),
        });
    let bytes = match bytes {
        Ok(bytes) => bytes,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let start_bytes = start
        .checked_mul(sector)
        .ok_or_else(|| Refusal::GeometryUnknown {
            why: "the partition start overflows the byte offset".to_owned(),
        });
    let start_bytes = match start_bytes {
        Ok(value) => value,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    if resize.dry_run {
        return super::report::decision(
            Report::Resize(ResizePreview {
                partition: number,
                start_lba: start,
                old_end_lba: old_end,
                new_end_lba: new_end,
                sector_size: sector,
                filesystem: Some(partition.content),
                grow: !shrink,
            }),
            vec![Step::new(StepKind::Inspect, Vec::new())],
        );
    }
    let mut segments = match table.resized(number, start, new_end, &[]) {
        Ok(segments) => segments,
        Err(why) => return Decision::Refuse(Refusal::TableDamaged { why }),
    };
    super::repair::append_protective(&mut segments, protective_mbr);
    let table_step = table_step(table.format(), sector, segments, snapshot.stamp);
    let refresh = Step::new(
        StepKind::RefreshKernelView {
            disk: online.disk.clone(),
            partition: number,
            start_bytes,
            bytes,
        },
        Vec::new(),
    );
    let filesystem_step = if shrink {
        Step::new(
            StepKind::ShrinkFilesystemOnline {
                mount_point: online.mount_point.clone(),
                device: online.device.clone(),
                disk: online.disk.clone(),
                partition: number,
                filesystem,
                bytes,
            },
            Vec::new(),
        )
    } else {
        Step::new(
            StepKind::GrowFilesystemOnline {
                mount_point: online.mount_point.clone(),
                device: online.device.clone(),
                disk: online.disk.clone(),
                partition: number,
                filesystem,
                bytes,
            },
            Vec::new(),
        )
    };
    // 收缩必须先缩文件系统，再写表与内核视图；扩容次序相反（B13）。
    let steps = if shrink {
        vec![filesystem_step, table_step, refresh]
    } else {
        vec![table_step, refresh, filesystem_step]
    };
    let plan = match Plan::write(steps, identity) {
        Ok(plan) => plan,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    finish(
        snapshot,
        plan,
        format!(
            "resized partition #{number} online to LBA {start}..{new_end} (verify with: diskedit layout {})",
            snapshot.target.display
        ),
    )
}

/// 在线扩容可用的文件系统族与工具；没有在线工具的内容类别如实拒绝。
fn online_filesystem(
    content: ContentKind,
    number: u32,
    tools: &[(Program, bool)],
) -> Result<diskedit_core::fs::FsKind, Refusal> {
    use diskedit_core::fs::OnlineGrow;
    let Some(filesystem) = content.filesystem() else {
        return Err(match content {
            ContentKind::Unknown => Refusal::FilesystemUnknown { number },
            ContentKind::Filesystem(_) => Refusal::ContentUnsupported {
                what: format!("resize {}", content.as_str()),
                why: "this filesystem carries no grow step".to_owned(),
            },
            ContentKind::Swap
            | ContentKind::Lvm2Pv
            | ContentKind::Luks
            | ContentKind::BitLocker => Refusal::NoFilesystem {
                number,
                what: content.as_str().to_owned(),
            },
        });
    };
    match catalog::online_grow(content) {
        OnlineGrow::Device(program) | OnlineGrow::MountPoint(program) => {
            if !tools
                .iter()
                .any(|(known, available)| *known == program && *available)
            {
                return Err(Refusal::ToolMissing { program });
            }
            Ok(filesystem)
        }
        OnlineGrow::Unsupported(why) => Err(Refusal::ContentUnsupported {
            what: format!("resize {} online", content.as_str()),
            why: why.to_owned(),
        }),
    }
}

fn admission(snapshot: &ResizeSnapshot) -> Option<Refusal> {
    if snapshot.is_partition_node {
        return Some(Refusal::PartitionNodeTarget {
            command: "resize".to_owned(),
            target: snapshot.target.display.clone(),
        });
    }
    match &snapshot.view {
        TableView::None => return Some(Refusal::TableMissing),
        TableView::Damaged { why, .. } => {
            return Some(Refusal::TableDamaged { why: why.clone() });
        }
        TableView::Gpt(_) | TableView::Mbr(_) => {}
    }
    match &snapshot.occupancy {
        Occupancy::Idle => None,
        Occupancy::Used { evidence } => Some(Refusal::Occupied {
            evidence: evidence.clone(),
        }),
        Occupancy::Unknown { why } => Some(Refusal::OccupancyUnknown { why: why.clone() }),
    }
}

fn finish(snapshot: &ResizeSnapshot, plan: Plan, note: String) -> Decision {
    match &snapshot.prior {
        PriorState::Absent => {}
        PriorState::Unreadable => {
            return Decision::Refuse(Refusal::RecordUnreadable {
                path: record_path(snapshot),
            });
        }
        PriorState::Present(prior) if prior.pending && prior.plan_digest != plan.digest() => {
            return Decision::Refuse(Refusal::RecordConflict {
                path: record_path(snapshot),
            });
        }
        PriorState::Present(_) => {}
    }
    Decision::Execute { plan, note }
}

fn record_path(snapshot: &ResizeSnapshot) -> String {
    snapshot
        .record
        .file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(record path unresolved)".to_owned())
}

fn unusable(snapshot: &ResizeSnapshot) -> Refusal {
    match &snapshot.view {
        TableView::None => Refusal::TableMissing,
        TableView::Damaged { why, .. } => Refusal::TableDamaged { why: why.clone() },
        TableView::Gpt(_) | TableView::Mbr(_) => Refusal::TableEdit {
            why: "the table bytes could not be read back for editing".to_owned(),
        },
    }
}

fn refuse_selector() -> Refusal {
    Refusal::TableEdit {
        why: "there is no partition to point the selector at".to_owned(),
    }
}

fn refuse(why: &str) -> Decision {
    Decision::Refuse(Refusal::TableEdit {
        why: why.to_owned(),
    })
}

/// 尺寸请求越出表可以表达的区间：拒绝写表，退出码 3，与基线一致（不是用法错误）。
fn beyond_table(why: impl Into<String>) -> Refusal {
    Refusal::TableEdit { why: why.into() }
}

/// 一个被 `--allow-move` 右移的 GPT 分区：条目下标与新旧起点，长度不变。
struct Relocation {
    index: usize,
    number: u32,
    old_start: u64,
    new_start: u64,
    sectors: u64,
}

impl Relocation {
    fn new_end(&self) -> u64 {
        self.new_start + self.sectors - 1
    }

    fn step(&self, sector: u64) -> Step {
        let shift = i64::try_from(i128::from(self.new_start) - i128::from(self.old_start))
            .unwrap_or(i64::MAX);
        Step::new(
            StepKind::MovePartition {
                partition: self.number,
                source_lba: self.old_start,
                offset_sectors: shift,
                chunk: DEFAULT_CHUNK,
                ntfs_hidden: false,
            },
            vec![region(self.new_start * sector, self.sectors * sector)],
        )
    }
}

/// `old_end` 之后的所有分区整体右移 `delta`，腾出目标终点所需的空间。
///
/// 只搬 `old_end` 右侧的分区（左侧与被扩分区无关），按起点从右到左排列执行序，
/// 右移时先动最右的，避免覆盖尚未搬走的分区。任何一个分区右移后越过
/// `last_usable` 即拒绝——搬不动就没有 `--allow-move` 能兑现的方案。
fn plan_relocations(
    table: &GptTable,
    old_end: u64,
    delta: u64,
    last_usable: u64,
) -> Result<Vec<Relocation>, Refusal> {
    let mut planned = Vec::new();
    for (index, entry) in table.entries.iter().enumerate() {
        if entry.partition_type_guid.is_zero() || entry.starting_lba <= old_end {
            continue;
        }
        let sectors = entry.ending_lba - entry.starting_lba + 1;
        let new_start = entry
            .starting_lba
            .checked_add(delta)
            .ok_or_else(|| beyond_table("the relocation target overflows the LBA range"))?;
        let new_end = new_start
            .checked_add(sectors - 1)
            .ok_or_else(|| beyond_table("the relocation target overflows the LBA range"))?;
        if new_end > last_usable {
            return Err(beyond_table(format!(
                "not enough room to relocate partition #{} — no space before last usable LBA {last_usable}",
                index + 1
            )));
        }
        planned.push(Relocation {
            index,
            number: u32::try_from(index + 1).unwrap_or(u32::MAX),
            old_start: entry.starting_lba,
            new_start,
            sectors,
        });
    }
    // 右移先动最右侧的，落盘顺序与数据搬运的依赖一致。
    planned.sort_by_key(|relocation| std::cmp::Reverse(relocation.old_start));
    Ok(planned)
}

fn region(start: u64, len: u64) -> diskedit_core::region::Region {
    diskedit_core::region::Region::sized(start, len).expect("由当前盘面导出的区间自洽")
}

// ---------------------------------------------------------------------------
// `--fs-only`：只动文件系统的两个形态（离线与挂载点在线）。
// ---------------------------------------------------------------------------

/// 判定 `resize --fs-only`：离线 `<TARGET>:N` 长满分区，或挂载点在线扩容。
pub fn decide_filesystem(snapshot: &FilesystemSnapshot, request: &Request) -> Decision {
    let Command::Resize(resize) = request.command() else {
        return refuse("resize --fs-only reached with a different command");
    };
    if resize.no_fs {
        return Decision::Refuse(Refusal::TableEdit {
            why: "--fs-only and --no-fs are mutually exclusive".to_owned(),
        });
    }
    if resize.partition.is_some() {
        fs_offline(snapshot)
    } else {
        filesystem_online(snapshot, resize.size.as_ref())
    }
}

/// 离线形态只扩容：文件系统末尾之前的分区空间此刻已经属于分区，工具把文件系统
/// 长满即可。
fn fs_offline(snapshot: &FilesystemSnapshot) -> Decision {
    if let Some(refusal) = fsops::admission(snapshot, "resize") {
        return Decision::Refuse(refusal);
    }
    let partition = match fsops::selected(snapshot) {
        Ok(partition) => partition,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let identity = match fsops::identity(snapshot) {
        Ok(identity) => identity,
        Err(refusal) => return Decision::Refuse(refusal),
    };
    let content = partition.content;
    match content {
        ContentKind::Unknown => {
            return Decision::Refuse(Refusal::FilesystemUnknown {
                number: partition.number,
            });
        }
        ContentKind::Swap => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: "resize --fs-only swap".to_owned(),
                why: "a swap area has no in-place resize; rebuild it at the target size".to_owned(),
            });
        }
        ContentKind::Lvm2Pv => {
            return Decision::Refuse(Refusal::NoFilesystem {
                number: partition.number,
                what: "an LVM physical volume".to_owned(),
            });
        }
        ContentKind::Filesystem(_) => {}
        ContentKind::Luks | ContentKind::BitLocker => {
            return Decision::Refuse(Refusal::NoFilesystem {
                number: partition.number,
                what: content.as_str().to_owned(),
            });
        }
    }
    match catalog::profile(content).support(Verb::Grow) {
        Support::Tools(_) => {}
        Support::Unsupported(why) => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resize --fs-only {}", content.as_str()),
                why: why.to_owned(),
            });
        }
        Support::NotApplicable => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resize --fs-only {}", content.as_str()),
                why: "this content kind carries no filesystem to resize".to_owned(),
            });
        }
    }
    let Some(filesystem) = content.filesystem() else {
        return Decision::Refuse(Refusal::ContentUnsupported {
            what: format!("resize --fs-only {}", content.as_str()),
            why: "this content kind carries no filesystem to resize".to_owned(),
        });
    };
    if let Some((program, _)) = snapshot.tools.iter().find(|(_, available)| !available) {
        return Decision::Refuse(Refusal::ToolMissing { program: *program });
    }
    let step = Step::new(
        StepKind::GrowFilesystem {
            partition: partition.number,
            filesystem,
            bytes: partition.bytes,
        },
        vec![fsops::region(partition)],
    );
    fsops::finish(
        snapshot,
        Plan::write(vec![step], identity),
        format!(
            "resized the filesystem on partition #{} (verify with: diskedit layout {})",
            partition.number, snapshot.target.display
        ),
    )
}

/// 挂载点形态：在用户已有的挂载上把文件系统撑满所在分区。
///
/// 不写分区表、不卸载挂载。尺寸只允许省略或等于分区容量：大于容量要先改表，
/// 小于容量是收缩，两者都在任何写盘之前拒绝。
fn filesystem_online(snapshot: &FilesystemSnapshot, requested: Option<&SizeSpec>) -> Decision {
    use crate::platform::MountFacts;

    let requested = match requested {
        None => None,
        Some(SizeSpec::Absolute(bytes)) => Some(*bytes),
        Some(_) => {
            return refuse("the mountpoint form takes an absolute BYTES value or none");
        }
    };
    let Some(MountFacts::Mounted {
        mount_point,
        device,
        disk,
        partition: number,
    }) = snapshot.mount.clone()
    else {
        return Decision::Refuse(Refusal::FactUnknown {
            what: "the mount point did not resolve to a mounted partition".to_owned(),
        });
    };
    let Some(selected) = snapshot
        .selected
        .as_ref()
        .filter(|facts| facts.number == number)
    else {
        return Decision::Refuse(Refusal::PartitionMissing { number });
    };
    if let Some(requested) = requested {
        if requested > selected.bytes {
            return Decision::Refuse(Refusal::CapacityInsufficient {
                needed: requested,
                available: selected.bytes,
            });
        }
        if requested < selected.bytes {
            return Decision::Refuse(Refusal::OnlineShrink {
                why: "this form grows the filesystem to fill its partition and never changes the \
                      partition table"
                    .to_owned(),
            });
        }
    }
    let content = selected.content;
    let Some(filesystem) = content.filesystem() else {
        return Decision::Refuse(Refusal::ContentUnsupported {
            what: format!("resize --fs-only {}", content.as_str()),
            why: "this content kind carries no filesystem to resize".to_owned(),
        });
    };
    match catalog::profile(content).support(Verb::Grow) {
        Support::Tools(_) => {}
        Support::Unsupported(why) => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resize --fs-only {}", content.as_str()),
                why: why.to_owned(),
            });
        }
        Support::NotApplicable => {
            return Decision::Refuse(Refusal::ContentUnsupported {
                what: format!("resize --fs-only {}", content.as_str()),
                why: "this content kind carries no filesystem to resize".to_owned(),
            });
        }
    }
    if let Some((program, _)) = snapshot.tools.iter().find(|(_, available)| !available) {
        return Decision::Refuse(Refusal::ToolMissing { program: *program });
    }
    let Ok(identity) = fsops::identity(snapshot) else {
        return Decision::Refuse(Refusal::IdentityUnprovable {
            why: "the target gives no comparable identity".to_owned(),
        });
    };
    let step = Step::new(
        StepKind::GrowFilesystemOnline {
            mount_point,
            device,
            disk,
            partition: number,
            filesystem,
            bytes: selected.bytes,
        },
        Vec::new(),
    );
    fsops::finish(
        snapshot,
        Plan::external(vec![step], identity),
        format!("resized the filesystem on partition #{number} through its mounted volume"),
    )
}
