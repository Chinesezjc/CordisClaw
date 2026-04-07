# 架构文档索引

本目录把原先聚合在一篇总文档里的内容拆成多个主题文件，方便按模块维护。

## 文档清单

- [system-overview.md](./system-overview.md)：项目定位、仓库边界、目录结构、核心原则和主流程。
- [design-blueprint.md](./design-blueprint.md)：承接历史设计蓝图，说明目标分层、执行模型、Context 与 Kernel 规划基线。
- [contracts-and-loading.md](./contracts-and-loading.md)：共享契约、artifact index、resolver、loader、registry、docs 与图服务。
- [async-workflow-api.md](./async-workflow-api.md)：非宏 async workflow API 草案（受控 await 原语与 runtime 边界）。
- [runtime-semantics.md](./runtime-semantics.md)：Context、Execution、Kernel、auto-update 等运行时语义。
- [plugins-and-tooling.md](./plugins-and-tooling.md)：外部插件样例、调用路径、CLI 与工件构建流程。
- [maintenance-guide.md](./maintenance-guide.md)：测试矩阵、阅读顺序、常见坑点和扩展方向。
- [status-and-open-items.md](./status-and-open-items.md)：当前架构/计划的完成度、部分完成项与未完成项整理。

配置约定补充：

- [docs/configuration.md](/root/CordisClaw/docs/configuration.md) 说明 `runtime.yaml`、`llm_api.yaml` 与 `plugins/*.yaml` 的用途。

## 推荐阅读顺序

1. [system-overview.md](./system-overview.md)
2. [design-blueprint.md](./design-blueprint.md)
3. [contracts-and-loading.md](./contracts-and-loading.md)
4. [async-workflow-api.md](./async-workflow-api.md)
5. [runtime-semantics.md](./runtime-semantics.md)
6. [plugins-and-tooling.md](./plugins-and-tooling.md)
7. [maintenance-guide.md](./maintenance-guide.md)
8. [status-and-open-items.md](./status-and-open-items.md)
