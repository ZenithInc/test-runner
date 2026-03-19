# 配置文件

`.testrunner/` 里最重要的几类配置文件是 `project.yaml`、`env/*.yaml`、`datasources/*.yaml`、`apis/*.yaml` 和 `mocks/routes/*.yaml`。

## `project.yaml`

```yaml
version: 1
project:
  name: sample-http-service
defaults:
  env: local
  execution_mode: serial
  timeout_ms: 30000
mock:
  enabled: true
  host: 127.0.0.1
  port: 18080
```

### 字段说明

- `project.name`：项目名，只用于上下文和报告。
- `defaults.env`：默认环境名。
- `defaults.execution_mode`：当前实现只按串行模式执行，推荐固定写 `serial`。
- `defaults.timeout_ms`：用于构建 HTTP 客户端的全局超时。
- `mock.enabled`：是否默认开启内嵌 Mock 服务。
- `mock.host` / `mock.port`：Mock 服务监听地址。

## `env/<name>.yaml`

```yaml
name: local
base_url: http://127.0.0.1:3000
headers:
  x-test-env: local
variables:
  tenant: local
  mock_base_url: http://127.0.0.1:18080
```

### 字段说明

- `base_url`：默认请求基地址。
- `headers`：附加到所有请求的默认 header。
- `variables`：注入到 DSL 上下文的环境变量，可以通过 `env.variables.*` 访问。

### 什么时候用环境文件

如果你要在不同环境间切换 `base_url`、租户信息、下游服务地址或 Mock 地址，优先放进 `env/*.yaml`，而不是在 case 里硬编码。

## `datasources/*.yaml`

一个文件里可以定义多个数据源。

### MySQL / PostgreSQL

```yaml
datasources:
  mysql.main:
    kind: mysql
    url: mysql://root:password@127.0.0.1:3306/app

  postgres.analytics:
    kind: postgres
    url: postgres://postgres:password@127.0.0.1:5432/app
```

### Redis

```yaml
datasources:
  redis.cache:
    kind: redis
    url: redis://127.0.0.1:6379/0
    key_prefix: test-runner
```

### 注意点

- 当前实现里 `key_prefix` 只是配置字段，运行器不会自动把它拼到 Redis 命令参数里。
- 换句话说，如果你依赖测试前缀，需要在 DSL 里自己写完整 key。

## `apis/*.yaml`

```yaml
name: Get user
method: GET
path: /users/{id}
headers:
  accept: application/json
query: {}
body: null
```

### 支持字段

- `name`
- `method`
- `path`
- `base_url`
- `headers`
- `query`
- `body`
- `timeout_ms`

### 执行时的合并优先级

- `base_url`：`request.base_url` > `api.base_url` > `env.base_url`
- `headers`：环境 header 先加载，再叠加 API header，最后叠加 `request.headers`
- `query`：API 默认 query 与 `request.query` 合并，step 层同名键会覆盖 API 层
- `body`：优先使用 `request.body`，否则回落到 API 定义里的 `body`

### 目前值得知道的实现细节

- `path` 里的 `{id}` 这类占位符由 `request.path_params` 替换。
- `query` 和 `body` 会经过 DSL 值解析。
- 如果 header 需要引用变量，建议写在 `request.headers`，因为 step 层 header 会做表达式解析。
- `timeout_ms` 字段目前已经在 schema 中，但运行器还没有按 API 粒度单独应用它。

## `mocks/routes/*.yaml`

```yaml
method: POST
path: /sms/send
status: 200
headers:
  content-type: application/json
body_file: mocks/fixtures/sms-send.json
```

上面这种写法仍然有效，适合最简单的静态 Stub。

如果你需要根据请求内容做分流，或者希望响应体里引用请求数据，可以切到增强写法：

```yaml
method: POST
path: /sms/send
when:
  - contains: ["request.json.message", "verification code"]
extract:
  phone: request.json.phone
steps:
  - if: "${request.json.phone == '13800000000'}"
    then:
      - set:
          request_id: mock-sms-001
    else:
      - set:
          request_id: mock-sms-fallback
respond:
  status: 200
  headers:
    content-type: application/json
    x-mock-phone: "{{ vars.phone }}"
  body:
    accepted: true
    provider: mock-sms
    request_id: "{{ vars.request_id }}"
```

### Mock 的行为边界

- 精确匹配 `method + path`
- 按 `priority` 从高到低、再按文件名顺序做 first-match
- `when` 使用与 case DSL 相同的断言表达式
- `extract` 会把请求上下文提取到 `vars.*`
- `steps` 目前支持 `set` 和 `if`
- `respond.status`、`respond.headers`、`respond.body`、`respond.body_file` 都支持 `${...}` / <code v-pre>{{ ... }}</code>
- `body_file` 会从 `.testrunner/` 根目录读取
- 未命中时返回 `404 mock route not found`
- 当前还不支持在 mock 里执行 `request` / `sql` / `redis` / `query_*`
