//! Windows 后端子树：镜像文件的句柄、身份、占用与记录目录。
//!
//! 本宿主没有可写的块设备视图，也不提供本项目依赖的外部文件系统工具，因此
//! 端口只回答镜像事实；写命令由分派层的平台能力门按 [`crate::platform::WRITING_HOSTS`]
//! 拒绝，不会走到这里。系统调用与 Win32 结构体只出现在本子树内，上层看到的是
//! 中立的端口形状。

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::Program;
use sha2::{Digest, Sha256};

use crate::platform::{
    DeviceMatch, LoopBinding, Occupancy, Platform, PlatformError, PlatformResult, RecordEntry,
    StateLock, TargetHandle, TargetKind, ToolRun,
};

mod identity;
mod random;
mod record;

/// Windows 宿主端口。
#[derive(Debug, Default)]
pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn open_target(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        WindowsHandle::open(path.to_path_buf(), file)
    }

    fn open_target_read_only(&self, path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        WindowsHandle::open(path.to_path_buf(), file)
    }

    fn path_kind(&self, path: &Path) -> TargetKind {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => TargetKind::File,
            _ => TargetKind::Other,
        }
    }

    fn is_partition_node(&self, _path: &Path) -> bool {
        // 本宿主的块设备没有可按路径打开的分区节点；`\\.\PhysicalDriveN` 不是
        // 分区视图，不能当作分区节点。
        false
    }

    fn identity(&self, path: &Path) -> PlatformResult<Fingerprint> {
        let metadata = std::fs::metadata(path).map_err(|error| PlatformError::io(path, error))?;
        if !metadata.is_file() {
            // 不是普通文件：目录、设备与管道都不该有记录身份。这里返回"不可比较"，
            // 由上层按形态拒绝；不退回按路径派生身份，那会把不同的对象认成同一个。
            return Ok(Fingerprint::unusable([0u8; 32]));
        }
        let file = File::open(path).map_err(|error| PlatformError::io(path, error))?;
        let (volume_serial, file_index) =
            identity::file_id(&file).map_err(|error| PlatformError::io(path, error))?;
        let len = metadata.len();
        // 与 Unix 侧同样的算法：文件身份加此刻长度。卷序列号参与材料，因为
        // 文件索引只在卷内唯一；跨卷的两个同名文件不能算同一目标。
        let mut hasher = Sha256::new();
        hasher.update(b"file\0");
        hasher.update(volume_serial.to_le_bytes());
        hasher.update(file_index.to_le_bytes());
        hasher.update(len.to_le_bytes());
        Ok(Fingerprint::strong(hasher.finalize().into()))
    }

    fn occupancy(&self, _path: &Path) -> PlatformResult<Occupancy> {
        // 本宿主没有 loop 设备，也没有可枚举的持有者视图：镜像被别的进程打开时
        // 无从判定。返回未知而不是空闲，写路径据此拒绝而不是当作可以写。
        Ok(Occupancy::Unknown {
            why: "this host cannot enumerate the holders of an image file".to_owned(),
        })
    }

    fn state_root(&self) -> Option<PathBuf> {
        if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
            return Some(PathBuf::from(root).join("diskedit"));
        }
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("diskedit"))
    }

    fn is_directory(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn list_record_entries(&self, root: &Path, name: &str) -> PlatformResult<Vec<PathBuf>> {
        record::list(root, name)
    }

    fn read_record(&self, path: &Path) -> PlatformResult<RecordEntry> {
        record::read(path)
    }

    fn write_record(&self, path: &Path, bytes: &[u8]) -> PlatformResult<()> {
        record::write(path, bytes)
    }

    fn remove_record(&self, path: &Path) -> PlatformResult<()> {
        record::remove(path)
    }

    fn rename_record(&self, from: &Path, to: &Path) -> PlatformResult<()> {
        record::rename(from, to)
    }

    fn run(&self, _program: Program, _args: &[String]) -> PlatformResult<ToolRun> {
        Err(PlatformError::not_supported(
            "this host does not run external filesystem tools",
        ))
    }

    fn tool_available(&self, _program: Program) -> bool {
        false
    }

    fn tool_name(&self, _program: Program) -> &'static str {
        "unavailable"
    }

    fn bind_loop(&self, _path: &Path, _offset: u64, _len: u64) -> PlatformResult<LoopBinding> {
        Err(PlatformError::not_supported(
            "this host has no loop devices",
        ))
    }

    fn partition_device(&self, _disk: &Path, _number: u32) -> PlatformResult<PathBuf> {
        Err(PlatformError::not_supported(
            "this host has no partition device nodes",
        ))
    }

    fn compare_devices(&self, _target: &Path, _reported: &Path) -> DeviceMatch {
        DeviceMatch::Unknown("this host cannot compare device identities".to_owned())
    }

    fn entropy(&self, len: usize) -> PlatformResult<Vec<u8>> {
        random::bytes(len)
    }

    fn create_directory_all(&self, path: &Path) -> PlatformResult<()> {
        std::fs::create_dir_all(path).map_err(|error| PlatformError::io(path, error))
    }

    fn remove_directory(&self, path: &Path) -> PlatformResult<()> {
        match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(PlatformError::io(path, error)),
        }
    }

    fn temporary_directory(&self, tag: &str) -> PlatformResult<PathBuf> {
        let suffix = self.entropy(8)?;
        let mut name = String::from("diskedit.");
        name.push_str(tag);
        name.push('.');
        for byte in suffix {
            name.push_str(&format!("{byte:02x}"));
        }
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).map_err(|error| PlatformError::io(&path, error))?;
        Ok(path)
    }

    fn lock_state(&self, path: &Path) -> PlatformResult<Box<dyn StateLock>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| PlatformError::io(path, error))?;
        file.try_lock().map_err(|error| {
            let kind = match error {
                std::fs::TryLockError::WouldBlock => io::ErrorKind::WouldBlock,
                std::fs::TryLockError::Error(_) => io::ErrorKind::Other,
            };
            PlatformError::io(
                path,
                io::Error::new(kind, format!("another run holds the lock: {error}")),
            )
        })?;
        Ok(Box::new(ExclusiveLock { _file: file }))
    }
}

