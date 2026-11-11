//! 宿主平台实现：识别与打开委托给已打开的目标层，身份取自句柄与 sysfs，
//! 不从命令行路径二次取事实。

use super::*;
use crate::dev::FileSource;

pub(crate) struct HostPlatform;

impl sealed::Sealed for HostPlatform {}

impl Platform for HostPlatform {
    fn identify(&self, spec: &TargetSpec) -> Result<TargetProbe, PlatformError> {
        let src = open_read_only(spec)?;
        probe_of(&src)
    }
}

/// 只读打开：块设备不能被 `--sector-size` 覆盖成非内核值；镜像按声明值打开
fn open_read_only(spec: &TargetSpec) -> Result<FileSource, PlatformError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(meta) = std::fs::metadata(spec.path())
            && meta.file_type().is_block_device()
        {
            if spec.sector_override().is_some() {
                return Err(PlatformError::Unsupported(
                    "a block device's sector size is reported by the kernel; --sector-size cannot override it"
                        .to_string(),
                ));
            }
            return FileSource::open_read_only(spec.path()).map_err(PlatformError::Io);
        }
    }
    FileSource::open_read_only_image(spec.path(), spec.sector_override().map(|s| s.get()))
        .map_err(PlatformError::Io)
}

/// 只读命令的打开失败，按成因分开由调用方落码
pub(crate) enum ReadOnlyOpen {
    /// 打开失败：目标不存在、无权限、块设备拓扑读不出
    Io(std::io::Error),
    /// `--sector-size` 落在块设备上：请求与目标形态不符
    SectorOverrideOnBlock(String),
    /// 扇区大小不在允许集合，几何无法构造。
    /// 判据来自 `dev::source` 的单点校验（`check_sector_size`，`InvalidInput`）
    SectorSizeNotConstructible,
}

/// 只读命令的数据句柄：只打开目标，不走平台端口的身份探测。
///
/// 身份是写命令的门槛（拿不到身份时按退出码 6 拒绝）；只读命令在身份不可得时
/// 照常工作，不新增失败面。
pub(crate) fn open_read_only_source(spec: &TargetSpec) -> Result<FileSource, ReadOnlyOpen> {
    match open_read_only(spec) {
        Ok(src) => Ok(src),
        Err(PlatformError::Unsupported(reason)) => Err(ReadOnlyOpen::SectorOverrideOnBlock(reason)),
        Err(PlatformError::Io(e)) if e.kind() == std::io::ErrorKind::InvalidInput => {
            Err(ReadOnlyOpen::SectorSizeNotConstructible)
        }
        Err(PlatformError::Io(e)) => Err(ReadOnlyOpen::Io(e)),
        // `open_read_only` 只产生 `Io` 与 `Unsupported`；此分支仅为穷尽
        Err(PlatformError::Unidentifiable(reason)) => Err(ReadOnlyOpen::Io(std::io::Error::other(reason))),
    }
}

pub(crate) fn probe_of(src: &FileSource) -> Result<TargetProbe, PlatformError> {
    SectorSize::try_new(src.sector_size).map_err(|_| {
        PlatformError::Unidentifiable(format!(
            "reported sector size {} is not a supported value",
            src.sector_size
        ))
    })?;
    if src.is_block {
        return block_probe(src);
    }
    image_probe(src)
}

/// 块设备：整盘设备号 + 平台读到的稳定设备 ID。设备 ID 缺项时仍保留设备号；
/// 设备号单独不构成跨现场身份，写命令由 `TransactionManager::refuse_unidentifiable`
/// 与记录落点派生（`TargetIdentity::record_path`）在任何字节落盘前拒绝
#[cfg(target_os = "linux")]
fn block_probe(src: &FileSource) -> Result<TargetProbe, PlatformError> {
    let node = crate::dev::sysfs_node(&src.path).map_err(PlatformError::Io)?;
    let (major, minor) = sysfs_device_number(&node)?;
    let (_, raw_id) = crate::dev::disk_identity(&node).map_err(PlatformError::Io)?;
    let id = TargetId::from_material(IdentityMaterial::block(major, minor, raw_id.as_deref()));
    Ok(TargetProbe::new(id, src.size))
}

