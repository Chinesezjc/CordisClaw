# 配置说明

`CordisClaw` 现在会在运行时自动查找 YAML 配置目录：

1. 如果 `fixtures_root` 目录名是 `fixtures`，优先读取它同级的 `config/`
2. 否则读取 `fixtures_root/config/`

当前默认文件：

- `runtime.yaml`
  - RuntimeHost / Kernel 的基础运行参数
- `llm_api.yaml`
  - 内建 kernel 后续接入大模型时使用的 API 配置
- `plugins/*.yaml`
  - 各插件自己的预留配置位

仓库同时提供了一份 `config.example/` 模板目录，方便新环境初始化。`config/` 作为本地运行目录默认不入库，运行时也不会自动读取 `config.example/`，需要按需复制到 `config/`。

建议：

- 把真实密钥放到环境变量里，例如 `OPENAI_API_KEY`
- 或者在本地新增 `config/*.local.yaml` / `config/plugins/*.local.yaml`
- 不要把真实密钥直接提交到仓库
