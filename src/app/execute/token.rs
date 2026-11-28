//! 写入令牌：证明本步原像已经落盘，且只授权本步声明的那组区间。
//!
//! 令牌只能由 [`crate::app::execute::transaction`] 或搬移会话在记录写入成功之后
//! 铸造；其余模块拿到的是已有值，无法自行构造。令牌绑定记录会话、步骤号与本步
//! 声明区间的摘要，并且不实现 `Copy`/`Clone`：构造写通道时按值交出，交出之后
//! 调用方不再持有，复制、跨步复用与重放都无法表达。

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

use diskedit_core::region::{Region, RegionSet};

use super::digest::{Digest, Hasher};
use super::transaction::Mint;

/// 一次记录事务的会话标识。
///
/// 由进程内计数器分配，不落盘；写通道只接受本会话铸造的令牌。它区分的是同一
/// 进程内同时打开的会话，不承担防伪：令牌不可构造这件事由 [`Mint`] 的私有
/// 构造函数保证。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionId(u64);

impl SessionId {
    pub(crate) fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// 本步原像已落盘，且只授权本步声明的那组区间。
#[derive(Debug)]
pub struct Recorded {
    session: SessionId,
    step: u64,
    region_digest: Digest,
    authority: PhantomData<Mint>,
}

impl Recorded {
    /// 由记录会话铸造；`Mint` 只能在其定义模块内构造，因此别处无法仿造令牌。
    ///
    /// `step` 是计划里的步骤号，`regions` 是该步声明的写入区间；两者一起决定
    /// 这枚令牌能授权哪一个写通道。
    pub(crate) fn mint(mint: &Mint, session: SessionId, step: u64, regions: &[Region]) -> Self {
        Self {
            session,
            step,
            region_digest: RegionDigest::of(regions).0,
            authority: mint.brand(),
        }
    }

    /// 令牌所属的会话。
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// 令牌对应的步骤号。
    pub const fn step(&self) -> u64 {
        self.step
    }

    /// 令牌授权的区间集合摘要。
    pub const fn region_digest(&self) -> Digest {
        self.region_digest
    }
}

/// 一组声明区间的稳定摘要：先排序合并，再按 `(起点, 长度)` 喂给哈希。
///
/// 声明顺序不影响结果，因此铸造点与写通道可以各自从同一组区间算出同一个值。
pub(crate) struct RegionDigest(Digest);

impl RegionDigest {
    pub(crate) fn of(regions: &[Region]) -> Self {
        let mut hasher = Hasher::new();
        let set = RegionSet::from_regions(regions.iter().copied());
        for region in set.regions() {
            hasher.feed_u64(region.start());
            hasher.feed_u64(region.len());
        }
        Self(hasher.finish())
    }

    pub(crate) const fn finish(self) -> Digest {
        self.0
    }
}
