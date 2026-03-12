# CordisClaw 项目总览

本文档现在作为文档入口页使用，不再承担全部细节说明。

如果你第一次进入仓库，建议按下面顺序阅读：

1. [架构文档索引](./architecture/README.md)
2. [系统概览](./architecture/system-overview.md)
3. [契约与加载链路](./architecture/contracts-and-loading.md)

## 快速结论

`CordisClaw` 当前是一个 Rust 运行时原型，重点不是单一业务功能，而是验证一套面向插件树的运行时体系：

- 插件发现依赖 `package.metadata.cordis.children`
- 插件加载依赖 ABI 指纹、artifact index 与 `sha256` 校验
- 插件能力通过 `docs/agent/interfaces.json` 暴露
- 服务注入通过父子边上的 `grants` 控制
- 运行时围绕 DAG、Gate、Router、Actor 和 Kernel 原型组织

## 文档地图

| 文档 | 关注点 |
|---|---|
| [architecture/system-overview.md](./architecture/system-overview.md) | 项目定位、仓库边界、目录、核心原则、主流程 |
| [architecture/design-blueprint.md](./architecture/design-blueprint.md) | 承接历史设计蓝图，说明目标分层、执行、Context、Loader 与 Kernel 规划基线 |
| [architecture/contracts-and-loading.md](./architecture/contracts-and-loading.md) | SDK 契约、metadata、docs、artifact、resolver、loader、registry、graph |
| [architecture/runtime-semantics.md](./architecture/runtime-semantics.md) | Context、执行引擎、Gate、Router、Kernel、auto-update |
| [architecture/plugins-and-tooling.md](./architecture/plugins-and-tooling.md) | `shell` / `expr` / `root` 样例插件、调用路径、CLI、脚本 |
| [architecture/maintenance-guide.md](./architecture/maintenance-guide.md) | 测试矩阵、推荐阅读顺序、常见坑点、扩展方向 |
| [architecture/status-and-open-items.md](./architecture/status-and-open-items.md) | 当前架构/计划的完成度、部分完成项、未完成项与建议优先级 |

## 按场景阅读

- 想快速理解全貌：先看 [系统概览](./architecture/system-overview.md)
- 想看原始设计蓝图与目标模型：看 [设计蓝图](./architecture/design-blueprint.md)
- 想看 loader / artifact / docs 契约：看 [契约与加载链路](./architecture/contracts-and-loading.md)
- 想看执行语义和 Kernel：看 [运行时语义](./architecture/runtime-semantics.md)
- 想看外部插件样例和命令入口：看 [插件与工具链](./architecture/plugins-and-tooling.md)
- 想知道现在还差什么：看 [架构与计划完成度](./architecture/status-and-open-items.md)
- 想开始修改代码：看 [维护指南](./architecture/maintenance-guide.md) 和 [rs-files-responsibility.md](./rs-files-responsibility.md)

## 相关文档

- [架构文档索引](./architecture/README.md)
- [Cargo Workspace Notes](./cargo-workspace-notes.md)
- [Rust 文件职责清单](./rs-files-responsibility.md)
