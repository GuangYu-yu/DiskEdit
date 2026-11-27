# sites dosfstools frontend error report 处置说明

判断：批内裁剪行绑定 CP850/码页选择面（iconv 初始化、码页参数）。
依据：SA2-011（固定参数、默认码页由工具内部处理）。
撤回条件：出现必须控制码页的外部契约时，把码页参数纳入固定调用面。
