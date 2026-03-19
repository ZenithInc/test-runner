# test-runner

`test-runner` 是一个用 Rust 编写的 CLI，用来为 **HTTP 为核心** 的服务提供统一的集成测试能力。

当前版本已经实现了下面这些基础能力：

- `init`：在目标项目根目录生成 `.testrunner/` 测试目录和样例文件
- `test`：按 `api` / `dir` / `all` 选择测试范围
- YAML DSL：描述变量、前置步骤、请求步骤、分支、循环、数据库查询、Redis 查询和断言
- 断言能力：HTTP、数据库查询结果、Redis 查询结果
- Mock：内嵌 HTTP Mock 服务，支持静态路由和轻量动态 DSL
- 报告：终端摘要输出，外加 `.testrunner/reports/last-run.json`

> 当前实现以 **串行执行** 为主，优先保证数据一致性和可重复性。

当前仓库已经按 monorepo 组织：

- `cli/`：`test-runner` CLI 本体
- `sample-projects/`：用于验证 CLI 的 Rust 样例服务
- `docs/`：基于 VitePress 的用户文档站点


## 1. 快速开始

### 1.1 构建

```bash
cargo build -p test-runner
```

开发时也可以直接使用：

```bash
cargo run -p test-runner -- <subcommand>
```


### 1.2 初始化测试目录

在被测项目根目录下生成 `.testrunner/`：

```bash
cargo run -p test-runner -- init --root /path/to/your-project
```

如果已经存在 `.testrunner/`，需要显式覆盖：

```bash
cargo run -p test-runner -- init --root /path/to/your-project --force
```


### 1.3 查看测试计划

初始化后，先用 `--dry-run` 看 CLI 实际会执行哪些用例：

```bash
cargo run -p test-runner -- test all --root /path/to/your-project --dry-run
```


### 1.4 执行测试

运行某个 API 的全部用例：

```bash
cargo run -p test-runner -- test api user/get-user --root /path/to/your-project
```

运行某个目录下的全部用例：

```bash
cargo run -p test-runner -- test dir user --root /path/to/your-project
```

运行全量：

```bash
cargo run -p test-runner -- test all --root /path/to/your-project
```

### 1.5 预览文档站点

```bash
npm install
npm run docs:dev
```

构建静态站点：

```bash
npm run docs:build
npm run docs:preview
```


## 2. 命令行说明

CLI 当前的顶层命令如下：

```text
test-runner init
test-runner test api <API_ID>
test-runner test dir <DIR>
test-runner test all
test-runner test workflow <WORKFLOW_ID>
```


### 2.1 `init`

```bash
test-runner init [OPTIONS]
```

参数：

- `--root <ROOT>`：目标项目根目录，默认 `.`。
- `--force`：覆盖已生成的模板文件。
- `--env-template <local|ci|minimal>`：初始化默认环境模板。
- `--with-mock <true|false>`：是否生成 Mock 服务模板，默认 `true`。


### 2.2 `test api`

```bash
test-runner test api [OPTIONS] <API_ID>
```

语义：

- 运行所有 `case.api == <API_ID>` 的测试用例。


### 2.3 `test dir`

```bash
test-runner test dir [OPTIONS] <DIR>
```

语义：

- 运行所有满足以下条件之一的用例：
  - 用例引用的 API ID 以 `<DIR>` 开头
  - 用例文件相对路径以 `<DIR>` 开头


### 2.4 `test all`

```bash
test-runner test all [OPTIONS]
```

语义：

- 运行整个 `.testrunner/cases/` 下发现的全部用例。
- **V1 中不包含工作流（workflow）**；工作流需要通过 `test workflow` 单独触发。


### 2.5 `test workflow`

```bash
test-runner test workflow [OPTIONS] <WORKFLOW_ID>
```

语义：

- 按顺序执行 `.testrunner/workflows/<WORKFLOW_ID>.yaml` 中定义的步骤。
- 步骤可以是 `run_case`（执行一条已有用例）或 `if/then/else` 条件分支。
- **注意**：`--tag` 和 `--case` 不适用于工作流，传入时会报错。
- 任意步骤失败都会将工作流整体标记为失败，但后续分支逻辑仍然正常执行。
- 工作流运行报告写入 `.testrunner/reports/last-workflow-run.json`。

