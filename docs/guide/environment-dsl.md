# 环境 DSL

`test-runner` 不只把 `env/*.yaml` 当成静态的 `base_url` 切换器。  
现在环境文件还可以声明 `runtime`、`readiness` 和 `logs`，让一次 `test-runner test ... --env <name>` 自动完成：

- 启动 Docker Compose 环境 **或** Testcontainers 容器
- 等待服务 ready
- 执行 case / workflow
- 收集环境日志产物
- 根据 cleanup 策略回收容器

如果你已经在用 `sample-projects/`，这套能力对应的就是 `sample-projects/.testrunner/env/docker.yaml`（Docker Compose 模式）或 `sample-projects/.testrunner/env/containers.yaml`（Testcontainers 模式）。

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
  - kind: redis_monitor
    service: redis
    output: env/redis-monitor.log
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

#### Testcontainers 模式（`kind: containers`）

如果你不想维护 `docker-compose.yml` 文件，可以直接在环境配置里声明容器：

```yaml
runtime:
  kind: containers
  parallel:
    slots: 4
  services:
    - name: mysql
      image: mysql:8.4
      ports:
        - "13306:3306"
      env:
        MYSQL_DATABASE: app
        MYSQL_ROOT_PASSWORD: root
      command:
        - --general_log=1
      wait_for:
        kind: log_message
        pattern: "ready for connections.*port: 3306"
        timeout_ms: 60000

    - name: redis
      image: redis:7.2-alpine
      ports:
        - "16379:6379"
      wait_for:
        kind: tcp
        port: 6379
        timeout_ms: 15000
        interval_ms: 500

  network_name: my-test-network
  cleanup: always
```

运行器会通过 Docker Engine API 直接管理容器生命周期：

1. 若本地不存在则自动拉取镜像（或从 Dockerfile 构建镜像）
2. 创建隔离的 Docker 网络（容器间可通过服务名互访）
3. 依次创建并启动容器
4. 按 `wait_for` 策略等待每个容器就绪
5. 测试完成后按 `cleanup` 策略清理

> 对只声明 `image:` 的服务，当前拉镜像语义接近 `IfNotPresent`：本地已存在就直接复用；只有 Docker 返回“本地不存在”（404）时才会 pull。

**`services` 字段说明：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `name` | string | 服务名称，同时作为网络内的 DNS 名 |
| `image` | string | Docker 镜像（与 `build` 二选一，或同时指定——`image` 作为构建后的 tag） |
| `build` | object | 从 Dockerfile 构建镜像（见下文） |
| `ports` | string[] | 端口映射，格式：`"host:container"` 或 `"container"`（自动分配 host 端口） |
| `env` | map | 环境变量 |
| `command` | string[] | 覆盖容器启动命令 |
| `volumes` | string[] | 卷挂载（Docker bind mount 格式） |
| `extra_hosts` | string[] | 额外 hosts 映射（如 `"host.docker.internal:host-gateway"`） |
| `wait_for` | object | 容器就绪等待策略（见下文） |

**`build` 构建配置：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `context` | string | 构建上下文目录（相对于项目根目录） |
| `dockerfile` | string? | Dockerfile 路径（相对于 context，默认 `Dockerfile`） |

示例——从本地源码构建应用镜像：

```yaml
services:
  - name: app
    build:
      context: .           # 项目根目录
    ports:
      - "8080:3000"
    env:
      DATABASE_URL: "mysql://app:app@mysql:3306/app"
```

> 如果同时指定了 `image` 和 `build`，构建完成后会以 `image` 的值作为镜像 tag；若省略 `image`，则自动命名为 `testrunner-{name}`。

**`wait_for` 等待策略：**

| kind | 说明 | 专属字段 |
|------|------|----------|
| `log_message` | 监听容器日志流，匹配到正则就视为就绪 | `pattern`（正则）、`timeout_ms` |
| `tcp` | 轮询 TCP 端口直到可连接 | `port`、`timeout_ms`、`interval_ms` |
| `http` | 轮询 HTTP 端点直到返回预期状态码 | `port`、`path`、`expect_status`、`timeout_ms`、`interval_ms` |

