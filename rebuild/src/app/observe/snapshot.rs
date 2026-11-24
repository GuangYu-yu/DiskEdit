//! 一次判定所需的全部事实。
//!
//! 快照不可变，逐项复用各探测子模块；`Unknown` 原样保留，判定层据此拒绝，
//! 不在这里补默认值。

use diskedit_core::evidence::{Generation, Observation};
use diskedit_core::fingerprint::Fingerprint;
use diskedit_core::fs::{ContentKind, Program};

use super::disk::DiskFacts;
use super::table::TableFacts;
use super::tools::{ToolAvailability, ToolState};
use super::{FIRST_GENERATION, Request, disk, filesystem, identity, table, tools};
use crate::platform::{FsSize, Occupancy, Partition, Platform};

/// 目标身上的一份分区：内核拓扑、可读内容类别与内容尺寸。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionFacts {
    pub partition: Partition,
    pub content: Observation<ContentKind>,
    /// 文件系统当前大小与可收缩的最小大小；未探测或非文件系统时保持未知。
    pub size: Observation<FsSize>,
}

/// 分区拓扑。
///
/// 空列表表示已观测到目标没有分区；读不到拓扑是 `Observation::Unknown`，
/// 两者在类型上不同。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    pub partitions: Vec<PartitionFacts>,
}

/// 一次观察的全部结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub generation: Generation,
    pub identity: Observation<Fingerprint>,
    pub disk: DiskFacts,
    pub topology: Observation<Topology>,
    pub occupancy: Observation<Occupancy>,
    pub table: TableFacts,
    pub tools: Vec<ToolAvailability>,
}

impl Snapshot {
    /// 某个程序的可用性事实；未登记的程序返回 `None`。
    pub fn tool_state(&self, program: Program) -> Option<&Observation<ToolState>> {
        self.tools
            .iter()
            .find(|entry| entry.program == program)
            .map(|entry| &entry.state)
    }

    /// 已观测的分区列表；拓扑未知时为 `None`。
    pub fn contents(&self) -> Option<&[PartitionFacts]> {
        self.topology
            .value()
            .map(|topology| topology.partitions.as_slice())
    }

    /// 按分区号查找；拓扑未知或没有该分区时为 `None`。
    pub fn partition(&self, number: u32) -> Option<&PartitionFacts> {
        self.contents()?
            .iter()
            .find(|facts| facts.partition.number == number)
    }
}