**常用参数：**

- `--root <ROOT>`、`--env <ENV>`、`--dry-run`、`--mock`/`--no-mock`、`--report-format` 与其他子命令相同。

**干跑示例：**

```bash
test-runner test workflow auth-flow --dry-run --root /path/to/your-project
```


### 2.6 `test` 共有参数

下面这些参数适用于 `test api` / `test dir` / `test all` / `test workflow`：

- `--root <ROOT>`：被测项目根目录，默认 `.`。
- `--env <ENV>`：使用 `.testrunner/env/<ENV>.yaml` 作为环境配置。
- `--tag <TAG>`：按标签过滤（**`test workflow` 不支持此参数**）。
- `--case <CASE_PATTERN>`：按用例 ID 或名称子串过滤（**`test workflow` 不支持此参数**）。
- `--fail-fast`：首个失败后停止继续调度后续用例。
- `--dry-run`：只展示执行计划，不真正发请求。
- `--mock`：强制启用内嵌 Mock 服务。
- `--no-mock`：强制禁用内嵌 Mock 服务。
- `--report-format <summary|json|junit>`：输出格式。

说明：

- `summary`：终端输出摘要。
- `json`：终端输出 JSON，同时仍然会把报告写入 `.testrunner/reports/last-run.json`。
- `junit`：**当前已预留参数，但尚未实现**，执行时会报错。


## 3. 初始化后生成的目录

`init` 默认会生成下面这套结构：

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
    get-user-flow.yaml
  reports/
```

每个目录的职责：

- `project.yaml`：项目级默认配置
- `env/`：不同环境的 base URL、header、变量
- `datasources/`：MySQL / PostgreSQL / Redis 连接配置
- `apis/`：接口定义
- `cases/`：测试用例 DSL
- `data/`：JSON / YAML 数据文件、SQL 文件
- `mocks/`：Mock 路由和响应体
- `workflows/`：工作流定义（V1，需通过 `test workflow` 单独触发）
- `reports/`：测试报告输出目录


## 4. 配置文件说明

### 4.1 `project.yaml`

示例：

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

字段说明：

- `project.name`：项目名，只用于上下文和报告。
- `defaults.env`：默认环境名。
- `defaults.execution_mode`：当前应填写 `serial`。
- `defaults.timeout_ms`：HTTP 客户端默认超时。
- `mock.*`：内嵌 Mock 服务配置。


### 4.2 `env/<name>.yaml`

示例：

```yaml
name: local
base_url: http://127.0.0.1:3000
headers:
  x-test-env: local
variables:
  tenant: local
  mock_base_url: http://127.0.0.1:18080
```

字段说明：

- `base_url`：默认请求基地址。
- `headers`：所有请求共享的 header。
- `variables`：注入 DSL 上下文的环境变量，路径为 `env.variables.*`。


### 4.3 `datasources/*.yaml`

一个文件里可以定义多个数据源。

MySQL / PostgreSQL：

```yaml
datasources:
  mysql.main:
    kind: mysql
    url: mysql://root:password@127.0.0.1:3306/app

  postgres.analytics:
    kind: postgres
    url: postgres://postgres:password@127.0.0.1:5432/app
```

Redis：

```yaml
datasources:
  redis.cache:
    kind: redis
    url: redis://127.0.0.1:6379/0
    key_prefix: test-runner
```

说明：

- 目前 `key_prefix` 只是配置字段，**运行器暂未自动把它加到 Redis 命令参数里**。
- 你可以先在 DSL 里手动带上测试前缀。


### 4.4 `apis/*.yaml`

示例：

```yaml
name: Get user
method: GET
path: /users/{id}
headers:
  accept: application/json
query: {}
```

支持字段：

- `name`
- `method`
- `path`
- `base_url`
- `headers`
- `query`
- `body`
- `timeout_ms`

说明：

- `path` 支持 `{id}` 这种占位符，实际值由 `request.path_params` 提供。
- `base_url` 优先级低于 `request.base_url`，高于 `env.base_url`。
- `timeout_ms` 字段当前已定义在 schema 中，**但运行器还没有按 API 粒度单独使用它**。


### 4.5 `mocks/routes/*.yaml`

示例：

```yaml
method: GET
path: /profiles/u-001
status: 200
headers:
  content-type: application/json
body_file: mocks/fixtures/user-profile.json
```

上面这种静态写法仍然可用。

如果需要根据请求内容生成不同响应，可以使用增强写法：

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

当前 Mock 能力特点：

- 精确匹配 `method + path`
- 支持静态响应和动态 `respond`
- `when` 复用 case DSL 的断言语义
- `extract`、`set`、`if` 可用于生成响应上下文
- 支持 `body` 或 `body_file`
- 仍然不支持在 Mock 内执行 `request` / `sql` / `redis` / `query_*`


## 5. DSL 概览

测试用例文件位于 `cases/**/*.yaml`，基本结构如下：

```yaml
name: get-user smoke
description: optional
api: user/get-user
tags:
  - smoke