/// 整盘设备号取自 sysfs 的 `dev` 文件（`maj:min`）：按设备号而非节点名认盘，
/// 分区节点因此与整盘共享同一份身份
#[cfg(target_os = "linux")]
fn sysfs_device_number(node: &Path) -> Result<(u32, u32), PlatformError> {
    let text = std::fs::read_to_string(node.join("dev")).map_err(PlatformError::Io)?;
    text.trim()
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .ok_or_else(|| {
            PlatformError::Unidentifiable(format!(
                "cannot parse the device number {text:?} of {}",
                node.display()
            ))
        })
}

#[cfg(not(target_os = "linux"))]
fn block_probe(_src: &FileSource) -> Result<TargetProbe, PlatformError> {
    Err(PlatformError::Unsupported(
        "block devices are not identified on this platform".to_string(),
    ))
}

/// 镜像：文件 ID 必需（复制/移动得到新 ID ⇒ 新目标）；卷序列号只作画像，不参与比较
fn image_probe(src: &FileSource) -> Result<TargetProbe, PlatformError> {
    let fp = src.fingerprint;
    let file_id = fp.has_inode_evidence().then(|| {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&fp.dev.to_be_bytes());
        id[8..].copy_from_slice(&fp.ino.to_be_bytes());
        id
    });
    // Unix 的 `dev` 是文件所在设备的设备号，不是卷序列号；Windows 的 `dev` 才是卷序列号
    #[cfg(windows)]
    let volume_serial = Some(fp.dev as u32);
    #[cfg(not(windows))]
    let volume_serial = None;
    let id = TargetId::from_material(IdentityMaterial::image(volume_serial, file_id)?);
    Ok(TargetProbe::new(id, src.size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_image(tag: &str) -> (crate::testdir::TestDir, std::path::PathBuf) {
        let dir = crate::testdir::test_dir(tag);
        let img = dir.join("disk.img");
        std::fs::write(&img, vec![0u8; 1024 * 1024]).unwrap();
        (dir, img)
    }

    /// 复制出的文件是新目标：路径可以同名，但文件 ID 不同
    #[test]
    fn a_copied_file_is_not_the_same_target() {
        let (dir, img) = temp_image("copy");
        let copy = dir.join("copy.img");
        std::fs::copy(&img, &copy).unwrap();
        let host = HostPlatform;
        let a = host.identify(&TargetSpec::new(&img, None)).unwrap();
        let b = host.identify(&TargetSpec::new(&copy, None)).unwrap();
        assert_ne!(a.id(), b.id());
    }

    /// 文件 ID 为全零说明平台没有给出身份证据：拒绝识别，路径不能顶上
    #[test]
    fn missing_file_id_is_refused_by_the_port() {
        let material = IdentityMaterial::image(None, None);
        assert!(matches!(material, Err(PlatformError::Unidentifiable(_))));
    }

    /// 真实块设备：身份取 sysfs 整盘设备号与设备 ID，扇区大小取内核报告值。
    /// 默认忽略：没有块设备的机器上不能凭空造出一个，节点也必须由运行者显式给出，
    /// 免得测试自己挑盘。运行方式：DISKEDIT_BLOCK_DEVICE=/dev/sdX cargo test -- --ignored
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a real block device named by DISKEDIT_BLOCK_DEVICE"]
    fn identifies_a_real_block_device() {
        let path = std::env::var("DISKEDIT_BLOCK_DEVICE")
            .expect("set DISKEDIT_BLOCK_DEVICE to a block device node");
        let host = HostPlatform;
        let spec = TargetSpec::new(path, None);
        let probe = host.identify(&spec).expect("a real block device is identifiable");
        assert!(probe.total_bytes() > 0);
    }
}
