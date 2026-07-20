# 任务分发：E 批四路并行（2026-07-20）

D 批（`7dc1a78`）之后的收尾与产品面推进，拆成 4 个互不重叠的任务，分发给 4 个 agent 并行执行。

## 公共规则（每个 agent 都要遵守）

1. **读完本节再开工。** 修改前先读 `CLAUDE.md` 与 `docs/architecture/status-and-open-items.md` 相关章节。
2. **分支纪律**：从 `main`（`7dc1a78` 或更新）切出各自分支 `e-batch/<task-id>`，完成后由人工 review 合回 `main`。不要直接改 `main`。
3. **文件域隔离**：只改自己任务列出的文件域。如果发现必须跨域改动，停下来在结论里说明，不要直接改别人的域。
4. **fixture docs 自愈文件**：commit 前 `git checkout -- fixtures/plugins/gacha/docs/agent/interfaces.json fixtures/plugins/qq/docs/agent/interfaces.json`（运行 serve/测试会自动重写这两个文件，不属于你的改动）。
5. **提交格式**：中文 commit message，**不加任何 Co-Authored-By trailer**。
6. **文档义务**：完成后在 `status-and-open-items.md` 追加自己的小节（5.2.23 起，编号先到先得，合并时人工调整）。四个任务都会碰这个文件，**只允许追加新小节，不要改动既有内容**，减少合并冲突。
7. **验证基线**：`cargo build`（零新增 warning）+ 各自任务的验收命令。工具链在 MomoiAiri（`export PATH=/root/.cargo/bin:$PATH`），仓库位于 `/root/CordisClaw`。

---

## 任务 A：修复 `tests/architecture.rs` 的 11 项失效测试

**文件域**：`crates/cordis-runtime/tests/architecture.rs`（必要时可在 `fixtures/plugins/` 下**新增**测试专用 fixture 目录，不许改动现存插件）

**背景**：D 批修掉该文件的编译错误（P2-7 遗留的 `run_deterministic` 死引用）后，暴露出 11 项运行失败。根因是这些测试写于旧 fixture 时代，指望 `root/child` 嵌套插件结构，而 P2-10 已把 `fixtures/plugins/root/child/` 拆掉（`root/Cargo.toml` 的 `children = []`）。当前失败清单：

```
child_path_escape_fails_fast
expr_dylib_subplugins_are_invokable
hash_mismatch_marks_child_unavailable_and_no_fallback
inject_on_unavailable_plugin_returns_unavailable_error
load_success_and_grants_enforced
multi_producer_input_generates_warning
optional_child_unavailable_does_not_block_parent
plugin_path_mismatch_fails_fast
registered_graph_json_and_html_are_available
registered_net_json_and_html_are_available
required_child_unavailable_blocks_parent_chain
```

**做法建议**（按测试语义分三类处理）：
1. **父子链路类**（`*_child_*`、`load_success_and_grants_enforced`）：这些测的是"required/optional child 不可用时父链的行为"，语义仍然有效。用 `setup_fixture_copy()` + `insert_test_plugin()` 现有 helper 在临时目录里**程序化构造**父子结构，摆脱对 on-disk `root/child` 的依赖。文件里已有 `patch_index` helper 可用。
2. **现存插件可替代类**（`expr_dylib_subplugins_are_invokable`、`registered_*_are_available`、`multi_producer_*`）：改成针对现存插件（`expr` 树仍在、`gacha`/`qq` 的 multi-producer 警告在 serve 日志里天天出现）断言。
3. **确认已死类**：如果某个测试的语义已被单元测试覆盖（先 grep `crates/cordis-runtime/src/` 确认），可以 stub 掉，但要像 D 批处理 `scheduler_is_deterministic_across_runs` 那样保留测试名 + 注释说明去处。stub 需要在总结里逐个列出理由。

**验收**：
```bash
cargo test --test architecture      # 全绿，0 ignored 之外不许有 failed
cargo test -p cordis-runtime        # 整个 crate 全绿（这是本批的最终目标）
```

---

## 任务 B：SDK 侧 service panic 隔离

**文件域**：`crates/cordis-plugin-sdk/src/lib.rs`（`export_plugin_api!` 宏、`ServiceVTable` 相关）+ 各 fixture 插件**因宏改动而必需的机械性重编译适配**（预期为零，宏内部改动应对插件源码透明）

**背景**：D 批已给 `handle`（普通 `fn` 指针）加了 runtime 侧 `catch_unwind`，但 service 的 `start`/`stop` 是 `extern "C" fn`（`cordis-plugin-sdk/src/lib.rs:302-304`），modern rustc 在 panic unwind 穿越 C ABI 时自动 `abort()`——runtime 侧拦截被实测证伪（SIGABRT，见 5.2.22）。隔离必须做在**插件自己这一侧**：panic 在跨 ABI 之前就地 catch。

**做法**：
1. 找到 SDK 中生成 service vtable 的路径（`_cordis_create_service`、`ServiceVTable` 的构造处，以及插件如何提供 start/stop 实现——先读 `qq` 插件的 `qq_serve` 是怎么接进来的，它是现存唯一真实 service）。
2. 在 SDK 提供的包装层里，把用户的 start/stop 函数包进 `std::panic::catch_unwind(AssertUnwindSafe(...))`，panic 时 `eprintln!` 带插件上下文并返回非零错误码（约定 `-1`），**绝不让 unwind 触碰 `extern "C"` 边界**。
3. 如果现状是插件直接手写 `extern "C" fn` 塞进 vtable（没有 SDK 包装层），则需要新增一个安全包装宏/构造函数，并迁移 `qq` 插件到新路径——这属于本任务文件域内允许的插件改动。
4. 测试写在 SDK crate 里：构造一个 panic 的 start 函数，经包装后调用，断言进程不 abort、返回 -1。参考 D 批被删掉的 `service_extern_panic_is_caught` 测试（git show `7dc1a78^` 期间的中间版本没有入库，直接重写即可，注意这次 catch 在 ABI 内侧所以能过）。

