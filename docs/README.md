# 文档目录

本目录用于说明 `CordisClaw` 运行时原型的代码职责与维护规则，目标是让新成员可以快速定位代码入口。

## 文档清单

- `rs-files-responsibility.md`：所有 `.rs` 文件的职责总表（按路径分组）。
- `cargo-workspace-notes.md`：解释 `members/default-members/exclude` 和本地 `path` 依赖的区别。

当前仓库还支持导出两类 HTML 图：

1. 已注册节点图：见根 [README.md](/root/CordisClaw/README.md) 的 `graph-html` 示例。
2. 已注册节点 DAG：见根 [README.md](/root/CordisClaw/README.md) 的 `dag-html` 示例。

## 维护约定

1. 新增 `.rs` 文件时，必须同步更新 `rs-files-responsibility.md`。
2. 文件职责描述保持“做什么/不做什么”两层语义，避免只写文件名翻译。
3. 如果某文件职责发生变化（例如从“示例”变为“生产逻辑”），应在同一次 PR 更新文档。