/// 句柄关闭即释放整文件锁。
struct ExclusiveLock {
    _file: File,
}

impl StateLock for ExclusiveLock {}

/// 已打开的 Windows 目标：一个按偏移读写的普通文件。
struct WindowsHandle {
    path: PathBuf,
    file: File,
    len: u64,
    kind: TargetKind,
}

impl WindowsHandle {
    fn open(path: PathBuf, file: File) -> PlatformResult<Box<dyn TargetHandle>> {
        let metadata = file
            .metadata()
            .map_err(|error| PlatformError::io(&path, error))?;
        let kind = if metadata.is_file() {
            TargetKind::File
        } else {
            TargetKind::Other
        };
        let len = metadata.len();
        Ok(Box::new(Self {
            path,
            file,
            len,
            kind,
        }))
    }
}

impl TargetHandle for WindowsHandle {
    fn path(&self) -> &Path {
        &self.path
    }

    fn len(&self) -> u64 {
        // 返回"此刻长度"而不是打开时的缓存：镜像可能被外部截断或增长。
        self.file.metadata().map_or(self.len, |meta| meta.len())
    }

    fn kind(&self) -> TargetKind {
        self.kind
    }

    fn sector_size(&self) -> Option<u64> {
        // 镜像没有内核扇区事实；`resolved_sector_size` 会退回镜像的固定块大小。
        None
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
        use std::os::windows::fs::FileExt;
        FileExt::seek_read(&self.file, buf, offset)
            .map_err(|error| PlatformError::io(&self.path, error))
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> PlatformResult<usize> {
        use std::os::windows::fs::FileExt;
        // 与 Unix 侧同一条上界：写入前取此刻长度，越界写拒绝，而不是让文件静默变长。
        let current = self
            .file
            .metadata()
            .map_err(|error| PlatformError::io(&self.path, error))?
            .len();
        let end = offset.checked_add(buf.len() as u64).ok_or_else(|| {
            PlatformError::io(
                &self.path,
                io::Error::new(io::ErrorKind::InvalidInput, "write range overflows u64"),
            )
        })?;
        if end > current {
            return Err(PlatformError::io(
                &self.path,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("write [{offset}, {end}) extends past the target length {current}"),
                ),
            ));
        }
        FileExt::seek_write(&self.file, buf, offset)
            .map_err(|error| PlatformError::io(&self.path, error))
    }

