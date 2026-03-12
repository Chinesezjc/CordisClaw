#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixtures_root="${1:-$repo_root/fixtures}"
artifacts_dir="$fixtures_root/artifacts"

cd "$repo_root"

echo "[1/5] build top-level external plugins"
cargo build --manifest-path fixtures/plugins/Cargo.toml -p expr -p shell

echo "[2/5] build nested expr dylib plugins"
for manifest in \
  fixtures/plugins/expr/lexer/Cargo.toml \
  fixtures/plugins/expr/parser/Cargo.toml \
  fixtures/plugins/expr/evaluator/Cargo.toml \
  fixtures/plugins/expr/evaluator/add/Cargo.toml \
  fixtures/plugins/expr/evaluator/sub/Cargo.toml \
  fixtures/plugins/expr/evaluator/mul/Cargo.toml \
  fixtures/plugins/expr/evaluator/div/Cargo.toml
  do
  cargo build --manifest-path "$manifest"
done

echo "[3/5] copy dylib artifacts"
install -m 755 fixtures/plugins/target/debug/libexpr.so "$artifacts_dir/expr.so"
install -m 755 fixtures/plugins/target/debug/libshell.so "$artifacts_dir/shell.so"
install -m 755 fixtures/plugins/expr/lexer/target/debug/libexpr_lexer.so "$artifacts_dir/expr_lexer.so"
install -m 755 fixtures/plugins/expr/parser/target/debug/libexpr_parser.so "$artifacts_dir/expr_parser.so"
install -m 755 fixtures/plugins/expr/evaluator/target/debug/libexpr_evaluator.so "$artifacts_dir/expr_evaluator.so"
install -m 755 fixtures/plugins/expr/evaluator/add/target/debug/libexpr_evaluator_add.so "$artifacts_dir/expr_evaluator_add.so"
install -m 755 fixtures/plugins/expr/evaluator/sub/target/debug/libexpr_evaluator_sub.so "$artifacts_dir/expr_evaluator_sub.so"
install -m 755 fixtures/plugins/expr/evaluator/mul/target/debug/libexpr_evaluator_mul.so "$artifacts_dir/expr_evaluator_mul.so"
install -m 755 fixtures/plugins/expr/evaluator/div/target/debug/libexpr_evaluator_div.so "$artifacts_dir/expr_evaluator_div.so"

perl -e 'unlink @ARGV' \
  "$artifacts_dir/expr.json" \
  "$artifacts_dir/expr_runner" \
  "$artifacts_dir/expr_lexer.json" \
  "$artifacts_dir/expr_parser.json" \
  "$artifacts_dir/expr_evaluator.json" \
  "$artifacts_dir/expr_evaluator_add.json" \
  "$artifacts_dir/expr_evaluator_sub.json" \
  "$artifacts_dir/expr_evaluator_mul.json" \
  "$artifacts_dir/expr_evaluator_div.json"

echo "[4/5] sync generated interfaces.json from plugin docs()"
cargo run -p cordis-runtime -- sync-plugin-docs "$fixtures_root"

echo "[5/5] refresh artifact index hashes"
cargo run -p cordis-runtime -- refresh-artifact-index "$fixtures_root"

perl -e 'unlink @ARGV' \
  "$repo_root/fixtures/plugins/Cargo.lock" \
  "$repo_root/fixtures/plugins/expr/lexer/Cargo.lock" \
  "$repo_root/fixtures/plugins/expr/parser/Cargo.lock" \
  "$repo_root/fixtures/plugins/expr/evaluator/Cargo.lock" \
  "$repo_root/fixtures/plugins/expr/evaluator/add/Cargo.lock" \
  "$repo_root/fixtures/plugins/expr/evaluator/sub/Cargo.lock" \
  "$repo_root/fixtures/plugins/expr/evaluator/mul/Cargo.lock" \
  "$repo_root/fixtures/plugins/expr/evaluator/div/Cargo.lock"

echo "done: rebuilt plugin artifacts under $fixtures_root"
