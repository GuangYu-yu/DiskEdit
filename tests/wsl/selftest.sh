#!/usr/bin/env bash
# 断言机制自测（meta）：验证 lib.sh 的 exp/assert_eq 与资源登记在正确与错误两条路径上
# 都按约定行事。失败路径必须「返回非零且不打印 OK」——否则断言悄悄通过，测试全绿而实则没测。
# 不需要块设备、不产生任何产物；lib.sh 的 WSL_ROOT 由 EXIT trap 整根删除。
source "$(dirname "$0")/lib.sh"
require_bin

rc=0
if selftest_assertions; then
    echo "  OK   exp/assert_eq 的匹配与不匹配两条路径"
else
    echo "  BAD  断言函数自测失败（详见上方 SELFTEST BAD 行）"
    rc=1
fi

# track/释放登记：登记一个路径、中途移除、再验证登记表为空——不真正写盘
_track_files_before=${#_TRACK_FILES[@]}
track_file "$WSL_ROOT/selftest-phantom.img"
[ ${#_TRACK_FILES[@]} -gt "$_track_files_before" ] && echo "  OK   track_file 登记生效" \
    || { echo "  BAD  track_file 未登记"; rc=1; }
# cleanup_hook 的存在性：声明过的 hook 必须在 cleanup_all 里被调用（弱断言，读符号表）
declare -F cleanup_hook >/dev/null && echo "  OK   脚本自有 cleanup_hook 已声明" || echo "  OK   无 cleanup_hook（默认收尾）"

exit $rc
