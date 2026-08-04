# CLAUDE.md

CordisClaw — 基于有色 Petri 网 (CPN) 的契约驱动插件树运行时。Rust workspace。

## 工作规则

**任何代码修改必须先写 plan，用户批准后再实施。** 不要直接改代码。

**任何代码修改必须补完整的测试。** 新功能配新测试（单测 + 涉及跨模块链路时的集成测试），改行为改断言，修 bug 先补能复现的用例；不允许"测试另立批次"式欠账。

**默认起 10 个命名 teammate 并行工作。** 实施批准后的 plan 时，默认将任务拆给 10 个命名 agent 并行推进（任务本身粒度不足 10 份时按实际拆分）。要求：任务边界切干净；并行改文件时用 worktree 隔离；汇合后由主会话统一验证（cargo build/test/clippy）再落盘。

**任何提交必须保持 `cargo clippy --all-targets -- -D warnings` 通过（零 warning 规范）。** 确需豁免时用带理由注释的 `#[expect(lint)]`，禁止裸 `#[allow]`。

**任何提交必须保持 CI coverage 门槛通过（行覆盖 100%，coverage workflow 对 PR 自动跑）。** 全仓已达 100%（26497/26497），**不使用任何覆盖率排除注释**（无 `COV_EXCL` / `#[coverage(off)]`）。文件级排除仅 `main.rs`（REPL/signal/exit）与 `agent.rs`（LLM 流式 I/O），属进程/网络边界。禁止用降门槛、加排除注释或加文件排除消化缺口。

新代码必须自带测试做到 100% 行覆盖。遇到"覆盖不到的行"先判性质——lcov 每行计数取该行起始的所有 region 的最大值，所以一行显示未覆盖有两种成因：

1. **真未执行的分支** → 故障注入。本仓已验证的手段：只读目录 / 目录占位文件路径（I/O 失败臂）、`mkfifo` + `flock` 返回 ENOTSUP（锁失败臂）、超长 session id 触发 ENAMETOOLONG（tmp 写入臂）、docs 声明超 `max_total_nodes` 触发 BudgetExceeded（reload 失败臂）、录制 invocation 后改写响应造成重放分歧（canary Fail）、不可 spawn 的命令 vs 可 spawn 但退出非零（区分 Err 与 Fail verdict）。
2. **零计数 edge 独占该行**（`?` 的错误分支、恒真 `if`/`if let` 门控的隐式 else、多行语句的中间片段、多行 `assert!` 的惰性消息实参）→ 改语句形态，使零计数 edge 与已覆盖 region 合行：
   - `if let Some(x) = always_some() { .. }` → `for x in opt.into_iter() { .. }`（必须带 `.into_iter()`，否则触发 clippy `for_loops_over_fallibles`）
   - 多行 `matches!` → `use Enum::{A,B};` 后单行，或类型 PartialEq 时直接 `assert_eq!` 整值比较
   - 多行 `assert!(cond, "msg {:?}", v)` → 先 `let cond = ...;` 再单行 assert
   - 多行 `?` 续行 → 预绑定中间变量压成单行
   - `if a { true } else { b? }` → `a || b?`
   - 嵌套守卫 → `.and_then()` / `.filter()` / `.then().flatten()` 链式扁平化
   - 测试内 `other => panic!(...)` 不可达臂 → 改 `assert_eq!` 整值比较（`RuntimeError` 无 `PartialEq`，用 `to_string()` 比较）

**注意**：把 `let ... else { panic!() }` 改成双臂 `match` 只是把不可达 panic 臂换成不可达 fallback 臂，覆盖率不变——必须同时补一个传入非匹配变体的测试让 fallback 臂真正执行。

**真不可达的臂**（如编译期常量保证不失败的注册、批内回滚失败、reqwest client 构造失败）→ 把臂体提取为具名 free function + 直接单测（含逐字节消息断言），原位只剩已覆盖的单行调用。若"体不执行"本身就是被测语义（如断言某闭包不该被调用），同样提取体为具名函数由独立单测覆盖。重构必须保持错误文本逐字节一致，可用字符串字面量多重集差分自检（删除集须为空）。

测试提速：JSON 工件 fixture（插件不声明 `crate-type = ["dylib"]`）使 `prepare_artifacts` 直接生成 `artifacts/<name>.json` 而不 shell 出去跑 `cargo build`，集成测试从 ~120s 降到 2-4s。

本地跑法：
```bash
cargo llvm-cov --fail-under-lines 100 --ignore-filename-regex 'cordis-runtime/src/(main|agent)\.rs' -- --test-threads=1
```

## 关键文档

修改代码前后应参考：

- [docs/architecture/status-and-open-items.md](docs/architecture/status-and-open-items.md) — 架构完成度与待办清单
- [docs/architecture/system-overview.md](docs/architecture/system-overview.md) — 系统总览
- [docs/architecture/design-blueprint.md](docs/architecture/design-blueprint.md) — 设计蓝图
- [docs/architecture/runtime-semantics.md](docs/architecture/runtime-semantics.md) — 运行时语义
- [docs/architecture/contracts-and-loading.md](docs/architecture/contracts-and-loading.md) — 契约与加载
- [docs/rs-files-responsibility.md](docs/rs-files-responsibility.md) — 文件职责索引

## 修改后必须更新文档

**每次代码修改完成后，必须检查并更新文档：**

