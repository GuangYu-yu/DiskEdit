//! 现场身份：一次探测能给出的目标事实，以及"还是不是同一个目标"的判据。
//!
//! 字节布局在 `record` 的 `write_scene` / `read_scene`；本模块只给字段语义、构造入口与
//! 比较。判据分两类：
//!
//! - 恒等字段：身份材料（设备类别 + 稳定身份串；镜像即 `file_id`）、载体 `file_id`、
//!   loop 映射。任一项不同即不是同一个目标。
//! - 画像字段：容量。只记录，不参与判定——合法扩容不得被判成身份不符。
//!
//! [`SceneIdentity::matches`] 等同于 `mismatch_reason(...).is_none()`，`PartialEq` 与
//! `matches` 同义（只比恒等字段，不比容量）。

use crate::dev::{FileSource, LiveProof, TargetFingerprint};
use crate::platform::{IdentityMaterial, TargetId, TargetProbe};

/// 身份证据的种类。记录里是单字节 tag（`record::write_scene` 写 1/2/3）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SceneProof {
    /// 整盘节点的稳定设备 ID（序列号 / WWN / dm uuid 等）
    DeviceId,
    /// inode 身份：镜像文件的 `(dev, ino)`，或 loop 后备文件的 `(dev, ino)`
    FileId,
    /// 没有可跨现场比较的身份证据
    Unavailable,
}

/// loop 载体的映射事实。两个槽位承载 sysfs 的 `loop/offset` 与 `loop/sizelimit`
/// （`FileSource::loop_mapping` 就是这一对），编码侧按 8+8 字节顺序写。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mapping {
    major: u64,
    minor: u64,
}

impl Mapping {
    pub(crate) fn new(major: u64, minor: u64) -> Self {
        Self { major, minor }
    }

    pub(crate) fn major(&self) -> u64 {
        self.major
    }

    pub(crate) fn minor(&self) -> u64 {
        self.minor
    }
}

/// 记录头里的目标身份。字段顺序与 `read_scene` 的读取顺序一致
#[derive(Clone, Debug)]
pub(crate) struct SceneIdentity {
    proof: SceneProof,
    mapping: Option<Mapping>,
    id: TargetId,
    size: Option<u64>,
    file_id: Option<[u8; 16]>,
}

impl SceneIdentity {
    /// 唯一构造点：解码器与三处采集入口都走这里
    pub(crate) fn with_details(
        id: TargetId,
        mapping: Option<Mapping>,
        proof: SceneProof,
        size: Option<u64>,
        file_id: Option<[u8; 16]>,
    ) -> Self {
        Self { proof, mapping, id, size, file_id }
    }

    /// 只有身份材料时的现场：容量、loop 映射都未知
    ///
    /// `proof` 按 [`TargetId`] 的材料强度取：镜像与带稳定设备 ID 的块设备取材料对应的
    /// 种类，只有设备号的块设备没有跨现场身份，记 [`SceneProof::Unavailable`]
    pub(crate) fn from_target(id: TargetId) -> Self {
        let (proof, file_id) = match id.material() {
            IdentityMaterial::Image { file_id, .. } => (SceneProof::FileId, Some(*file_id)),
            IdentityMaterial::Block { device_id: Some(_), .. } => (SceneProof::DeviceId, None),
            IdentityMaterial::Block { device_id: None, .. } => (SceneProof::Unavailable, None),
        };
        Self::with_details(id, None, proof, None, file_id)
    }

    /// 只有探测结果时的现场：容量已知；载体 `file_id` 与 loop 映射要打开句柄后才有
    pub(crate) fn from_probe(probe: &TargetProbe) -> Self {
        let mut scene = Self::from_target(probe.id().clone());
        scene.size = Some(probe.total_bytes());
        scene
    }

    /// 已打开数据句柄的现场：容量、loop 映射与载体 `file_id` 都从句柄取。
    /// 镜像的指纹就是文件自身的 `(dev, ino)`，loop 目标的是后备文件的 `(dev, ino)`，
    /// 真块设备节点两者都为 0，因此没有载体 `file_id`
    pub(crate) fn of_source(probe: &TargetProbe, src: &FileSource) -> Self {
        Self::with_details(
            probe.id().clone(),
            src.loop_mapping.map(|(offset, limit)| Mapping::new(offset, limit)),
            proof_of_source(src.proof()),
            Some(src.size),
            carrier_file_id(&src.fingerprint),
        )
    }