vars:
  user_id: "${data.common.users[0].id}"
setup: []
steps: []
teardown: []
```

顶层字段：

- `name`：用例名
- `description`：可选描述
- `api`：默认引用的 API ID
- `tags`：标签列表
- `vars`：初始变量
- `setup`：前置步骤
- `steps`：主体步骤
- `teardown`：后置步骤


## 6. DSL 表达式与变量规则

### 6.1 上下文对象

执行过程中可以访问这些对象：

- `env`：环境信息
- `project`：项目信息
- `case`：当前用例信息
- `api`：当前 API 信息
- `vars`：运行中变量
- `data`：`.testrunner/data/` 下加载的数据
- `response`：最近一次 `request` 的结果
- `result`：最近一次 `sql` / `redis` / `query_db` / `query_redis` / `request` 的结果

常见路径示例：

- `data.common.users[0].id`
- `response.status`
- `response.headers.content-type`
- `response.json.name`
- `result.row_count`
- `result.rows[0].status`
- `result.value`
- `env.variables.tenant`


### 6.2 两种插值语法

#### `${expr}`

用于“把整个值当成表达式求值”，会尽量保留类型。

示例：

```yaml
vars:
  user_id: "${data.common.users[0].id}"
  expected_status: "${200}"
```

如果表达式结果是数字 / 布尔 / 数组 / 对象，会以对应 JSON 类型进入上下文。


#### `{{ expr }}`

用于“把表达式嵌入字符串模板”。

示例：

```yaml
path_params:
  id: "{{ user_id }}"

query_db:
  datasource: mysql.main
  sql: "select * from users where id = '{{ user_id }}'"
```


### 6.3 裸表达式

当前实现里，某些看起来像路径的裸字符串也会自动解析。比如在 `request` / `query_db` / `query_redis` 的 `assert` 内：

```yaml
assert:
  - eq: [response.status, 200]
  - eq: [user_id, "u-001"]
```

不过为了可读性和避免歧义，**更推荐**：

- 整个值就是表达式时用 `${...}`
- 字符串拼接时用 `{{ ... }}`


### 6.4 支持的表达式能力

当前表达式引擎支持：

- 路径访问：`response.json.name`
- 数组下标：`data.common.users[0].id`
- 比较运算：`==` `!=` `>` `>=` `<` `<=`
- 长度函数：`len(response.json.roles)`
- 字面量：
  - 字符串：`"ok"` 或 `'ok'`
  - 数字：`123`、`3.14`
  - 布尔：`true` / `false`
  - 空值：`null`

示例：

```yaml
if: "${response.status == 200}"
```


## 7. DSL Step 语法

当前支持的步骤类型如下。

> 注意：当前实现里，`assert` 和 `extract` **不是独立 step**，只能作为 `request`、`query_db`、`query_redis` 的子字段使用。


### 7.1 `use_data`

把一个数据文件按相对路径加载到 `data.*` 树里。

```yaml
- use_data: common/users.json
```

说明：

- `data/` 目录下的 JSON / YAML 文件在启动时也会被自动加载。
- `use_data` 的主要作用是让测试依赖更显式。


### 7.2 `set`

设置或覆盖运行时变量。

```yaml
- set:
    expected_status: 200
    cache_key: "user:{{ user_id }}:profile"
