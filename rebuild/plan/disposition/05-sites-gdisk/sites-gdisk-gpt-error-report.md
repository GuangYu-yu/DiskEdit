# 处置说明：gdisk gpt error-report（头/表校验、修复、写路径）

- 批次：`05-sites-gdisk/sites-gdisk-gpt-error-report`（参考账本 sites/gdisk）

## 判断

头/表校验、主备回退、备份头重定位、写失败即未完成、分区号/名称/类型校验认领（SA3-003/SA3-004/SA1-011/SA1-013/SA1-018）；SizesOK 类型断言与分配失败 exit 拒绝（架构差异-多虑）；备份文件/专家菜单/混合 MBR 行裁剪（SA1-020/SA3-002）。

## 依据

SA1-005/SA1-011/SA1-013/SA1-018/SA1-020/SA3-002/SA3-003/SA3-004；crates/core/src/table/gpt/parse.rs、policy/gpt.rs、src/app/execute/write.rs。

## 撤回条件

撤回条件：若引入菜单式备份文件或专家表搬移，先定义文件格式与身份校验（SA1-020 撤回条件同款）。
