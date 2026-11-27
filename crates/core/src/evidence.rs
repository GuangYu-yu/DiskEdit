//! 外部事实的三态容器：已观测、不可判定、不适用。
//!
//! 一条事实 = 值 + 来源 + 观测代次。事实位不得用 `Option` 或空集合表达"不知道"；
//! "未观测"与"观测到没有"在类型上不同。读不到、判不出只能是 [`Observation::Unknown`]，
//! 不适用且已证明不适用才是 [`Observation::NotApplicable`]。
//!
//! [`Evidence`] 描述对一个对象的识别结论：存在且已解释、已证明不存在、
//! 存在但不属于任何已知格式、读取失败。后两者不得进入修复或清理路径。

/// 事实来源：产生这条观测的探测点或工具的稳定标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Source(&'static str);

impl Source {
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub const fn name(self) -> &'static str {
        self.0
    }
}

/// 观测代次：同一目标上依次递增的读数序号。
///
/// 代次只表示先后，不表示时间；执行期重探所得代次必须不早于判定所用快照的代次，
/// 否则该事实按"目标已变"处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Generation(u64);

impl Generation {
    /// 首个观测代次。
    pub const FIRST: Generation = Generation(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 一条带来源与代次的事实。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observation<T> {
    /// 已读到的值。
    Known {
        value: T,
        source: Source,
        generation: Generation,
    },
    /// 读不出或判不出，附带原因。
    Unknown { why: String },
    /// 已证明该事实对目标不适用。
    NotApplicable { why: String },
}

impl<T> Observation<T> {
    pub fn known(value: T, source: Source, generation: Generation) -> Self {
        Self::Known {
            value,
            source,
            generation,
        }
    }

    pub fn unknown(why: impl Into<String>) -> Self {
        Self::Unknown { why: why.into() }
    }

    pub fn not_applicable(why: impl Into<String>) -> Self {
        Self::NotApplicable { why: why.into() }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    pub fn is_not_applicable(&self) -> bool {
        matches!(self, Self::NotApplicable { .. })
    }

    /// 已观测的值；未知与不适用都返回 `None`。
    ///
    /// 返回 `Option` 不是把缺失折叠成默认值：调用方必须显式处理 `None`，
    /// 事实位本身仍是 [`Observation`]。
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Known { value, .. } => Some(value),
            Self::Unknown { .. } | Self::NotApplicable { .. } => None,
        }
    }

    /// 已观测事实的来源；未知与不适用没有来源。
    pub fn source(&self) -> Option<Source> {
        match self {
            Self::Known { source, .. } => Some(*source),
            Self::Unknown { .. } | Self::NotApplicable { .. } => None,
        }
    }

    /// 已观测事实的代次；未知与不适用没有代次。
    pub fn generation(&self) -> Option<Generation> {
        match self {
            Self::Known { generation, .. } => Some(*generation),
            Self::Unknown { .. } | Self::NotApplicable { .. } => None,
        }
    }

    /// 不可判定或不适用的原因。
    pub fn why(&self) -> Option<&str> {
        match self {
            Self::Known { .. } => None,
            Self::Unknown { why } | Self::NotApplicable { why } => Some(why),
        }
    }

    /// 观测代次是否不早于 `floor`；未知与不适用不满足任何代次下限。
    pub fn is_at_least(&self, floor: Generation) -> bool {
        matches!(self, Self::Known { generation, .. } if *generation >= floor)
    }

    /// 把早于 `floor` 的已观测事实降级为不可判定，其余原样返回。
    pub fn require_generation(self, floor: Generation) -> Self {
        match self {
            Self::Known { generation, .. } if generation < floor => Self::unknown(format!(
                "observation generation {} predates the required generation {}",
                generation.get(),
                floor.get()
            )),
            other @ (Self::Known { .. } | Self::Unknown { .. } | Self::NotApplicable { .. }) => {
                other
            }
        }
    }

    /// 只变换已观测的值；未知与不适用原样保留。
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Observation<U> {
        match self {
            Self::Known {
                value,
                source,
                generation,
            } => Observation::known(f(value), source, generation),
            Self::Unknown { why } => Observation::unknown(why),
            Self::NotApplicable { why } => Observation::not_applicable(why),
        }
    }
}

/// 对一个对象的识别证据。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Evidence<T> {
    /// 对象存在，且已按已知格式解释。
    Present(T),
    /// 已证明对象不存在。
    Absent,
    /// 对象存在可读内容，但不匹配任何已知格式。
    UnknownFormat { why: String },
    /// 读取失败，无法判定对象是否存在。
    Unreadable { why: String },
}

