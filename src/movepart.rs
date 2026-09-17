//! 分区搬移与中间分区扩容闭环。
//!
//! 前向恢复、无回滚；单分区 = 单事务；checkpoint 原子写
//! （tmp → sync → rename → fsync 父目录）；复制方向：delta ≥ 0 且尾→头推进，
//! 写点恒在未读源之上（右移时目的地址总是大于已读位置），顺序固化不提供方向参数。

use crate::dev::FileSource;
use crate::table::{self, RawGpt};
use std::io;

pub const CKPT_MAGIC: &[u8; 8] = b"DKECKPT1";
pub const CKPT_VERSION: u32 = 3;

/// chunk 字节数：命令行 --chunk-size（MiB）传入；checkpoint 记录该值，
/// 续传时不一致即拒绝（chunks_done 是按 chunk 计数的，换大小会错位）
pub fn chunk_bytes(mib: u64) -> io::Result<u64> {
    if !(1..=1024).contains(&mib) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "--chunk-size out of range (1..=1024 MiB)"));
    }
    Ok(mib * 1024 * 1024)
}

/// 拒绝搬移（起始 LBA 变化）的 GPT 类型 GUID：swap/LUKS/LVM PV——swap 内容可弃可重建，
/// LUKS/LVM 搬移后其元数据语义不明。纯扩缩（起点不动、数据不搬）不受此限。
/// 同时匹配磁盘字节序（混合端）与内存自然序，两种布局都拦。
const FORBIDDEN_TYPE_GUIDS: [[u8; 16]; 6] = [
    // swap 0657FD6D-A4AB-43C4-84E5-0933C84B4F4F
    [
        0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F,
    ],
    *b"\x06\x57\xFD\x6D\xA4\xAB\x43\xC4\x84\xE5\x09\x33\xC8\x4B\x4F\x4F",
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

/// swap GUID 两种字节序（磁盘混合端 / 内存自然序）
fn is_swap_guid(g: &[u8; 16]) -> bool {
    *g == [0x6D, 0xFD, 0x57, 0x06, 0xAB, 0xA4, 0xC4, 0x43, 0x84, 0xE5, 0x09, 0x33, 0xC8, 0x4B, 0x4F, 0x4F]
        || *g == *b"\x06\x57\xFD\x6D\xA4\xAB\x43\xC4\x84\xE5\x09\x33\xC8\x4B\x4F\x4F"
}

pub struct Plan {
    pub ss: u64,
    pub last_usable_lba: u64,
    pub grow_part: u32,
    pub moves: Vec<PlanEntry>,
    /// 待修复项：plan 只记录，apply 执行（plan 本身不写盘）
    pub repair: Option<Repair>,
}

/// plan 阶段发现的、需在 apply 阶段执行的修复项
#[derive(Clone)]
pub struct Repair {
    /// 盘上 backup GPT 当前所在 LBA（仅用于打印）
    pub backup_lba: u64,
    /// 目标末端 LBA（设备最后一个 LBA）
    pub file_last_lba: u64,
    /// 备份头是否需搬移；false = 备份头已在末端，只有 SizeInLBA 过期的 PMBR 需重写
    pub backup_stale: bool,
}

/// 修复后的 last_usable_lba = 新末端 − 数组跨度 − 1（备份数组位于备份头之前）。
/// 容器容不下最小跨度、或现有分区越出新区间 → 拒绝（不写盘，纯计算）
pub fn repaired_last_usable(g: &table::RawGpt, file_last_lba: u64) -> io::Result<u64> {
    let span = table::array_span_sectors(g.header.number_of_partition_entries, g.header.size_of_partition_entry, g.ss);
    let new_last_usable = file_last_lba
        .checked_sub(span)
        .and_then(|v| v.checked_sub(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "container too small for GPT backup header — refusing"))?;
    let max_end = g.entries.iter().filter(|e| e.ending_lba != 0).map(|e| e.ending_lba).max().unwrap_or(0);
    if max_end > new_last_usable {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "partitions exceed new usable range after enlarge"));
    }
    Ok(new_last_usable)
}