    pub(crate) fn proof(&self) -> SceneProof {
        self.proof
    }

    pub(crate) fn mapping(&self) -> Option<Mapping> {
        self.mapping
    }

    pub(crate) fn id(&self) -> &TargetId {
        &self.id
    }

    pub(crate) fn size(&self) -> Option<u64> {
        self.size
    }

    pub(crate) fn file_id(&self) -> Option<[u8; 16]> {
        self.file_id
    }

    /// 差异说明，给拒绝信息当括注用。容量不参与判定，因此不会出现在这里
    pub(crate) fn mismatch_reason(&self, other: &Self) -> Option<String> {
        if self.id.material() != other.id.material() {
            return Some(format!(
                "recorded {}, current {}",
                describe_material(self.id.material()),
                describe_material(other.id.material())
            ));
        }
        // 一侧没有载体 `file_id` 就是没有证据，不参与比对：仅凭探测结果构造的现场
        // （`from_probe`）本来就没有这一项，不能因此把同一条记录判成外来目标
        if let (Some(recorded), Some(current)) = (self.file_id, other.file_id)
            && recorded != current
        {
            return Some(format!(
                "recorded carrier file-id {}, current {}",
                crate::support::hex_lower(&recorded),
                crate::support::hex_lower(&current)
            ));
        }
        if self.mapping != other.mapping {
            return Some(format!(
                "recorded loop mapping {}, current {}",
                describe_mapping(self.mapping),
                describe_mapping(other.mapping)
            ));
        }
        None
    }

    /// 同一个目标：恒等字段全部相符（含 loop 映射）。**只用于认领**：判断这条记录
    /// 是不是本次目标的（`Evidence::classify` 的 `Recognized`、续跑的现场匹配）。
    /// 释放不得用它——loop 映射已经改了也仍是同一载体上的旧记录
    pub(crate) fn matches(&self, other: &Self) -> bool {
        self.mismatch_reason(other).is_none()
    }

    /// 同一个载体：只比身份材料与载体 `file_id`。**只用于释放**：`abandon` 据此决定
    /// 这份记录能不能释放；容量变化或 loop 映射变化说明记录不可回放，但仍属于这个目标。
    /// 认领不得用它——少了 loop 映射就会把已指向别处的映射当成本次目标
    pub(crate) fn same_carrier(&self, other: &Self) -> bool {
        self.id.material() == other.id.material()
            && (match (self.file_id, other.file_id) {
                (Some(recorded), Some(current)) => recorded == current,
                _ => true,
            })
    }
}

/// 只比恒等字段。容量不同仍是同一目标（合法扩容），因此不能按字段逐个比较
impl PartialEq for SceneIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other)
    }
}

impl Eq for SceneIdentity {}

/// `LiveProof` 到记录 tag 的映射：只有 `Inode` 换成记录侧的 `FileId`，其余同名
fn proof_of_source(proof: LiveProof) -> SceneProof {
    match proof {
        LiveProof::DeviceId => SceneProof::DeviceId,
        LiveProof::Inode => SceneProof::FileId,
        LiveProof::Unavailable => SceneProof::Unavailable,
    }
}

/// 载体文件 ID，与 `platform::probe::image_probe` 同构：大端 `dev` 接大端 `ino`
fn carrier_file_id(fingerprint: &TargetFingerprint) -> Option<[u8; 16]> {
    fingerprint.has_inode_evidence().then(|| {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&fingerprint.dev.to_be_bytes());
        id[8..].copy_from_slice(&fingerprint.ino.to_be_bytes());
        id
    })
}

fn describe_material(material: &IdentityMaterial) -> String {
    match material {
        IdentityMaterial::Block { major, minor, device_id } => match device_id {
            Some(id) => format!("block {major}:{minor} ({id})"),
            None => format!("block {major}:{minor} (no stable device ID)"),
        },
        IdentityMaterial::Image { file_id, .. } => format!("image file-id {}", crate::support::hex_lower(file_id)),
    }
}

fn describe_mapping(mapping: Option<Mapping>) -> String {
    match mapping {
        Some(m) => format!("offset {} with size limit {}", m.major(), m.minor()),
        None => "none".to_string(),
    }
}