```


### 7.3 `sql`

执行 SQL 脚本，通常用于 `setup` / `teardown`。

可以内联：

```yaml
- sql:
    datasource: mysql.main
    sql: "delete from users where id = '{{ user_id }}'"
```

也可以引用文件：

```yaml
- sql:
    datasource: mysql.main
    file: data/sql/cleanup.sql
```

执行完成后，最新结果会放到 `result`，结构类似：

```yaml
result:
  affected_rows: 1
```

注意：

- `sql` 步骤当前不带独立 `extract` / `assert` 配置。
- 如果要对数据库内容做断言，请使用 `query_db`。


### 7.4 `redis`

执行原始 Redis 命令，通常用于准备数据或清理数据。

```yaml
- redis:
    datasource: redis.cache
    command: DEL
    args:
      - "user:{{ user_id }}:profile"
```

说明：

- 当前实现是“命令透传”模式：`command + args` 直接发送给 Redis。
- 返回结果会更新到 `result`。
- 如果你要做断言，推荐使用 `query_redis`。


### 7.5 `request`

发起 HTTP 请求。

```yaml
- request:
    api: user/get-user
    path_params:
      id: "{{ user_id }}"
    query:
      verbose: true
    headers:
      x-request-id: "{{ case.id }}"
    body:
      tenant: "${env.variables.tenant}"
  extract:
    status_code: response.status
    user_name: response.json.name
  assert:
    - eq: [response.status, 200]
    - not_empty: [response.json.id]
```

字段说明：

- `api`：可选；不写时默认使用顶层 `api`
- `base_url`：可选；优先级最高
- `path_params`：用于替换 API `path` 中的 `{name}`
- `query`
- `headers`
- `body`
- `extract`
- `assert`

请求完成后，`response` / `result` 都会指向 HTTP 结果，结构为：

```yaml
response:
  status: 200
  headers:
    content-type: application/json
  body: "{\"ok\":true}"
  json:
    ok: true
```

说明：

- header key 在运行结果里会被统一转成小写。
- 如果响应体不是合法 JSON，`response.json` 会是 `null`，但 `response.body` 仍然保留原始文本。


### 7.6 `query_db`

查询数据库，并对查询结果做提取与断言。

```yaml
- query_db:
    datasource: mysql.main
    sql: "select id, status from users where id = '{{ user_id }}'"
  extract:
    db_status: result.rows[0].status
  assert:
    - eq: [result.row_count, 1]
    - eq: [db_status, active]
```

也可以：

```yaml
- query_db:
    datasource: postgres.analytics
    file: data/sql/check-user.sql
```

查询结果结构：

```yaml
result:
  row_count: 1
  rows:
    - id: u-001
      status: active
```


### 7.7 `query_redis`

查询 Redis，并对结果做提取与断言。

```yaml
- query_redis:
    datasource: redis.cache
    command: GET
    args:
      - "user:{{ user_id }}:profile"
  extract:
    cached_profile: result.value
  assert:
    - not_empty: [cached_profile]
```

返回结果会被包装成：

```yaml
result:
  value: ...
```

不同 Redis 返回值会尽量转成 JSON：

- `Nil` -> `null`
- 整数 -> number
- 字符串 -> string
- JSON 字符串 -> 尝试解析成 JSON
- Bulk -> array


### 7.8 `if`

条件分支。

```yaml
- if: "${response.json.active == true}"
  then:
    - set:
        branch_result: active
  else:
    - set:
        branch_result: inactive
```

说明：

- `then` 必填
- `else` 可选


### 7.9 `foreach`

遍历数组。

```yaml
- foreach: "${response.json.roles}"
  as: role
  steps:
    - query_redis:
        datasource: redis.cache
        command: SISMEMBER
        args:
          - "user:{{ user_id }}:roles"
          - "{{ role }}"
      assert:
        - eq: [result.value, 1]
```

说明：

- `foreach` 表达式必须解析成数组
- `as` 指定循环变量名
- 循环体里的变量可以直接通过 `role` 访问


## 8. `extract` 语法

`extract` 只能出现在 `request`、`query_db`、`query_redis` 中。

它是一个字符串映射表，左边是变量名，右边是表达式：

```yaml
extract:
  status_code: response.status
  user_name: response.json.name
  first_role: response.json.roles[0]
