---
layout: home

hero:
  name: test-runner
  text: 面向 HTTP 服务的集成测试 CLI
  tagline: 用 YAML DSL 编排请求、数据库、Redis、Mock、Callback 与环境生命周期，让接口集成测试可以被初始化、复用和持续演进。
  actions:
    - theme: brand
      text: 开始阅读
      link: /guide/getting-started
    - theme: alt
      text: DSL 语法
      link: /guide/dsl

features:
  - title: CLI 驱动
    details: 支持 `init`、`test api`、`test dir`、`test all`、`test workflow`，可以按 API、目录、全量或工作流运行测试。
  - title: YAML DSL
    details: 在测试用例里描述变量、HTTP 请求、SQL、Redis、分支、循环、extract 和 assert。
  - title: Workflow 编排
    details: 在 case 之上增加 workflow 层，用 YAML 编排跨 case 的顺序、分支、输入输出和 cleanup 策略。
  - title: 环境 DSL
    details: 在 `env/*.yaml` 里声明 Docker Compose 或 Testcontainers 容器的生命周期、readiness 和日志采集，让测试命令自动托管环境。
  - title: Callback 与副作用验证
    details: 支持在 case 和 mock route 里安排异步 callback，再通过 Redis / 数据库断言验证最终副作用。
  - title: 可追踪结果
    details: 每次执行都能输出终端摘要或 JSON，并把报告写入 `.testrunner/reports/last-run.json` 或 `.testrunner/reports/last-workflow-run.json`。
---

## 这份文档覆盖什么

这份站点基于仓库根目录的 `README.md` 和 `cli/` 当前实现整理而成，重点覆盖：

- 如何初始化 `.testrunner/` 目录
- 如何使用命令行选择和执行用例
- 如何用环境 DSL 托管 Docker Compose 或 Testcontainers 容器、readiness 和环境日志
- 如何安排 callback，并在 case / mock / workflow 里验证异步副作用
- 如何在 case 之上编排 workflow 流程
- `.testrunner/` 的目录约定与配置文件
- YAML DSL 的上下文、插值、Step、`extract` 与断言
- `sample-projects/` 里的实际示例与当前限制

如果你准备把 `test-runner` 接入现有项目，建议先读「快速开始」和「命令行使用」，再进入 DSL 页面补充细节。

如果你当前最关心的是异步回调或 Docker 环境托管，也可以直接进入：

- [环境 DSL](/guide/environment-dsl)
- [Callback](/guide/callbacks)