/// 判定该表是否需要修复（不写盘）：backup 头不在末端（Stale）或 PMBR SizeInLBA 过期。
/// PMBR 不一致（SizeInLBA 大于容器）→ 拒绝自动修复，交用户外部处理
pub fn classify_repair(g: &table::RawGpt, file_last_lba: u64) -> io::Result<Option<Repair>> {
    if g.pmbr == table::PmbrSize::Inconsistent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "protective MBR SizeInLBA exceeds container — refusing auto-repair (use sgdisk/parted)",
        ));
    }
    let backup_stale = g.state != table::GptState::Valid;
    if !backup_stale && g.pmbr == table::PmbrSize::Normal {
        return Ok(None);
    }
    Ok(Some(Repair { backup_lba: g.header.backup_lba, file_last_lba, backup_stale }))
}

/// 执行修复（写入路径）：搬移备份头（如需）→ 重写保护 MBR → 重读校验
pub fn perform_repair(src: &mut FileSource, r: &Repair) -> io::Result<()> {
    if r.backup_stale {
        let mut g = table::load_gpt(src)?.ok_or_else(|| io::Error::other("GPT vanished before repair"))?;
        g.header.last_usable_lba = repaired_last_usable(&g, r.file_last_lba)?;
        table::commit_gpt(src, &g, r.file_last_lba)?;
    }
    table::ensure_protective_mbr(src)?;
    let g2 = table::load_gpt(src)?.ok_or_else(|| io::Error::other("re-read after repair failed"))?;
    if g2.state != table::GptState::Valid || g2.pmbr != table::PmbrSize::Normal {
        return Err(io::Error::other("repair did not converge — refusing"));
    }
    Ok(())
}

/// plan 生成：只解析 + 验证 + 计算，不写盘。stale 表按"修复后"的几何生成计划，
/// 并把修复动作记入 plan.repair，由 apply 执行
pub fn make_plan(src: &mut FileSource, grow_part: u32) -> io::Result<Plan> {
    let mut g = table::load_gpt(src)?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no GPT"))?;
    let file_last_lba = src.size / g.ss - 1;
    let repair = classify_repair(&g, file_last_lba)?;
    if let Some(r) = &repair
        && r.backup_stale
    {
        g.header.last_usable_lba = repaired_last_usable(&g, file_last_lba)?;
    }
    let ss = g.ss;

    // 目标分区必须存在
    let target = g.entries.get((grow_part - 1) as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("partition {grow_part} not found")))?;
    if target.ending_lba == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("partition {grow_part} is empty")));
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
        if e.ending_lba < e.starting_lba {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("partition {num} has ending LBA below starting LBA (corrupted table)")));
        }
        let len = e.ending_lba - e.starting_lba + 1;
        // LUKS/LVM 拒绝；swap 走"重建"而非搬移：内容可弃，UUID/PARTUUID/分区号保持
        if FORBIDDEN_TYPE_GUIDS.contains(&e.partition_type_guid) && !is_swap_guid(&e.partition_type_guid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("partition {num} is LUKS/LVM PV — relocation refused"),
            ));
        }
        // 尾部打包：末端对齐 cursor（含），不留缝隙
        let new_first = cursor.checked_sub(len - 1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "insufficient tail space for relocation")
        })?;
        let delta = new_first as i64 - e.starting_lba as i64;
        if delta < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "insufficient tail space for relocation"));
        }
        // saturating：new_first 理论上 ≥1（LBA0 保护 MBR + LBA1 主头），
        // 极端输入下防下溢 panic，退化为 0 后由下轮 checked_sub 报空间不足
        cursor = new_first.saturating_sub(1);
        moves.push(PlanEntry { part_num: *num, first_lba: e.starting_lba, len_lba: len, delta_lba: delta as u64, is_swap: is_swap_guid(&e.partition_type_guid) });
    }
    Ok(Plan { ss, last_usable_lba: g.header.last_usable_lba, grow_part, moves, repair })
}