> `log_message` 是 Testcontainers 模式的核心优势——不需要额外的 readiness 声明，直接通过容器日志判断服务是否真的可用。

**动态端口映射：**

当端口格式写成 `"3306"`（只有容器端口）时，Docker 会自动分配空闲的 host 端口。运行器会将实际映射注入到 `env.variables.runtime_ports` 中，你可以在 DSL 里引用：

```yaml
variables:
  db_port: "{{ env.variables.runtime_ports.mysql.3306 }}"
```

**并行 slot（`parallel.slots`）：**

当你给 `containers` runtime 增加：

```yaml
runtime:
  kind: containers
  parallel:
    slots: 4
```

并以 `--parallel` 运行时，执行器会启动 4 套完全隔离的容器组。

- `test api` / `test dir` / `test all`：按 **case** 并行
- `test workflow --all`：按 **workflow** 并行
- 单个 workflow 内部仍保持顺序执行，不会拆成 step 级并发
- `--jobs N` 可以覆盖 `parallel.slots`

每个 slot 都会拥有自己的网络、端口映射、callback runtime 和内嵌 mock server，所以：

- 不需要修改应用代码
- 在环境、API、datasource 以及 `runtime.services[*].env` 里显式声明的固定 host 端口 / mock URL，会自动改写到当前 slot 的实际端口
- 日志会写到 `.testrunner/reports/slot-<id>/...`

#### 内嵌 mock + `runtime.services[*].env`

如果你的应用不是在 case 里显式传 `provider_base_url`，而是直接从容器进程环境变量读取第三方 / mock 地址，那么推荐把**占位 URL** 明确写在 `runtime.services[*].env`：

```yaml
runtime:
  kind: containers
  parallel:
    slots: 2
  services:
    - name: app
      build:
        context: .
      env:
        PAYMENT_PROVIDER_BASE_URL: "http://host.docker.internal:18081"
      extra_hosts:
        - "host.docker.internal:host-gateway"
```

在 `containers + --parallel + 内嵌 mock` 场景下，运行器会按下面的顺序处理：

1. 先为每个 slot 预留一个宿主机 mock 端口
2. 在容器启动前，把 `runtime.services[*].env` 里显式声明的占位 URL 改写为当前 slot 的实际 mock URL
3. 启动容器并执行 readiness
4. 再在这些预留端口上真正启动每个 slot 的 mock server
5. 同时继续把执行上下文里的环境、API、datasource、readiness URL 改写到当前 slot

这条自动改写**只覆盖 DSL 里显式声明的位置**：

- `environment.base_url`
- `environment.variables`
- `environment.readiness`
- `apis[*].base_url`
- `datasources[*].url`
- `runtime.services[*].env`

它**不会**自动扫描应用镜像内部的 `.env`、配置文件或启动脚本；如果你的应用通过文件读取 mock 地址，这属于后续“显式文件注入 / 模板渲染”能力的范围。

**两种模式对比：**

| 特性 | `docker_compose` | `containers` |
|------|-----------------|--------------|
| 需要 docker-compose.yml | ✅ | ❌ |
| 需要 docker compose CLI | ✅ | ❌（直接调用 Docker API） |
| 服务依赖顺序 | Compose 管理 | 按 `services` 声明顺序 |
| 从 Dockerfile 构建 | Compose 管理 | ✅ `build.context` |
| 日志等待（log_message） | ❌ | ✅ |
| 自动网络隔离 | Compose 管理 | ✅ 自动创建 |
| 动态端口注入 | ❌ | ✅ |
| 适合场景 | 已有 Compose 文件 | 纯声明式、CI 环境 |

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

当前支持三类来源：

- `kind: compose_service`
  - 通过 `docker compose logs` 收集服务日志
