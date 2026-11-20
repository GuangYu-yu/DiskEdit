//! 无载荷枚举与它们在磁盘上的单字节码值。
//!
//! 一张 `变体 = 码值` 表同时生成枚举、判别值与双向转换：写出的值与读回的值来自同一
//! 张表，不会出现"写一个变体、读回另一个变体"的漂移。

/// 由一张 `变体 = 码值` 表生成 `#[repr(u8)]` 枚举、`tag()` 与 `from_tag()`。
///
/// 码值是磁盘格式：表外的字节解码失败，改判别值就是改格式。调用方给出的属性原样
/// 保留在前面（文档注释、额外的 `derive`），`Clone/Copy/Debug/PartialEq/Eq` 由宏提供。
macro_rules! tagged_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident = $tag:literal,
            )*
        }
    ) => {
        #[repr(u8)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        $(#[$meta])*
        $vis enum $name {
            $(
                $(#[$variant_meta])*
                $variant = $tag,
            )*
        }

        impl $name {
            /// 磁盘上的码值
            pub(crate) fn tag(self) -> u8 {
                self as u8
            }

            /// 码值对应的变体；表外取值为 `None`
            pub(crate) fn from_tag(tag: u8) -> Option<Self> {
                match tag {
                    $($tag => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

pub(crate) use tagged_enum;
