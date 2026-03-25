# 环境 DSL

`test-runner` 不只把 `env/*.yaml` 当成静态的 `base_url` 切换器。  
现在环境文件还可以声明 `runtime`、`readiness` 和 `logs`，让一次 `test-runner test ... --env <name>` 自动完成：

- 启动 Docker Compose 环境
- 等待服务 ready
- 执行 case / workflow
- 收集环境日志产物
- 根据 cleanup 策略回收容器

如果你已经在用 `sample-projects/`，这套能力对应的就是 `sample-projects/.testrunner/env/docker.yaml`。

## 什么时候应该用环境 DSL

推荐在这些场景里启用环境 DSL：

- 被测服务需要和 MySQL / Redis / 其他依赖一起拉起
- 你想把 `docker compose up/down` 从手工前置步骤收回到 `test-runner`
- 你希望把 MySQL query log、slow log、服务 stdout 作为测试产物一起收集
- 你需要在同一条命令里保证“启动 -> readiness -> 执行 -> 日志 -> 回收”的顺序

如果你只是切换 `base_url`、header 或租户变量，而不需要环境生命周期，那么普通的 `env/*.yaml` 仍然够用，不必强行加 `runtime`。

## sample-project 完整示例

下面这份环境文件已经在仓库里实际运行：

```yaml
name: docker
base_url: http://127.0.0.1:18080
headers:
  x-test-env: docker
variables:
  service_base_url: http://127.0.0.1:18080
  sms_provider_base_url: http://host.docker.internal:18081
  payment_provider_base_url: http://host.docker.internal:18081

runtime:
  kind: docker_compose
  project_directory: .
  files:
    - docker-compose.yml
  project_name: test-runner-sample
  up:
    - --build
    - -d
    - --wait
  down:
    - -v
    - --remove-orphans
  cleanup: always

readiness:
  - kind: http
    url: "{{ env.variables.service_base_url }}/health"
    expect_status: 200
    timeout_ms: 60000
    interval_ms: 1000
  - kind: tcp
    host: 127.0.0.1
    port: 13306
    timeout_ms: 60000
    interval_ms: 1000

logs:
  - kind: compose_service
    service: app
    output: env/app.log
  - kind: container_file
    service: mysql
    path: /var/lib/mysql/general.log
    output: env/mysql-query.log
  - kind: container_file
    service: mysql
    path: /var/lib/mysql/slow.log
    output: env/mysql-slow.log
```

## 三组字段分别负责什么

### `runtime`

`runtime` 负责“如何启动 / 回收环境”。

- `kind: docker_compose`：当前实现的运行时类型
- `project_directory`：执行 `docker compose` 的目录
- `files`：Compose 文件列表
- `project_name`：Compose project name；不写时运行器会自动生成
- `up` / `down`：分别追加到 `docker compose up` / `docker compose down`
- `cleanup`：回收策略，支持 `always`、`on_success`、`never`

最常见的用法就是把你原本手工执行的：

```bash
docker compose up -d --wait
docker compose down -v --remove-orphans
```

改成由环境 DSL 托管。

### `readiness`

`readiness` 负责“什么时候算环境真的可用了”。

当前支持两类检查：

- `kind: http`
  - 适合服务健康检查，例如 `/health`
- `kind: tcp`
  - 适合数据库、Redis 这类“端口先可连再说”的依赖

运行器会在 `docker compose up ...` 之后按顺序执行这些检查；任何一个失败，整次运行都会在 case / workflow 开始前终止。

### `logs`

`logs` 负责“要把哪些环境侧证据收回来”。

当前支持两类来源：

- `kind: compose_service`
  - 通过 `docker compose logs` 收集服务日志
- `kind: container_file`
  - 直接从容器里复制文件，例如 MySQL 的 general log / slow log

这特别适合把“业务断言之外的环境证据”一起留档，例如：

- 应用启动日志
- MySQL general query log
- MySQL slow query log

## 执行时会发生什么

当你运行：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker
```

运行器的顺序大致是：

1. 加载 `env/docker.yaml`
2. 如果启用了内嵌 mock，先启动 mock，并把实际地址回写到 `env.variables.mock_base_url`
3. 执行 `docker compose up ...`
4. 逐条执行 readiness 检查
5. 运行 case 或 workflow
6. 刷新 callback 队列
7. 收集 `logs:` 里声明的产物
8. 按 `cleanup` 策略执行 `docker compose down ...`
9. 把环境元数据写进最终报告

这意味着“环境是否 ready”“环境日志是否采集到”“环境回收是否成功”都会和测试结果一起进入报告。

## 报告和日志产物在哪里

真实执行结束后，环境相关信息会出现在：

- `.testrunner/reports/last-run.json`
- `.testrunner/reports/last-workflow-run.json`

结构里会包含 `environment_artifacts`，例如：

```json
{
  "environment_artifacts": {
    "runtime": {
      "kind": "docker_compose",
      "startup_status": "passed",
      "shutdown_status": "passed"
    },
    "readiness": [
      {
        "kind": "http",
        "status": "passed"
      }
    ],
    "logs": [
      {
        "kind": "compose_service",
        "service": "app",
        "status": "passed",
        "output": "env/app.log"
      }
    ]
  }
}
```

真正的日志文件会落到：

```text
.testrunner/reports/env/
```

对于 `sample-projects/`，你可以直接看到：

- `env/app.log`
- `env/mysql-query.log`
- `env/mysql-slow.log`

## sample-project 推荐命令

```bash
test-runner test api system/health --root sample-projects --env docker
test-runner test workflow register-login-create-order --root sample-projects --env docker
test-runner test workflow payment-callback-flow --root sample-projects --env docker --no-mock
```

## 当前边界

这套环境 DSL 当前有几个明确边界：

- MVP 只支持外部 Compose 文件引用，不支持内联 Compose YAML
- 生命周期是“按一次命令运行”管理，而不是“每个 case 各起一套环境”
- MySQL query log / slow log 是否开启，仍然由环境作者在 Compose / 容器配置里负责；运行器只负责采集
- 如果你想在失败后保留容器排查，可以把 `cleanup` 调成 `never`

## 继续阅读

- [配置文件](/guide/configuration)
- [示例与最佳实践](/guide/examples)
- [工作流使用说明](/workflow/)
