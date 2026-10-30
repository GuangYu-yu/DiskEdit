//! undo journal 的记录格式、文件生命周期与落点状态分类。

use super::*;
use std::num::NonZeroUsize;
use std::sync::Mutex;

/// 本进程用 `create_new` 建成的 journal 落点，连同建立时该文件的指纹。
/// `TransactionManager::commit` 据此区分"本次事务写下的文件"与"命令返回之后同目标
/// 另一进程建出的同名文件"——两者在路径与记录身份上完全相同，只有创建事件连同建立时的
/// 文件身份能分开它们
static CREATED_JOURNAL: Mutex<Option<(PathBuf, TargetFingerprint)>> = Mutex::new(None);

/// 本进程是否用 `create_new` 建过 `p`；命中即清除记录并返回建立时的文件指纹。
/// 调用方只比 `dev` / `ino`（[`TargetFingerprint::still_names`]）：journal 的 size
/// 随记录追加而变，不能参与比对
pub(crate) fn take_created_journal_fp(p: &Path) -> Option<TargetFingerprint> {
    let mut g = CREATED_JOURNAL.lock().unwrap_or_else(|e| e.into_inner());
    match g.as_ref() {
        Some((path, fp)) if path == p => {
            let fp = *fp;
            *g = None;
            Some(fp)
        }
        _ => None,
    }
}

/// 一次 durable mutation 的**语义**：用户看到的"做了什么"，也是 undo policy 的输入。
///
/// 语义与恢复数据分开表达是刻意的：字节 diff 反推不出业务语义（"这两段字节变了"不等于
/// "分区被搬走了"），而业务语义也替代不了 pre-image（想回滚就得有被覆盖的原文）。
/// 两者各记一份在**同一条记录**里，不设第二套 history
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    /// 分区表/条目字节的改写：new / add / delete / set / create / resize-part / 搬移的表项部分
    PartitionTable,
    /// 分区数据块被搬移
    RelocatePartition { partition: u32, from_lba: u64, to_lba: u64 },
    /// 分区数据块被复制到别处
    CopyPartition { partition: u32, from_lba: u64, to_lba: u64 },
    /// 外部 FS 工具写进了分区内容：resize2fs / xfs_growfs / mkswap / e2fsck / pvresize…
    ExternalFsTool,
    /// 在分区上创建了文件系统
    Mkfs,
}

impl Mutation {
    /// 人读的一句话。undo 的拒绝理由与 abandon 的清单都用它，避免两处各自措辞
    pub fn describe(&self) -> String {
        match self {
            Mutation::PartitionTable => "the partition table was rewritten".to_string(),
            Mutation::RelocatePartition { partition, from_lba, to_lba } => {
                format!("partition {partition} data was moved from LBA {from_lba} to {to_lba}")
            }
            Mutation::CopyPartition { partition, from_lba, to_lba } => {
                format!("partition {partition} data was copied from LBA {from_lba} to {to_lba}")
            }
            Mutation::ExternalFsTool => "an external filesystem tool wrote to the partition".to_string(),
            Mutation::Mkfs => "a filesystem was created on the partition".to_string(),
        }
    }
}

/// 一条记录里的**恢复依据**：undo 真正依赖的那部分
pub enum RecoveryData {
    /// 被覆盖字节的原文与当时的写后内容。回放原文即回到这次写入之前；
    /// 写后内容是回放前的分叉核对依据——盘上字节与它不符，说明这次写入之后
    /// 有外部改动动过同一段字节，回放会覆盖它
    PreImage { off: u64, bytes: Vec<u8>, after: Vec<u8> },
    /// 从这里起该 mutation 越过了不可回滚点：只回滚它之前的记录会得到"表与盘上内容
    /// 自相矛盾"的布局，故 undo 见到即整体拒绝
    Barrier,
}

/// 一条 durable 记录：发生了什么 + 怎么恢复
pub struct JournalRecord {
    pub mutation: Mutation,
    pub recovery: RecoveryData,
}

/// 记录头 `[len u32][crc32 u32]` 的字节数。`len` 计入 CRC（见 [`Journal::read_entries`]）
pub(crate) const REC_HDR: usize = 8;

/// 身份段：dev / ino / size / 设备 ID 哈希 各 u64 + "有无 loop 映射"1 字节 + 映射的
/// off / limit 各 u64 + 身份种类 1 字节。映射取值即使缺席也照样占位——定长头让记录扫描
/// 的起点是个常量，不必先解析变长字段
pub(crate) const IDENTITY_LEN: usize = 8 * 4 + 1 + 8 * 2 + 1;

/// 版本错误：本工具别的版本写下的、本构建不认识的 journal。版本识别必须先于一切
/// payload 解析——别的版本的记录布局可能不同，按当前格式去解会把"版本不认识"报成
/// "CRC 不符 / 记录损坏"，把用户指向错误的修法
pub(crate) fn unsupported_version_error(version: u8) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "journal format v{version} is not supported by this build (it reads and writes v{}) — the \
             journal cannot be replayed; release it with `diskedit abandon` (that gives up the ability to roll \
             back, not the data)",
            Journal::MAGIC[4]
        ),
    )
}

/// 不是本工具的 journal（magic 不符）。版本不符走 [`unsupported_version_error`]，
/// 那条路径报的是"版本不支持"，不会被说成"这文件不是 journal"
pub(crate) fn not_a_journal_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "existing file is not a diskedit journal (magic mismatch) — release it with `diskedit abandon` \
         if it is a stuck recovery record of this tool",
    )
}

