//! 单分区原地扩缩（resize-part）：自带 checkpoint（DKECKPT2），在提交前后处理 FS 与 NTFS 隐藏扇区。

use super::*;

// ---------- 通用 resize-part：grow / shrink / move 三合一 ----------

pub(crate) const CKPT2_MAGIC: &[u8; 8] = b"DKECKPT2";

/// 字段变动即升版本：版本号先行拦下旧布局，CRC 覆盖长度也随之不同。
/// 旧版本现场不会按"无 ckpt"重跑——read_checkpoint 把"现场存在但不可判读"报成 Infra
/// 并列明死因，出路是 abandon。
/// v4：追加目标指纹（3×u64）与 loop 映射（1+2×u64），语义同 CKPT1 v5
/// v5：追加 FS 类型码（见 [`FSTYPE_CODES`]）：续跑不再回源起点重探
/// v6：指纹加设备 ID 哈希（块设备整盘身份），尾部加身份种类；读 v5 时设备 ID 按零、
/// 种类按 `Legacy` 处理
/// v7：shrink 状态由 bool 变三态（字节语义变、布局不变）；v6 及更早只接受 0/1
pub(crate) const CKPT2_VERSION: u32 = 7;

/// v6：shrink 状态还是 bool（0 = 未缩、1 = 已验证）
const CKPT2_VERSION_BOOL_SHRINK: u32 = 6;

/// 兼容读取的最旧版本（没有设备 ID 与身份种类）
const CKPT2_VERSION_LEGACY: u32 = 5;

/// 现场记下的 FS 类型码：0 = 未识别，其余为数组下标 + 1。
/// **只允许在末尾追加**——已经在盘上的现场按码值回读，顺序变动即把类型读成另一个
const FSTYPE_CODES: &[FsKind] = &[
    FsKind::Squashfs,
    FsKind::Erofs,
    FsKind::Ext,
    FsKind::Xfs,
    FsKind::Btrfs,
    FsKind::F2fs,
    FsKind::Exfat,
    FsKind::Vfat,
    FsKind::Ntfs,
    FsKind::HfsPlus,
    FsKind::Apfs,
    FsKind::Swap,
    FsKind::Lvm2Pv,
];

fn fstype_code(kind: Option<FsKind>) -> u8 {
    match kind {
        None => 0,
        // 表覆盖全部变体（新增变体必须同时追加），取不到即代码与表脱节
        Some(k) => FSTYPE_CODES.iter().position(|c| *c == k).expect("every FsKind has a checkpoint code") as u8 + 1,
    }
}

fn fstype_from_code(code: u8) -> io::Result<Option<FsKind>> {
    match code {
        0 => Ok(None),
        c => FSTYPE_CODES
            .get(c as usize - 1)
            .copied()
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown resize checkpoint filesystem code")),
    }
}

/// checkpoint 记下的缩容状态。三态而非 bool：`Unverified` 表示缩容命令已返回、落点却
/// 回读不出来——搬数据按它继续就可能切进未缩的元数据，故不能与 `Verified` 等同
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum FsShrink {
    NotShrunk = 0,
    Verified = 1,
    Unverified = 2,
}

impl FsShrink {
    fn from_code(code: u8, tri_state: bool) -> io::Result<Self> {
        match (code, tri_state) {
            (0, _) => Ok(Self::NotShrunk),
            (1, _) => Ok(Self::Verified),
            (2, true) => Ok(Self::Unverified),
            // v6 及更早这一字节是 bool，2 只能是损坏
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "implausible resize checkpoint shrink state")),
        }
    }
}

/// resize-part 的 checkpoint（v7）：搬移覆盖源区，中途断电后新旧两处都不完整，
/// 已完成位置只能由 checkpoint 判定。"表项是否已提交"不在这里存——盘上的分区几何
/// 就是那个事实，另存一份标记只会与它分叉（见 prepare_resize 的恢复分支）
pub(crate) struct RsCheckpoint {
    pub(crate) disk_size: u64,
    pub(crate) ss: u64,
    pub(crate) part: u32,
    pub(crate) old_start: u64,
    pub(crate) old_end: u64,
    pub(crate) new_start: u64,
    pub(crate) new_end: u64,
    pub(crate) fs_shrunk: FsShrink,
    pub(crate) chunks_done: u64,
    pub(crate) chunk_bytes: u64,
    pub(crate) fp: crate::dev::TargetFingerprint, // 目标文件指纹（"同路径换文件"防线）
    pub(crate) map: Option<(u64, u64)>,           // 创建时的 loop 映射；None = 非 loop 进入
    pub(crate) proof: crate::dev::IdentityProof,  // 指纹里哪一类字段才是身份证据
    /// 识别出的 FS 类型：续跑直接用它，不再回源起点重探（源区头部可能已被搬移数据覆盖）
    pub(crate) fstype: Option<FsKind>,
}

