# sites libfdisk mbr error report 处置说明

判断：批内裁剪行绑定 CHS 几何换算类校验/提示（cylinder boundary、物理/逻辑 CHS 一致性等）。
依据：SA1-027、SA1-028（CHS 一律占位，不实现 BIOS 几何换算）。
撤回条件：同 SA1-028——出现必须写真实 CHS 的外部契约且几何来源可探测。
补充：交互提示行按交互面裁剪；verify 汇总差异与读侧宽容差异分别见 verify 清单与读侧宽容登记。
