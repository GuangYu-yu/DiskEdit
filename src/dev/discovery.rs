//! 记录发现：只按路径结构判断"这是不是某个记录后端里的直接子项"，不读文件内容、
//! 不比对身份。解码与状态判定由调用方在此之上做。
//!
//! 后端只有两类：sidecar 是镜像旁的 `<name>.diskedit` 目录，主机仓库是
//! `<state_root>/recovery/<identity-hash>`。判据只认路径结构——记录目录里的 `record`
//! 在被删除目标之后仍应可读，所以不能反过来要求目标存在。

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::{
    host_state_root, QuarantineMark, QUARANTINE_ABANDONED, QUARANTINE_TORN, RECORD_LOCK_NAME, RECORD_SUFFIX,
};

/// 记录目录属于哪一侧
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordBackend {
    /// 镜像旁 `<name>.diskedit`
    Sidecar,
    /// `<state_root>/recovery/<identity-hash>`
    HostRepo,
}

impl RecordBackend {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Sidecar => "sidecar",
            Self::HostRepo => "host-recovery-repository",
        }
    }
}

/// 记录后端目录里的一项，足够调用方决定"读它、跳过它、还是标记它"
#[derive(Clone, Debug)]
pub(crate) struct RecordSpot {
    pub(crate) dir: PathBuf,
    pub(crate) name: std::ffi::OsString,
    pub(crate) backend: RecordBackend,
    /// 隔离区条目：`.abandoned[.N]` 或 `.torn[.N]`
    pub(crate) quarantine: Option<QuarantineMark>,
}

impl RecordSpot {
    pub(crate) fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    /// sidecar 目录名去掉 `.diskedit` 就是它服务的镜像路径；主机仓库目录不指向具体路径，
    /// 因为 `identity-hash` 不可逆
    pub(crate) fn sidecar_target(&self) -> Option<PathBuf> {
        if self.backend != RecordBackend::Sidecar {
            return None;
        }
        let name = self.dir.file_name()?.to_str()?;
        let stem = name.strip_suffix(RECORD_SUFFIX)?;
        Some(match self.dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join(stem),
            _ => PathBuf::from(stem),
        })
    }
}

/// 目录是不是两个记录后端之一。只认结构：sidecar 名以 `.diskedit` 结尾且前缀非空；
/// 主机仓库是 `<state_root>/recovery/` 下的 32 位小写十六进制目录名
pub(crate) fn record_backend(dir: &Path) -> Option<RecordBackend> {
    record_backend_under(dir, &host_state_root())
}

/// [`record_backend`] 的可注入版本：`state_root` 由调用方给，测试与扫描不必依赖
/// 进程环境里的配置
pub(crate) fn record_backend_under(dir: &Path, state_root: &Path) -> Option<RecordBackend> {
    if !dir.is_dir() {
        return None;
    }
    if let Some(name) = dir.file_name().and_then(|n| n.to_str())
        && let Some(stem) = name.strip_suffix(RECORD_SUFFIX)
        && !stem.is_empty()
    {
        return Some(RecordBackend::Sidecar);
    }
    let hash = dir.file_name().and_then(|n| n.to_str())?;
    let recovery = dir.parent()?;
    if hash.len() != 32 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    if recovery.file_name().and_then(|n| n.to_str()) != Some("recovery") {
        return None;
    }
    if recovery.parent()? != state_root {
        return None;
    }
    Some(RecordBackend::HostRepo)
}

/// 路径是记录后端的直接子项时返回它；`record.lock` 是锁不是记录，排除在外。
/// 其余名字（含隔离项与残留临时文件）都交给调用方解码——不在这里预判"像不像记录"
pub(crate) fn record_spot(path: &Path) -> Option<RecordSpot> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_os_string();
    if name.to_str() == Some(RECORD_LOCK_NAME) {
        return None;
    }
    let backend = record_backend(dir)?;
    let quarantine = quarantine_mark(&name);
    Some(RecordSpot { dir: dir.to_path_buf(), name, backend, quarantine })
}

/// 文件名是不是隔离区条目。编号只认十进制数字，`0` 也接受：本工具不产生它，
/// 但按名字识别出来比把它当活跃记录更接近事实
pub(crate) fn quarantine_mark(name: &std::ffi::OsStr) -> Option<QuarantineMark> {
    let text = name.to_str()?;
    for (stem, mark) in [
        (QUARANTINE_ABANDONED, QuarantineMark::Abandoned),
        (QUARANTINE_TORN, QuarantineMark::Torn),
    ] {
        if text == stem {
            return Some(mark);
        }
        if let Some(digits) = text.strip_prefix(stem).and_then(|rest| rest.strip_prefix('.'))
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return Some(mark);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> crate::testdir::TestDir {
        crate::testdir::test_dir(tag)
    }

    #[test]
    fn sidecar_directory_is_recognized_and_names_its_image() {
        let base = scratch("sidecar");
        let dir = base.join("disk.img.diskedit");
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(record_backend_under(&dir, Path::new("/nowhere")), Some(RecordBackend::Sidecar));
        let spot = record_spot(&dir.join("record")).expect("a direct entry is a record spot");
        assert_eq!(spot.backend, RecordBackend::Sidecar);
        assert!(spot.quarantine.is_none());
        assert_eq!(spot.sidecar_target(), Some(base.join("disk.img")));
        // 目录本身不是记录后端的直接子项
        assert!(record_spot(&dir).is_none());
    }

    #[test]
    fn lock_file_is_not_a_record_entry() {
        let base = scratch("lock");
        let dir = base.join("disk.img.diskedit");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(record_spot(&dir.join("record.lock")).is_none());
    }

    #[test]
    fn quarantine_names_are_marked() {
        for (name, mark) in [
            (".abandoned", QuarantineMark::Abandoned),
            (".abandoned.1", QuarantineMark::Abandoned),
            (".abandoned.27", QuarantineMark::Abandoned),
            (".torn", QuarantineMark::Torn),
            (".torn.3", QuarantineMark::Torn),
        ] {
            assert_eq!(quarantine_mark(std::ffi::OsStr::new(name)), Some(mark), "{name}");
        }
        for name in [".abandoned.", ".abandoned.x", ".torn.-1", "abandoned", "record", ".record.12.abcd.tmp"] {
            assert!(quarantine_mark(std::ffi::OsStr::new(name)).is_none(), "{name}");
        }
    }

    #[test]
    fn host_repo_directory_needs_its_configured_root() {
        let root = scratch("hostrepo");
        let recovery = root.join("recovery");
        let dir = recovery.join("0123456789abcdef0123456789abcdef");
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(record_backend_under(&dir, &root), Some(RecordBackend::HostRepo));
        assert_eq!(
            record_backend_under(&dir, Path::new("/elsewhere")),
            None,
            "同一个名字挂在别的根下不是主机仓库"
        );
        // identity-hash 是小写十六进制；大写不是本工具写出的目录名
        let upper = recovery.join("0123456789ABCDEF0123456789ABCDEF");
        std::fs::create_dir_all(&upper).unwrap();
        assert_eq!(record_backend_under(&upper, &root), None);
    }

    #[test]
    fn plain_directory_is_not_a_record_backend() {
        let base = scratch("plain");
        let dir = base.join("photos");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(record_backend_under(&dir, Path::new("/nowhere")), None);
    }
}
