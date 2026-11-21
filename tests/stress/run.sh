#!/bin/sh
# 一键跑压力测试（手动跑，不进 verify）：release 构建，起真的 ai daemon（HOME 在 /tmp 下的临时目录，不碰真实数据），
# 依次跑五项，每项打印 STRESS| 开头的数据行。默认高速输出 30 分钟，先试一遍可以 STRESS_MINUTES=1 tests/stress/run.sh。
#   STRESS_MINUTES  20 条对话高速输出的时长（分钟，默认 30）
#   STRESS_RATE_KB  每条对话每秒输出多少 KB（默认 1024）
# 只跑其中一项：tests/stress/run.sh <测试名的一部分>，如 tests/stress/run.sh stuck
# 6 小时长跑不在「全部」里，要单独点名：tests/stress/run.sh soak（SOAK_MINUTES 改时长，SOAK_CSV 存每分钟的数据）。
set -eu
cd "$(dirname "$0")/../.."
[ $# -eq 0 ] && set -- --skip soak
exec cargo test --release --test stress -- --ignored --nocapture --test-threads=1 "$@"
