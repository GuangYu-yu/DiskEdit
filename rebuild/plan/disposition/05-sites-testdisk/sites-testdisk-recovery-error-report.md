# 处置说明：testdisk recovery error-report（头字段告警/主备回退）

- 批次：`05-sites-testdisk/sites-testdisk-recovery-error-report`（参考账本 sites/testdisk）

## 判断

头字段严格校验与主备回退认领（宽松告警收敛为严格校验，SA3-003/SA3-004）；内容标记检查裁剪（SA1-017）。

## 依据

SA3-003/SA3-004/SA1-017；crates/core/src/table/gpt/parse.rs。

## 撤回条件

撤回条件：若需要把头字段警告降级为 inspect 诊断输出（保留严格校验），登记提示面工作项。