/// 所有需要做空间算术的命令（add/new/del/rename/flag/resize-part）都先经过这里：
/// 读取 → 判定（classify_repair）→ 需要时就地修复（perform_repair，搬移备份头 + 重写保护 MBR）
/// → 返回修复后的表。
/// 表自身的几何自洽性（主头位置、可用区、备份头是否越界）由 table::validate_geometry 在解析层强制；
/// plan 不走这里（plan 不写盘，修复记入 plan.repair 由 apply 执行）。
pub fn ensure_geometry(src: &mut FileSource) -> io::Result<Option<RawGpt>> {
    let g = table::load_gpt(src)?;
    let Some(g) = g else { return Ok(None) };
    let file_last_lba = src.size / g.ss - 1;
    if let Some(r) = classify_repair(&g, file_last_lba)? {
        perform_repair(src, &r)?;
        return table::load_gpt(src);
    }
    Ok(Some(g))
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

/// checkpoint 落点：镜像 = 同目录 `<名>.diskedit.ckpt`；块设备 = /var/lib/diskedit/<disk_guid>.ckpt
pub fn checkpoint_path(src: &FileSource, disk_guid: [u8; 16]) -> io::Result<std::path::PathBuf> {
    if src.is_block {
        let dir = std::path::Path::new("/var/lib/diskedit");
        std::fs::create_dir_all(dir)?;
        let hex: String = disk_guid.iter().map(|b| format!("{b:02X}")).collect();
        Ok(dir.join(format!("{hex}.ckpt")))
    } else {
        let mut p = src.path.clone().into_os_string();
        p.push(".diskedit.ckpt");
        Ok(std::path::PathBuf::from(p))
    }
}

/// 原子写：tmp → sync_all → rename → fsync 父目录
fn atomic_write_ckpt(path: &std::path::Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut tmp = parent.to_path_buf().into_os_string();
    tmp.push(format!("/.diskedit.ckpt.tmp.{}", std::process::id()));
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

/// 扩容终点：`moves` 是按"末→首"的尾部紧凑打包序，各分区新起点中最小者即本次腾出空间的
/// 左边界，故 grow_end = min(new_first) − 1；无 movable 时扩到 last_usable_lba。
/// 加法一律 checked：越界/损坏表下报错而非回绕（回绕会把荒谬的 LBA 写进表再交给 resize）
fn grow_end_for(plan: &Plan) -> io::Result<u64> {
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

pub fn apply(src: &mut FileSource, plan: &Plan, chunk_len: u64, log: &mut dyn FnMut(&str)) -> io::Result<()> {
    let g0 = table::load_gpt(src)?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no GPT"))?;
    let disk_guid = g0.header.disk_guid;
    // plan 不写盘：待修复项（备份头搬移 + 保护 MBR 重写）在 apply 里先执行，
    // 使后续所有写入都基于修复后的几何（磁盘 GUID 不变，checkpoint 路径不受影响）
    if let Some(r) = &plan.repair {
        perform_repair(src, r)?;
        log(&format!("[repair] backup GPT → LBA {} / protective MBR rewritten", r.file_last_lba));
    }
    let ckpt_path = checkpoint_path(src, disk_guid)?;

    // 恢复三态：有效 → 续传 / 分区已提交 → 跳过 / 无效 → 拒绝
    let existing = std::fs::read(&ckpt_path).ok().and_then(|b| Checkpoint::deserialize(&b).ok());
    let mut ckpt = match existing {
        Some(c) => {
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
                return Err(io::Error::new(io::ErrorKind::InvalidData, "existing checkpoint does not match current disk/plan — refusing"));
            }
            log("valid checkpoint found — resuming");
            fresh
        }
        None => Checkpoint {
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

    // 搬移循环：单分区 = 单事务；尾→头推进
    for mi in (ckpt.cur_index as usize)..plan.moves.len() {
        let m = &plan.moves[mi];
        ckpt.cur_index = mi as u32;
        ckpt.chunks_done = 0;
        // swap：内容可弃 → 不搬数据，表项落位后 mkswap 重建（UUID/卷标保持）
        if m.is_swap {
            let mut g = table::load_gpt(src)?.ok_or_else(|| io::Error::other("GPT vanished mid-apply"))?;
            // PARTUUID/分区号由条目原样保留（unique guid 不动），只改位置。
            // 绝对赋值而非 += delta：commit 后、ckpt 更新前崩溃的重放幂等
            {
                let e = &mut g.entries[(m.part_num - 1) as usize];
                e.starting_lba = m.first_lba + m.delta_lba;
                e.ending_lba = m.first_lba + m.len_lba - 1 + m.delta_lba;
            }
            let last_lba = src.size / plan.ss - 1;
            table::commit_gpt(src, &g, last_lba)?;
            // 旧 swap 签名区在源位置（数据区未搬移），从那里读 UUID/卷标
            let ident = read_swap_identity(src, m.first_lba, m.len_lba, plan.ss);
            match crate::fsops::recreate_swap(src, m.part_num, ident) {
                Ok(()) => log(&format!("swap {} recreated at new location (UUID preserved)", m.part_num)),
                Err(e) => log(&format!("swap {} relocated but mkswap failed: {e} — run mkswap manually", m.part_num)),
            }
            ckpt.chunks_done = 0;
            ckpt.cur_index = mi as u32 + 1;
            atomic_write_ckpt(&ckpt_path, &ckpt.serialize())?;
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
        for (i, (within, len)) in chunk_from_tail.iter().enumerate() {
            if (i as u64) < ckpt.chunks_done {
                continue; // 续传：已提交的 chunk 视为完成
            }
            let mut buf = vec![0u8; *len as usize];
            src.read_at(src_off + within, &mut buf)?;
            src.write_at(dst_off + within, &buf)?;
            src.sync_data()?; // 数据 chunk 用 sync_data；表结构提交用 sync_all
            ckpt.chunks_done = i as u64 + 1;
            atomic_write_ckpt(&ckpt_path, &ckpt.serialize())?;
        }
        // 本分区完成 → 按崩溃安全四结构序列提交表项（整表重写）。
        // 绝对赋值（非 += delta）：commit 后 ckpt 更新前崩溃的重放幂等
        let mut g = table::load_gpt(src)?.ok_or_else(|| io::Error::other("GPT vanished mid-apply"))?;
        {
            let e = &mut g.entries[(m.part_num - 1) as usize];
            e.starting_lba = m.first_lba + m.delta_lba;
            e.ending_lba = m.first_lba + m.len_lba - 1 + m.delta_lba;
        }
        let last_lba = src.size / plan.ss - 1;
        table::commit_gpt(src, &g, last_lba)?;
        // 起始位置变化的 NTFS 分区需修 HiddenSectors（数据是字节拷贝，boot sector 带着旧值）
        let new_first = m.first_lba + m.delta_lba;
        if crate::fsid::identify(src, new_first, m.len_lba)? == "ntfs" {
            fix_ntfs_hidden_sectors(src, new_first, plan.ss, log)?;
        }
        log(&format!("partition {} relocated (delta {} sectors)", m.part_num, m.delta_lba));
        ckpt.chunks_done = 0;
        ckpt.cur_index = mi as u32 + 1;
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize())?;
    }

    // 扩容收尾：目标分区扩到 movable 新区域之前（防止与刚打包的分区重叠）；
    // 无 movable 时到 last_usable。commit → resize FS
    let last_lba = src.size / plan.ss - 1;
    let mut g = table::load_gpt(src)?.ok_or_else(|| io::Error::other("GPT vanished before grow"))?;
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

    let _ = std::fs::remove_file(&ckpt_path);

    // FS resize（退出码不等于事实，读回复核交给调用方 info/verify）
    let fstype = crate::fsid::identify(src, grow_start, grow_len)?;
    if matches!(fstype, "unknown" | "swap" | "lvm2_pv") {
        log("partition extended (no resizable filesystem inside)");
    } else {
        match crate::fsops::resize_fs(src, plan.grow_part, fstype) {
            Ok(()) => log("filesystem resized"),
            Err(e) => log(&format!("partition extended but FS resize skipped: {e}")),
        }
    }
    Ok(())
}

/// 从 swap 首部读 UUID/卷标（内核 include/linux/swap.h union swap_header：info @1024，
/// sws_uuid@1036、sws_volume@1052）。非 swap 签名或读取失败 → None（mkswap 生成随机 UUID）
pub(crate) fn read_swap_identity(src: &FileSource, first_lba: u64, len_lba: u64, ss: u64) -> (Option<[u8; 16]>, Option<String>) {
    // 签名在"创建机页大小"末尾（内核 include/linux/swap.h）：读取机页大小可能与
    // 创建机不同（跨架构镜像、Android 模拟页）→ 64K 内候选页逐一探测
    // 运行系统页大小（man sysconf(3) 的 _SC_PAGESIZE）
    #[cfg(target_os = "linux")]
    let local = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    #[cfg(not(target_os = "linux"))]
    let local = 4096u64;
    let base = first_lba * ss;
    let mut page = None;
    for cand in [local, 4096, 8192, 16384, 32768, 65536] {
        if len_lba * ss < cand {
            continue;
        }
        let mut magic = [0u8; 10];
        if src.read_at(base + cand - 10, &mut magic).is_ok() && &magic == b"SWAPSPACE2" {
            page = Some(cand);
            break;
        }
    }
    if page.is_none() {
        return (None, None); // 未格式化或非 swap → 随机 UUID
    }
    let mut uuid = [0u8; 16];
    let mut vol = [0u8; 16];
    if src.read_at(base + 1036, &mut uuid).is_err() {
        return (None, None);
    }
    let _ = src.read_at(base + 1052, &mut vol);
    let vol_s: String = vol.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
    let uuid_opt = if uuid == [0u8; 16] { None } else { Some(uuid) };
    let vol_opt = if vol_s.is_empty() { None } else { Some(vol_s) };
    (uuid_opt, vol_opt)
}

// ---------- 通用 resize-part：grow / shrink / move 三合一 ----------

/// 缩容拒绝的 FS（xfs 无缩容能力；f2fs 缩容工具链未接入）
pub fn fs_can_shrink(fstype: &str) -> bool {
    matches!(fstype, "ext" | "ext2" | "ext3" | "ext4" | "ntfs" | "btrfs")
}

const CKPT2_MAGIC: &[u8; 8] = b"DKECKPT2";
const CKPT2_VERSION: u32 = 2;
const PHASE_MOVE: u8 = 0;
const PHASE_COMMITTED: u8 = 1;

/// resize-part 的 checkpoint（v2）：搬移覆盖源区，中途断电后新旧两处都不完整，
/// 已完成位置只能由 checkpoint 判定。
struct RsCheckpoint {
    disk_size: u64,
    ss: u64,
    part: u32,
    old_start: u64,
    old_end: u64,
    new_start: u64,
    new_end: u64,
    fs_shrunk: bool,
    phase: u8,
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
        b.push(self.phase);
        b.extend_from_slice(&self.chunks_done.to_le_bytes());
        b.extend_from_slice(&self.chunk_bytes.to_le_bytes());
        let crc = table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }
    fn deserialize(b: &[u8]) -> io::Result<Self> {
        // 最短完整布局 = 8(magic)+4(ver)+6×u64+2+8+8(chunks_done/chunk_bytes)+4(crc) = 86，
        // CRC 4 字节必须计入：截断文件走 InvalidData 而非在尾部切片时 panic
        if b.len() < 8 + 4 + 8 * 6 + 2 + 8 + 4 + 8 + 4 || &b[0..8] != CKPT2_MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint v2 invalid"));
        }
        if u32::from_le_bytes(b[8..12].try_into().unwrap()) != CKPT2_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported checkpoint v2 version"));
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
        let phase = *b.get(o).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated"))?; o += 1;
        let chunks_done = rd64(b, &mut o)?;
        let chunk_bytes = rd64(b, &mut o)?;
        if table::crc32(&b[..o]) != u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint v2 CRC mismatch"));
        }
        // 字段一致性：区间端点不得倒挂，chunk_bytes 不得为 0（防除零/死循环回放）
        if old_start > old_end || new_start > new_end {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint v2 range inconsistent"));
        }
        if chunk_bytes == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checkpoint v2 chunk_bytes invalid"));
        }
        Ok(RsCheckpoint { disk_size, ss, part, old_start, old_end, new_start, new_end, fs_shrunk, phase, chunks_done, chunk_bytes })
    }
}

