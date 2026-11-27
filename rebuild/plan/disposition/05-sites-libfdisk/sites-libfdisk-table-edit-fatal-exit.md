# sites libfdisk table edit fatal exit 处置说明

判断：批内裁剪行绑定的参考行为是 fdisk/cfdisk/sfdisk 的交互面（菜单、ask 回调、prompt、TUI、交互脚本输入）。本实现为非交互单命令 CLI，不提供这些特性。
依据：特性依据=交互面；SA1-010（输入面定案）、SA1-022（防御面口径）。
撤回条件：决定引入交互式界面（TUI/向导）时按 ask 回调面重建；或出现必须依赖交互提示的外部契约（无人值守以外场景）时重评。