- `kind: container_file`
  - 直接从容器里复制文件，例如 MySQL 的 general log / slow log
- `kind: redis_monitor`
  - 在容器内执行 `redis-cli MONITOR`，抓取 Redis 命令流

这特别适合把“业务断言之外的环境证据”一起留档，例如：

- 应用启动日志
- MySQL general query log
- Redis command stream

## 执行时会发生什么

当你运行：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker
```

运行器的顺序大致是：

1. 加载 `env/docker.yaml`
2. 如果是 `containers + parallel + 内嵌 mock`，先为每个 slot 预留 mock 端点，并把 `runtime.services[*].env` 里的显式占位 URL 改写到对应 slot
3. 启动 runtime（`docker compose up ...` 或 Docker API 容器）
4. 逐条执行 readiness 检查
5. 如果启用了内嵌 mock，启动 1 个或 N 个 slot 级 mock，并把实际地址回写到执行上下文
6. 运行 case 或 workflow
7. 刷新 callback 队列
8. 收集 `logs:` 里声明的产物
9. 按 `cleanup` 策略回收环境
10. 把环境元数据写进最终报告

这意味着“环境是否 ready”“环境日志是否采集到”“环境回收是否成功”都会和测试结果一起进入报告。

## 报告和日志产物在哪里

真实执行结束后，环境相关信息会出现在：

- `.testrunner/reports/last-run.json`
- `.testrunner/reports/last-workflow-run.json`
- `.testrunner/reports/last-workflows-run.json`

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
- `env/redis-monitor.log`

## 实时跟随环境日志（tail -f 风格）

如果你希望在执行过程中直接盯着 MySQL / Redis / app 的输出看，可以在命令上加：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env containers --follow-env-logs
```

行为上有几个关键点：

- live follow 是 **CLI 级开关**，不会改变 `logs:` 的 artifact 收集行为
- `kind: compose_service` 会实时跟随对应服务日志
- `kind: container_file` 在 `--follow-env-logs` 下会直接从容器里 `tail -F` 目标文件，适合盯 MySQL general log 里的 SQL
- `kind: redis_monitor` 会在容器内启动 `redis-cli MONITOR`，实时输出 Redis 命令并同步写入 artifact
- 当 `stderr` 是 TTY 且未设置 `NO_COLOR` 时，live 输出会按来源着色，便于区分 MySQL、Redis 与应用日志
- 查询 / 命令级 live logs 会在 readiness 通过后开始输出，这样可以少看一大段容器启动噪音
- live logs 会输出到 `stderr`，所以即使你使用 `--report-format json`，`stdout` 里的 JSON 仍然保持机器可读
- 多 slot 并行时，控制台前缀会带上 slot 信息，方便区分是哪一套环境打出来的日志

## sample-project 推荐命令

```bash
test-runner test api system/health --root sample-projects --env docker
test-runner test workflow register-login-create-order --root sample-projects --env docker
test-runner test workflow payment-callback-flow --root sample-projects --env docker --no-mock
test-runner test workflow register-login-create-order --root sample-projects --env containers --follow-env-logs
```

## 当前边界

这套环境 DSL 当前有几个明确边界：

- 支持两种运行时：`docker_compose`（外部 Compose 文件）和 `containers`（直接 Docker API 管理）
- 默认仍是“按一次命令运行”管理；只有 `containers + parallel.slots + --parallel` 会按 slot 拉起多套隔离环境
- MySQL query log / slow log 是否开启，仍然由环境作者在 Compose / 容器配置里负责；运行器只负责采集
- `redis_monitor` 依赖容器里存在 `redis-cli`
- 如果你想在失败后保留容器排查，可以把 `cleanup` 调成 `never`
- `containers` 模式需要本机安装 Docker Engine（但不需要 `docker compose` CLI）

## 继续阅读

- [配置文件](/guide/configuration)
- [示例与最佳实践](/guide/examples)
- [工作流使用说明](/workflow/)
