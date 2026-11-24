//! 目标打开、身份与拓扑。
//!
//! 用路径元数据区分普通文件与块设备。普通文件走 [`FileSource`]，块设备走
//! [`BlockSource`]。打开后保存路径、种类、字节长度与逻辑扇区；普通文件没有设备级
//! 扇区，`sector_size` 为 `None`。
//!
//! 身份规则：块设备以容量为底，有 `device/wwid`、`device/serial` 或 `serial` 时
//! 一并入指纹；loop 与部分 dm 设备没有这些属性，仅容量仍足以区分目标。普通文件
//! 用设备号与 inode 号混合。major:minor、路径、当前挂载点这类易变值不构成强指纹。

use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;

use super::source::{BlockSource, FileSource, Lease, ReadAt, WriteAt};
use super::{PlatformError, PlatformResult, read_optional_attr, read_required_attr, sysfs_root};
use crate::platform::{Partition, TargetHandle, TargetKind};

/// 目标底层句柄。
#[derive(Debug)]
enum TargetSource {
    File(FileSource),
    Block(BlockSource),
}

impl TargetSource {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        match self {
            Self::File(source) => ReadAt::read_at(source, offset, buf),
            Self::Block(source) => ReadAt::read_at(source, offset, buf),
        }
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        match self {
            Self::File(source) => WriteAt::write_at(source, offset, buf),
            Self::Block(source) => WriteAt::write_at(source, offset, buf),
        }
    }

    fn barrier(&mut self) -> PlatformResult<()> {
        match self {
            Self::File(source) => WriteAt::barrier(source),
            Self::Block(source) => WriteAt::barrier(source),
        }
    }

    fn metadata(&self) -> PlatformResult<std::fs::Metadata> {
        let result = match self {
            Self::File(source) => source.file().metadata(),
            Self::Block(source) => source.file().metadata(),
        };
        result.map_err(|error| PlatformError::io("<open target>", error))
    }
}

/// 已打开的目标。
#[derive(Debug)]
pub struct Target {
    path: PathBuf,
    kind: TargetKind,
    len: u64,
    sector_size: Option<u64>,
    source: TargetSource,
    lease: Option<Lease>,
}

impl Target {
    /// 以读写方式打开目标。
    pub fn open(path: &Path) -> PlatformResult<Self> {
        Self::open_with(path, true)
    }

    /// 以只读方式打开目标；用于身份、拓扑与占用探测。
    pub fn open_read_only(path: &Path) -> PlatformResult<Self> {
        Self::open_with(path, false)
    }

    fn open_with(path: &Path, writable: bool) -> PlatformResult<Self> {
        let metadata = std::fs::metadata(path).map_err(|error| PlatformError::io(path, error))?;
        let file_type = metadata.file_type();
        if file_type.is_file() {
            let source = if writable {
                FileSource::open(path)?
            } else {
                FileSource::open_read_only(path)?
            };
            let len = source.len_bytes();
            return Ok(Self {
                path: path.to_path_buf(),
                kind: TargetKind::File,
                len,
                sector_size: None,
                source: TargetSource::File(source),
                lease: None,
            });
        }
        #[cfg(unix)]
        if std::os::unix::fs::FileTypeExt::is_block_device(&file_type) {
            let source = BlockSource::open(path)?;
            let len = source.len_bytes();
            let sector_size = source.sector_size();
            return Ok(Self {
                path: path.to_path_buf(),
                kind: TargetKind::Block,
                len,
                sector_size: Some(sector_size),
                source: TargetSource::Block(source),
                lease: None,
            });
        }
        Err(PlatformError::unsupported(format!(
            "{} 既不是普通文件也不是块设备",
            path.display()
        )))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> TargetKind {
        self.kind
    }

    pub fn is_block(&self) -> bool {
        matches!(self.kind, TargetKind::Block)
    }

    pub fn len_bytes(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn sector_size(&self) -> Option<u64> {
        self.sector_size
    }

    /// 字节位宽建议值：块设备取逻辑扇区，普通文件取 512。
    pub fn sector_width(&self) -> u64 {
        self.sector_size.unwrap_or(512)
    }

    pub fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        self.source.read_at(offset, buf)
    }

    pub fn write_bytes(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        self.source.write_at(offset, buf)
    }

    pub fn metadata(&self) -> PlatformResult<std::fs::Metadata> {
        self.source.metadata()
    }

    /// 取得整盘独占地。租约随目标句柄存活。
    pub fn lock_exclusive(&mut self) -> PlatformResult<()> {
        if self.lease.is_none() {
            self.lease = Some(Lease::acquire(&self.path)?);
        }
        Ok(())
    }
}

impl TargetHandle for Target {
    fn path(&self) -> &Path {
        &self.path
    }

