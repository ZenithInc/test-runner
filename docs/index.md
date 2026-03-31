---
layout: home

hero:
  name: test-runner
  text: 面向 AI / Agent 的集成测试 CLI
  tagline: 让 Agent 生成 YAML DSL、用 JSON Schema 做前置校验、再通过 dry-run / JSON 报告完成生成-执行-修复闭环。
  actions:
    - theme: brand
      text: Agent-first 快速开始
      link: /guide/getting-started
    - theme: alt
      text: AI / Agent 指南
      link: /guide/schema

features:
  - title: Agent-first 工作流
    details: 这个项目的首要目标不是让人手写 YAML，而是让内部 AI / Agent 稳定生成 case、workflow 和环境配置，再通过 schema、dry-run 和 JSON 报告自我修复。
  - title: CLI 驱动
    details: 支持 `init`、`test api`、`test dir`、`test all`、`test workflow`，并可在 Testcontainers slot 上并行运行 case / workflow。
  - title: YAML DSL
    details: DSL 形状尽量保持规则化和可预测，让 Agent 更容易按 schema 和现有样例生成变量、HTTP 请求、SQL、Redis、分支、循环、extract 和 assert。
  - title: Workflow 编排
    details: 在 case 之上增加 workflow 层，用 YAML 明确表达跨 case 的顺序、分支、输入输出和 cleanup 策略，避免 Agent 依赖隐式上下文。
  - title: 环境 DSL
    details: 在 `env/*.yaml` 里声明 Docker Compose 或 Testcontainers 容器的生命周期、readiness、日志采集与 slot 并行隔离，让测试命令自动托管环境。
  - title: Callback 与副作用验证
    details: 支持在 case 和 mock route 里安排异步 callback，再通过 Redis / 数据库断言验证最终副作用。
  - title: 可追踪结果
    details: 每次执行都能输出终端摘要或 JSON，并把报告写入 `.testrunner/reports/last-run.json`、`last-workflow-run.json` 或批量 workflow 的 `last-workflows-run.json`，方便 Agent 解析失败并重试。
---

## 这个项目的定位

`test-runner` 的主要使用场景是：**团队内部让 AI / Agent 自动生成测试 DSL，并持续执行、回看、修正**。

这意味着文档里的很多设计点都应该按“机器能否稳定消费”来理解，而不只是“人手写是否舒服”：

- path-based ID 约定，减少人工维护 ID
- JSON Schema，给 Agent 明确结构约束
- `--dry-run`，让 Agent 在真正发请求前先看选择计划
- `--report-format json`，让 Agent 回收结构化运行结果
- `sample-projects/` 样例，作为 few-shot / 检索参考语料

## 这份文档覆盖什么

这份站点基于仓库根目录的 `README.md` 和 `cli/` 当前实现整理而成，重点覆盖：

- 为什么这个项目默认按 **AI / Agent-first** 方式接入
- 如何初始化 `.testrunner/` 目录
- 如何导出 schema，让 Agent 先做结构校验
- 如何使用命令行选择和执行用例
- 如何用环境 DSL 托管 Docker Compose 或 Testcontainers 容器、readiness 和环境日志
- 如何安排 callback，并在 case / mock / workflow 里验证异步副作用
- 如何在 case 之上编排 workflow 流程
- `.testrunner/` 的目录约定与配置文件
- YAML DSL 的上下文、插值、Step、`extract` 与断言
- `sample-projects/` 里的实际示例与当前限制

如果你准备把 `test-runner` 接入现有项目，建议先读「快速开始」和「面向 AI / Agent 的生成与校验」，再进入 DSL 页面补充细节。

如果你当前最关心的是“如何让 Agent 稳定生成配置”，优先看：

- [面向 AI / Agent 的生成与校验](/guide/schema)
- [DSL 语法](/guide/dsl)
- [示例与最佳实践](/guide/examples)

如果你当前最关心的是异步回调或 Docker 环境托管，也可以直接进入：

- [环境 DSL](/guide/environment-dsl)
- [Callback](/guide/callbacks)
