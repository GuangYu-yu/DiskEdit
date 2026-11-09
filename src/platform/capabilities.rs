//! 能力矩阵：平台 × 目标形态 × 原语的三态结论。
//!
//! 本表只回答设计结论：支持 / 不支持 / 设计限制 / 不适用。只有「支持」放行；
//! 「设计限制」在写路径上等同「不支持」。实测状态不混入本表。
//!
//! 快照由平台探测产出后作为值传给命令层；命令层只读，不查询平台。

use crate::domain::MediaKind;

/// 原语清单
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CapabilityId {
    ReadTarget,
    WriteTable,
    SectorIo,
    ExternalTool,
    KernelResync,
    ExclusiveLock,
    AtomicReplace,
    DirPersist,
    StableIdentity,
}

impl CapabilityId {
    pub(crate) const ALL: [Self; 9] = [
        Self::ReadTarget,
        Self::WriteTable,
        Self::SectorIo,
        Self::ExternalTool,
        Self::KernelResync,
        Self::ExclusiveLock,
        Self::AtomicReplace,
        Self::DirPersist,
        Self::StableIdentity,
    ];

    /// 文档矩阵与运行期拒绝文案共用同一个名字，避免两处各写一份
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadTarget => "ReadTarget",
            Self::WriteTable => "WriteTable",
            Self::SectorIo => "SectorIo",
            Self::ExternalTool => "ExternalTool",
            Self::KernelResync => "KernelResync",
            Self::ExclusiveLock => "ExclusiveLock",
            Self::AtomicReplace => "AtomicReplace",
            Self::DirPersist => "DirPersist",
            Self::StableIdentity => "StableIdentity",
        }
    }

    fn index(self) -> u16 {
        match self {
            Self::ReadTarget => 0,
            Self::WriteTable => 1,
            Self::SectorIo => 2,
            Self::ExternalTool => 3,
            Self::KernelResync => 4,
            Self::ExclusiveLock => 5,
            Self::AtomicReplace => 6,
            Self::DirPersist => 7,
            Self::StableIdentity => 8,
        }
    }
}

/// 一项能力的结论。`DesignLimit` 与 `Unsupported` 分开记录：前者是这个平台
/// 没有等价原语，后者是尚未提供；两者在写路径上都不放行
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapabilityState {
    Supported,
    Unsupported,
    DesignLimit,
    NotApplicable,
}

impl CapabilityState {
    /// 文档矩阵与运行期拒绝文案共用同一份措辞
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::DesignLimit => "design limit",
            Self::NotApplicable => "not applicable",
        }
    }
}

/// 能力矩阵的平台轴
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostPlatform {
    Linux,
    Windows,
    Other,
}

impl HostPlatform {
    /// 当前构建的平台。矩阵按平台判定，不按命令名
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }

    /// 本平台真正能打开的载体形态。与 `platform::probe::open_read_only` 的编译分支
    /// 一一对应：只有 Linux 有块设备分支，其余平台一律把目标当普通文件（镜像）打开；
    /// 非 Linux 的 `block_probe` 也直接返回 `Unsupported`。
    ///
    /// 能力门只查这些形态。查一个本平台根本打不开的形态，会让「块设备列缺能力」
    /// 反过来挡住一个完全可运行的只读镜像命令——这正是删掉平台硬门之后必须补的一步
    pub(crate) const fn openable_media(self) -> &'static [MediaKind] {
        match self {
            Self::Linux => &[MediaKind::Block, MediaKind::Image],
            Self::Windows | Self::Other => &[MediaKind::Image],
        }
    }
}

/// 位集：一项能力在一位
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CapabilitySet(u16);

impl CapabilitySet {
    const fn empty() -> Self {
        Self(0)
    }

    fn insert(&mut self, id: CapabilityId) {
        self.0 |= 1 << id.index();
    }