/// 通用分区重定位：new_start/new_end 任意（grow/shrink/move 组合）。
/// 顺序：缩容 FS（如有）→ 数据搬移（方向感知 + checkpoint 续传）→ 提交表项 → 扩容 FS（如有）。
/// 所有拒绝性校验在第一次写盘前完成（fail-fast）。
pub fn resize_part(
    src: &mut FileSource,
    part: u32,
    new_start: u64,
    new_end: u64,
    chunk_len: u64,
    log: &mut dyn FnMut(&str),
) -> io::Result<()> {
    let g = ensure_geometry(src)?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no GPT"))?;
    let ss = g.ss;
    let e = g.entries.get((part - 1) as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("partition {part} is empty")));
    }
    // 只拦搬移（起点变化）：LUKS/LVM PV/swap 的纯扩缩不搬数据，允许
    if FORBIDDEN_TYPE_GUIDS.contains(&e.partition_type_guid) && new_start != e.starting_lba {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "swap/LUKS/LVM PV relocation refused (size-only changes are allowed)"));
    }
    if new_start < g.header.first_usable_lba || new_end > g.header.last_usable_lba || new_start > new_end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("new range {new_start}..{new_end} outside usable {}..{}", g.header.first_usable_lba, g.header.last_usable_lba),
        ));
    }
    let old_start = e.starting_lba;
    let old_end = e.ending_lba;
    for (i, other) in g.entries.iter().enumerate() {
        if i + 1 == part as usize || other.ending_lba == 0 {
            continue;
        }
        if !(new_end < other.starting_lba || new_start > other.ending_lba) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("new range overlaps partition #{}", i + 1)));
        }
    }
    let fstype = crate::fsid::identify(src, old_start, old_end - old_start + 1)?;
    let disk_guid = g.header.disk_guid;
    let ckpt_path = checkpoint_path(src, disk_guid)?;

    // 恢复：checkpoint 与当前参数一致才允许续传，否则拒绝
    let existing = std::fs::read(&ckpt_path).ok().and_then(|b| RsCheckpoint::deserialize(&b).ok());
    let (fs_shrunk, resume_chunks) = match existing {
        Some(c) => {
            let matches = c.disk_size == src.size && c.ss == ss && c.part == part
                && c.old_start == old_start && c.old_end == old_end
                && c.new_start == new_start && c.new_end == new_end
                && c.chunk_bytes == chunk_len;
            if !matches {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "existing checkpoint does not match this resize — refusing"));
            }
            log("valid checkpoint found — resuming");
            (c.fs_shrunk, c.chunks_done)
        }
        None => (false, 0),
    };

    let old_bytes = (old_end - old_start + 1) * ss;
    let new_bytes = (new_end - new_start + 1) * ss;
    let mut ckpt = RsCheckpoint {
        disk_size: src.size, ss, part,
        old_start, old_end, new_start, new_end,
        fs_shrunk, phase: PHASE_MOVE, chunks_done: resume_chunks,
        chunk_bytes: chunk_len,
    };
    let save = |c: &RsCheckpoint| atomic_write_ckpt(&ckpt_path, &c.serialize());

    // ---- 阶段 0：缩容 FS 先缩（只做一次）----
    if new_bytes < old_bytes && !ckpt.fs_shrunk {
        // LVM PV 缩容需要 lvreduce/pvresize 链（新末端之后不得有已分配 extent），
        // 本工具不做该链，写盘前整体拒绝
        if fstype == "lvm2_pv" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot shrink: LVM PV shrink requires the lvreduce/pvresize chain (unsupported here; see pvresize(8))",
            ));
        }
        // unknown FS 无法先缩 FS：缩分区后 FS 越界写坏数据，直接拒绝
        if fstype == "unknown" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot shrink: filesystem type unrecognized (shrinking the partition without resizing the FS first would corrupt data)",
            ));
        }
        if !fs_can_shrink(fstype) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("fs {fstype} cannot shrink; aborting before any write"),
            ));
        }
        // ext 预查 FS 最小尺寸（resize2fs -P × dumpe2fs -h 块大小），缩太小在写盘前拒绝
        if let Some(min) = crate::fsops::fs_min_bytes(src, part, fstype)?
            && new_bytes < min
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("refused: target size {new_bytes} < minimum FS size {min} bytes (resize2fs -P)"),
            ));
        }
        crate::fsops::shrink_fs(src, part, fstype, new_bytes)?;
        ckpt.fs_shrunk = true;
        save(&ckpt)?;
        log(&format!("fs shrunk to {new_bytes} bytes"));
    }

    // ---- 阶段 1：数据搬移（方向感知 + chunk 续传）----
    let delta = new_start as i64 - old_start as i64;
    if delta != 0 && ckpt.phase == PHASE_MOVE {
        let src_off = old_start * ss;
        let dst_off = (old_start as i64 + delta) as u64 * ss;
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
            src.write_at(dst_off + within, &buf)?;
            src.sync_data()?;
            ckpt.chunks_done = i as u64 + 1;
            save(&ckpt)?;
        }
        log(&format!("data moved by {delta} sectors"));
    }

    // ---- 阶段 2：提交表项（幂等：重复执行结果相同）----
    let mut g2 = table::load_gpt(src)?.ok_or_else(|| io::Error::other("GPT vanished mid-resize"))?;
    {
        let te = &mut g2.entries[(part - 1) as usize];
        te.starting_lba = new_start;
        te.ending_lba = new_end;
    }
    let last_lba = src.size / ss - 1;
    table::commit_gpt(src, &g2, last_lba)?;
    table::ensure_protective_mbr(src)?;
    ckpt.phase = PHASE_COMMITTED;
    save(&ckpt)?;
    log(&format!("partition {part} committed at {new_start}..{new_end}"));

    // 起始 LBA 变了才需要修 NTFS HiddenSectors（扩缩不动 start 时跳过）
    if delta != 0 && fstype == "ntfs" {
        fix_ntfs_hidden_sectors(src, new_start, ss, log)?;
    }

    // ---- 阶段 3：扩容 FS ----
    let _ = std::fs::remove_file(&ckpt_path);
    // 扩容 FS：unknown/LVM PV 无本工具可扩的文件系统，跳过
    if new_bytes > old_bytes && !matches!(fstype, "unknown" | "lvm2_pv") {
        if fstype == "swap" {
            // swap：内容可弃，表项已扩 → mkswap 重建使新空间生效（UUID/卷标保持；
            // swap 目标拒绝搬移，起始未变，旧头部仍在原位可读）
            let ident = read_swap_identity(src, old_start, old_end - old_start + 1, ss);
            match crate::fsops::recreate_swap(src, part, ident) {
                Ok(()) => log("swap recreated (UUID preserved)"),
                Err(e) => log(&format!("partition resized but mkswap failed: {e} — run mkswap manually")),
            }
        } else {
            match crate::fsops::resize_fs(src, part, fstype) {
                Ok(()) => log("fs grown"),
                Err(e) => log(&format!("partition resized but fs grow failed: {e}")),
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
/// 目标范围由 add_entry 的重叠校验把关；数据复制先于表项提交。
pub fn copy_part(src: &mut FileSource, part: u32, new_start: u64, name: &str, chunk_len: u64, log: &mut dyn FnMut(&str)) -> io::Result<u32> {
    let g = ensure_geometry(src)?.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no GPT"))?;
    let ss = g.ss;
    let e = g.entries.get((part - 1) as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("partition {part} not found")))?;
    if e.ending_lba == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, format!("partition {part} is empty")));
    }
    let len = e.ending_lba - e.starting_lba + 1;
    // checked：new_start 来自 CLI 原始输入（--align none 时无上界），回绕会骗过下方边界校验
    let new_end = new_start.checked_add(len - 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "copy target overflows address space"))?;
    if new_start < g.header.first_usable_lba || new_end > g.header.last_usable_lba {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "copy target outside usable range"));
    }
    for (i, other) in g.entries.iter().enumerate() {
        if other.ending_lba == 0 {
            continue;
        }
        if !(new_end < other.starting_lba || new_start > other.ending_lba) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("copy target overlaps partition #{}", i + 1)));
        }
    }
    let src_off = e.starting_lba * ss;
    let dst_off = new_start * ss;
    let total = len * ss;
    let mut pos = 0u64;
    while pos < total {
        let len_c = chunk_len.min(total - pos);
        let mut buf = vec![0u8; len_c as usize];
        src.read_at(src_off + pos, &mut buf)?;
        src.write_at(dst_off + pos, &buf)?;
        src.sync_data()?;
        pos += len_c;
    }
    let guid = e.partition_type_guid;
    let num = table::add_entry_at(src, new_start, new_end, name, guid, e.unique_partition_guid)?;
    // 副本的 boot sector 原样带来旧 HiddenSectors，按新起始位置修正
    if crate::fsid::identify(src, new_start, len)? == "ntfs" {
        fix_ntfs_hidden_sectors(src, new_start, ss, log)?;
    }
    log(&format!("partition {part} copied to #{num} at {new_start}..{new_end}"));
    Ok(num)
}

