# sites dosfstools check fatal exit 处置说明

判断：批内裁剪行绑定 dosfstools 工具自身 CLI/usage/选项面（mkfs.fat/fsck.fat/fatlabel 的选项解析、usage 文本、getopt 内部）。本实现以固定参数调用这些工具，不复刻其命令行面。
依据：SA2-011（固定随发行版版本、只解析文档化稳定字段）、SA2-012（真机覆盖面）。
撤回条件：出现固定参数不足以覆盖的合法用法需求，或上游工具改参导致固定调用失败时重评。
补充：check-fatal-exit 中修复路径 die 行按交互修复面裁剪（见 DOSREPAIR）。
