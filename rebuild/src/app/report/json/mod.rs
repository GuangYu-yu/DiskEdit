//! 机器可读输出。
//!
//! 手工序列化，字段顺序固定；`Observation` 统一渲染成 `state`/`value`/`why`
//! 三态，避免把未知折叠成 `null` 值。不确定性字段（来源、代次）一并输出，便于
//! 复核者按证据路径回查。

pub mod check;
pub mod info;
pub mod inspect;
pub mod records;

use diskedit_core::evidence::Observation;

/// JSON 字符串转义。
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control < ' ' => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out
}

/// 带引号的 JSON 字符串。
pub fn quote(text: &str) -> String {
    format!("\"{}\"", escape(text))
}

/// 按固定字段顺序拼对象。
pub fn object(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{}:{value}", quote(key)))
        .collect();
    format!("{{{}}}", body.join(","))
}

/// 拼数组。
pub fn array(items: Vec<String>) -> String {
    format!("[{}]", items.join(","))
}

/// 无符号整数。
pub fn number(value: u64) -> String {
    value.to_string()
}

/// 可选的字符串；`None` 渲染为 `null`。
pub fn optional(value: Option<&str>) -> String {
    value.map_or_else(|| "null".to_owned(), quote)
}

/// 可观测的无符号整数：三态保留，不折成 `null`。
pub fn observed_number(observation: &Observation<u64>) -> String {
    match observation {
        Observation::Known {
            value,
            source,
            generation,
        } => object(&[
            ("state", quote("known")),
            ("value", number(*value)),
            ("source", quote(source.name())),
            ("generation", number(generation.get())),
        ]),
        Observation::Unknown { why } => object(&[("state", quote("unknown")), ("why", quote(why))]),
        Observation::NotApplicable { why } => {
            object(&[("state", quote("not_applicable")), ("why", quote(why))])
        }
    }
}

/// 可观测的字符串：三态保留。
pub fn observed_text<T: std::fmt::Display>(observation: &Observation<T>) -> String {
    match observation {
        Observation::Known {
            value,
            source,
            generation,
        } => object(&[
            ("state", quote("known")),
            ("value", quote(&value.to_string())),
            ("source", quote(source.name())),
            ("generation", number(generation.get())),
        ]),
        Observation::Unknown { why } => object(&[("state", quote("unknown")), ("why", quote(why))]),
        Observation::NotApplicable { why } => {
            object(&[("state", quote("not_applicable")), ("why", quote(why))])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_control_characters() {
        assert_eq!(escape("a\"b\\c\n"), "a\\\"b\\\\c\\n");
    }

    #[test]
    fn unknown_is_never_null() {
        let rendered = observed_number(&Observation::unknown("读不到"));
        assert!(rendered.contains("\"state\":\"unknown\""));
        assert!(!rendered.contains("\"value\":null"));
    }
}