impl<T> Evidence<T> {
    /// 已解释的对象；其余形态返回 `None`。
    pub fn present(&self) -> Option<&T> {
        match self {
            Self::Present(value) => Some(value),
            Self::Absent | Self::UnknownFormat { .. } | Self::Unreadable { .. } => None,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    pub fn is_unknown_format(&self) -> bool {
        matches!(self, Self::UnknownFormat { .. })
    }

    pub fn why(&self) -> Option<&str> {
        match self {
            Self::Present(_) | Self::Absent => None,
            Self::UnknownFormat { why } | Self::Unreadable { why } => Some(why),
        }
    }

    /// 只变换已解释的对象；其余形态原样保留。
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Evidence<U> {
        match self {
            Self::Present(value) => Evidence::Present(f(value)),
            Self::Absent => Evidence::Absent,
            Self::UnknownFormat { why } => Evidence::UnknownFormat { why },
            Self::Unreadable { why } => Evidence::Unreadable { why },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: Source = Source::new("probe.test");

    #[test]
    fn known_carries_value_source_and_generation() {
        let fact = Observation::known(4096u64, PROBE, Generation::new(7));
        assert_eq!(fact.value(), Some(&4096));
        assert_eq!(fact.source(), Some(PROBE));
        assert_eq!(fact.generation(), Some(Generation::new(7)));
        assert_eq!(fact.why(), None);
        assert!(fact.is_known());
        assert!(!fact.is_unknown());
    }

    #[test]
    fn unknown_has_no_value_and_keeps_its_reason() {
        let fact: Observation<u64> = Observation::unknown("探测结果不可解析");
        assert_eq!(fact.value(), None);
        assert_eq!(fact.source(), None);
        assert_eq!(fact.generation(), None);
        assert_eq!(fact.why(), Some("探测结果不可解析"));
        assert!(fact.is_unknown());
    }

    #[test]
    fn not_applicable_is_distinct_from_unknown() {
        let fact: Observation<u64> = Observation::not_applicable("目标没有该字段");
        assert!(fact.is_not_applicable());
        assert!(!fact.is_unknown());
        assert_eq!(fact.value(), None);
        assert_eq!(fact.why(), Some("目标没有该字段"));
    }

    #[test]
    fn map_touches_only_known_values() {
        let mapped = Observation::known(2u8, PROBE, Generation::new(1)).map(u64::from);
        assert_eq!(mapped.value(), Some(&2));

        let unknown: Observation<u8> = Observation::unknown("读不出");
        assert!(unknown.map(u64::from).is_unknown());

        let absent: Observation<u8> = Observation::not_applicable("不适用");
        assert!(absent.map(u64::from).is_not_applicable());
    }

    #[test]
    fn generation_floor_downgrades_only_stale_known_facts() {
        let floor = Generation::new(5);
        let fresh = Observation::known(1u8, PROBE, Generation::new(5));
        assert!(fresh.is_at_least(floor));
        assert_eq!(fresh.clone().require_generation(floor), fresh);

        let stale = Observation::known(1u8, PROBE, Generation::new(4));
        assert!(!stale.is_at_least(floor));
        assert!(stale.require_generation(floor).is_unknown());

        let unknown: Observation<u8> = Observation::unknown("读不出");
        assert!(!unknown.is_at_least(floor));
        assert!(unknown.clone().require_generation(floor).is_unknown());
    }

    // covers: I10
    #[test]
    fn evidence_separates_absence_from_unknown_format() {
        let present: Evidence<u32> = Evidence::Present(7);
        assert_eq!(present.present(), Some(&7));
        assert!(!present.is_absent());

        let absent: Evidence<u32> = Evidence::Absent;
        assert!(absent.is_absent());
        assert_eq!(absent.present(), None);
        assert_eq!(absent.why(), None);

        let unknown: Evidence<u32> = Evidence::UnknownFormat {
            why: "签名不匹配".to_owned(),
        };
        assert!(unknown.is_unknown_format());
        assert!(!unknown.is_absent());
        assert_eq!(unknown.why(), Some("签名不匹配"));

        let unreadable: Evidence<u32> = Evidence::Unreadable {
            why: "读取失败".to_owned(),
        };
        assert_eq!(unreadable.present(), None);
        assert_eq!(unreadable.why(), Some("读取失败"));
    }

    #[test]
    fn evidence_map_touches_only_present() {
        let mapped = Evidence::Present(3u8).map(u64::from);
        assert_eq!(mapped.present(), Some(&3));
        assert!(Evidence::<u8>::Absent.map(u64::from).is_absent());
        assert!(
            Evidence::<u8>::UnknownFormat {
                why: "x".to_owned()
            }
            .map(u64::from)
            .is_unknown_format()
        );
    }
}
