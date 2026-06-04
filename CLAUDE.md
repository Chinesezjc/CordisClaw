# CLAUDE.md

CordisClaw — 基于有色 Petri 网 (CPN) 的契约驱动插件树运行时。Rust workspace。

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

**Kernel（`crates/cordis-runtime`）提供机制，Plugin（`fixtures/plugins`）提供能力。**

### Kernel 应该保留的
- CPN 执行引擎（engine/net/gate/scheduler）— 令牌流调度
- Plugin Loader / Registry — 发现、解析、加载插件的引导机制
- Context 系统（依赖注入、作用域、slot）— 跨插件的状态传递基础设施
- Service trait + ServiceRegistry — 后台服务生命周期契约的定义方
- Agent 对话管理（LLM 调用循环、工具分发、历史管理）— Agent 本身是"机制"
- **5 个内核自省工具**（`get_runtime_status`、`list_plugins`、`list_nodes`、`get_kernel_status`、`get_kernel_issues`、`reload_runtime`）— 内核状态的查询入口
- Plugin 调用入口（`invoke_plugin`、`execute_target`）— 这是 Kernel 暴露给 Agent 的"万能手柄"

### 应该做成 Plugin 的
- **文件操作**（read/write/search）— 应作为 `filesystem` 插件
- **Shell 执行**（run_command）— 应作为 `shell` 插件的节点
- **Web 访问**（web_search/web_fetch）— `web` 插件
- **Git 操作**（git_diff/log/status/commit）— `git` 插件
- **外部协议适配**（QQ/OneBot 等）— 各自独立插件
- 任何**新能力**默认做成插件，除非它属于"内核机制"

### 判断标准
问自己：**"去掉这个功能，Kernel 还是一个完整的 CPN 运行时吗？"**
- 是 → 可以做成插件
- 否 → 必须在 Kernel

### 为什么
- Kernel 工具不可热替换——改 web 搜索结果源必须改 Kernel 代码
- 硬编码导致 Agent 工具集膨胀——每加一个能力就要改 agent.rs
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
cargo clippy                          # lint（关注新增的 warning）
```

## 提交格式

- 提交信息使用中文，末尾加 `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`
- 在切分支前确认当前在 `main`