**验收**：
```bash
cargo test -p cordis-plugin-sdk     # 含新增 panic 隔离测试
cargo build && cargo test -p cordis-runtime --lib
# 手工验证：临时给 qq_serve 的 start 注入 panic!，serve 启动不崩、打印错误、其余插件正常加载（验证完撤销注入）
```

---

## 任务 C：REPL / serve 收尾正确性

**文件域**：`crates/cordis-runtime/src/main.rs`

**背景**：E2E 二轮验证（5.2.22）中，`kernel iterate-plugins` 的最终结果 JSON 没有打印完整——stdin 里排队的 `quit` 在 iterate 内部两次 registry reload 期间被消费，进程在 `println!` 完成前退出（run log 停在第二次 reload 的 `[snapshot] detected ...`，`final_verdict` 缺失）。批处理式使用 REPL（`cat cmds | cordis-runtime serve fixtures`）时结果不可靠。

**做法**：
1. 先复现：`printf 'kernel iterate-plugins {...}\nquit\n' | ./target/debug/cordis-runtime serve fixtures`，用一个立刻返回的 request（比如 target 不存在的 plugin path，走 InvalidArgument 快速失败路径）确认输出完整性；再用真实慢路径确认截断。
2. 排查方向（按可能性排序）：
   - REPL 循环是否在命令 handler 还没返回时就把下一条 stdin 读进来并处理（并发消费）；
   - `quit`/`exit` 分支（`main.rs:620` 附近）返回 `Ok(false)` 后的退出路径是否 `std::process::exit` 而跳过了 stdout flush；
   - iterate 内部 reload 是否 spawn 了会调 `exit` 的东西。
3. 修复原则：每条命令**处理完、stdout flush 完**才读下一条；退出路径统一走 flush-then-return，不允许命令处理中途 `process::exit`。
4. 顺带：给 `kernel iterate-plugins` 的 REPL 输出加一行人类可读摘要（`final_verdict` + `changed_paths` + `blocked_reason`），完整 JSON 保留在其后——现在 450 行 JSON 一坨，肉眼找 verdict 很费劲。

**验收**：
```bash
# 快速失败路径与慢路径各跑一次，final JSON 必须完整（jq 能 parse），quit 不吞输出
printf '...' | ./target/debug/cordis-runtime serve fixtures | tail -1 | jq .final_verdict
```

---

## 任务 D：产品面——QQ 对话链路端到端验证

**文件域**：`fixtures/plugins/qq/`、`fixtures/startup_invoke.json`、`fixtures/notify_handlers.json`（runtime 侧只许读不许改；发现 runtime bug 记录下来交回，不要自己修）

**背景**：基建批次已收尾（79/80 findings + 崩溃恢复 + 自迭代 E2E 闭环），转产品面。QQ 插件已具备：HTTP webhook（含 X-Signature 校验，P0-23）、消息去重与队列、`qq_send`/`qq_serve`/`qq_system_notify` 节点。但**从 QQ 消息进来 → agent 处理 → 回复发出去**的完整链路从未端到端验证过。

**做法**：
1. 读 `fixtures/plugins/qq/src/lib.rs` 与 `main.rs` 的 serve 分发逻辑（`[QQ group from` 的 session 路由，`main.rs:322` 附近），画出消息流转链路图（写进交付文档）。
2. 不依赖真实 QQ 服务端：用 `curl` 构造带合法 `X-Signature` 的 OneBot v11 事件 POST 到 `127.0.0.1:8099/onebot/event`，模拟群消息进入。
3. 验证链路各段：webhook 收到 → 去重/队列 → session 路由（对 group_id 建 session）→ agent 触发（这一步消耗 DeepSeek token，先用一条消息验证）→ `qq_send` 回复动作产生（拦在 HTTP 出口即可，mock 一个本地 sink 或断言出站请求构造正确）。
4. 每段写成可重复执行的脚本，落在 `fixtures/plugins/qq/tests/` 或 `scripts/`（新建目录允许）。签名构造已有测试可参考（qq 插件里的 4 个 signature case）。
5. 发现的断链/bug：属于 qq 插件域内的直接修；属于 runtime 域的（session 路由、inject 队列等）只记录、不修改，写进交付文档的"移交 runtime 的问题"清单。

**验收**：
- 交付一份链路验证报告（追加到 status 文档自己的小节），明确每一段 PASS/FAIL；
- 模拟 webhook 的脚本可重复运行；
- `cargo test -p qq --manifest-path fixtures/plugins/Cargo.toml` 全绿（含新增测试）。

---

## 合并顺序建议

A 和 B 无交集可任意先后；C 只碰 `main.rs` 与 A/B 无冲突；D 只碰 fixtures/qq。唯一共享文件是 `status-and-open-items.md`（都是追加小节，冲突好解）。建议合并顺序 **C → B → A → D**（C 最小、D 的报告小节最长放最后）。全部合完后在 `main` 跑一次 `cargo build && cargo test`（此时 A 已保证全绿）再 push origin。