/// 头没读全。与 [`not_a_journal_error`] 分开：magic 对上了（或只是没读全）说明文件**可能是**
/// 本工具的，把它说成"不是 journal"会让用户以为可以随手删
pub(crate) fn truncated_header_error(have: usize, need: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("journal header is truncated ({have} bytes, needs {need})"),
    )
}

/// 一个 journal 落点的状态。**四态是现场判定的唯一来源**：写闸口、`undo` 的挑选
/// 与 `abandon` 的枚举都从它派生，不各自实现"文件在不在 / 读不读得出"。
///
/// `Absent` 覆盖两种"不是现场"的形态：落点不存在、0 字节残骸。符号链接与非普通文件不在此
/// 列——它们要么根本不是本工具写下的，要么指向别处，按 `Damaged` 交用户处置。0 字节有两种
/// 在现有状态下不可区分的来源（`ensure` 建好文件、头还没写；已有 journal 被外部截断到 0），
/// 一律按零次写入收养——它不可能含记录，因为记录只能接在完整头之后。这是已接受的限制而非
/// 安全保证（见 `journal_truncated_to_zero_is_adopted_as_a_remnant` 测试）。
///
/// **1..HEADER_LEN-1 字节不在此列**：初始化没写完与原 journal 被外部截断到这么短在内容上
/// 不可区分，后者是一份丢了记录的现场。把不可证明当已证明，就会替用户宣布"这里什么都没有"；
/// 故那种形态按 `Damaged` 拒绝，由 `abandon` 显式释放（它是改名保留字节，不是覆盖）。
pub(crate) enum JournalCandidate {
    Absent,
    /// 头完整、零条完整记录：零次写入的壳
    Shell,
    /// 读不出来（陌生文件、magic 不符、CRC 损坏、属性读取失败）
    Damaged(String),
    /// 至少一条完整记录
    Records(JournalRead),
}

/// 认一个恢复现场的落点本身：`Ok(None)` = 不存在，`Err(reason)` = 不可认领，
/// `Ok(Some(meta))` = 普通文件的元数据。
///
/// `symlink_metadata` 不跟随符号链接：认的是落点本身的条目。跟随的话，指向别处
/// 普通文件的链接会被当成现场打开，随后的收养、截断与删除都作用在链接目标上。
/// 符号链接不是本工具写下的任何东西，也绝不认领——`abandon` 改名保留的是链接本身，
/// 不会碰它指向的目标。非普通文件（FIFO / 设备节点 / 目录）同归不可认领：
/// `open` 一个 FIFO 会阻塞到有写端出现，放行等于让收养卡死在那里
pub(crate) fn recovery_path_metadata(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot stat: {e}")),
    };
    if meta.file_type().is_symlink() {
        return Err(
            "the recovery path is a symbolic link — refusing to treat another file as this tool's recovery record"
                .to_string(),
        );
    }
    if !meta.is_file() {
        return Err(
            "the recovery path is not a regular file — it cannot be a recovery record written by this tool".to_string(),
        );
    }
    Ok(Some(meta))
}

/// 判一个落点属于哪一态。**只有 `NotFound` 是"不在"**：属性读不出来按 `Damaged` 处理，
/// 否则一次瞬态 I/O 错误就会把一份可能完好的现场当成不存在，后续 `commit` 还会
/// 把它静默删掉
///
/// **头之后还有字节却读不出一条完整记录，不是空壳**：撕裂的首次追加与"已有记录被
/// 截掉 / 写坏"在内容上不可区分，前者没有对应的数据写入、后者有。无法证明"空"就
/// 不能按空壳销毁——写闸口会因此放行，undo 与 abandon 会删掉唯一一份 pre-image
pub(crate) fn classify_journal(path: &Path) -> JournalCandidate {
    let meta = match recovery_path_metadata(path) {
        Ok(Some(meta)) => meta,
        Ok(None) => return JournalCandidate::Absent,
        Err(reason) => return JournalCandidate::Damaged(reason),
    };
    // 长度 0 的普通文件按"不在"处理：没有头就没有可回放的记录，与打开时的
    // NotFound 同义
    if meta.len() == 0 {
        return JournalCandidate::Absent;
    }
    match Journal::read_entries(path) {
        Ok(JournalRead::Complete(v)) if v.is_empty() => JournalCandidate::Shell,
        Ok(JournalRead::TruncatedTail { records, cut }) if records.is_empty() => JournalCandidate::Damaged(
            format!(
                "the journal header is followed by {cut} byte(s) that form no complete record — this cannot be shown \
                 to be empty, and it may be a torn first record or a record that was truncated away"
            ),
        ),
        Ok(read) => JournalCandidate::Records(read),
        // 属性读到之后文件被删掉：本次判断按"不在"处理，与打开时的 NotFound 同义
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => JournalCandidate::Absent,
        Err(e) => JournalCandidate::Damaged(e.to_string()),
    }
}

pub(crate) fn journal_decode_err(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("journal record is malformed: {what}"))
}