    fn contains(self, id: CapabilityId) -> bool {
        self.0 & (1 << id.index()) != 0
    }
}

/// 一次探测得到的能力快照：每项能力的结论。
/// 字段私有，只能由 [`CapabilitySnapshot::design`] 产出
#[derive(Clone, Debug)]
pub(crate) struct CapabilitySnapshot {
    supported: CapabilitySet,
    design_limit: CapabilitySet,
    not_applicable: CapabilitySet,
}

impl CapabilitySnapshot {
    /// 设计矩阵。`design` 是唯一的完整构造点
    pub(crate) fn design(platform: HostPlatform, media: MediaKind) -> Self {
        let mut supported = CapabilitySet::empty();
        let mut design_limit = CapabilitySet::empty();
        let mut not_applicable = CapabilitySet::empty();
        for id in CapabilityId::ALL {
            match design_state(platform, media, id) {
                CapabilityState::Supported => supported.insert(id),
                CapabilityState::DesignLimit => design_limit.insert(id),
                CapabilityState::NotApplicable => not_applicable.insert(id),
                CapabilityState::Unsupported => {}
            }
        }
        Self { supported, design_limit, not_applicable }
    }

    pub(crate) fn state(&self, id: CapabilityId) -> CapabilityState {
        if self.supported.contains(id) {
            CapabilityState::Supported
        } else if self.design_limit.contains(id) {
            CapabilityState::DesignLimit
        } else if self.not_applicable.contains(id) {
            CapabilityState::NotApplicable
        } else {
            CapabilityState::Unsupported
        }
    }

    /// 该项在此形态上是否无需补足：「支持」，或对当前载体「不适用」。
    /// 「设计限制」与「不支持」都不是放行
    pub(crate) fn admit(&self, id: CapabilityId) -> bool {
        matches!(self.state(id), CapabilityState::Supported | CapabilityState::NotApplicable)
    }

    /// 所需能力里第一个不放行的项；全放行时为 None
    pub(crate) fn first_gap(&self, required: &[CapabilityId]) -> Option<(CapabilityId, CapabilityState)> {
        required
            .iter()
            .map(|id| (*id, self.state(*id)))
            .find(|(id, _)| !self.admit(*id))
    }
}