    fn kind(&self) -> TargetKind {
        self.kind
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> Option<u64> {
        self.sector_size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        self.source.read_at(offset, buf)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        self.source.write_at(offset, buf)
    }

    fn barrier(&mut self) -> PlatformResult<()> {
        self.source.barrier()
    }

    fn lock_exclusive(&mut self) -> PlatformResult<()> {
        Target::lock_exclusive(self)
    }
}

/// 计算目标指纹。
pub fn fingerprint(target: &Target) -> PlatformResult<Fingerprint> {
    match target.kind() {
        TargetKind::Block => block_fingerprint(target),
        TargetKind::File => file_fingerprint(target),
    }
}

fn block_fingerprint(target: &Target) -> PlatformResult<Fingerprint> {
    let kname = disk_name(target.path())?;
    let base = sysfs_root().join("block").join(&kname);
    // 容量是每块设备都有的辅助证据；loop 与部分 dm 设备没有 wwid/serial，只有容量
    // 仍足以给出可用指纹，不能因此把整类目标判成"身份不可证"。
    let mut material = target.len_bytes().to_le_bytes().to_vec();
    for candidate in ["device/wwid", "device/serial", "serial"] {
        if let Ok(Some(value)) = read_optional_attr(&base.join(candidate)) {
            if !value.is_empty() {
                material.extend_from_slice(candidate.as_bytes());
                material.push(b'=');
                material.extend_from_slice(value.as_bytes());
                break;
            }
        }
    }
    Ok(Fingerprint::strong(stable_hash32(&material)))
}

fn file_fingerprint(target: &Target) -> PlatformResult<Fingerprint> {
    let metadata = target.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev = metadata.dev();
        let ino = metadata.ino();
        if dev == 0 && ino == 0 {
            return Ok(Fingerprint::unusable([0u8; 32]));
        }
        let mut material = [0u8; 16];
        material[..8].copy_from_slice(&dev.to_le_bytes());
        material[8..].copy_from_slice(&ino.to_le_bytes());
        Ok(Fingerprint::strong(stable_hash32(&material)))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(PlatformError::not_applicable(
            "当前平台没有设备号与 inode 号，无法构造文件指纹",
        ))
    }
}

/// 由稳定材料得到 32 字节摘要，只用于身份比较。
fn stable_hash32(bytes: &[u8]) -> [u8; 32] {
    let mut lanes: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x8422_2325_cbf2_9ce4,
        0x9e37_79b9_7f4a_7c15,
        0x517c_c1b7_2722_0a95,
    ];
    let lane_count = lanes.len();
    for (index, byte) in bytes.iter().enumerate() {
        let lane = index % lane_count;
        lanes[lane] ^= u64::from(*byte);
        lanes[lane] = lanes[lane].wrapping_mul(0x0000_0100_0000_01b3);
    }
    let length = bytes.len() as u64;
    let mut digest = [0u8; 32];
    for (lane, value) in lanes.iter().enumerate() {
        let mixed = mix64(*value ^ length.rotate_left((lane as u32) * 8));
        digest[lane * 8..lane * 8 + 8].copy_from_slice(&mixed.to_le_bytes());
    }
    digest
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 33;
    value = value.wrapping_mul(0xff51_afd7_ed55_8ccd);
    value ^= value >> 33;
    value = value.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    value ^= value >> 33;
    value
}

/// 从设备路径提取内核设备名。
pub fn disk_name(path: &Path) -> PlatformResult<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PlatformError::parse(format!("{} 中没有设备名", path.display())))?;
    if name.is_empty() {
        return Err(PlatformError::parse(format!(
            "{} 中的设备名为空",
            path.display()
        )));
    }
    Ok(name.to_string())
}