impl RsCheckpoint {
    pub(crate) fn serialize(&self) -> Vec<u8> {
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
        b.extend_from_slice(&self.fp.id.to_le_bytes());
        b.push(self.map.is_some() as u8);
        if let Some((off, limit)) = self.map {
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&limit.to_le_bytes());
        }
        // 类型码与身份种类追加在尾部：已有的字段偏移不因它们变动
        b.push(fstype_code(self.fstype));
        b.push(self.proof.tag());
        let crc = table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }
    pub(crate) fn deserialize(b: &[u8], lim: &GeometryLimits) -> io::Result<Self> {
        // 前置只护边界检查 get 之前的直接切片：magic(8) + version(4)。其后字段逐个经
        // get 兜底截断，尾部 CRC 读也走 get——截断一律 InvalidData 而非 panic
        if b.len() < 12 || &b[0..8] != CKPT2_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint invalid"));
        }
        let ver = u32::from_le_bytes(b[8..12].try_into().unwrap());
        if ver != CKPT2_VERSION && ver != CKPT2_VERSION_BOOL_SHRINK && ver != CKPT2_VERSION_LEGACY {
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
        let fs_shrunk = FsShrink::from_code(
            *b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?,
            ver >= CKPT2_VERSION,
        )?;
        o += 1;
        let chunks_done = rd64(b, &mut o)?;
        let chunk_bytes = rd64(b, &mut o)?;
        // 指纹与映射属于"创建时事实"：解析只负责取值，比对在 read_checkpoint。
        // v5 没有设备 ID 字段，读作零（无证据）
        let fp = crate::dev::TargetFingerprint {
            dev: rd64(b, &mut o)?,
            ino: rd64(b, &mut o)?,
            size: rd64(b, &mut o)?,
            id: if ver >= CKPT2_VERSION_BOOL_SHRINK { rd64(b, &mut o)? } else { 0 },
        };
        // 标志字节先行跨过，再按标志决定是否读它后面那两个 u64
        let flagged = *b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?;
        o += 1;
        let map = match flagged {
            0 => None,
            1 => Some((rd64(b, &mut o)?, rd64(b, &mut o)?)),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible resize checkpoint mapping flag")),
        };
        // 类型码在尾部：认不出的码即现场不可判读，不退回"未识别"继续跑
        let fstype = fstype_from_code(*b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?)?;
        o += 1;
        let proof = if ver >= CKPT2_VERSION_BOOL_SHRINK {
            let tag = *b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?;
            o += 1;
            crate::dev::IdentityProof::from_tag(tag).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "implausible resize checkpoint identity kind")
            })?
        } else {
            crate::dev::IdentityProof::Legacy
        };
        if table::crc32(&b[..o])
            != u32::from_le_bytes(
                b.get(o..o + 4)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?
                    .try_into()
                    .unwrap(),
            )
        {
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
        // 进度上界同 Checkpoint：搬移区间的 chunk 总数，块布局取自唯一实现
        // （见 `move_chunk_layout`）。拷贝总长与搬移循环同口径：move+shrink 只拷新旧
        // 区间的较小者，两侧一致才不会误判损坏
        let moved = old_start.abs_diff(new_start);
        let old_len = old_end - old_start + 1;
        let copy_len = old_len.min(new_end - new_start + 1);
        let chunks_limit = move_chunk_layout(copy_len, ss, moved, old_len, chunk_bytes)
            .map(|(_, count)| count)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "resize checkpoint chunk count overflow"))?;
        if chunks_done > chunks_limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible resize checkpoint chunk progress"));
        }
        Ok(RsCheckpoint {
            disk_size, ss, part, old_start, old_end, new_start, new_end,
            fs_shrunk, chunks_done, chunk_bytes, fp, map, fstype, proof,
        })
    }
}

/// 本次 resize 请求的目标形状——与 ckpt、盘上几何并列的第三个维度
pub(crate) struct Requested {
    pub(crate) part: u32,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) chunk_len: u64,
}

