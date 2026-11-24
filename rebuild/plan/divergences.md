# 差异登记

与基线的刻意差异只允许下列条目，每条必须有测试与撤回条件。其余任何行为差异按未登记处理，等同未通过。

| # | 行为 | 理由 | 撤回条件 | 测试位置 |
| --- | --- | --- | --- | --- |
| D1 | 写盘之后不可能得出"本次未改动" | 拒绝只发生在未写状态 | 出现必须在写后报告"未改动"的真实场景 | `src/app/execute/state.rs` 的状态机用例、`tests/case_runner.rs` 的拒侧用例 |
| D2 | 非本实现写下的记录视为外来，拒绝恢复 | 记录是崩溃恢复中间物，非用户数据 | 确认存在必须现场恢复、无法用写它的版本收尾的记录 | `crates/core/src/record/{codec,recover}.rs` 的外来格式用例 |
| D3 | 检查器原文输出与失败原因可见 | 结论必须可诊断 | 输出体积成为实际障碍且有替代证据 | `src/app/decide/check.rs` 与 `check` 用例 |
| D4 | 需修复才能判定的操作先修复再判定；修复写入目标后失败一律部分完成（`5`） | 避免修复自身改变判定前提 | 修复被证明不写目标字节 | 脏 ext 收缩用例（`tests/case_runner.rs` 的夹具项） |
| D5 | 退出码只由终态映射 | 消除调用点自行选码 | 出现终态无法表达的对外契约 | `src/app/report/exit.rs` 的码表用例 |
| D6 | 缺失与空集合不折叠为默认值 | 未观测不等于没有 | 出现必须把未观测当默认值的外部契约 | `tests/case_runner.rs` 的未知事实拒侧用例、`Observation` 用例 |
| D7 | 载体路径与基线行为等价 | 等价指行为一致，不按可运行性裁剪 | 基线载体在目标环境可运行而本实现不能 | 载体对照用例（`tests/equivalence.rs` 之外的特权夹具） |
| D8 | 不因几何变化改写用户已有的分区类型 GUID | 类型 GUID 是用户数据 | 确认基线确实改写且外部依赖该改写 | `tests/equivalence.rs` 的 "D8: the partition type GUID survives a resize" |
| D9 | 只读命令统一输出本实现的结构化 JSON；字段集与 `inspect`/`check` 的输出形态不逐字复刻基线 | 基线实际只有 `info` 与 `records *` 输出 JSON，`inspect`/`check` 输出人读文本且无 `--json`；人读文本不是契约，结构化事实由用例表与独立 oracle 覆盖 | 出现依赖基线文本措辞或基线 JSON 字段名的外部契约 | `tests/equivalence.rs` 的结构化投影对照、`tests/case_runner.rs` |
| D10 | 本期只交付 Linux 后端；Windows/macOS 宿主的表编辑与文件系统操作返回 `NotSupported`（退出码 `6`） | `01` §7.4 排除非 Linux 后端；端口形状已留出，补齐时只加子树 | Windows/macOS 上的等价表编辑成为交付条件 | `src/platform/mod.rs` 的 `unsupported_platform!` 与按平台能力表生成物（`tests/generated_docs.rs`） |

条目与 `plan/01-目标与边界.md` 第 5 节一一对应。新增差异必须先改 `01` 再改本表。
