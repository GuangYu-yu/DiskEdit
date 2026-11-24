//! 端到端用例数据：每条用例一种输入、一个期望，由 `case_runner` 统一执行。
//!
//! 用例从生产入口喂入（构建出的二进制），断言只看退出码、JSON 字段与目标字节摘要。
//! 需要特权的用例声明前置条件；环境不满足时 runner 回报 `NotApplicable`，不静默跳过。

/// 镜像的前置布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrange {
    /// 空文件，只有长度。
    Fresh,
    /// 已写入 GPT。
    Gpt,
    /// 已写入 GPT 并新增一个 16 MiB 分区。
    GptWithPartition,
}

/// 用例需要的环境能力。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    /// 普通文件即可。
    None,
    /// 运行前把镜像挂到一个 loop 设备上，制造"目标在用"的现场；运行后释放。
    LoopBacked,
    /// 运行前在记录目录预置一份外来实现写下的记录。
    ForeignRecord,
}

/// 一条用例。
pub struct Case {
    pub name: &'static str,
    pub image_bytes: u64,
    pub arrange: Arrange,
    /// 参数里的 `{image}` 与 `{image}:1` 会被替换成临时镜像路径。
    pub argv: &'static [&'static str],
    pub expected_code: i32,
    /// 期望 stdout 包含的片段。
    pub expected_stdout: &'static [&'static str],
    /// 输出为 JSON 时，要求的顶层键。
    pub expected_json_keys: &'static [&'static str],
    /// 目标字节在运行前后必须相同。
    pub unchanged: bool,
    pub requirement: Requirement,
}

/// 全部用例。
pub const CASES: &[Case] = &[
    Case {
        name: "new below minimum size is refused",
        image_bytes: 4 * 1024,
        arrange: Arrange::Fresh,
        argv: &["new", "{image}", "--yes"],
        expected_code: 3,
        expected_stdout: &["refused"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "new rejects an unknown table type",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Fresh,
        argv: &["new", "{image}", "--table", "zfs", "--yes"],
        expected_code: 2,
        expected_stdout: &["--table"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "add without --yes is a usage error",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &["add", "{image}", "16M"],
        expected_code: 2,
        expected_stdout: &["--yes"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "add over an existing partition is refused",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::GptWithPartition,
        argv: &["add", "{image}", "16M", "--start", "2048", "--yes"],
        expected_code: 3,
        expected_stdout: &["overlap"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "add with a zero size is a usage error",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &["add", "{image}", "0", "--yes"],
        expected_code: 2,
        expected_stdout: &["valid range"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "delete of a missing partition is refused",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::GptWithPartition,
        argv: &["delete", "{image}:9", "--yes"],
        expected_code: 3,
        expected_stdout: &["partition number"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "resizefs on an unidentified filesystem is refused",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::GptWithPartition,
        argv: &["resizefs", "{image}:1", "--yes"],
        expected_code: 3,
        expected_stdout: &["unknown"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "check on an unidentified filesystem is refused",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::GptWithPartition,
        argv: &["check", "{image}:1"],
        expected_code: 3,
        expected_stdout: &["unknown"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "inspect reports the partition list as JSON",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::GptWithPartition,
        argv: &["inspect", "{image}"],
        expected_code: 0,
        expected_stdout: &[],
        expected_json_keys: &["target", "partitions", "topology"],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "info reports identity and capacity as JSON",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &["info", "{image}"],
        expected_code: 0,
        expected_stdout: &[],
        expected_json_keys: &[
            "target",
            "generation",
            "identity",
            "kind",
            "bytes",
            "sector_size",
            "table",
            "partitions",
            "occupancy",
        ],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "records list reports an empty container as JSON",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &["records", "list", "{image}"],
        expected_code: 0,
        expected_stdout: &[],
        expected_json_keys: &["entries"],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "records verify reports a missing record as JSON",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &[
            "records",
            "verify",
            "{image}",
            "00000000000000000000000000000000",
        ],
        expected_code: 3,
        expected_stdout: &["not_found"],
        expected_json_keys: &["id", "verdict", "records"],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "records scan of a regular file is refused",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &["records", "scan", "{image}"],
        expected_code: 3,
        expected_stdout: &["not a directory"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "an unknown option is a usage error",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &["info", "{image}", "--bogus"],
        expected_code: 2,
        expected_stdout: &["--bogus"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "resizefs on a path that is not a mount point is refused",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::GptWithPartition,
        argv: &["resizefs", "{image}", "--yes"],
        expected_code: 3,
        expected_stdout: &["request refused"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::None,
    },
    Case {
        name: "resize of an occupied target is refused",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::GptWithPartition,
        argv: &["resize", "{image}:1", "32M", "--yes"],
        expected_code: 3,
        expected_stdout: &["usage"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::LoopBacked,
    },
    Case {
        name: "add on a block device as a non-root user is unsupported",
        image_bytes: 0,
        arrange: Arrange::Fresh,
        argv: &["add", "/dev/null", "16M", "--yes"],
        expected_code: 6,
        expected_stdout: &["whole block device"],
        expected_json_keys: &[],
        unchanged: false,
        requirement: Requirement::None,
    },
    Case {
        name: "recover refuses a record written by another implementation",
        image_bytes: 64 * 1024 * 1024,
        arrange: Arrange::Gpt,
        argv: &["recover", "{image}", "--yes"],
        expected_code: 3,
        expected_stdout: &["record"],
        expected_json_keys: &[],
        unchanged: true,
        requirement: Requirement::ForeignRecord,
    },
];
