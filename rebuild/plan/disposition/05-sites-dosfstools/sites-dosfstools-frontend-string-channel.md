# sites dosfstools frontend string channel 处置说明

判断：批内裁剪行绑定 fsck.fat 交互修复/清理路径（get_choice、MODIFY、auto_rename 等）。
本实现 check 固定只读 -n，修复路径不触发。
依据：SA2-012（检查覆盖面）、SA2-011（固定参数）。
撤回条件：决定提供 vfat 离线修复写路径时，先建真机用例再评估暴露。
