#!/usr/bin/env bash
#
# fixtures 测试总入口：CI 与本地共用。
#
# 为什么需要这个脚本：CI 原来只跑主 workspace 的 `cargo test --locked`，
# fixtures 的测试一个都不在门禁里——12 个 workspace member、10 个 expr 子
# crate（在 fixtures workspace 的 `exclude` 里、各自独立 manifest）、以及
# root 这个 scaffold 占位。后果是 `expr/lexer/tests/lexer.rs` 里一条从 HEAD
# 起就红着的断言能一直活到 5.2.53 才被发现（那里 `1 + a` 被当成词法错误，
# 但 `a` 是合法标识符），因为没有任何 job 跑过它。
#
# 用法：`bash scripts/test-fixtures.sh`（从任意目录都可以，脚本自己 cd 到仓库根）
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# 12 个 member：fixtures workspace 的 lock（`fixtures/plugins/Cargo.lock`）
# 入库，所以 `--locked` 能保证依赖可复现。
echo "==> fixtures workspace members (locked)"
cargo test --locked --manifest-path fixtures/plugins/Cargo.toml

# 10 个 expr 子 crate + root：都**不是** fixtures workspace 的 member，各自
# 是独立 manifest，也就没有自己的 Cargo.lock（`.gitignore` 明确忽略
# `fixtures/plugins/**/Cargo.lock`，只有 workspace 那份入库）。因此这里不能
# 加 `--locked`——干净 checkout 上加 `--locked` 会因为 lock 不存在/需更新而失败。
#
# 共享 `fixtures/plugins/target`：每个独立 manifest 默认有自己的 target 目录，
# 逐个跑会各编一次 cordis-plugin-sdk（实测 10 个 crate 共 154s）；共享后
# 只编一次，冷跑 22s。这个目录本来就在 CI 的 rust-cache `workspaces` 列表里。
export CARGO_TARGET_DIR="$root/fixtures/plugins/target"
for manifest in \
  fixtures/plugins/expr/lexer/Cargo.toml \
  fixtures/plugins/expr/parser/Cargo.toml \
  fixtures/plugins/expr/evaluator/Cargo.toml \
  fixtures/plugins/expr/evaluator/add/Cargo.toml \
  fixtures/plugins/expr/evaluator/sub/Cargo.toml \
  fixtures/plugins/expr/evaluator/mul/Cargo.toml \
  fixtures/plugins/expr/evaluator/div/Cargo.toml \
  fixtures/plugins/expr/evaluator/pow/Cargo.toml \
  fixtures/plugins/expr/evaluator/modulo/Cargo.toml \
  fixtures/plugins/expr/evaluator/factorial/Cargo.toml \
  fixtures/plugins/root/Cargo.toml; do
  echo "==> $manifest"
  cargo test --manifest-path "$manifest"
done

echo "fixtures tests: ok"