/// resize 的恢复判定结论。三个维度缺一不可：
///   requested = 本次请求的目标区间
///   ckpt      = 持久状态（搬移前/后区间、FS 是否已缩、chunk 进度）
///   on_disk   = 表里**当前**的分区区间
///
/// 判据不能只取其中两个。"盘上几何 == ckpt.new_"只说明表已提交，还要问本次请求是否就是那次
/// 作业的目标；少了后一问，崩溃后改了 --end 重跑会被判成"已提交，收尾即可"，
/// 于是表不动、FS 不动，却报成功——用户拿着一次什么都没做的成功去用那块空间
pub(crate) enum RestoreState<'a> {
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
pub(crate) fn classify_restore<'a>(
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
    src: &mut WriteSource,
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
    let p = match prepare_resize(src, g, repair, part, new_start, new_end, chunk_len, no_fs, log) {
        Ok(p) => p,
        Err(f) => return f.into_outcome(),
    };
    // 修复先于任何目标写入；放 Fail 域以保住分类（同 apply）
    if let Some(what) = p.repair.describe() {
        if let Err(f) = gpt_policy::apply_repair(src, &p.repair, g.sector_size()) {
            return f.into_outcome();
        }
        log(&format!("[repair] {what}"));
    }
    fault_after_repair();
    // 越界之后只剩两条出口：写入完成，或"盘可能已改变"的执行失败
    let mut post = PostWrite::new();
    match execute_resize(src, p, log, &mut post) {
        Ok(()) => post.applied(),
        Err(e) => post.failed(e.to_string()),
    }
}

/// 本次对 FS 的动作。prepare 算一次，execute 只 match——若两处各自从尺寸重新推导，
/// "要不要缩/扩"就有了两个来源（尺寸没变、缩容已完成、`--no-fs` 都落进 `Leave`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsAction {
    /// 不碰 FS
    Leave,
    Shrink,
    Grow,
}

/// 事前判定的产物，也是 execute 的**唯一输入源**
pub(crate) struct ResizeDecision {
    pub(crate) ss: u64,
    pub(crate) part: u32,
    pub(crate) old_start: u64,
    pub(crate) old_end: u64,
    pub(crate) new_start: u64,
    pub(crate) new_end: u64,
    pub(crate) fstype: Option<FsKind>,
    pub(crate) fs_shrunk: FsShrink,
    pub(crate) resume_chunks: u64,
    pub(crate) committed_old: Option<(u64, u64)>,
    /// 本次要对 FS 做的动作（唯一决策键：preflight 与 execute 同键）
    pub(crate) fs_action: FsAction,
    pub(crate) repair: RepairAction,
    pub(crate) new_bytes: u64,
    pub(crate) chunk_len: u64,
    pub(crate) no_fs: bool,
    pub(crate) ckpt_path: PathBuf,
    /// 随行几何：执行半程的表项提交基于它（不重读盘上的表）
    pub(crate) g: ValidatedGeometry,
}