1. **`docs/architecture/status-and-open-items.md`** — 如果改动涉及：
   - 闭合了某个 TODO / 部分完成项 → 将状态从"部分完成"改为"已完成"，从 TODO 列表中移除
   - 新增了未完成的能力 → 添加到对应章节
   - 修改了执行引擎、插件加载、服务生命周期、配置等核心链路 → 更新状态描述
2. **其他文档** — 如果改动使其过时，同步更新。
3. **日期** — 更新 `status-and-open-items.md` 开头的"最近更新"日期。

## 架构原则：Kernel vs Plugin 边界

**Kernel（`crates/cordis-runtime`）= 最小可用单位**：agent 拿起来就能跑的最小自包含 runtime，包括 agent 直接依赖的基础工具（file/shell/search 等）。**Plugin（`fixtures/plugins`）= 扩展与覆写点**：给 agent 提供额外能力，或替换 kernel 内建的默认实现。

### Kernel 应该保留的
- CPN 执行引擎（engine/net/gate/scheduler）— 令牌流调度
- Plugin Loader / Registry — 发现、解析、加载插件的引导机制
- Context 系统（依赖注入、作用域、slot）— 跨插件的状态传递基础设施
- Service trait + ServiceRegistry — 后台服务生命周期契约的定义方
- Agent 对话管理（**循环编排**、工具分发、历史管理/压缩、会话快照）— Agent 循环本身是"机制"。
  注意**不含 LLM 传输**：与供应商对话的 HTTP/SSE、鉴权、重试、wire format 已整体移出，
  见下方"可以做成 Plugin 的"。kernel 只构造请求体、消费结构化的 `tool_calls`。
- **内核自省工具**（`get_runtime_status`、`list_plugins`、`list_nodes`、`get_kernel_status`、`get_kernel_issues`、`reload_runtime`）— 内核状态的查询入口
- **Agent 基础工具**（`read_file` / `write_file` / `search_code` / `run_command` 等）— agent 干活的最小必需集，Kernel 保留一份默认实现避免"没插件就不能启动"
- Plugin 调用入口（`invoke_plugin`、`execute_target`）— 这是 Kernel 暴露给 Agent 的"万能手柄"

### 可以做成 Plugin 的（扩展 / 覆写用）
- **文件操作**（read/write/search）— 除 Kernel 默认实现外，可加 `filesystem` 插件覆写（例如加密文件系统）
- **Shell 执行**（run_command）— 除 Kernel 默认外，可加 `shell` 插件（例如切换到 nushell）
- **Web 访问**（web_search/web_fetch）— `web` 插件（切换搜索源）
- **Git 操作**（git_diff/log/status/commit）— `git` 插件（切换 git 后端）
- **外部协议适配**（QQ/OneBot 等）— 各自独立插件
- **LLM provider**（与模型对话的传输层）— `llm_openai` 等插件。声明 `llm_complete`
  能力节点即接管（同 soul 的 `soul_get`/`soul_set` 约定）。**kernel 不留内建实现**：
  没装 provider 插件时 `agent_send` 报 `NoLlmProvider`，而 boot / REPL /
  `command_router`（`/status`、`/help`）照常工作。这是本项目里唯一一个
  "kernel 无兜底"的覆写点——因为传输天然是供应商专有的，留一份 OpenAI 实现在
  kernel 里既不通用、又会让 `provider` 字段继续名存实亡。
- 任何**新能力**默认做成插件，除非它属于"agent 最小可用集"或"内核机制"

### 判断标准
- **"去掉这个功能后 agent 还能启动、看到自己的代码、跑测试吗？"** —— 不能 → 必须在 Kernel。
- **"能替换实现从而变换体验吗？"** —— 能 → 值得做成插件（即使 kernel 也有一份默认）。

**LLM 传输是第一条的一个刻意例外**，理由记在这里以免下次被当成违规改回去：
去掉 provider 插件后 agent **能启动、能看代码、能跑测试**，只是不能跟模型说话——
第一条问的三件事都仍然成立，所以它不属于"必须在 Kernel"。而它高度供应商专有
（OpenAI 与 Anthropic 的 wire format 不兼容），留一份内建实现既不通用，又会让
配置里的 `provider` 字段继续名存实亡（拆分前全仓无一处按它分支 wire format，
只当白名单闸门用）。因此这是唯一一个 **kernel 不留兜底**的覆写点。

### 为什么
- Kernel 内建工具保证冷启动可用性（没插件 agent 也能干活）
- Plugin 提供扩展 / 覆写：改 web 搜索源可以只写插件，不动 kernel
- Plugin 可以独立 reload、独立版本管理、独立安全边界
- Plugin 通过 `NodeType::Task` 声明，Agent 通过 `invoke_plugin` 自动发现

## 项目结构

```
crates/
  cordis-plugin-sdk/     — 插件 ABI、文档类型、导出宏
  cordis-runtime/        — 核心运行时：loader、引擎、agent、host
  cordis-plugin-host/    — 插件宿主抽象
fixtures/                — 测试 fixtures（插件样例、工件索引）
docs/                    — 架构文档
```

## 测试

```bash
cargo build                           # 编译（零 warning）
cargo test                            # 全部测试
cargo test --test semantics           # 引擎语义测试
cargo test --test architecture        # 架构集成测试
cargo clippy --all-targets -- -D warnings  # lint：必须零 warning（warning 即失败）
```

## 提交格式

- 在切分支前确认当前在 `main`
- 提交信息不带 Co-Authored-By 等 coauthor 尾注
