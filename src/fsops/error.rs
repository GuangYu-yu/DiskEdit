//! FS 工具链错误的分类。

use super::*;

/// FS 层失败的分类。fsops 只回答"操作层面发生了什么"，"这该算拒绝还是故障"要结合
/// durable boundary 才能回答，属命令层语义（映射见 `outcome::From<FsError> for Fail`）。
///
/// 只有三类是**在发生处就能确定**的，故单列；其余一律 `Io`——环境故障是最保守的缺省，
/// 它不承诺"改参数重试有意义"。用 io::ErrorKind 反推这三类是行不通的：
/// kind 一旦成形就分不出"类型不认得"与"设备读不到"
#[derive(Debug)]
pub enum FsError {
    /// 该类型/该操作没有接线的工具（不认得的 FS、本工具不负责的组合）
    UnsupportedFs(String),
    /// 参数非法，或目标现状与请求不符（分区不存在/为空、容器分区、尺寸不是扇区倍数）
    InvalidArgument(String),
    /// PATH 里找不到必需的工具
    ToolMissing(String),
    /// 读写、权限、挂载态等环境故障
    Io(io::Error),
    /// 外部工具跑起来了但非零退出
    CommandFailed(String),
}

impl FsError {
    pub(crate) fn unsupported(msg: impl Into<String>) -> Self {
        Self::UnsupportedFs(msg.into())
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }

    pub(crate) fn missing(msg: impl Into<String>) -> Self {
        Self::ToolMissing(msg.into())
    }

    /// 外部工具非零退出：`<tool> failed: <stderr 原文>`
    pub(crate) fn command(tool: &str, out: &std::process::Output) -> Self {
        Self::CommandFailed(format!("{tool} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFs(m)
            | Self::InvalidArgument(m)
            | Self::ToolMissing(m)
            | Self::CommandFailed(m) => f.write_str(m),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<io::Error> for FsError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// 越过 durable boundary 之后（`execute_*` 内部），`FsError` 的分类不再有出口语义：
/// 那里的任何失败对外都只能是 `Failed`（盘可能已改变）。压平是显式的，不是 From——
/// 需要区分分类的地方必须自己表态
impl From<FsError> for io::Error {
    fn from(e: FsError) -> Self {
        io::Error::other(e.to_string())
    }
}