    fn barrier(&mut self) -> PlatformResult<()> {
        self.file
            .sync_all()
            .map_err(|error| PlatformError::io(&self.path, error))
    }

    fn refresh_partitions(&mut self) -> PlatformResult<()> {
        // 普通文件没有内核分区视图。
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::RecordEntry;

    /// 临时目录里的一个同名文件；用进程号与序号避开并发。
    fn temporary_file(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "diskedit-windows-{tag}-{}-{serial}.img",
            std::process::id()
        ));
        drop(std::fs::remove_file(&path));
        std::fs::write(&path, vec![0u8; 4096]).expect("临时镜像可写");
        path
    }

    #[test]
    fn an_image_identity_is_stable_and_distinct_per_file() {
        let first = temporary_file("identity");
        let second = temporary_file("identity");
        let left = WindowsPlatform.identity(&first).expect("身份可读");
        let right = WindowsPlatform.identity(&second).expect("身份可读");
        assert_eq!(
            left,
            WindowsPlatform.identity(&first).expect("身份可读"),
            "同一文件两次取身份必须一致"
        );
        assert_ne!(left, right, "两个文件不得共用身份");

        // 长度参与身份：同一路径重建为不同长度即另一个目标。
        std::fs::write(&first, vec![0u8; 8192]).expect("重建临时镜像");
        assert_ne!(
            left,
            WindowsPlatform.identity(&first).expect("身份可读"),
            "长度变化必须改变身份"
        );
        drop(std::fs::remove_file(&first));
        drop(std::fs::remove_file(&second));
    }

    #[test]
    fn a_directory_has_no_file_identity() {
        let directory = std::env::temp_dir();
        let identity = WindowsPlatform
            .identity(&directory)
            .expect("目录元数据可读");
        assert_eq!(
            identity.strength(),
            diskedit_core::fingerprint::Strength::Unusable,
            "目录不该有文件身份"
        );
    }

    #[test]
    fn a_write_past_the_target_length_is_refused() {
        let path = temporary_file("bounded");
        let mut handle = WindowsPlatform.open_target(&path).expect("目标可打开");
        let error = handle
            .write_at(4090, &[0u8; 16])
            .expect_err("越界写必须被拒绝");
        assert!(error.to_string().contains("extends past"), "{error}");
        // 区间内写入成立，且能被读回。
        handle.write_at(0, b"hello").expect("区间内写入成立");
        let mut buffer = [0u8; 5];
        assert_eq!(handle.read_at(0, &mut buffer).expect("可读回"), 5);
        assert_eq!(&buffer, b"hello");
        drop(std::fs::remove_file(&path));
    }

    #[test]
    fn a_record_round_trips_and_a_directory_is_not_a_record() {
        let path = temporary_file("record");
        WindowsPlatform
            .write_record(&path, b"payload")
            .expect("记录可写");
        assert_eq!(
            WindowsPlatform.read_record(&path).expect("记录可读"),
            RecordEntry::Contents(b"payload".to_vec())
        );

        let renamed = path.with_extension("renamed");
        WindowsPlatform
            .rename_record(&path, &renamed)
            .expect("记录可改名");
        assert_eq!(
            WindowsPlatform.read_record(&path).expect("原位置可读"),
            RecordEntry::Absent,
            "改名后原位置应为空"
        );
        WindowsPlatform.remove_record(&renamed).expect("记录可删除");
        assert_eq!(
            WindowsPlatform.read_record(&renamed).expect("删除后可读"),
            RecordEntry::Absent
        );

        // 目录不是记录：给出明确结论，而不是"读不到"。
        let directory = std::env::temp_dir();
        assert_eq!(
            WindowsPlatform.read_record(&directory).expect("目录可探测"),
            RecordEntry::NotARecoveryFile
        );
    }

    #[test]
    fn entropy_is_not_a_constant() {
        let first = WindowsPlatform.entropy(16).expect("随机源可用");
        let second = WindowsPlatform.entropy(16).expect("随机源可用");
        assert_eq!(first.len(), 16);
        assert_ne!(first, second, "两次取随机字节不应相同");
    }
}
