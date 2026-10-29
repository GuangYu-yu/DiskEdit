//! 搬移 checkpoint（DKECKPT1）的编解码、原子落盘与归属校验。

use super::*;

pub const CKPT_MAGIC: &[u8; 8] = b"DKECKPT1";

/// 指纹含设备 ID 哈希（块设备整盘身份），尾部带身份种类
pub const CKPT_VERSION: u32 = 6;

/// checkpoint 批量提交粒度：性能参数，不属于恢复协议。恢复永远从最近一次
/// durable checkpoint 继续；此值只决定提交频率，即崩溃后最多重做多少个
/// 已完成但未持久化的 chunk。调整（或换成按字节/自适应策略）无需改恢复逻辑
pub(crate) const CKPT_BATCH_CHUNKS: u64 = 16;

/// 现场的两副面孔由 `P` 分开：写入侧只带当前目标给出的 [`crate::dev::LiveProof`]，
/// 解析侧带 [`crate::dev::IdentityProof`]。这样"把解析出的种类写回"在类型上就不成立，
/// 而不是靠序列化分支去兜
pub struct Checkpoint<P = crate::dev::IdentityProof> {
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
    pub proof: P,                         // 指纹里哪一类字段才是身份证据
}

impl Checkpoint<crate::dev::LiveProof> {
    pub(crate) fn serialize(&self) -> Vec<u8> {
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
        b.extend_from_slice(&self.fp.id.to_le_bytes());
        b.push(self.map.is_some() as u8);
        if let Some((off, limit)) = self.map {
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&limit.to_le_bytes());
        }
        // 身份种类追加在尾部：已有的字段偏移不因它变动
        b.push(self.proof.tag());
        let crc = table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }
}

impl Checkpoint<crate::dev::IdentityProof> {
    /// 判据来源 `lim` 是**已验证几何**（见 [`GeometryLimits`]）：分区号上界、可用区、
    /// 容器末端都取自盘上那张表的自述几何，故 256 槽位的表与 128 槽位的一样合法
    pub(crate) fn deserialize(b: &[u8], lim: &GeometryLimits) -> io::Result<Self> {
        // 前置只护 rd 之前的直接切片：magic(8) + version(4)。其后的字段逐个经 rd 兜底
        // 截断，尾部 CRC 读同样走 rd——手工枚举字段总长会随结构演进数错（少算时让给
        // rd 尚安全，多算则误拒合法现场），故不设第二份长度事实
        if b.len() < 12 {
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
        // moves 为空时它就是 grow 终点（grow_end_for），直接进表项 ending_lba：只验到
        // 容器末端不够——越过可用区末端会覆盖 GPT 备份头。上界取已验证几何的可用区
        if last_usable_lba > lim.last_usable_lba {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint last_usable_lba beyond usable area"));
        }
        let count = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap()) as usize;
        off += 4;
        // count 是盘上可控值：上界是该表的条目数，超界即损坏，防巨型 with_capacity
        if count as u64 > lim.entry_count as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint move count"));
        }
        let mut moves: Vec<PlanEntry> = Vec::with_capacity(count);
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
            // 目标区间起点是源起点加无符号位移，故 `first_lba` 的可用区下界已蕴含了新起点
            // 的下界；这里补的是 plan 生成器的另外两条保证：同一个分区只搬一次，被扩容的
            // 那个分区不参与搬移。CRC 自洽但违反它们的现场会让同一分区被搬两次，或把正在
            // 扩容的分区当成成员搬走，写进表项后不可逆
            if part_num == grow_part {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "implausible checkpoint move of the grow partition",
                ));
            }
            if moves.iter().any(|m| m.part_num == part_num) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "implausible checkpoint duplicate move partition number",
                ));
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
            id: u64::from_le_bytes(rd(off + 24, 8)?.try_into().unwrap()),
        };
        off += 32;
        let map = match rd(off, 1)?[0] {
            0 => None,
            1 => Some((
                u64::from_le_bytes(rd(off + 1, 8)?.try_into().unwrap()),
                u64::from_le_bytes(rd(off + 9, 8)?.try_into().unwrap()),
            )),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint mapping flag")),
        };
        off += 1 + if map.is_some() { 16 } else { 0 };
        let proof = {
            let tag = rd(off, 1)?[0];
            off += 1;
            crate::dev::IdentityProof::from_tag(tag).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint identity kind")
            })?
        };
        let stored = u32::from_le_bytes(rd(off, 4)?.try_into().unwrap());
        if table::crc32(&b[..off]) != stored {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint CRC mismatch"));
        }
        // 进度字段同属盘上可控值：CRC 自洽但值荒谬的 ckpt 会让续传把该条目的全部 chunk
        // 跳过后仍提交表项——表将指向从未复制过的位置。上界在解析期可算：当前条目的
        // chunk 总数（chunk 数按写入时确定性布局重算，自重叠条目 chunk 已收敛到位移
        // 以内，重做读到的仍是原始源数据；越界值只会跳过未写区间，故只可能损坏）
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
            // 与搬移循环同一份确定性布局（见 `move_chunk_layout`）：chunk 数按收敛后的
            // 块大小算，两侧口径一致才不会误判损坏
            Some(m) => move_chunk_layout(m.len_lba, ss, m.delta_lba, m.len_lba, chunk_bytes)
                .map(|(_, count)| count)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "checkpoint chunk count overflow"))?,
            // 全部条目已提交：进度归零，残留值即损坏
            None => 0,
        };
        if chunks_done > chunks_limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible checkpoint chunk progress"));
        }
        Ok(Checkpoint {
            disk_size, ss, grow_part, last_usable_lba, moves, kind, cur_index, chunks_done, chunk_bytes, fp, map, proof,
        })
    }
}

