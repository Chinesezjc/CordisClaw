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
