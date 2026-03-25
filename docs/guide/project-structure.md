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

## `reports/last-workflow-run.json`

执行 workflow 后，最新 workflow 报告会固定写到：

```text
.testrunner/reports/last-workflow-run.json
```

文件里会包含 workflow 级汇总信息、每个 `run_case` step 的状态、导出值，以及 deferred teardown 的执行结果。

如果你想看 `env/*.yaml` 如何进一步声明环境生命周期、就绪检查和日志产物，请继续阅读 [环境 DSL](/guide/environment-dsl)。