```

提取后，这些值会写入运行时变量，可以在后续步骤中直接用：

```yaml
assert:
  - eq: [status_code, 200]
  - not_empty: [user_name]
```


## 9. 断言语法

断言只能写在 `request`、`query_db`、`query_redis` 的 `assert:` 数组里，每项只能有一个操作符。

示例：

```yaml
assert:
  - eq: [response.status, 200]
  - contains: [response.body, "ok"]
  - not_empty: [response.json]
```

当前支持的操作符：

- `eq`
- `ne`
- `contains`
- `not_empty`
- `exists`
- `gt`
- `ge`
- `lt`
- `le`


### 9.1 `eq`

两个参数，相等断言：

```yaml
- eq: [response.status, 200]
```


### 9.2 `ne`

两个参数，不相等断言：

```yaml
- ne: [response.status, 500]
```


### 9.3 `contains`

两个参数，语义如下：

- 左值是字符串：右值必须是其子串
- 左值是数组：右值必须是数组成员
- 左值是对象：右值必须是对象键名

```yaml
- contains: [response.body, "healthy"]
- contains: [response.json.roles, "admin"]
```


### 9.4 `not_empty`

一个参数，不能是：

- `null`
- 空字符串
- 空数组
- 空对象

```yaml
- not_empty: [response.json.id]
```


### 9.5 `exists`

一个参数，只要求不是 `null`：

```yaml
- exists: [response.json]
```


### 9.6 `gt` / `ge` / `lt` / `le`

两个参数，优先按数字比较；如果无法转成数字，就退化成字符串比较。

```yaml
- gt: [result.row_count, 0]
- ge: [response.status, 200]
- lt: [response.status, 500]
```


## 10. 一个最小的 health 接口示例

如果你的服务有一个简单的 `GET /health` 接口，可以这样定义。

API：

```yaml
# .testrunner/apis/system/health.yaml
name: Health check
method: GET
path: /health
headers:
  accept: application/json
query: {}
```

Case：

```yaml
# .testrunner/cases/system/health/smoke.yaml
name: health smoke
api: system/health
tags:
  - smoke
steps:
  - request:
      api: system/health
    assert:
      - eq: [response.status, 200]
      - exists: [response.json]
```

执行：

```bash
test-runner test api system/health --root /path/to/your-project
```


## 11. 一个包含 DB / Redis 断言的示例

```yaml
name: get-user smoke
api: user/get-user
vars:
  user_id: "${data.common.users[0].id}"

setup:
  - use_data: common/users.json
  - sql:
      datasource: mysql.main
      file: data/sql/seed.sql

steps:
  - request:
      api: user/get-user
      path_params:
        id: "{{ user_id }}"
    extract:
      status_code: response.status
    assert:
      - eq: [status_code, 200]
      - not_empty: [response.json.id]

  - query_db:
      datasource: mysql.main
      sql: "select status from users where id = '{{ user_id }}'"
    extract:
      db_status: result.rows[0].status
    assert:
      - eq: [result.row_count, 1]
      - eq: [db_status, active]

  - query_redis:
      datasource: redis.cache
      command: GET
      args:
        - "user:{{ user_id }}:profile"
    extract:
      cached_profile: result.value
    assert:
      - not_empty: [cached_profile]

teardown:
  - sql:
      datasource: mysql.main
      file: data/sql/cleanup.sql
```


## 12. 报告与执行行为

当前执行行为：

- **串行**执行 case
- 单个 case 的 `setup -> steps -> teardown` 也按顺序执行
- 每次运行结束后写入：

```text
.testrunner/reports/last-run.json
```

终端摘要示例：

```text
Run finished: 2 passed, 0 failed, 2 total (report: /path/to/.testrunner/reports/last-run.json)
  [PASSED] user/create-user/happy-path (12)
  [PASSED] user/get-user/smoke (34)
```

工作流运行结果写入 `.testrunner/reports/last-workflow-run.json`，终端摘要示例：

```text
Workflow `auth-flow` finished: 2 passed, 0 failed, 2 total (report: ...)
  [PASSED] send-sms → user/send-sms-code/happy-path (85ms)
  [PASSED] login → user/login/happy-path (120ms)
