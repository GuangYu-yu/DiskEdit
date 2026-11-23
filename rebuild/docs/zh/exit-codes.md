由 `app/docsgen` 从 `app/cli/spec.rs` 与 `diskedit_core::fs::catalog` 生成。
用 `DISKEDIT_WRITE_DOCS=1 cargo test --test generated_docs` 重新生成。
CI 比对工作树与生成物；不要手工编辑。

# diskedit 退出状态

七个值；映射只在 `app/report/exit.rs` 一处产生。

| 代码 | 含义 |
|---|---|
| 0 | 成功：未写目标且未建记录，或记录已清理且视图已同步，或完整回滚已完成 |
| 1 | 写目标字节前的不变量破坏或内部错误 |
| 2 | 用法与解析错误 |
| 3 | 请求被拒、记录损坏、证据指向外来或损坏 |
| 4 | 已进入执行、目标字节未变的 I/O 或环境故障 |
| 5 | 部分完成、待恢复，或持久状态已达成但内核视图未同步 |
| 6 | 能力不支持 |

