# sites f2fs tools mkfs fatal exit 处置说明

判断：批内裁剪行绑定 f2fs-tools 子工具/usage 面（dump/defrag/sload/label/选项解析）。
本实现只按档案表固定参数调用 mkfs.f2fs/fsck.f2fs/resize.f2fs。
依据：SA2-010、SA2-011、SA2-012、SA2-013。
撤回条件：出现需要这些子工具能力的契约时先建真机用例再纳入档案表。