```


## 13. 工作流 DSL（Workflow DSL）

工作流文件放在 `.testrunner/workflows/*.yaml`，通过 `test workflow <WORKFLOW_ID>` 触发。

> **V1 语义**：工作流不包含在 `test all` 中；需要显式运行。

### 13.1 工作流文件结构

```yaml
name: auth flow
description: 可选描述
vars:
  phone: "13800000000"
steps:
  - run_case:
      id: send-sms
      case: user/send-sms-code/happy-path
      cleanup: defer          # immediate | defer | skip
      inputs:                 # 注入到 case 的 vars（覆盖 case 自身的 vars）
        phone: "{{ workflow.vars.phone }}"
      exports:                # 从 case 执行结果提取到工作流上下文
        sms_code: vars.sms_code
  - if: "${workflow.steps.send-sms.passed}"
    then:
      - run_case:
          id: login
          case: user/login/happy-path
          cleanup: immediate
    else:
      - run_case:
          id: health-fallback
          case: system/health/smoke
```

### 13.2 `run_case` 字段

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `id` | string | 必填 | 步骤唯一标识，用于条件引用 |
| `case` | string | 必填 | 用例路径，与 `.testrunner/cases/` 下的文件路径对应（不含扩展名） |
| `inputs` | map | `{}` | 注入 case vars 的键值对，使用工作流上下文插值 |
| `exports` | map | `{}` | 从 case 执行完成后的上下文提取值，格式 `export_name: path.in.case.context` |
| `cleanup` | enum | `immediate` | 见下方 cleanup 策略 |

### 13.3 cleanup 策略

| 值 | 说明 |
|----|------|
| `immediate` | case 执行完成后立即运行 `teardown`（默认行为，与独立运行 case 一致） |
| `defer` | 延迟到整个工作流所有步骤执行完成后才运行 `teardown`，执行顺序与步骤声明顺序**相反** |
| `skip` | 跳过 `teardown` |

> **延迟 teardown**：延迟 teardown 在执行时会恢复 case 执行结束时的变量快照，确保类似 `{{ access_token }}` 的模板仍然能正确渲染。

### 13.4 工作流上下文

工作流条件表达式（`if`）和 `inputs` 插值都可以使用工作流上下文：

| 路径 | 类型 | 说明 |
|------|------|------|
| `workflow.vars.*` | any | 工作流顶层 `vars` 中的变量 |
| `workflow.steps.<id>.status` | string | `"passed"` 或 `"failed"` |
| `workflow.steps.<id>.passed` | bool | 步骤是否通过 |
| `workflow.steps.<id>.error` | string\|null | 失败时的错误信息 |
| `workflow.steps.<id>.exports.*` | any | 该步骤声明的 `exports` 提取结果 |

### 13.5 失败语义（V1）

- 某个步骤失败后，工作流**不会**中止——后续步骤和条件分支仍然执行。
- 只要有任意一个步骤失败，工作流整体状态就为 `failed`，CLI 退出码为非零。
- 通过 `if: "${workflow.steps.<id>.passed}"` 可以在 YAML 中处理失败分支。


## 14. 当前限制

为了避免误解，下面这些点需要特别注意：

- 目前是 **串行执行**，还没有做并行调度。
- `report-format=junit` 尚未实现。
- `api.timeout_ms` 只是 schema 字段，当前不会覆盖全局 HTTP 超时。
- Redis `key_prefix` 目前不会自动加到命令参数里。
- Mock 已支持基于请求上下文的动态响应，但不支持在 Mock 内发请求或访问数据库 / Redis。
- `sql` 步骤通过分号简单拆语句，复杂 SQL 脚本需要谨慎验证。
- 运行 DB / Redis 相关步骤时，CLI 会直接连接真实数据源；请优先使用专用测试库 / 测试前缀。
- 工作流 V1 **不包含在 `test all` 中**，必须通过 `test workflow <id>` 单独触发。
- `--tag` 和 `--case` 过滤参数不适用于 `test workflow`，传入时会报错。


## 15. 推荐使用流程

1. 用 `init` 生成 `.testrunner/`
2. 按项目实际接口修改 `apis/`
3. 按需要删除没用到的样例 case / mock / datasource
4. 先跑 `test all --dry-run`
5. 再跑单 API 的 smoke case
6. 最后逐步补充 DB / Redis 断言
7. 如果需要跨 case 共享状态，在 `.testrunner/workflows/` 下创建工作流 YAML 并用 `test workflow` 触发

如果你要给一个现有 HTTP 项目接入，最推荐从最简单的 `GET /health` case 开始。


## 16. 仓库内示例被测服务

仓库里现在提供了一个可运行的 Rust 示例项目：`sample-projects/`。

它包含五个接口：

```text
GET  /health
POST /orders
POST /register
POST /login
POST /send-sms-code
```

其中：

- `/health`：最小健康检查
- `/orders`：一个无外部依赖的下单接口，返回嵌套数组/对象/布尔/null/数值结构，适合验证 DSL 表达式
- `/register`：把用户写入 MySQL，并返回新建用户信息
- `/login`：校验 MySQL 中的密码哈希和短信验证码，签发 JWT，并把 token 写入 Redis
- `/send-sms-code`：调用一个被 Mock 的短信 HTTP 服务，并把短信验证码写入 Redis

### 15.1 推荐方式：用 Docker Compose 启动整个样例

```bash
cd sample-projects
docker compose up --build -d
```

启动后，可以直接在仓库根目录运行：

```bash
cargo run -p test-runner -- test api system/health --root sample-projects
cargo run -p test-runner -- test api order/create --root sample-projects
cargo run -p test-runner -- test api user/register --root sample-projects
cargo run -p test-runner -- test api user/login --root sample-projects
cargo run -p test-runner -- test api user/send-sms-code --root sample-projects
cargo run -p test-runner -- test workflow register-login-create-order --root sample-projects
```

其中 `user/login` 的 happy-path 用例现在会先调用 `/send-sms-code` 获取验证码，再带着 `email + password + phone + sms_code` 去登录；同时还会断言验证码先写入 Redis、登录成功后再被消费掉。

`register-login-create-order` 是一个 sample workflow，会依次执行：

- `user/register/happy-path`
- `user/send-sms-code/happy-path`
- `workflow/user/login-after-register`
- `workflow/order/create-after-login`

这条流程会同时验证：

- register 产生的用户副作用被后续 login 复用
- send-sms 产生的验证码副作用被后续 login 复用
- login 产生的 token 副作用在 create-order 前仍可见
- workflow 结束后，deferred teardown 会把这些副作用清理掉

停止并清理：

```bash
cd sample-projects
docker compose down -v
```

默认会把应用暴露在 `127.0.0.1:18080`，把 MySQL 暴露在 `127.0.0.1:13306`，把 Redis 暴露在 `127.0.0.1:16379`。另外，`test-runner` 的内嵌 Mock Server 会在 `18081` 启动，样例项目的默认 `docker`/`local` 环境已经分别把短信服务地址指向 `host.docker.internal:18081` 和 `127.0.0.1:18081`，所以上面的 `test-runner` 命令可以直接执行。

### 15.2 仅本地启动 Rust 服务

如果你只是想快速验证健康检查，也可以直接启动 Rust 服务：

```bash
cargo run -p health-service
```

这时如果没有设置 `DATABASE_URL`，服务仍然会启动，但 `/register` 和 `/login` 会返回 `503`；如果没有设置 `REDIS_URL`，`/login` 和 `/send-sms-code` 也会返回 `503`；如果没有可访问的短信服务地址，`/send-sms-code` 会返回 `503` 或 `502`。不过 `/health` 和 `/orders` 这两个接口不依赖外部服务，适合直接本地验证。要完整跑注册/登录/短信链路，请优先使用上面的 Docker Compose 方式，或自行准备 MySQL 与 Redis，并在需要时设置短信服务地址：

```bash
DATABASE_URL=mysql://app:app@127.0.0.1:13306/app \
REDIS_URL=redis://127.0.0.1:16379/0 \
SMS_PROVIDER_BASE_URL=http://127.0.0.1:18081 \
cargo run -p health-service
```

如果你是用这种“本地直接启动”的方式跑 `test-runner`，记得切回 `local` 环境：

```bash
cargo run -p test-runner -- test api system/health --root sample-projects --env local
```