/// 设计矩阵的单点实现。矩阵的行是能力，列是平台 × 载体形态
fn design_state(platform: HostPlatform, media: MediaKind, id: CapabilityId) -> CapabilityState {
    use CapabilityState::{DesignLimit, NotApplicable, Supported, Unsupported};
    use HostPlatform::{Linux, Other, Windows};

    match id {
        // 只读与稳定身份：镜像在所有平台都能按路径打开并取文件身份；块设备只有 Linux
        // 能从 sysfs 取设备号与稳定 ID（非 Linux 的 block_probe 直接 Unsupported）。
        // 此前这两行无条件 Supported，与探测侧实现相反，把矩阵变成了谎报
        CapabilityId::ReadTarget | CapabilityId::StableIdentity => match (platform, media) {
            (_, MediaKind::Image) => Supported,
            (Linux, MediaKind::Block) => Supported,
            (Windows | Other, MediaKind::Block) => Unsupported,
        },
        // 写表、扇区 I/O、外部工具、原子替换只有 Linux 具备
        CapabilityId::WriteTable
        | CapabilityId::SectorIo
        | CapabilityId::ExternalTool
        | CapabilityId::AtomicReplace => match platform {
            Linux => Supported,
            Windows | Other => Unsupported,
        },
        // 内核刷新只对块设备有意义；镜像没有内核视图
        CapabilityId::KernelResync => match (platform, media) {
            (Linux, MediaKind::Block) => Supported,
            (_, MediaKind::Image) => NotApplicable,
            (Windows | Other, MediaKind::Block) => Unsupported,
        },
        // 独占锁在 Windows 上只有字节范围锁，不能当完整互斥
        CapabilityId::ExclusiveLock => match platform {
            Linux => Supported,
            Windows => DesignLimit,
            Other => Unsupported,
        },
        // 目录项持久化：Linux 有 fsync(dirfd) 等价原语，Windows 没有可直接依赖的通用接口
        CapabilityId::DirPersist => match platform {
            Linux => Supported,
            Windows => DesignLimit,
            Other => Unsupported,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 六个平台 × 形态组合；矩阵必须对每一项都给出显式结论，不靠"默认不支持"掩盖缺行
    const SHAPES: [(HostPlatform, MediaKind); 6] = [
        (HostPlatform::Linux, MediaKind::Block),
        (HostPlatform::Linux, MediaKind::Image),
        (HostPlatform::Windows, MediaKind::Block),
        (HostPlatform::Windows, MediaKind::Image),
        (HostPlatform::Other, MediaKind::Block),
        (HostPlatform::Other, MediaKind::Image),
    ];

    fn snapshot(platform: HostPlatform, media: MediaKind) -> CapabilitySnapshot {
        CapabilitySnapshot::design(platform, media)
    }

    /// 每个组合的结论数量固定：任何一行被改动或漏写都会改变计数
    #[test]
    fn every_shape_has_the_declared_state_counts() {
        fn counts(snap: &CapabilitySnapshot) -> (usize, usize, usize, usize) {
            let mut c = (0, 0, 0, 0);
            for id in CapabilityId::ALL {
                match snap.state(id) {
                    CapabilityState::Supported => c.0 += 1,
                    CapabilityState::Unsupported => c.1 += 1,
                    CapabilityState::DesignLimit => c.2 += 1,
                    CapabilityState::NotApplicable => c.3 += 1,
                }
            }
            c
        }
        // (支持, 不支持, 设计限制, 不适用)
        assert_eq!(counts(&snapshot(HostPlatform::Linux, MediaKind::Block)), (9, 0, 0, 0));
        assert_eq!(counts(&snapshot(HostPlatform::Linux, MediaKind::Image)), (8, 0, 0, 1));
        assert_eq!(counts(&snapshot(HostPlatform::Windows, MediaKind::Block)), (0, 7, 2, 0));
        assert_eq!(counts(&snapshot(HostPlatform::Windows, MediaKind::Image)), (2, 4, 2, 1));
        assert_eq!(counts(&snapshot(HostPlatform::Other, MediaKind::Block)), (0, 9, 0, 0));
        assert_eq!(counts(&snapshot(HostPlatform::Other, MediaKind::Image)), (2, 6, 0, 1));
    }

    /// 设计矩阵的关键单元格
    #[test]
    fn design_matrix_key_cells() {
        use CapabilityId::*;
        let linux_block = snapshot(HostPlatform::Linux, MediaKind::Block);
        let linux_image = snapshot(HostPlatform::Linux, MediaKind::Image);
        let win_block = snapshot(HostPlatform::Windows, MediaKind::Block);
        let win_image = snapshot(HostPlatform::Windows, MediaKind::Image);
        let other_image = snapshot(HostPlatform::Other, MediaKind::Image);

        assert_eq!(linux_block.state(WriteTable), CapabilityState::Supported);
        assert_eq!(linux_image.state(WriteTable), CapabilityState::Supported);
        assert_eq!(win_block.state(WriteTable), CapabilityState::Unsupported);
        assert_eq!(win_image.state(WriteTable), CapabilityState::Unsupported);
        assert_eq!(other_image.state(WriteTable), CapabilityState::Unsupported);

        // 块设备有内核视图，镜像没有
        assert_eq!(linux_block.state(KernelResync), CapabilityState::Supported);
        assert_eq!(linux_image.state(KernelResync), CapabilityState::NotApplicable);

        // Windows 的两项设计限制
        assert_eq!(win_block.state(ExclusiveLock), CapabilityState::DesignLimit);
        assert_eq!(win_image.state(DirPersist), CapabilityState::DesignLimit);
        assert_eq!(win_block.state(DirPersist), CapabilityState::DesignLimit);

        // 只读与身份：镜像六列里三列都支持；块设备只有 Linux 支持。
        // 与 probe.rs 的 block_probe 非 Linux 分支一致，不再谎报
        for platform in [HostPlatform::Linux, HostPlatform::Windows, HostPlatform::Other] {
            let image = snapshot(platform, MediaKind::Image);
            assert_eq!(image.state(ReadTarget), CapabilityState::Supported, "{platform:?}/image");
            assert_eq!(image.state(StableIdentity), CapabilityState::Supported, "{platform:?}/image");
        }
        assert_eq!(snapshot(HostPlatform::Linux, MediaKind::Block).state(ReadTarget), CapabilityState::Supported);
        for platform in [HostPlatform::Windows, HostPlatform::Other] {
            let block = snapshot(platform, MediaKind::Block);
            assert_eq!(block.state(ReadTarget), CapabilityState::Unsupported, "{platform:?}/block");
            assert_eq!(block.state(StableIdentity), CapabilityState::Unsupported, "{platform:?}/block");
        }
    }

    /// 门只查本平台能打开的形态：非 Linux 打不开块设备，就不该被块设备列挡住
    #[test]
    fn openable_media_matches_the_probe_backend() {
        assert_eq!(HostPlatform::Linux.openable_media(), &[MediaKind::Block, MediaKind::Image]);
        assert_eq!(HostPlatform::Windows.openable_media(), &[MediaKind::Image]);
        assert_eq!(HostPlatform::Other.openable_media(), &[MediaKind::Image]);
        // 每个本平台可打开的形态上，只读原语都必须真的支持；六个组合逐一核过
        for (platform, media) in SHAPES {
            let expected = matches!(
                (platform, media),
                (_, MediaKind::Image) | (HostPlatform::Linux, MediaKind::Block)
            );
            let snap = snapshot(platform, media);
            assert_eq!(
                snap.state(CapabilityId::ReadTarget) == CapabilityState::Supported,
                expected,
                "{platform:?}/{media:?}"
            );
            if platform.openable_media().contains(&media) {
                assert_eq!(snap.state(CapabilityId::ReadTarget), CapabilityState::Supported, "{platform:?}/{media:?}");
            }
        }
    }

    /// 缺口报告按声明顺序取第一项，不隐去具体是哪一项
    #[test]
    fn first_gap_names_the_missing_capability() {
        let snap = snapshot(HostPlatform::Windows, MediaKind::Image);
        let gap = snap.first_gap(&[CapabilityId::ReadTarget, CapabilityId::WriteTable]);
        assert_eq!(gap, Some((CapabilityId::WriteTable, CapabilityState::Unsupported)));
        assert_eq!(snap.first_gap(&[CapabilityId::ReadTarget, CapabilityId::StableIdentity]), None);
        // 对镜像不适用 ≠ 缺能力：内核刷新不构成缺口
        let image = snapshot(HostPlatform::Linux, MediaKind::Image);
        assert_eq!(image.first_gap(&[CapabilityId::KernelResync]), None);
        // 设计限制不是放行
        let win = snapshot(HostPlatform::Windows, MediaKind::Image);
        assert_eq!(
            win.first_gap(&[CapabilityId::DirPersist]),
            Some((CapabilityId::DirPersist, CapabilityState::DesignLimit))
        );
    }

    /// 构建平台与矩阵的平台轴一致（Windows 测试机读到 Windows）
    #[test]
    fn host_platform_matches_the_build_target() {
        let expected = if cfg!(target_os = "linux") {
            HostPlatform::Linux
        } else if cfg!(windows) {
            HostPlatform::Windows
        } else {
            HostPlatform::Other
        };
        assert_eq!(HostPlatform::current(), expected);
    }
}