impl Mutation {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Mutation::PartitionTable => 0,
            Mutation::RelocatePartition { .. } => 1,
            Mutation::CopyPartition { .. } => 2,
            Mutation::ExternalFsTool => 3,
            Mutation::Mkfs => 4,
        }
    }

    pub(crate) fn write_meta(self, out: &mut Vec<u8>) {
        match self {
            Mutation::RelocatePartition { partition, from_lba, to_lba }
            | Mutation::CopyPartition { partition, from_lba, to_lba } => {
                out.extend_from_slice(&partition.to_le_bytes());
                out.extend_from_slice(&from_lba.to_le_bytes());
                out.extend_from_slice(&to_lba.to_le_bytes());
            }
            _ => {}
        }
    }

    /// 解出 mutation 与它消耗的字节数（调用方据此找到后面的 recovery 段）
    pub(crate) fn read(tag: u8, rest: &[u8]) -> io::Result<(Self, usize)> {
        let reloc = |kind: u8| -> io::Result<(Mutation, usize)> {
            if rest.len() < 20 {
                return Err(journal_decode_err("truncated mutation payload"));
            }
            let partition = u32::from_le_bytes(rest[0..4].try_into().unwrap());
            let from_lba = u64::from_le_bytes(rest[4..12].try_into().unwrap());
            let to_lba = u64::from_le_bytes(rest[12..20].try_into().unwrap());
            let m = if kind == 1 {
                Mutation::RelocatePartition { partition, from_lba, to_lba }
            } else {
                Mutation::CopyPartition { partition, from_lba, to_lba }
            };
            Ok((m, 20))
        };
        match tag {
            0 => Ok((Mutation::PartitionTable, 0)),
            1 | 2 => reloc(tag),
            3 => Ok((Mutation::ExternalFsTool, 0)),
            4 => Ok((Mutation::Mkfs, 0)),
            other => Err(journal_decode_err(&format!("unknown mutation tag {other}"))),
        }
    }
}

impl JournalRecord {
    /// `payload = [mutation tag][mutation meta][recovery tag][recovery body]`；
    /// PreImage body = `off(8) + pre_len(u32) + 原文 + 写后内容`
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut p = Vec::new();
        p.push(self.mutation.tag());
        self.mutation.write_meta(&mut p);
        match &self.recovery {
            RecoveryData::PreImage { off, bytes, after } => {
                p.push(0);
                p.extend_from_slice(&off.to_le_bytes());
                p.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                p.extend_from_slice(bytes);
                p.extend_from_slice(after);
            }
            RecoveryData::Barrier => p.push(1),
        }
        p
    }

    pub(crate) fn decode(payload: &[u8]) -> io::Result<Self> {
        let tag = *payload.first().ok_or_else(|| journal_decode_err("empty payload"))?;
        let (mutation, used) = Mutation::read(tag, &payload[1..])?;
        let rest = &payload[1 + used..];
        let rtag = *rest.first().ok_or_else(|| journal_decode_err("missing recovery tag"))?;
        let recovery = match rtag {
            0 => {
                if rest.len() < 13 {
                    return Err(journal_decode_err("truncated pre-image"));
                }
                let off = u64::from_le_bytes(rest[1..9].try_into().unwrap());
                let pre_len = u32::from_le_bytes(rest[9..13].try_into().unwrap()) as usize;
                let body = rest.get(13..).ok_or_else(|| journal_decode_err("truncated pre-image"))?;
                if body.len() < pre_len {
                    return Err(journal_decode_err("truncated pre-image"));
                }
                RecoveryData::PreImage {
                    off,
                    bytes: body[..pre_len].to_vec(),
                    after: body[pre_len..].to_vec(),
                }
            }
            1 => RecoveryData::Barrier,
            other => return Err(journal_decode_err(&format!("unknown recovery tag {other}"))),
        };
        Ok(JournalRecord { mutation, recovery })
    }
}

/// journal 整份读取的结论：区分"读完了"与"尾部有一笔未完成的事务"。
/// 后者是 append-only 语义下的正常产物（最后一次追加中途断电），不是损坏——两者若共用一个
/// 表示，"崩溃后能不能回滚"就变成靠错误文案猜的事
pub enum JournalRead {
    /// 全部记录完整且 CRC 校验通过
    Complete(Vec<JournalRecord>),
    /// 尾部存在一条未完成的记录，已丢弃；前面已完整的记录前缀原样返回。
    /// `cut` 是被丢掉的字节数——撕裂只在扫描停在文件内部时才成立，故它恒非零：
    /// "撕裂但一个字节都没丢"不是一个可达状态
    TruncatedTail { records: Vec<JournalRecord>, cut: NonZeroUsize },
}

/// 撕裂尾字节的归档后缀。内容是**被截掉的那段原始字节**，不是一份可回放的 journal：
/// 与 undo 归档整份 journal 的 `.torn` 分开，两者不能被同一个读者当成同一种证据
const TORN_TAIL_SUFFIX: &str = ".torn-tail";