#[cfg(test)]
mod tests {
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
        let mut src = FileSource { file: f, path: path.clone(), sector_size: 512, size, is_block: false, journal: None };
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
        let mut src = FileSource { file: f, path: tmp.clone(), sector_size: 512, size, is_block: false, journal: None };
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
        let g = table::load_gpt(&src).unwrap().unwrap();
        let ckpt_path = checkpoint_path(&src, g.header.disk_guid).unwrap();
        let ckpt = RsCheckpoint {
            disk_size: size, ss, part: 1,
            old_start: 2048, old_end: 10239, new_start: 12288, new_end: 20479,
            fs_shrunk: false, phase: PHASE_MOVE, chunks_done: 1, chunk_bytes: chunk,
        };
        atomic_write_ckpt(&ckpt_path, &ckpt.serialize()).unwrap();

        // 续传：跳过 chunk 0，补齐 chunk 1..3，提交表项
        resize_part(&mut src, 1, 12288, 20479, chunk, &mut |_| {}).unwrap();

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
        let _ = std::fs::remove_file(&ckpt_path);
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
        let mut src = FileSource { file: f, path: tmp.clone(), sector_size: 512, size, is_block: false, journal: None };
        // gptman 只写 GPT 结构，保护 MBR 需自行补——load_gpt 以前者为前置
        crate::table::ensure_protective_mbr(&mut src).unwrap();
        (src, tmp)
    }

    fn plan_open(path: &std::path::Path) -> FileSource {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
        let size = std::fs::metadata(path).unwrap().len();
        FileSource { file: f, path: path.into(), sector_size: 512, size, is_block: false, journal: None }
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
        let plan_of = |moves: Vec<PlanEntry>, last_usable: u64| Plan { ss: 512, last_usable_lba: last_usable, grow_part: 1, moves, repair: None };
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

    /// 真实路径：truncate 预扩后 backup GPT 与保护 MBR 同时过期 —— 解析标记 Stale、
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
        assert_eq!(g.state, table::GptState::Stale { backup_lba: old_file_last, file_last_lba: new_file_last });
        assert_eq!(g.pmbr, table::PmbrSize::Stale);
        assert_eq!(g.header.backup_lba, old_file_last, "解析不得偷偷改写盘上表");
        // plan 只记录修复动作，不写盘；计划几何按修复后的值算
        let plan = make_plan(&mut src, 1).unwrap();
        assert!(plan.repair.as_ref().is_some_and(|r| r.backup_stale));
        assert_eq!(plan.last_usable_lba, new_file_last - 32 - 1);
        assert_eq!(table::load_gpt(&src).unwrap().unwrap().state,
            table::GptState::Stale { backup_lba: old_file_last, file_last_lba: new_file_last },
            "plan 不得写盘");
        // 写入路径：修复到新末端 + 保护 MBR 与容器一致
        let g2 = ensure_geometry(&mut src).unwrap().unwrap();
        assert_eq!(g2.state, table::GptState::Valid);
        assert_eq!(g2.pmbr, table::PmbrSize::Normal);
        assert_eq!(g2.header.backup_lba, new_file_last);
        assert_eq!(g2.header.last_usable_lba, new_file_last - 32 - 1); // span = 128×128/512
        assert!(classify_repair(&g2, new_file_last).unwrap().is_none(), "修复须幂等");
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
        assert_eq!(g.state, table::GptState::Stale { backup_lba: 20000, file_last_lba: file_last });
        assert_eq!(g.pmbr, table::PmbrSize::Normal, "夹具的保护 MBR 与容器一致");
        assert_eq!(g.header.backup_lba, 20000, "解析不得偷偷改写盘上表");
        // 写入路径：就地修复并把备份头搬到设备末端
        let g2 = ensure_geometry(&mut src).unwrap().unwrap();
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
        assert!(err.to_string().contains("last_usable_lba beyond container end"), "{err}");
        assert!(ensure_geometry(&mut src).is_err());
        drop(src);
        let _ = std::fs::remove_file(&p);
    }
}