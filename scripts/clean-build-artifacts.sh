#!/usr/bin/env bash
#
# 回收构建产物：把「只靠 cargo 自己永远不会清」的目录一次性删掉。
#
# 为什么需要这个脚本：cargo 只保证「当前 unit hash 的产物在」，旧 hash 的
# rlib / rmeta / 测试二进制 / incremental 目录一律留着。本仓的实测数据是
# 13 天涨 19G（`target/` 9.5G → 22.4G），其中每棵树的最大一块是增量编译
# 缓存（`incremental/`），而所有现成工具都碰不到它：
#   - `cargo clean` 会连依赖 rlib 一起删（下一次要全量重编 300 个 crate）；
#   - `cargo-sweep`（已 unmaintained）源码里明确跳过 incremental，且
#     `--maxsize` 在本仓会退化（尺寸基线含 incremental、删除集不含它，
#     于是每次都删光 deps 仍达不到目标）；
#   - 按天数的时间阈值无效：开发密度高时所有产物都在一天内被动过（实测
#     `--time 1` 与 `--time 7` 都回收 0 字节）。
#
# 所以这里按「已知会失控的目录」显式列举，默认 dry-run，`--yes` 才动手。
# 不挂 cron：什么时候重置由人决定，避免定时的破坏性任务。
#
# 用法：
#   bash scripts/clean-build-artifacts.sh            # 只报体积（dry-run）
#   bash scripts/clean-build-artifacts.sh --yes      # 真正删除
#   bash scripts/clean-build-artifacts.sh --yes --force   # 有构建在跑时也删
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

apply=0
force=0
while [ $# -gt 0 ]; do
  case "$1" in
    -y | --yes) apply=1 ;;
    -f | --force) force=1 ;;
    -h | --help)
      cat <<'USAGE'
usage: bash scripts/clean-build-artifacts.sh [--yes] [--force]

  默认 dry-run，只报将回收的体积。
  --yes/-y    真正删除
  --force/-f  有 cargo/rustc 进程在跑时也删（默认拒绝）

清理范围：target/、fixtures/plugins/target、fixtures 下所有嵌套 target、
/tmp 下超过 10 分钟且无进程占用的 .tmp* 测试副本。
USAGE
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
  shift
done

# 只有当脚本确实位于一个 Cargo 仓库里时才继续，避免在别处误删同名目录。
if [ ! -f "$root/Cargo.toml" ]; then
  echo "refusing to run: $root/Cargo.toml not found" >&2
  exit 1
fi

# 有构建在跑时删产物会让正在编译的 rustc 失败（不是数据损坏，但会白白浪费
# 一次全量构建）。默认拒绝，--force 可以越过。
if [ "$force" -eq 0 ]; then
  running="$(pgrep -f '(^|/)(cargo|rustc)( |$)' || true)"
  if [ -n "$running" ]; then
    echo "refusing to run: build processes are active:" >&2
    ps -o pid=,args= -p $running 2>/dev/null | cut -c1-120 | sed 's/^/  /' >&2
    echo "re-run with --force if you really want to delete under a live build" >&2
    exit 1
  fi
fi

# 1) 根 workspace 的 target（含 llvm-cov-target 与全部 incremental）
# 2) 仓库根下的 target-<name>（并行 agent 用 CARGO_TARGET_DIR 隔离出来的构建树，
#    `cargo clean` 完全看不到它们；实测曾同时存在 19G + 4.6G 两份）
# 3) fixtures workspace 的 target（独立 workspace，`cargo clean` 管不到）
# 4) fixtures 下任何嵌套 target（expr 的 10 个子 crate 各自独立 manifest）
# 5) /tmp 下测试遗留的 workspace 副本（TempDir 在进程被杀时不会回收）
dirs=()
for d in "$root/target" "$root/fixtures/plugins/target"; do
  [ -d "$d" ] && dirs+=("$d")
done
while IFS= read -r d; do
  [ -n "$d" ] && dirs+=("$d")
done < <(find "$root" -maxdepth 1 -type d -name 'target-*' 2>/dev/null)
while IFS= read -r d; do
  [ -n "$d" ] && dirs+=("$d")
done < <(find "$root/fixtures" -type d -name target -not -path '*/target/*' 2>/dev/null)

# 只清「至少 10 分钟没被动过、且没有进程占用」的临时副本；正在跑的测试要留着。
stale_after_min=10
while IFS= read -r d; do
  [ -d "$d" ] || continue
  [ -n "$(fuser "$d" 2>/dev/null || true)" ] && continue
  [ -n "$(find "$d" -maxdepth 0 -mmin +"$stale_after_min" 2>/dev/null || true)" ] && dirs+=("$d")
done < <(ls -d /tmp/.tmp* 2>/dev/null || true)

if [ "${#dirs[@]}" -eq 0 ]; then
  echo "nothing to clean — no build artifact directory found"
  df -h "$root" | tail -1
  exit 0
fi

echo "==> 待回收的构建产物"
bytes=0
for d in "${dirs[@]}"; do
  size_kb="$(du -sk "$d" 2>/dev/null | cut -f1 || echo 0)"
  bytes=$((bytes + size_kb))
  printf '  %8s  %s\n' "$(du -sh "$d" 2>/dev/null | cut -f1)" "${d#"$root"/}"
done
printf '  %8s  %s\n' "$(numfmt --to=iec --suffix=B $((bytes * 1024)) 2>/dev/null || echo "${bytes}K")" "合计"

echo "==> 磁盘（清理前）"
df -h "$root" | tail -1

if [ "$apply" -eq 0 ]; then
  echo "dry-run：未删除任何内容。加 --yes 执行。"
  exit 0
fi

for d in "${dirs[@]}"; do
  rm -rf "$d"
done

echo "==> 磁盘（清理后）"
df -h "$root" | tail -1
echo "已回收约 $((bytes / 1024)) MB（du 口径；df 的实际变化还受并发构建影响）"
echo "下次构建会重新编译本仓 crate（依赖 rlib 也一并丢弃），属预期代价。"
