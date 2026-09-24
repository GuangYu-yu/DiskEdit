#!/usr/bin/env bash
# 断言机制自测（meta）：验证 exp / assert_eq 的通过与失败两条路径都正确——
# 失败必须返回非零且不打印 OK，否则断言会静默放行，套件全绿而实则没测。
# 不触盘、不需要被测二进制；由 run_all.sh 登记执行。
source "$(dirname "$0")/lib.sh"

if selftest_assertions; then
    echo "SELFTEST OK: exp/assert_eq 通过路径与失败路径行为均正确"
    exit 0
fi
echo "SELFTEST FAILED: 断言机制行为不符合预期（见上方 SELFTEST BAD 行）"
exit 1