/// 当前目标与 checkpoint 记录的"创建时事实"比对：判据与 journal 共用 `dev::scene_mismatch`
/// （同一种异常在两条恢复路径上必须得到同一个结论）
pub(crate) fn checkpoint_foreign(
    src: &FileSource,
    fp: &crate::dev::TargetFingerprint,
    map: &Option<(u64, u64)>,
    proof: crate::dev::IdentityProof,
) -> Option<String> {
    let recorded = crate::dev::SceneIdentity { fp: *fp, map: *map, proof };
    crate::dev::scene_mismatch(&recorded, &src.scene())
}

/// 读 checkpoint 现场：在身份给出的候选落点上找**可读**的那份，不做目录扫描。
/// 多份候选同时可读即报歧义——猜错会把中断的搬移现场丢掉。
/// 记录的合法边界来自调用方已解析的几何（见 [`GeometryLimits`]）
///
/// 返回命中的落点路径，写与删消费同一路径。槽位空时返回候选首项，新作业照常落在那里
///
/// 候选逐个独立定性（与 `active_recovery_records` / `pick_journal` 同一口径）：
/// 不存在 → 跳过；损坏 → **记录后继续**看后面的候选——提前 return 会让
/// 一份损坏的候选遮住一份完好的候选，用户看到的是"目标坏了去 abandon"，
/// 而真实情况是现场完好可续跑。只有"所有候选都不可读"才把损伤如实报成 Infra
pub(crate) fn read_checkpoint(src: &FileSource, g: &ValidatedGeometry) -> Result<(CheckpointSlot, PathBuf), Fail> {
    let candidates = src.identity.checkpoint_candidates();
    let lim = g.limits();
    let mut found: Vec<(PathBuf, CheckpointSlot)> = Vec::new();
    let mut damaged: Vec<(PathBuf, String)> = Vec::new();
    // 现场可读但"创建时事实"对不上：不是损坏（CRC 自洽），也不可取信（继续执行会把
    // 为别的目标录下的现场续进换了文件/换了映射的目标）。单独归档，唯一时给出点名拒绝
    let mut foreign: Vec<(PathBuf, String)> = Vec::new();
    for path in candidates {
        // 与 journal 同一条判据：符号链接与非普通文件在 `read` 之前就记成"内容读不出来"。
        // 直接读会跟随链接，或在落点是 FIFO 时阻塞到有写端出现；这一份不可读只坏这一份，
        // 后面的候选照常看
        match crate::dev::recovery_path_metadata(path) {
            Ok(Some(_)) => {}
            Ok(None) => continue,
            Err(reason) => {
                damaged.push((path.clone(), reason));
                continue;
            }
        }
        let bytes = match std::fs::read(path) {
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
                if let Some(why) = checkpoint_foreign(src, &c.fp, &c.map, c.proof) {
                    foreign.push((path.clone(), why));
                } else {
                    found.push((path.clone(), CheckpointSlot::Relocation(Box::new(c))));
                }
            }
            (_, Ok(c)) => {
                if let Some(why) = checkpoint_foreign(src, &c.fp, &c.map, c.proof) {
                    foreign.push((path.clone(), why));
                } else {
                    found.push((path.clone(), CheckpointSlot::Resize(Box::new(c))));
                }
            }
            (Err(re), Err(_)) => damaged.push((path.clone(), re.to_string())),
        }
    }
    match found.len() {
        0 if foreign.is_empty() && damaged.is_empty() => {
            Ok((CheckpointSlot::Empty, candidates[0].clone()))
        }
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
            let (p, c) = found.swap_remove(0);
            Ok((c, p))
        }
        _ => {
            let paths = found.into_iter().map(|(p, _)| p).collect();
            Ok((CheckpointSlot::Ambiguous(paths), candidates[0].clone()))
        }
    }
}

/// 多份 checkpoint 同时可用 ⇒ 不猜：猜错会把中断的搬移现场丢掉
pub(crate) fn ambiguous_checkpoint(paths: &[PathBuf]) -> Fail {
    let listed: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    Fail::refused(format!("multiple checkpoints exist for this target — refusing: {}", listed.join(", ")))
}

/// 原子写：tmp → sync_all → rename → fsync 父目录。
/// 临时名带 PID + 进程内自增序号：同目录下的两个 ckpt 可能由同一进程并行写
/// （测试里就会发生），只用 PID 会撞同一个临时路径。前缀取自 `crate::dev::CKPT_TMP_PREFIX`
/// ——打开目标时按它清掉崩溃残骸
pub(crate) fn atomic_write_ckpt(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // 相对路径（如 "a.ckpt"）的 parent() 是空而非 None：空父目录拼出的临时路径会带根
    // 组件，落进当前盘的根目录
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
    // 落点目录归文件自己保证：路径由身份派生，身份不知道目录是否存在
    crate::dev::best_effort_mkdir(parent);
    // 暂存名带本落点的 token：清扫侧按同一前缀认领残骸（见 `ckpt_temp_prefix`）
    let tmp = parent.join(format!(
        "{}{}.{}",
        crate::dev::ckpt_temp_prefix(path),
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

/// checkpoint 槽位的占用者。两族作业（plan 型搬移 / 单分区 resize）共用同一路径，
/// 于是"解析不出本族的结构"**不等于**"槽位是空的"——把另一族作业的现场当空槽会直接覆盖它，
/// 使被中断的作业（表可能已改到半途）永久失去续传信息。故读取必须点名占用者，
/// 由调用方对"别人的作业"显式拒绝
pub(crate) enum CheckpointSlot {
    Empty,
    Relocation(Box<Checkpoint>),
    Resize(Box<RsCheckpoint>),
    /// 多份候选同时可解析：不猜，交调用方拒绝
    Ambiguous(Vec<PathBuf>),
}