/// 事前判定（只读）：几何、范围、重叠、槽位、恢复三态、FS 能力与工具 preflight。
/// 返回 `Err` 一律发生在任何写盘之前；签名里没有 `&mut WriteSource`，因此它**不可能**写盘
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_resize(
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
    let (slot, ckpt_path) = read_checkpoint(src, &g)?;
    let ss = g.sector_size();
    let e = gpt_policy::live_entry(&g, part)?;
    // 占用闸：搬移/改写一个被挂载或活动 swap 的分区会让现场失效。镜像无块设备占用
    // 语义，闸内自会放行（支持边界见 README）。起点不变、末端不变或只增的请求按 Extend
    // 放行 DM 栈持有（LVM PV / dm-crypt 的增长先长分区再调栈）；搬移或缩小要求无持有者
    let edit = if new_start == e.starting_lba && new_end >= e.ending_lba {
        crate::fsops::WriteKind::Extend
    } else {
        crate::fsops::WriteKind::Overwrite
    };
    crate::fsops::ensure_idle_before_write(src, part, e.starting_lba.checked_mul(ss)
        .ok_or_else(|| Fail::infra("partition start overflows byte offset"))?, edit)?;
    // 只拦搬移（起点变化）：LUKS/LVM PV/swap 的纯扩缩不搬数据，允许。
    // swap 在此一并拒绝：plan/apply 有 mkswap 重建流程，单分区 resize-part 没有
    if (is_forbidden_type_guid(&e.partition_type_guid) || is_swap_guid(&e.partition_type_guid))
        && new_start != e.starting_lba
    {
        return Err(Fail::refused("swap/LUKS/LVM PV relocation refused (size-only changes are allowed)"));
    }
    if new_start < g.first_usable_lba() || new_end > g.last_usable_lba() || new_start > new_end {
        return Err(Fail::refused(format!(
            "new range {new_start}..{new_end} outside usable {}..{}",
            g.first_usable_lba(), g.last_usable_lba()
        )));
    }
    let old_start = e.starting_lba;
    let old_end = e.ending_lba;
    for (i, other) in g.entries().iter().enumerate() {
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
    let (fs_shrunk, resume_chunks, committed_old, recorded_fstype) = match state {
        RestoreState::Fresh => (FsShrink::NotShrunk, 0, None, None),
        // Y = durable 恢复点（ckpt 文件里的值），不是本进程内存进度
        RestoreState::Resume(c) => {
            log(&format!("resuming at chunk {} (durable checkpoint)", c.chunks_done));
            (c.fs_shrunk, c.chunks_done, None, Some(c.fstype))
        }
        RestoreState::Committed(c) => {
            log("resize already committed — finishing filesystem step");
            (c.fs_shrunk, c.chunks_done, Some((c.old_start, c.old_end)), Some(c.fstype))
        }
        // 有 ckpt 而三个维度对不上：不猜是哪一次作业，也不当作"没有 ckpt"重做一遍
        RestoreState::Divergent(why) => {
            return Err(Fail::refused(format!("existing checkpoint does not match this resize — refusing: {why}")));
        }
    };

    // 阶段 0/3 的 FS 大小判据：FS 尺寸在已提交态下取 ckpt 记录的搬移前形状（旧区间），
    // 否则取本次请求的旧形状；FS 位置始终以盘上条目起点为准
    let (fb_start, fb_end) = committed_old.unwrap_or((old_start, old_end));
    let byte_overflow = || Fail::infra("resize LBA arithmetic overflows byte offset");
    let old_bytes = (fb_end - fb_start + 1).checked_mul(ss).ok_or_else(byte_overflow)?;
    let new_bytes = (new_end - new_start + 1).checked_mul(ss).ok_or_else(byte_overflow)?;
    // 表项 LBA 的单位是表自身的 ss。identify 只读目标内容，仍属事前判定。
    // 续跑取 ckpt 记下的类型：源区头部可能已被搬移数据覆盖（自重叠右移且本次扩容时），
    // 回源起点重探会认不出 FS，把"只差提交"的作业拒掉
    let fstype = match recorded_fstype {
        Some(kind) => kind,
        None => crate::fsid::identify(src, old_start.checked_mul(ss).ok_or_else(byte_overflow)?,
            (old_end - old_start + 1).checked_mul(ss).ok_or_else(byte_overflow)?).map_err(Fail::infra_io)?,
    };

    // preflight：本操作的后置条件含 FS 调整，工具缺失必须在阶段 0
    // （缩容时 FS shrink 即首次写盘）之前拒绝，否则会留下"表已改、FS 未改"的中间态。
    // --no-fs 与缩容不可共存：分区末端越过未缩的 FS 元数据即数据损坏，这不是"少做一步"
    // 而是另一种操作，必须在写盘前拒绝（已缩过 FS 的续传不在此列）
    let needs_shrink = new_bytes < old_bytes && fs_shrunk != FsShrink::Verified;
    if no_fs && needs_shrink {
        return Err(Fail::refused(
            "--no-fs cannot shrink: the filesystem has to be shrunk first, otherwise the new partition end would cut into filesystem metadata",
        ));
    }
    // 已提交而 FS 步仍未完成的扩容：--no-fs 会让收尾以"无事可做"结束并撤掉 ckpt，
    // 而盘上条目已停在新区间——之后再跑算出的差值为零，只会得到 Leave，那条待办
    // 从此没有任何持久记录
    if no_fs && committed_old.is_some() && new_bytes > old_bytes {
        return Err(Fail::refused(
            "--no-fs cannot drop the filesystem step of an unfinished resize: the partition is already extended but its filesystem is not — re-run without --no-fs",
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
        FsAction::Shrink => crate::fsops::check_shrink(fstype, src.is_block,
            crate::fsops::needs_loop(&crate::fsops::DeviceScope::Partition(part), src.is_block))?,
        // 扩的目标与写盘后的收尾取同一判据（grow_target_at）：要动的那段可能是分区尾部的
        // RW overlay 层，也可能是分区自己。这道写盘前的判定与收尾同源，缺了它就会把
        // "里面是什么都不知道"当成功放过去——正因如此它必须在这里，而不是写表之后
        FsAction::Grow => {
            // FS 现在占据的区间 = 盘上条目起点 × ckpt 记录的搬移前长度：Fresh/Resume 下
            // 就是旧区间；Committed 下表项已停在新位置，probe 旧起点会探到空段。
            // 与收尾 finalize_growth 探测的是同一段字节，两侧结论不会分叉
            crate::fsops::check_grow_step(crate::fsops::grow_target_at(src, part,
                old_start.checked_mul(ss).ok_or_else(byte_overflow)?, old_bytes)?, src.is_block)?;
        }
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
        ckpt_path,
        g,
    })
}

// ---- durable boundary：以上判定全部结束，以下开始写盘 ----

/// 执行（写盘）。入参 `&mut WriteSource` + 返回 `io::Result` 共同构成边界：
/// 函数体内**构造不出** `Fail::Refused`（返回类型里放不下它），
/// 于是"写盘之后还能返回 Refused"在这里连编译都过不去
pub(crate) fn execute_resize(
    src: &mut WriteSource,
    p: ResizeDecision,
    log: &mut dyn FnMut(&str),
    post: &mut PostWrite,
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
        fstype,
        proof: src.proof(),
    };
    let save = |c: &RsCheckpoint| atomic_write_ckpt(&p.ckpt_path, &c.serialize());
    // 撤 ckpt 的判据是"本次作业没留下任何待办"，所以基数必须在**开工前**取：阶段 0 的
    // 卸载失败、阶段 2 的 HiddenSectors 未修同样会让重跑按 Fresh 算出"尺寸未变 → Leave"
    // 而报成功，只拿阶段 3 的基数就看不见它们
    let pending_before = post.len();

    // 纯扩容（不搬移、无缩容）没有其余 save 落点：FS 步失败或崩溃后，盘上条目已是新
    // 尺寸，重跑会按 Fresh 算出 old_bytes == new_bytes → Leave 而报成功，FS 永不扩。
    // 开工即落 ckpt，重跑经 Committed 分支用 ckpt 记录的搬移前区间判 Grow、补试 FS 步；
    // 成功后照常在收尾撤除
    if p.fs_action == FsAction::Grow && new_start == old_start && committed_old.is_none() {
        save(&ckpt)?;
    }

    // ---- 阶段 0：缩容 FS 先缩（只做一次）----
    // 本块只在 !no_fs 时可达（no_fs + 缩容 + FS 未缩已在上面整体拒绝），因此
    // "能否缩 / 类型是否认得 / 工具是否齐备"三重判据已由上面的 check_shrink 一次性给出。
    // 此处不另设判据，以 check_shrink 的三重结果（能否缩 / 类型 / 工具）为准
    if p.fs_action == FsAction::Shrink {
        // 缩容**不**落不可回滚标记：缩小之后再回滚表项只会得到"FS 小于分区"这一自洽状态
        // （与扩容方向相反），undo 仍然安全可用。判据是可逆性，不是"有没有调外部工具"。
        // shrink_fs 的契约按旧分区尺寸比对（FS 小于分区时实为良性扩容，见其文档）：
        // new_bytes ≤ 旧分区尺寸由 plan 的缩容方向保证
        // FS 已缩，卸载的成败是另一条后置条件：它不改写缩容这一事实，只作为待办浮出。
        // 回读不出来（缩容可能已完成、落点未证实）同理——两者都是"核心操作已完成、
        // 某项未收口"
        // 上次缩容命令已返回但落点未证实：重试前先回读实际尺寸，已缩到位就不再缩一次
        let mut shrink_done = false;
        if p.fs_shrunk == FsShrink::Unverified {
            match crate::fsops::shrink_already_at(src, part, fstype, new_bytes) {
                Some(true) => {
                    ckpt.fs_shrunk = FsShrink::Verified;
                    save(&ckpt)?;
                    log(&format!("filesystem already at {new_bytes} bytes (re-read) — shrink step skipped"));
                    fault_rs_after_fs_shrink();
                    shrink_done = true;
                }
                Some(false) => {}
                None => {
                    // 回读仍不可用：缩容是否到位无从判断，搬数据一旦切进未缩的元数据就不可逆
                    post.pending(crate::outcome::Pending::new(
                        part,
                        crate::outcome::PendingKind::Other("fs shrink"),
                        "the filesystem size still cannot be read back, so the earlier shrink cannot be confirmed".to_string(),
                        "inspect the filesystem size by hand, then re-run the same command".to_string(),
                    ));
                    return Ok(());
                }
            }
        }
        if !shrink_done {
            let o = crate::fsops::shrink_fs(src, part, fstype, new_bytes)?;
            // 落点证实了才往下搬：回读不出实际尺寸时无法排除"工具以 0 退出但没缩到位"，
            // 而接着搬数据就会把分区末端切进未缩的 FS 元数据。此时停在缩容这一步，
            // ckpt 的 fs_shrunk 记为 Unverified——重跑先回读，仍未缩才重做一次缩容
            let line = o.verify.ok_line(new_bytes);
            if let Some(p) = o.verify.pending(part) {
                post.pending(p);
            }
            if let Some(leak) = o.leak {
                post.pending(leak.into_pending(part));
            }
            if line.is_none() {
                ckpt.fs_shrunk = FsShrink::Unverified;
                save(&ckpt)?;
                return Ok(());
            }
            ckpt.fs_shrunk = FsShrink::Verified;
            save(&ckpt)?;
            if let Some(l) = line {
                log(&l);
            }
            fault_rs_after_fs_shrink();
        }
    }

    // ---- 阶段 1：数据搬移（方向感知 + chunk 续传）----
    // 方向与幅度分开算：abs_diff 不经 i64 cast，差值再大也不会翻转符号
    let forward = new_start > old_start;
    let moved = new_start.abs_diff(old_start);
    if moved != 0 && committed_old.is_none() {
        // 开工即落 ckpt，再落屏障——顺序不可换：屏障一旦落地 undo 即永久失效，
        // ckpt 若晚于它，二者之间中断就"既不能回滚也不能续传"。纯搬移（Leave）不命中
        // 上面的 Grow 提前落点，唯一的开工落点在这里（execute_apply 的同族顺序与此一致）
        save(&ckpt)?;
        // 含数据搬移：字节不入 journal，落屏障令 undo 拒绝（前向恢复、无回滚）
        src.set_mutation(crate::dev::Mutation::RelocatePartition {
            partition: part,
            from_lba: old_start,
            to_lba: new_start,
        });
        src.mark_non_reversible()?;
        // 拷贝长度取新旧区间的较小者：move+shrink 时 FS 已在阶段 0 缩到 new_bytes，
        // 旧区间尾部是死数据；按旧区间全长拷贝会让写落点越过 new_end 进相邻分区。
        // 恢复重放由盘上几何（表未提交时 old 区间不变）重算出同一 copy_len，chunk 进度口径一致
        let copy_len = (old_end - old_start + 1).min(new_end - new_start + 1);
        let byte_overflow = || io::Error::new(io::ErrorKind::InvalidData, "relocation byte offset overflows — refusing");
        let src_off = old_start.checked_mul(ss).ok_or_else(byte_overflow)?;
        let dst_off = new_start.checked_mul(ss).ok_or_else(byte_overflow)?;
        let total = copy_len.checked_mul(ss).ok_or_else(byte_overflow)?;
        // 自重叠搬移（位移小于分区长度）下，重做 chunk 的源区间会被先落盘 chunk 的写
        // 覆盖，除非每个 chunk 不大于位移（两个方向同理：写区间起点与重做源区间终点
        // 至少隔一个位移）。chunk 收敛到位移字节以内；此处本就逐 chunk checkpoint。
        // 块布局取自唯一实现，与 checkpoint 的进度上界同源
        let (entry_chunk, _) = move_chunk_layout(copy_len, ss, moved, old_end - old_start + 1, chunk_len)
            .ok_or_else(byte_overflow)?;
        let order: Vec<(u64, u64)> = {
            let mut v = Vec::new();
            let mut pos = 0u64;
            while pos < total {
                let len = entry_chunk.min(total - pos);
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
        g2.update_entry((part - 1) as usize, |te| {
            te.starting_lba = new_start;
            te.ending_lba = new_end;
        })?;
        g2.commit(src)?;
        table::ensure_protective_mbr(src, ss)?;
        // 提交后不写 ckpt：恢复分支以盘上几何判定"是否已提交"，不需要第二份标记。
        // 此处中断后的重放由表项的绝对赋值保证幂等 —— fault_rs_after_commit 正测这一点
        log(&format!("partition {part} committed at {new_start}..{new_end}"));
        fault_rs_after_commit();
    }

    // 起始 LBA 变了才需要修 NTFS HiddenSectors（扩缩不动 start 时跳过；
    // 已提交态续跑时盘上几何已是新值，搬移与否改由 ckpt 记录的搬移前起点判定）
    let start_moved = committed_old.map_or(moved != 0, |(os, _)| os != new_start);
    if start_moved && fstype == Some(FsKind::Ntfs) {
        fix_ntfs_hidden_sectors(src, new_start, ss, log)?;
    }

    // ---- 阶段 3：扩容 FS ----
    // 要不要扩由 prepare 的决策给出，此处不比尺寸；家族处置与 apply 共用同一实现
    if p.fs_action == FsAction::Grow {
        // 扩到设备末端的工具以内核尺寸为准：内核视图停在旧尺寸时会以"无需处理"退出 0。
        // 重读失败时不跑 FS 步，把扩容记成待办
        if crate::support::kernel_resync(src) {
            finalize_growth(src, part, GrowthRegions {
                old: (old_start, old_end - old_start + 1),
                new: (new_start, new_end - new_start + 1),
            }, ss, log, post)?;
        } else {
            post.pending(Pending::new(
                part,
                PendingKind::Fs,
                "the kernel partition view still reports the old size, so the filesystem was not resized — refresh the view (partprobe / partx) and re-run to finish",
                String::new(),
            ));
        }
    } else if no_fs {
        log("partition resized (--no-fs: filesystem left untouched)");
    }
    // ckpt 在本次作业的所有后置条件定论后才撤：留下任何待办而 ckpt 已删时，重跑会按
    // 全新作业从旧区重拷（自重叠搬移下旧区字节已被写覆盖，重拷即把覆盖后的数据盖回
    // 新区），或算出"尺寸未变 → Leave"而报成功、把没做完的那一步永远留在盘上
    if post.len() == pending_before && !crate::dev::remove_recovery_file(&p.ckpt_path) {
        // 撤不掉就不算收干净：残留现场会让下一次 resize 被判 Divergent 而拒绝
        post.pending(Pending::new(
            part,
            PendingKind::Other("checkpoint"),
            format!(
                "the job finished but its checkpoint {} could not be removed — remove it by hand",
                p.ckpt_path.display()
            ),
            String::new(),
        ));
    }
    Ok(())
}

pub(crate) fn ntfs_hidden_value(start_lba: u64) -> (u32, bool) {
    if start_lba >> 32 == 0 { (start_lba as u32, false) } else { (0u32, true) }
}

/// NTFS boot sector BPB HiddenSectors（偏移 0x1C，u32 LE，值 = 分区起始 LBA；
/// NTFS 引导扇区布局，ntfs-3g bootsect.c 与微软 NTFS 规范同此定义）。
/// 单位是设备逻辑扇区（4Kn 原生盘上即 4Kn LBA，不折算 512）：bytes_per_sector 记录
/// 各自的扇区大小，hidden_sectors 语义随它计。字段只有 32 位：start ≥ 2^32 时写 0 并
/// 警告（Windows 将无法从该分区引导，数据不受影响）。仅在分区起始 LBA 变化后调用。
pub(crate) fn fix_ntfs_hidden_sectors(src: &mut WriteSource, new_first_lba: u64, ss: u64, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    let (val, warn) = ntfs_hidden_value(new_first_lba);
    // 与 execute_apply / execute_copy 的 LBA→字节换算同口径：起点来自调用方传入的
    // 几何，裸乘回绕会把 HiddenSectors 写到盘上另一个位置
    let off = new_first_lba
        .checked_mul(ss)
        .and_then(|b| b.checked_add(0x1C))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("NTFS HiddenSectors offset overflows ({new_first_lba} × {ss} + 0x1C)"),
            )
        })?;
    src.write_at(off, &val.to_le_bytes())?;
    src.sync_data()?;
    if warn {
        log("warning: partition starts beyond 2^32; HiddenSectors zeroed — Windows cannot boot from it");
    }
    Ok(())
}