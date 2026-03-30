# 项目结构

执行 `test-runner init` 后，默认会得到下面这套结构：

```text
.testrunner/
  project.yaml
  env/
    local.yaml
    ci.yaml
  datasources/
    mysql.yaml
    postgres.yaml
    redis.yaml
  apis/
    user/
      get-user.yaml
      create-user.yaml
  cases/
    user/
      get-user/
        smoke.yaml
      create-user/
        happy-path.yaml
  data/
    common/
      users.json
    sql/
      seed.sql
      cleanup.sql
  mocks/
    server.yaml
    routes/
      user-profile.yaml
    fixtures/
      user-profile.json
  hooks/
    setup/
    teardown/
  workflows/
    register-login-create-order.yaml
  reports/
```

## 每个目录负责什么

| 路径 | 用途 |
| --- | --- |
| `project.yaml` | 项目级默认配置，例如默认环境、全局超时和 Mock 开关。 |
| `env/` | 环境级配置，例如 `base_url`、通用 headers、环境变量，也可以声明 Docker Compose runtime / readiness / logs。 |
| `datasources/` | MySQL、PostgreSQL、Redis 连接定义。 |
| `apis/` | API 元数据，例如方法、路径、默认 query、默认 body。 |
| `cases/` | 测试用例 YAML DSL。 |
| `data/` | JSON/YAML 数据文件，以及给 `sql` / `query_db` 引用的 SQL 文件。 |
| `mocks/` | 内嵌 Mock 服务的路由、动态响应 DSL 与 fixture 文件。 |
| `hooks/` | 当前只是预留目录，CLI 还不会自动执行这里的脚本。 |
| `workflows/` | workflow 定义文件，描述跨 case 的顺序、分支、输入输出与 cleanup 策略。 |
| `reports/` | 每次运行产出的报告目录；如果启用了环境日志采集，相关文件会额外写到 `reports/env/`。 |

## API ID 和 Case ID 怎么来的

CLI 不需要你手写单独的 ID 字段，而是直接从相对路径推导：

- `apis/system/health.yaml` -> API ID `system/health`
- `cases/user/login/happy-path.yaml` -> Case ID `user/login/happy-path`
- `workflows/register-login-create-order.yaml` -> Workflow ID `register-login-create-order`

这也是为什么命令行里会写成：

```bash
test-runner test api system/health
test-runner test dir user/login
test-runner test workflow register-login-create-order
```

## `data/` 的加载规则

运行时会自动扫描 `data/` 目录下的 `.json`、`.yaml`、`.yml` 文件，并把它们注入到 DSL 上下文的 `data.*` 树里。

例如：

- `data/common/users.json` -> `data.common.users`
- `data/fixtures/order.yaml` -> `data.fixtures.order`

`use_data` step 会再次把指定文件按同样规则加载到 `data.*`，主要作用是让依赖关系更显式。

## `reports/last-run.json`

实际执行测试后，最新报告会固定写到：

```text
.testrunner/reports/last-run.json
```

文件里包含项目名、环境名、目标、汇总信息以及每个 case 的步骤结果。

如果这次运行涉及 callback 或环境托管，`last-run.json` 里还会额外带上：

- `callback_summary` 和 `callbacks`
- `environment_artifacts`
- 并行运行时的 `parallel` 元数据

## `reports/last-workflow-run.json`

执行 workflow 后，最新 workflow 报告会固定写到：

```text
.testrunner/reports/last-workflow-run.json
```

文件里会包含 workflow 级汇总信息、每个 `run_case` step 的状态、导出值，以及 deferred teardown 的执行结果。

如果 workflow 里安排了 callback，或者执行前后托管了环境，这个文件同样会包含：

- `callback_summary` 和 `callbacks`
- `environment_artifacts`
- 并行时的 `slot_id`

## `reports/last-workflows-run.json`

当你执行：

```bash
test-runner test workflow --all --parallel --jobs 2
```

批量 workflow 报告会固定写到：

```text
.testrunner/reports/last-workflows-run.json
```

文件里会包含：

- 本次执行的 workflow 列表
- workflow 级汇总（passed / failed / total）
- 并行元数据（jobs、slots、unit）
- callback 汇总和环境产物索引

其中 `workflows[]` 里的每个元素都会保留单条 workflow 的：

- `workflow_id` / `workflow_name`
- `status` / `error`
- `summary`
- `steps`
- 可选的 `slot_id`

## `reports/slot-<id>/`

在 Testcontainers slot 并行模式下，环境日志产物会按 slot 自动分目录：

```text
.testrunner/reports/slot-0/...
.testrunner/reports/slot-1/...
```

这样可以避免不同 slot 的 app / mysql / redis 日志互相覆盖。

如果环境文件把日志输出写成 `env/app.log`、`env/mysql-query.log` 这类相对路径，那么并行时通常会落成：

```text
.testrunner/reports/slot-0/env/app.log
.testrunner/reports/slot-0/env/mysql-query.log
.testrunner/reports/slot-0/env/redis-monitor.log
```

如果你想看 `env/*.yaml` 如何进一步声明环境生命周期、就绪检查和日志产物，请继续阅读 [环境 DSL](/guide/environment-dsl)。