/// 观察一次：请求加宿主端口，产出快照。
///
/// 没有目标参数的请求（目前只有 `records list`）没有可观测的盘面事实，统一记
/// `Unknown`，不去猜一个路径。
pub fn observe(request: &Request, platform: &dyn Platform) -> Snapshot {
    let generation = FIRST_GENERATION;
    let Some(target) = request.target() else {
        return Snapshot {
            generation,
            identity: Observation::unknown("请求没有目标参数"),
            disk: DiskFacts {
                kind: Observation::unknown("请求没有目标参数"),
                byte_len: Observation::unknown("请求没有目标参数"),
                sector_size: Observation::unknown("请求没有目标参数"),
            },
            topology: Observation::unknown("请求没有目标参数"),
            occupancy: Observation::unknown("请求没有目标参数"),
            table: TableFacts {
                gpt: Observation::unknown("请求没有目标参数"),
                mbr: Observation::unknown("请求没有目标参数"),
            },
            tools: tools::observe(platform, generation),
        };
    };
    let identity = identity::observe(platform, target, generation);
    let (mut disk, handle) = disk::observe(platform, target, generation);
    if let Some(sector) = request.sector_size() {
        disk.sector_size = Observation::known(
            sector,
            diskedit_core::evidence::Source::new("--sector-size"),
            generation,
        );
    }
    let table = table::observe(handle.as_deref(), &disk, generation);
    let topology = filesystem::observe_topology(
        platform,
        target,
        &disk,
        &table,
        handle.as_deref(),
        generation,
    );
    let topology = match request.command() {
        super::Command::Resizefs { .. } | super::Command::Resize(_) => {
            filesystem::observe_sizes(platform, target, &topology, &disk, generation)
        }
        super::Command::Info
        | super::Command::Inspect
        | super::Command::RecordsList
        | super::Command::RecordsVerify { .. }
        | super::Command::RecordsScan
        | super::Command::Move(_)
        | super::Command::Copy(_)
        | super::Command::Delete { .. }
        | super::Command::Set { .. }
        | super::Command::Check { .. }
        | super::Command::Mkfs { .. }
        | super::Command::Recover(_)
        | super::Command::New { .. }
        | super::Command::Add(_) => topology,
    };
    let occupancy = disk::observe_occupancy(platform, target, generation);
    let tools = tools::observe(platform, generation);
    Snapshot {
        generation,
        identity,
        disk,
        topology,
        occupancy,
        table,
        tools,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use diskedit_core::evidence::{Evidence, Observation};
    use diskedit_core::fingerprint::Fingerprint;
    use diskedit_core::fs::{ContentKind, FsKind, Program};
    use diskedit_core::table::GptEvidence;

    use super::*;
    use crate::platform::{
        MountEntry, Occupancy, Output, Partition, PlatformError, PlatformResult, SwapEntry,
        TargetHandle, TargetKind,
    };

    const SECTOR: u64 = 512;

    struct MemoryHandle {
        bytes: Vec<u8>,
        sector: Option<u64>,
        kind: TargetKind,
    }

    impl TargetHandle for MemoryHandle {
        fn path(&self) -> &Path {
            Path::new("<memory>")
        }

        fn kind(&self) -> TargetKind {
            self.kind
        }

        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn sector_size(&self) -> Option<u64> {
            self.sector
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> PlatformResult<usize> {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= self.bytes.len() {
                return Ok(0);
            }
            let available = self.bytes.len() - start;
            let len = buf.len().min(available);
            buf[..len].copy_from_slice(&self.bytes[start..start + len]);
            Ok(len)
        }

        fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> PlatformResult<usize> {
            Err(PlatformError::not_supported("测试句柄只读"))
        }

        fn barrier(&mut self) -> PlatformResult<()> {
            Ok(())
        }

        fn lock_exclusive(&mut self) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试句柄不提供租约"))
        }
    }

    struct FakeHost {
        image: Vec<u8>,
        sector: Option<u64>,
        kind: TargetKind,
        partitions: Vec<Partition>,
        partition_error: Option<&'static str>,
        open_error: Option<&'static str>,
        tool_error: Option<&'static str>,
    }

    impl FakeHost {
        fn image(image: Vec<u8>) -> Self {
            Self {
                image,
                sector: Some(SECTOR),
                kind: TargetKind::File,
                partitions: Vec::new(),
                partition_error: None,
                open_error: None,
                tool_error: None,
            }
        }
    }

    impl Platform for FakeHost {
        fn open_target(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
            Err(PlatformError::not_supported("观察不使用写句柄"))
        }

        fn open_target_read_only(&self, _path: &Path) -> PlatformResult<Box<dyn TargetHandle>> {
            if let Some(why) = self.open_error {
                return Err(PlatformError::not_supported(why));
            }
            Ok(Box::new(MemoryHandle {
                bytes: self.image.clone(),
                sector: self.sector,
                kind: self.kind,
            }))
        }

        fn fingerprint(&self, _path: &Path) -> PlatformResult<Fingerprint> {
            Ok(Fingerprint::strong([3; 32]))
        }

        fn partitions(&self, _path: &Path) -> PlatformResult<Vec<Partition>> {
            match self.partition_error {
                Some(why) => Err(PlatformError::parse(why)),
                None => Ok(self.partitions.clone()),
            }
        }

        fn occupancy(&self, _path: &Path) -> PlatformResult<Occupancy> {
            Ok(Occupancy {
                mounts: Vec::<MountEntry>::new(),
                swaps: Vec::<SwapEntry>::new(),
                holders: Vec::new(),
                array_member: false,
                loops: Vec::new(),
            })
        }

        fn reread(&self, _path: &Path) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试宿主不重读"))
        }

        fn update_table(&self, _path: &Path) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试宿主不更新表"))
        }

        fn btrfs_show(&self, _path: &Path) -> PlatformResult<crate::platform::BtrfsShow> {
            Err(PlatformError::not_supported("测试宿主不读 btrfs"))
        }

        fn pvs(&self, _path: &Path) -> PlatformResult<Vec<crate::platform::PvFacts>> {
            Ok(Vec::new())
        }

        fn loop_attach(
            &self,
            _backing: &Path,
            _offset: u64,
            _size: u64,
        ) -> PlatformResult<PathBuf> {
            Err(PlatformError::not_supported("测试宿主没有 loop"))
        }

        fn loop_detach(&self, _device: &Path) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试宿主没有 loop"))
        }

        fn mount(
            &self,
            _source: &Path,
            _mount_point: &Path,
            _fstype: Option<&str>,
            _options: &[&str],
        ) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试宿主不挂载"))
        }

        fn umount(&self, _mount_point: &Path) -> PlatformResult<()> {
            Err(PlatformError::not_supported("测试宿主不卸载"))
        }

        fn require(&self, program: Program) -> PlatformResult<PathBuf> {
            match self.tool_error {
                Some(why) => Err(PlatformError::tools(format!("{program:?}"), why)),
                None => Ok(PathBuf::from("/usr/bin/program")),
            }
        }

        fn run(&self, _program: Program, _args: &[&str]) -> PlatformResult<Output> {
            Err(PlatformError::not_supported("测试宿主不执行程序"))
        }
    }

    fn request() -> Request {
        use super::super::TargetSpec;
        Request::new(
            TargetSpec::new("/tmp/image", None),
            super::super::Command::Inspect,
        )
    }

    #[test]
    fn unknown_topology_is_not_an_empty_partition_list() {
        let mut host = FakeHost::image(vec![0u8; 2 * 1024 * 1024]);
        host.kind = TargetKind::Block;
        host.partition_error = Some("拓扑不可解析");
        let snapshot = observe(&request(), &host);
        assert!(snapshot.topology.is_unknown(), "{:?}", snapshot.topology);
        assert_eq!(snapshot.contents(), None);
        assert!(snapshot.partition(1).is_none());
    }

    #[test]
    fn unopenable_target_leaves_disk_and_tables_unknown() {
        let mut host = FakeHost::image(vec![0u8; 2 * 1024 * 1024]);
        host.open_error = Some("设备拒绝只读打开");
        let snapshot = observe(&request(), &host);
        assert!(snapshot.disk.kind.is_unknown());
        assert!(snapshot.disk.byte_len.is_unknown());
        assert!(snapshot.disk.sector_size.is_unknown());
        assert!(snapshot.table.gpt.is_unknown());
        assert!(snapshot.table.mbr.is_unknown());
        assert!(snapshot.identity.is_known());
    }

    #[test]
    fn partition_prefix_identifies_ext() {
        let mut image = vec![0u8; 4 * 1024 * 1024];
        let partition_start = 2048 * SECTOR as usize;
        image[partition_start + 0x438..partition_start + 0x43A]
            .copy_from_slice(&0x53EFu16.to_le_bytes());
        let mut host = FakeHost::image(image);
        host.kind = TargetKind::Block;
        host.partitions = vec![Partition {
            number: 1,
            start_lba: 2048,
            sectors: 2048,
        }];
        let snapshot = observe(&request(), &host);
        let facts = snapshot.contents().expect("拓扑应已观测");
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0].content.value(),
            Some(&ContentKind::Filesystem(FsKind::Ext))
        );
    }

    #[test]
    fn blank_image_reports_absent_tables_rather_than_unknown() {
        let host = FakeHost::image(vec![0u8; 4 * 1024 * 1024]);
        let snapshot = observe(&request(), &host);
        assert_eq!(snapshot.table.gpt.value(), Some(&GptEvidence::Absent));
        assert_eq!(snapshot.table.mbr.value(), Some(&Evidence::Absent));
        assert_eq!(snapshot.contents().map(|facts| facts.len()), Some(0));
    }

    #[test]
    fn every_program_has_an_availability_fact_and_missing_is_known() {
        let mut host = FakeHost::image(vec![0u8; 1024]);
        host.tool_error = Some("不在 PATH 中");
        let snapshot = observe(&request(), &host);
        assert_eq!(snapshot.tools.len(), Program::ALL.len());
        let state = snapshot
            .tool_state(Program::E2fsck)
            .expect("每个程序都应有可用性事实");
        assert!(matches!(
            state,
            Observation::Known {
                value: ToolState::Missing { .. },
                ..
            }
        ));
    }

    #[test]
    fn facts_carry_their_generation() {
        let host = FakeHost::image(vec![0u8; 4 * 1024 * 1024]);
        let snapshot = observe(&request(), &host);
        assert_eq!(snapshot.generation, FIRST_GENERATION);
        match &snapshot.identity {
            Observation::Known { generation, .. } => assert_eq!(*generation, FIRST_GENERATION),
            other @ (Observation::Unknown { .. } | Observation::NotApplicable { .. }) => {
                panic!("身份应已观测：{other:?}")
            }
        }
    }
}
