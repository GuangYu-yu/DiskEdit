//! 观察层：把一次请求与宿主端口转成不可变的事实快照。
//!
//! 快照里每条事实都是 [`Observation`]，带来源与观测代次；读不到、判不出记
//! `Unknown`，不折叠成默认值或空集合。本层只读目标，不写盘、不执行外部程序，
//! 也不做判定。

pub mod disk;
pub mod filesystem;
pub mod identity;
pub mod snapshot;
pub mod table;
pub mod tools;

use std::path::{Path, PathBuf};

use diskedit_core::evidence::Generation;

use crate::platform::TargetHandle;

pub use snapshot::{Snapshot, observe};

/// 一次观察的输入：已解析的用户意图，不含任何事实。
///
/// 解析层只填充目标路径；打开、识别与判定都发生在后续层。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    target: PathBuf,
}

impl Request {
    pub fn new(target: impl Into<PathBuf>) -> Self {
        Self {
            target: target.into(),
        }
    }

    pub fn target(&self) -> &Path {
        &self.target
    }
}

/// 首次观察使用的代次。执行期重探所得代次不得早于此值。
pub const FIRST_GENERATION: Generation = Generation::new(1);

/// 从目标按偏移读满一段。
///
/// 短读与 I/O 失败都折算成不可判定的原因文本：调用方据此产出 `Unknown`，
/// 不把残缺字节当成正常内容继续解释。
pub(super) fn read_exact_at(
    handle: &dyn TargetHandle,
    offset: u64,
    len: usize,
) -> Result<Vec<u8>, String> {
    let mut buffer = vec![0u8; len];
    let mut filled = 0;
    while filled < buffer.len() {
        let position = offset
            .checked_add(filled as u64)
            .ok_or_else(|| "读取偏移越过可寻址上界".to_owned())?;
        match handle.read_at(position, &mut buffer[filled..]) {
            Ok(0) => return Err(format!("目标在偏移 {position} 处提前结束")),
            Ok(read) => filled += read,
            Err(error) => return Err(format!("读取偏移 {position} 失败：{error}")),
        }
    }
    Ok(buffer)
}