/// 读取目标的分区集合；非块设备返回空集合。
pub fn partitions(target: &Target) -> PlatformResult<Vec<Partition>> {
    if !target.is_block() {
        return Ok(Vec::new());
    }
    let kname = disk_name(target.path())?;
    let device_dir = sysfs_root().join("block").join(&kname);
    let entries = match std::fs::read_dir(&device_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(PlatformError::io(&device_dir, error)),
    };

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(&device_dir, error))?;
        // 整盘目录混有 `uevent`、`size` 等普通文件；只把子目录当候选分区。
        if !entry
            .file_type()
            .map_err(|error| PlatformError::io(entry.path(), error))?
            .is_dir()
        {
            continue;
        }
        let part_dir = entry.path();
        let number_text = match read_optional_attr(&part_dir.join("partition"))? {
            Some(text) => text,
            None => continue,
        };
        let number = parse_u64("分区号", &number_text)?;
        let number = u32::try_from(number)
            .map_err(|_| PlatformError::parse(format!("分区号 {number} 超出 u32 范围")))?;
        let start_text = read_required_attr(&part_dir.join("start"))?;
        let size_text = read_required_attr(&part_dir.join("size"))?;
        found.push(Partition {
            number,
            start_lba: parse_u64("分区起点", &start_text)?,
            sectors: parse_u64("分区长度", &size_text)?,
        });
    }
    found.sort_by_key(|partition| partition.number);
    Ok(found)
}

/// 定位块设备上某个分区的设备节点。
///
/// 先按 sysfs 中的 `partition` 属性找到分区目录，读 `dev` 得到 `major:minor`，
/// 优先使用 `/dev/block/<major>:<minor>`；没有该符号链接时按内核命名规则拼出
/// 分区节点（`sda1`、`nvme0n1p1`、`mmcblk0p1`）。两条路径都不存在时如实失败，
/// 不用整盘节点冒充分区。
pub fn partition_node(target: &Target, number: u32) -> PlatformResult<PathBuf> {
    let kname = disk_name(target.path())?;
    let device_dir = sysfs_root().join("block").join(&kname);
    let entries =
        std::fs::read_dir(&device_dir).map_err(|error| PlatformError::io(&device_dir, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| PlatformError::io(&device_dir, error))?;
        if !entry
            .file_type()
            .map_err(|error| PlatformError::io(entry.path(), error))?
            .is_dir()
        {
            continue;
        }
        let part_dir = entry.path();
        let number_text = match read_optional_attr(&part_dir.join("partition"))? {
            Some(text) => text,
            None => continue,
        };
        if parse_u64("分区号", &number_text)? != u64::from(number) {
            continue;
        }
        if let Some(dev) = read_optional_attr(&part_dir.join("dev"))? {
            let candidate = PathBuf::from("/dev/block").join(&dev);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
        let node = conventional_partition_path(&kname, number);
        if node.exists() {
            return Ok(node);
        }
        return Err(PlatformError::not_supported(format!(
            "{} 的分区 {number} 在 /dev 下没有可用的设备节点",
            target.path().display()
        )));
    }
    Err(PlatformError::parse(format!(
        "{} 上没有编号为 {number} 的分区",
        target.path().display()
    )))
}

fn conventional_partition_path(kname: &str, number: u32) -> PathBuf {
    let ends_with_digit = kname
        .chars()
        .last()
        .is_some_and(|character| character.is_ascii_digit());
    let name = if ends_with_digit {
        format!("{kname}p{number}")
    } else {
        format!("{kname}{number}")
    };
    PathBuf::from("/dev").join(name)
}

fn parse_u64(what: &str, text: &str) -> PlatformResult<u64> {
    text.trim()
        .parse::<u64>()
        .map_err(|_| PlatformError::parse(format!("{what}不是十进制整数：{text:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hash_is_deterministic_and_separating() {
        assert_eq!(stable_hash32(b"disk-one"), stable_hash32(b"disk-one"));
        assert_ne!(stable_hash32(b"disk-one"), stable_hash32(b"disk-two"));
        assert_ne!(stable_hash32(b"ab"), stable_hash32(b"ba"));
    }

    #[test]
    fn disk_name_takes_last_component() {
        assert_eq!(
            disk_name(Path::new("/dev/nvme0n1")).expect("有设备名"),
            "nvme0n1"
        );
        assert!(disk_name(Path::new("/")).is_err());
    }

    #[test]
    fn parse_u64_reads_decimal_only() {
        assert_eq!(parse_u64("起点", " 2048\n").expect("可解析"), 2048);
        assert!(parse_u64("起点", "0x800").is_err());
    }
}
