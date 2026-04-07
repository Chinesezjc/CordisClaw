# 文档目录

本目录用于说明 `CordisClaw` 运行时原型的代码职责与维护规则，目标是让新成员可以快速定位代码入口。

配置说明已经收口到 [configuration.md](/root/CordisClaw/docs/configuration.md)，用于说明运行时、Kernel、LLM API 与插件 YAML 配置。

## 文档清单

- `project-overview.md`：项目入口页，概括整体定位并把阅读路径分发到模块化文档。
- `configuration.md`：运行时、Kernel、LLM API 与插件 YAML 配置说明。
- `architecture/README.md`：架构文档索引，汇总各主题文档及推荐阅读顺序。
- `architecture/system-overview.md`：项目定位、仓库边界、目录、核心原则与主流程。
- `architecture/design-blueprint.md`：承接历史设计蓝图，说明目标分层、执行模型、Context 与 Kernel 规划基线。
- `architecture/contracts-and-loading.md`：共享契约、artifact index、resolver、loader、registry 与图服务。
- `architecture/async-workflow-api.md`：非宏 async workflow API 草案（受控 await 原语、WaitHandle 边界与 runtime 契约）。
- `architecture/runtime-semantics.md`：Context、执行引擎、Kernel 与 auto-update 语义。
- `architecture/plugins-and-tooling.md`：样例插件、调用路径、CLI 与工件构建流程。
- `architecture/maintenance-guide.md`：测试矩阵、阅读顺序、常见坑点与扩展方向。
- `architecture/status-and-open-items.md`：当前架构/计划的完成度、部分完成项与未完成项整理。
- `rs-files-responsibility.md`：所有 `.rs` 文件的职责总表（按路径分组）。
- `cargo-workspace-notes.md`：解释 `members/default-members/exclude` 和本地 `path` 依赖的区别。

当前仓库还支持导出两类 HTML 图：

1. 已注册节点图：使用 `cargo run -p cordis-runtime -- graph-html fixtures --output=registered-nodes.html`。
2. 已注册节点 Net：使用 `cargo run -p cordis-runtime -- net-html fixtures --output=registered-net.html`。

插件工件重建与 docs 同步入口：

1. `cargo run -p cordis-runtime -- rebuild-fixture-artifacts [fixtures_root]`
2. `cargo run -p cordis-runtime -- sync-plugin-docs fixtures`
3. `cargo run -p cordis-runtime -- refresh-artifact-index fixtures`

## 维护约定

1. 新增 `.rs` 文件时，必须同步更新 `rs-files-responsibility.md`。
2. 文件职责描述保持“做什么/不做什么”两层语义，避免只写文件名翻译。
3. 如果某文件职责发生变化（例如从“示例”变为“生产逻辑”），应在同一次 PR 更新文档。