/// 把撕裂尾的原始字节复制到 journal 旁边作为归档。名字被占用时顺延，绝不覆盖既有归档——
/// 那一份同样是一次中断留下的证据。失败只由调用方告警：拒绝收养会让目标被一份读不全的
/// 文件永久挡住，而截断丢弃的本就是无法回放的字节
///
/// 写不全时不留下半截文件：残缺的归档与完整归档同名同形，后一个读者无法分辨，而它比起
/// 没有归档只多一个"看起来是证据"的误导。名字因此重新空出来，下一次归档不会顺延跳过它
fn archive_torn_tail(journal: &Path, tail: &[u8]) -> io::Result<PathBuf> {
    use std::io::Write;
    for n in 1..u32::MAX {
        let to = if n == 1 {
            crate::dev::suffix_path(journal, TORN_TAIL_SUFFIX)
        } else {
            crate::dev::suffix_path(journal, &format!("{TORN_TAIL_SUFFIX}.{n}"))
        };
        match OpenOptions::new().write(true).create_new(true).open(&to) {
            Ok(mut f) => {
                let written = f.write_all(tail).and_then(|()| f.sync_all());
                drop(f);
                match written {
                    Ok(()) => {
                        crate::dev::best_effort_dir_fsync(&to);
                        return Ok(to);
                    }
                    Err(e) => {
                        return Err(match std::fs::remove_file(&to) {
                            Ok(()) => e,
                            Err(rm) => io::Error::other(format!(
                                "{e}; the partial archive at {} could not be removed either ({rm})",
                                to.display()
                            )),
                        });
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free torn-tail name available"))
}

/// durable transaction log：追加式 `[len u32][crc32 u32][payload]` 记录流，其中
/// `payload = [mutation][recovery]`（见 [`JournalRecord`]）。
///
/// 每条记录先于实际写入持久化，故任意落点断电后 undo 都能还原已发生的写入。
/// pre-image 只覆盖本工具**元数据**写入（write_at）；回滚不了的那些写入——分区搬移的
/// **数据块**（走 write_data_at）、外部 FS 工具的写入（resize2fs/mkswap/mkfs…）、
/// 内核侧的分区表写入——不记字节，改为在**首次此类写入之前**记一条
/// [`RecoveryData::Barrier`]，其 `mutation` 说明是哪一类越过了不可回滚点。
/// 回放时整卷日志全量载入内存。
///
/// 文件是**惰性**创建的：写下第一条记录之前磁盘上没有它（见 open）
pub struct Journal {
    pub(crate) file: Option<File>,
    pub(crate) path: PathBuf,
    /// 之后写入的 pre-image 记在哪个 mutation 名下。由知道自己在做什么的那一层设置
    /// （见 [`WriteSource::set_mutation`]）；journal 不猜语义
    pub(crate) mutation: Mutation,
    /// 本目标是哪个目标：随头一起落盘（首条记录之前），也在采纳既有 journal 时与头里的
    /// 记录值比对（见 [`Self::open`]）
    pub(crate) identity: LiveScene,
    /// 当前 mutation 是否已落过屏障。屏障回答的是"这一类改动越界了没有"，同一 mutation 下
    /// 重复记录不增加任何信息；`write_data_at` 逐 chunk 自带屏障，靠这个标记让每条
    /// chunk 各落一条屏障不会发生
    pub(crate) barred: bool,
}

impl Journal {
    /// 尾字节是格式版本。版本识别先于任何记录解析，故不认识的格式报"版本不支持"
    /// 而非"记录损坏"
    pub(crate) const MAGIC: &[u8; 5] = b"DEJL\x06";

    /// 首部是否为本工具写下的 journal
    pub(crate) fn is_own_header(hdr: &[u8]) -> bool {
        hdr.len() >= Self::MAGIC.len() && &hdr[..Self::MAGIC.len()] == Self::MAGIC
    }

    /// 头总长（magic + 身份段 + 身份段 CRC32）。身份段不参与任何记录级 CRC，
    /// 故自带一个：头被改坏时要报"头校验失败"，而不是让改坏后的指纹去和当前目标比出个
    /// "指纹不符"
    pub(crate) const HEADER_LEN: usize = Self::MAGIC.len() + IDENTITY_LEN + 4;

    /// 头（含身份段的 CRC）。写出与校验两处只用这一份布局。
    /// 身份种类只从 [`LiveScene`] 取
    pub(crate) fn header(identity: &LiveScene) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::HEADER_LEN);
        b.extend_from_slice(Self::MAGIC);
        b.extend_from_slice(&identity.fp.dev.to_le_bytes());
        b.extend_from_slice(&identity.fp.ino.to_le_bytes());
        b.extend_from_slice(&identity.fp.size.to_le_bytes());
        b.extend_from_slice(&identity.fp.id.to_le_bytes());
        b.push(identity.map.is_some() as u8);
        let (off, limit) = identity.map.unwrap_or((0, 0));
        b.extend_from_slice(&off.to_le_bytes());
        b.extend_from_slice(&limit.to_le_bytes());
        b.push(identity.proof.tag());
        let crc = crate::table::crc32(&b);
        b.extend_from_slice(&crc.to_le_bytes());
        b
    }

    /// 解析并校验头。**版本识别在一切之前**：尾字节不是当前版本，报的是版本错误，
    /// 不是长度不足、更不是 CRC——那两种措辞会把用户送去怀疑文件被写坏
    pub(crate) fn parse_header(data: &[u8]) -> io::Result<SceneIdentity> {
        if data.len() < Self::MAGIC.len() {
            return Err(truncated_header_error(data.len(), Self::HEADER_LEN));
        }
        if !Self::is_own_header(&data[..Self::MAGIC.len()]) {
            if data[..Self::MAGIC.len() - 1] == Self::MAGIC[..Self::MAGIC.len() - 1] {
                return Err(unsupported_version_error(data[Self::MAGIC.len() - 1]));
            }
            return Err(not_a_journal_error());
        }
        if data.len() < Self::HEADER_LEN {
            return Err(truncated_header_error(data.len(), Self::HEADER_LEN));
        }
        let body = &data[..Self::MAGIC.len() + IDENTITY_LEN];
        let want = u32::from_le_bytes(
            data[Self::MAGIC.len() + IDENTITY_LEN..Self::HEADER_LEN].try_into().unwrap(),
        );
        if crate::table::crc32(body) != want {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "journal header CRC mismatch"));
        }
        let u64_at = |i: usize| {
            u64::from_le_bytes(data[Self::MAGIC.len() + i..Self::MAGIC.len() + i + 8].try_into().unwrap())
        };
        let fp = TargetFingerprint { dev: u64_at(0), ino: u64_at(8), size: u64_at(16), id: u64_at(24) };
        const MAP_AT: usize = 32;
        let map = (data[Self::MAGIC.len() + MAP_AT] != 0)
            .then(|| (u64_at(MAP_AT + 1), u64_at(MAP_AT + 9)));
        let proof = IdentityProof::from_tag(data[Self::MAGIC.len() + IDENTITY_LEN - 1]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "implausible journal identity kind")
        })?;
        Ok(SceneIdentity { fp, map, proof })
    }

    /// 只读头：undo 在回放前用它取"这份 journal 是为哪个目标写的"。与 [`Self::open`] 用
    /// 同一份布局与同一套校验，故两条路径对"头是否可信"不会分叉
    pub fn read_identity(path: &Path) -> io::Result<SceneIdentity> {
        Self::parse_header(&std::fs::read(path)?)
    }

    pub(crate) fn at(path: &Path, file: Option<File>, identity: LiveScene) -> Self {
        Journal { file, path: path.to_path_buf(), mutation: Mutation::PartitionTable, identity, barred: false }
    }

    /// 打开（必要时先校验）落点，**不产生任何新痕迹**：文件已存在则必须是我们自己写的
    /// journal——拒绝把陌生文件当 journal 追加；不存在则什么都不建。
    ///
    /// 撤销窗口的痕迹应当由"确实记了什么"产生，而不是由"打开目标"产生：否则一条在
    /// 校验阶段就被拒的命令会留下空 journal，让后续 undo 报"journal 是空的"而不是
    /// "没有 journal"，也占住了落点。
    ///
    /// 既有文件还要过身份核对：它记的是**哪个目标**。指纹或 loop 映射不符即拒绝
    /// 采纳——把别的目标记下的原文当成"本目标的待回滚字节"，是这条路径最坏的后果。
    /// 唯一不适用的是**头之后一个字节都没有**的文件（零记录且无残尾）：零记录意味着无可
    /// 回滚的字节，覆盖头收养不丢信息，而拒绝会让它永久占住本目标的 journal 落点（写命令
    /// 全拒，undo 与 abandon 又都不把它当现场）。头后还有字节而读不出完整记录的**不收养**：
    /// 那截字节可能就是被截掉的记录，与 [`classify_journal`] 的判据同源
    ///
    /// 其余例外：（0 字节的残骸 → 补写头）与
    /// （撕裂尾物理截断，见 [`Self::scan_entries`]；被截掉的字节先归档到 `.torn-tail`）。
    /// 两者都是收养既有现场的一部分而非新痕迹
    /// ——不截断的话垃圾尾会永远留在文件里，之后的新记录接在垃圾之后，`read_entries` 会把
    /// "垃圾之后的一切"都判进 TruncatedTail，新记录从此无法回放
    ///
    /// **截断本身要交给调用方**（[`Self::adopt`]）：那次追加没有写完，对应的写入没发生；
    /// 但同样的形状也可能是"记录被外部截掉"的残骸，两种成因不可区分。丢弃是可以的（这段
    /// 字节不构成可回放记录），但这次运行不能因此自称完整——见 `transaction::SessionJournal`
    ///
    /// 文件以 `read(true).write(true)` 打开（**不用 append 模式**）：补头、撕裂尾截断
    /// （`set_len`）与之后的追加全部复用这一个句柄。append 模式在 Windows 上只拿到
    /// `FILE_APPEND_DATA`、没有 `set_len` 所需的 `FILE_WRITE_DATA`，会迫使截断再按路径
    /// 开第二个句柄——两次打开之间路径可被替换，截断就落到另一个文件上
    #[cfg(test)]
    pub fn open(path: &Path, identity: LiveScene) -> io::Result<Self> {
        Self::adopt(path, identity).map(|(j, _)| j)
    }

    /// [`Self::open`] 的完整形态：附带被截掉的残尾字节数，`None` 表示文件原本就停在
    /// 记录边界上。写路径据此把本次运行报成未完整收尾（见 `transaction::SessionJournal`）
    pub(crate) fn adopt(path: &Path, identity: LiveScene) -> io::Result<(Self, Option<NonZeroUsize>)> {
        // 打开之前先认落点本身的条目：符号链接与非普通文件都不可认领——`open` 一个 FIFO 会
        // 阻塞到有写端出现，跟随链接则会让随后的写入与截断作用于链接目标。落点不存在时
        // 照旧走到下面的 `NotFound` 分支（可能新建）。认领与打开之间落点仍可能被换成
        // 链接：unix 由下面的 O_NOFOLLOW 兜住，其余平台这一段窗口依赖落点所在目录
        // 不被本进程之外的人写入
        if let Err(reason) = recovery_path_metadata(path) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
        }
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        // 不跟随符号链接：分类与收养之间落点仍可能被换成链接，跟随会让写入与
        // 后续的截断作用于链接目标而不是本工具的 journal
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        match opts.open(path) {
            Ok(mut f) => {
                use std::io::Read;
                let len = f.metadata()?.len();
                // 0 字节按残骸就地补头：它不含记录（记录只能接在完整头之后），补头不覆盖
                // 任何已知内容；但"初始化没写完"与"被外部截断到 0"不可区分，后者丢了记录
                // 也无从得知——按残骸收养是已接受的限制。**1..HEADER_LEN-1 字节不在此列**，
                // 一律走下面的三关校验——把那种现场也当残骸，就等于替用户宣布"这里什么
                // 都没有"，而头没写全、身份段 CRC 验不了，证明不了这一点
                if len == 0 {
                    Self::rewrite_header(path, &mut f, &identity)?;
                    return Ok((Self::at(path, Some(f), identity), None));
                }
                let have = usize::try_from(len.min(Self::HEADER_LEN as u64)).unwrap();
                let mut head = vec![0u8; have];
                f.read_exact(&mut head)?;
                // 版本识别排在长度与 CRC 之前：一份**别的版本**的 journal 可能只有几十字节，
                // 按长度先判会把它说成"头没写完"，按 CRC 先判会说成"记录损坏"——两种都会把
                // 用户送去怀疑文件被写坏，而真实原因是版本不认识
                let recorded = if head.len() >= Self::MAGIC.len() {
                    Self::parse_header(&head)? // 版本、长度、头 CRC 三关；不认的文件在此被拒
                } else if head.len() < Self::MAGIC.len() && Self::MAGIC.starts_with(&head) {
                    // 1..4 字节且是本工具 magic 的前缀：不足一个 magic，按头截断报（不是残骸）
                    return Err(truncated_header_error(head.len(), Self::HEADER_LEN));
                } else {
                    return Err(not_a_journal_error());
                };
                // 撕裂尾的物理截断与"零记录"判定共用一次全量读：截断点之前的记录
                // 每条都通过了 CRC，其对应的写入都确实发生过；被截掉的只有"没走完的
                // 追加"或其后的垃圾。`set_len` 与追加共用上面那个句柄：不按路径二次打开
                let mut data = {
                    use std::io::{Read, Seek, SeekFrom};
                    let mut data = Vec::new();
                    f.seek(SeekFrom::Start(0))?;
                    f.read_to_end(&mut data)?;
                    data
                };
                if let Some(why) = scene_mismatch(&recorded, &identity) {
                    // 零完整记录：这份文件没有任何可回滚的字节，身份不符也不构成
                    // "别人的现场"。覆盖头收养它不丢信息；不收养的话，一份落在本目标
                    // 路径上的空壳会永久挡住所有写命令，而 undo/abandon 都不把空壳当
                    // 现场，三条命令就都没有出路。扫描本身失败时无法证明"零记录"，
                    // 按别人的现场拒绝
                    // 零记录指的是**头之后一个字节都没有**：头后还有字节而读不出完整记录时，
                    // 那截字节可能就是被截掉的记录，不能按空壳收养
                    let no_records = matches!(Self::scan_entries(&data), Ok((ref out, end)) if out.is_empty() && end == data.len());
                    if no_records {
                        Self::rewrite_header(path, &mut f, &identity)?;
                        eprintln!(
                            "note: journal {} held no complete records and was not recorded for this target ({why}) — reclaimed it as this target's journal",
                            path.display()
                        );
                        return Ok((Self::at(path, Some(f), identity), None));
                    }
                    // why 里已带着逐字段差异；这里补的是"换回记录时的目标，或用 abandon 释放现场"
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{why} — this journal is not adopted here; restore the target to the state it was in, \
                             or release the journal with `diskedit abandon` if it is no longer needed"
                        ),
                    ));
                }
                let (_, end) = Self::scan_entries(&data)?;
                let cut = NonZeroUsize::new(data.len() - end);
                if let Some(cut) = cut {
                    // 归档先于截断：这份字节此后不再存在于 journal 里，而"没走完的追加"与
                    // "记录被外部截掉"在这里无法区分——后者对应的写入可能已经落盘
                    match archive_torn_tail(path, &data[end..]) {
                        Ok(to) => eprintln!(
                            "note: journal {} ends with an incomplete record — archived the {cut} byte(s) after the last complete record to {} and truncated the journal back to that record; what was recorded there cannot be replayed",
                            path.display(),
                            to.display()
                        ),
                        Err(e) => eprintln!(
                            "warning: cannot archive the {cut} byte(s) the journal {} ends with ({e}) — truncating them away; what was recorded there cannot be replayed",
                            path.display()
                        ),
                    }
                    // 截断只改长度：被截掉的这段字节仍留在文件占用的同一个数据块里。直接缩长度的话，
                    // 下一次追加若只丢掉帧首所在的那个扇区、后续扇区照常落盘，再打开就会读到
                    // "旧帧头 + 新载荷"——len 落在文件内、CRC 必不符，整份 journal 被判 Damaged。
                    // 先把这段写零，使该形态退化成可识别的零长度撕裂尾。归档先于这一步，
                    // 被截掉的原始字节已在 `.torn-tail` 里留底
                    data[end..].fill(0);
                    {
                        use std::io::{Seek, SeekFrom, Write};
                        f.seek(SeekFrom::Start(end as u64))?;
                        f.write_all(&data[end..])?;
                    }
                    f.set_len(end as u64)?;
                    f.sync_all()?;
                }
                Ok((Self::at(path, Some(f), identity), cut))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok((Self::at(path, None, identity), None))
            }
            Err(e) => Err(e),
        }
    }

    /// 把残骸的头重写成当前格式与身份：截到 0 再写，否则残余的半个头会留在新头之前。
    /// 复用调用方已打开的那个句柄（不按路径二次打开）
    pub(crate) fn rewrite_header(path: &Path, f: &mut File, identity: &LiveScene) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&Self::header(identity))?;
        f.sync_all()?;
        best_effort_dir_fsync(path);
        Ok(())
    }

    /// 首次记录时才落盘。magic 先于记录、记录先于实际写入，故任何时刻的盘上内容
    /// 都不会超前于已经被记录的写入。
    ///
    /// 用 `create_new` 而不是 `create`：`open` 的"文件不存在"与本处的"创建它"之间存在
    /// TOCTOU 窗口，普通的 create 会安静地打开一个刚被别人建好的文件，把两方记录交错进
    /// 同一份 journal（一次 undo 就会回放出不属于本次的字节）。`create_new` 是原子断言
    /// "此前不存在"，失败即说明有人抢先——要么是另一个 diskedit 正在用同一个目标，
    /// 要么是上一次的 journal 没处理干净
    pub(crate) fn ensure(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            // 落点目录归文件自己保证：路径由身份派生，身份不知道目录是否存在
            if let Some(dir) = self.path.parent() {
                best_effort_mkdir(dir);
            }
            let mut f = OpenOptions::new().read(true).write(true).create_new(true).open(&self.path).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "{} appeared after the target was opened — another diskedit run may be using this target, or a previous journal was left behind (resolve it with `diskedit undo` first)",
                            self.path.display()
                        ),
                    )
                } else {
                    e
                }
            })?;
            use std::io::Write;
            let head = Self::header(&self.identity);
            let init = f.write_all(&head).and_then(|()| f.sync_all());
            if let Err(e) = init {
                // 头没写完的文件不属于可采纳的形态（0 字节残骸或完整头），重试会报成
                // "另一个 diskedit 在用"，下次 open 又只能 abandon——清掉自己刚建的文件，
                // 让本地 I/O 故障保持成"没有 journal"，而不是留下需要人工释放的假现场
                drop(f);
                #[allow(clippy::let_underscore_must_use)] // 清理失败不改变主错误：残留的空文件下次打开会报成占用
                let _ = std::fs::remove_file(&self.path);
                return Err(e);
            }
            // 新建文件的目录项必须落盘：否则断电后 journal 整体消失，而写入已经发生。
            // 这一步失败即中止本次写入——没有可回滚凭据的写入不做。与头没写完同一处置：
            // 清掉自己刚建的文件，把本地 I/O 故障留成"没有 journal"，而不是一份要被
            // 下一次运行重新认领的壳
            if let Err(e) = sync_dir_entry(&self.path) {
                drop(f);
                #[allow(clippy::let_underscore_must_use)] // 清理失败不改变主错误：残留的空文件下次打开会报成占用
                let _ = std::fs::remove_file(&self.path);
                return Err(io::Error::other(format!(
                    "cannot persist the directory entry of the journal at {}: {e} — no record was made",
                    self.path.display()
                )));
            }
            // 建立时的文件身份与创建事件一起记下：commit 只凭路径删不掉已被换成
            // 另一个文件的落点。**取不到身份就清掉文件并报错**——留一份零指纹等于给
            // commit 的删除动作发一张空白凭据，而这一侧猜错的后果是删掉别人的现场
            let fp = match TargetFingerprint::from_file(&f) {
                Ok(fp) => fp,
                Err(e) => {
                    drop(f);
                    #[allow(clippy::let_underscore_must_use)] // 清理失败不改变主错误：残留的空文件下次打开会报成占用
                    let _ = std::fs::remove_file(&self.path);
                    return Err(io::Error::other(format!(
                        "cannot read the identity of the journal just created at {}: {e} — no record was made and the file has been removed",
                        self.path.display()
                    )));
                }
            };
            *CREATED_JOURNAL.lock().unwrap_or_else(|e| e.into_inner()) = Some((self.path.clone(), fp));
            self.file = Some(f);
        }
        Ok(self.file.as_mut().expect("the branch above fills an empty slot"))
    }

    pub fn record(&mut self, off: u64, bytes: &[u8], after: &[u8]) -> io::Result<()> {
        let rec = JournalRecord {
            mutation: self.mutation,
            recovery: RecoveryData::PreImage { off, bytes: bytes.to_vec(), after: after.to_vec() },
        };
        self.append(&rec)
    }

    /// 落一条屏障记录：从这一刻起，当前 mutation 已越过不可回滚点，undo 见到即整体拒绝
    /// （见 [`WriteSource::mark_non_reversible`]）
    pub fn barrier(&mut self) -> io::Result<()> {
        if self.barred {
            return Ok(());
        }
        let rec = JournalRecord { mutation: self.mutation, recovery: RecoveryData::Barrier };
        self.append(&rec)?;
        self.barred = true;
        Ok(())
    }

    /// 设置后续记录的 mutation。命令层在自己即将做的那类改动发生变化时调用
    /// （例如表项写完后要搬数据、要交给外部 FS 工具）
    pub fn set_mutation(&mut self, m: Mutation) {
        if self.mutation != m {
            // 换了一类改动就换了一条不可回滚线：新 mutation 要自己的屏障
            self.barred = false;
        }
        self.mutation = m;
    }

    /// 记录的 CRC 覆盖 `[len][payload]`：长度字段被位翻转时校验失败，而不是被当成
    /// "更长的载荷"继续扫描。写入与读取共用这一处口径
    pub(crate) fn record_crc(len: [u8; 4], payload: &[u8]) -> u32 {
        let mut frame = Vec::with_capacity(len.len() + payload.len());
        frame.extend_from_slice(&len);
        frame.extend_from_slice(payload);
        crate::table::crc32(&frame)
    }

    /// 追加一条记录。调用方必须持有目标锁（生产路径上只有 [`WriteSource::write_at`] /
    /// [`WriteSource::mark_non_reversible`] 会走到这里，而 journal 只挂在持有独占所有权的
    /// 写句柄上）：`seek(End)` 与随后的写入之间不能有第二个写者。
    ///
    /// 句柄不是 append 模式，追加位置因此由这里显式给出——只 seek 一次，然后把
    /// `[len][crc][payload]` 连续写完，避免帧中间夹进别人的写入
    pub(crate) fn append(&mut self, rec: &JournalRecord) -> io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let payload = rec.encode();
        // 长度字段按 u32 落盘：超限静默截断会破坏 [len][crc][payload] 的帧定位，显式拒绝
        let n = u32::try_from(payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "journal record exceeds u32 length"))?;
        let f = self.ensure()?;
        f.seek(SeekFrom::End(0))?;
        let n_le = n.to_le_bytes();
        f.write_all(&n_le)?;
        f.write_all(&Self::record_crc(n_le, &payload).to_le_bytes())?;
        f.write_all(&payload)?;
        f.sync_data()
    }

    /// 逐条校验并读取。**契约不变**：已形成的记录必须 CRC 正确，任一条损坏即整体拒绝，
    /// 不做"跳过坏记录继续回放"式的部分回放。
    ///
    /// 额外单列的是"**未完成的尾部记录**"——它与"损坏"不是一回事：`record` 先写头再写数据、
    /// 最后才 sync，而调用方在 record 返回之后才真正写盘，因此尾部撕裂只可能来自一次没走完的
    /// 追加，那条记录对应的数据写入**根本没有发生**，丢弃它是安全的。中途（非尾部）CRC 不符
    /// 才是真实损坏，仍整体拒绝
    ///
    /// 记录头里的 len 参与 CRC，故"len 被写坏"不会落进"载荷只写了一半"那一类：
    /// 在记录头完整、载荷长度仍落在文件内时，校验失败即整体拒绝。
    /// 越界帧的载荷读不全，CRC 无从校验，改由"越界之后是否还有 CRC 通过的完整帧"区分：
    /// 有则长度字段在文件中段损坏，整体拒绝；无才是尾部撕裂。返回的前缀每条都通过了
    /// 该文件的 CRC，且各自对应的写入确实发生过
    pub fn read_entries(path: &Path) -> io::Result<JournalRead> {
        let data = std::fs::read(path)?;
        // 头（版本 + 身份 + 头 CRC）先整体校验：版本不符在这里就报出来，不会走到记录解析
        Self::parse_header(&data)?;
        let (out, end) = Self::scan_entries(&data)?;
        Ok(match NonZeroUsize::new(data.len() - end) {
            Some(cut) => JournalRead::TruncatedTail { records: out, cut },
            None => JournalRead::Complete(out),
        })
    }

    /// 逐条扫描 journal 内容（含 magic 与身份段的头），供 [`Self::read_entries`]（回放）与
    /// [`Self::open`]（采纳时的撕裂尾截断）共用同一套判据。返回
    /// `(完整记录, 最后一个完整记录的结束偏移)`——结束偏移与 `data.len()` 之差即撕裂尾的
    /// 字节数；两者相等表示尾部完整。零完整记录且撕裂时结束偏移即头长
    pub(crate) fn scan_entries(data: &[u8]) -> io::Result<(Vec<JournalRecord>, usize)> {
        let mut out = Vec::new();
        let mut pos = Self::HEADER_LEN;
        while pos < data.len() {
            // 记录头尚未写全 → 尾部未完成的事务（加法 checked：输入是外部文件内容）
            if pos.checked_add(REC_HDR).is_none_or(|end| end > data.len()) {
                return Ok((out, pos));
            }
            let n_le: [u8; 4] = data[pos..pos + 4].try_into().unwrap();
            let len = u32::from_le_bytes(n_le) as usize;
            let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
            pos += REC_HDR;
            // len == 0 是"帧头只写了一半"的形态（8 个零字节）：先看其后是否还有完整
            // 记录——有则是中段损坏（截断会删掉真实记录），没有则只是撕裂尾。
            // 不先算 CRC：crc32 对空载荷的取值会让零帧"通过"校验，再在 decode
            // 处炸成整份 journal 不可读，一份本可回滚的现场就只剩 abandon 一条路
            if len == 0 {
                if Self::later_complete_record(data, pos) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "journal entry has zero length but complete records follow it",
                    ));
                }
                return Ok((out, pos - REC_HDR));
            }
            // 头写全了但载荷没写全：可能是一次没走完的追加，也可能是中段 len 被写坏。
            // 后者不能按撕裂丢弃——截断会把其后每条 CRC 通过、写入确实发生过的记录删掉
            if pos.checked_add(len).is_none_or(|end| end > data.len()) {
                if Self::later_complete_record(data, pos) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "journal entry length runs past the end of the file (records follow it)",
                    ));
                }
                return Ok((out, pos - REC_HDR));
            }
            let payload = &data[pos..pos + len];
            if Self::record_crc(n_le, payload) != crc {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "journal entry CRC mismatch"));
            }
            pos += len;
            // 载荷解不出来同样是"读不出来"：宁可让调用方按"有事没做完"处理，也不静默跳过
            out.push(JournalRecord::decode(payload)?);
        }
        Ok((out, data.len()))
    }

    /// `from` 之后是否存在一条头完整、载荷落在文件内、CRC 通过且能解码的记录。
    /// 越界帧的真实载荷起点未知（len 本身不可信），逐字节试；载荷须能解码，避免把
    /// 随机字节序列的 CRC 巧合当成一条记录
    fn later_complete_record(data: &[u8], from: usize) -> bool {
        let mut pos = from;
        while pos + REC_HDR <= data.len() {
            let n_le: [u8; 4] = data[pos..pos + 4].try_into().unwrap();
            let len = u32::from_le_bytes(n_le) as usize;
            let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
            let start = pos + REC_HDR;
            if start.checked_add(len).is_some_and(|end| end <= data.len()) {
                let payload = &data[start..start + len];
                if Self::record_crc(n_le, payload) == crc && JournalRecord::decode(payload).is_ok() {
                    return true;
                }
            }
            pos += 1;
        }
        false
    }
}
