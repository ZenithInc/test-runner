# DSL 语法

`test-runner` 的用例 DSL 使用 YAML 编写，文件位置约定为：

```text
.testrunner/cases/**/*.yaml
```

## Case 顶层结构

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

### 顶层字段

- `name`：用例名
- `description`：可选描述
- `api`：默认 API ID
- `tags`：标签列表
- `vars`：初始变量
- `setup`：前置步骤
- `steps`：主体步骤
- `teardown`：后置步骤

## 执行上下文

执行过程中可以访问这些对象：

| 对象 | 说明 |
| --- | --- |
| `env` | 当前环境配置，包含 `name`、`base_url`、`headers`、`variables` |
| `project` | 项目信息，例如 `project.name` |
| `case` | 当前用例信息，例如 `case.id`、`case.name`、`case.api` |
| `api` | 当前 API 信息，例如 `api.id`、`api.method`、`api.path` |
| `vars` | 运行中的变量 |
| `data` | `.testrunner/data/` 自动加载的数据树 |
| `response` | 最近一次 `request` 的结果 |
| `result` | 最近一次 `sql`、`redis`、`query_db`、`query_redis` 或 `request` 的结果 |

常见路径示例：

- `data.common.users[0].id`
- `env.variables.tenant`
- `response.status`
- `response.json.name`
- `result.row_count`
- `result.rows[0].status`
- `result.value`

## 值解析规则

### `${expr}`：整个值就是表达式

```yaml
vars:
  user_id: "${data.common.users[0].id}"
  expected_status: "${200}"
```

这种写法会尽量保留结果类型。数字仍然是数字，布尔仍然是布尔，对象和数组也不会退化成字符串。

### 双花括号模板

语法写作 <code v-pre>{{ expr }}</code>，用于把表达式嵌入字符串模板。

```yaml
path_params:
  id: "{{ user_id }}"

query_db:
  datasource: mysql.main
  sql: "select * from users where id = '{{ user_id }}'"
```

模板渲染的最终结果总是字符串。

### 裸表达式

当前实现里，如果一个字符串看起来像路径访问，或者能命中上下文对象 / 变量，也会自动解析：

```yaml
assert:
  - eq: [response.status, 200]
  - eq: [user_id, "u-001"]
```

这是为什么很多断言里可以直接写 `response.status`，不用再包 `${...}`。

::: tip 建议
为了减少歧义，整值求值优先用 `${...}`，字符串拼接用 <code v-pre>{{ ... }}</code>，断言和 `extract` 里再使用裸表达式。
:::

### `if` 的 truthy 规则

条件分支里的表达式会按下面的 truthy 规则判断：

- `false`、`null`、数字 `0`、空字符串、空数组、空对象 -> false
- 其他值 -> true

## 支持的表达式能力

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

例如：

```yaml
- if: "${response.status == 200}"
  then:
    - set:
        branch_result: ok
```

## Step 类型

> `extract` 和 `assert` 不是独立 step，只能挂在 `request`、`query_db`、`query_redis` 下。

### `use_data`

把 `data/` 下的 JSON/YAML 文件加载到 `data.*` 树里：

```yaml
- use_data: common/users.json
```

`common/users.json` 会变成 `data.common.users`。

### `set`

设置或覆盖运行时变量：

```yaml
- set:
    expected_status: 200
    cache_key: "user:{{ user_id }}:profile"
```

右侧值会先经过 DSL 解析，再写入 `vars.*`。

### `sql`

执行 SQL 脚本，常用于 `setup` 或 `teardown`：

```yaml
- sql:
    datasource: mysql.main
    sql: "delete from users where id = '{{ user_id }}'"
```

或：

```yaml
- sql:
    datasource: mysql.main
    file: data/sql/cleanup.sql
```

要求二选一提供 `sql` 或 `file`。执行结果会写到：

```yaml
result:
  affected_rows: 1
```

### `redis`

透传 Redis 命令：

```yaml
- redis:
    datasource: redis.cache
    command: DEL
    args:
      - "user:{{ user_id }}:profile"
```

返回值会写到 `result`。如果要做断言，更推荐使用 `query_redis`。

### `request`

发起 HTTP 请求：

```yaml
- request:
    api: user/get-user
    base_url: "${env.variables.service_base_url}"
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

- `api`：可选，不写时默认使用 case 顶层 `api`
- `base_url`：可选，优先级最高
- `path_params`：替换 API `path` 中的 `{name}`
- `query`
- `headers`
- `body`
- `extract`
- `assert`

执行后，`response` 和 `result` 都会指向 HTTP 结果：

```yaml
response:
  status: 200
  headers:
    content-type: application/json
  body: "{\"ok\":true}"
  json:
    ok: true
```

### `query_db`

查询数据库并做提取 / 断言：

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

也支持 `file`：

```yaml
- query_db:
    datasource: postgres.analytics
    file: data/sql/check-user.sql
```

返回结构：

```yaml
result:
  row_count: 1
  rows:
    - id: u-001
      status: active
```

### `query_redis`

查询 Redis 并做提取 / 断言：

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

Redis 的 `Nil`、整数、字符串、JSON 字符串、Bulk 返回值都会尽量转换成 JSON 友好的值。

### `if`

条件分支：

```yaml
- if: "${response.json.active == true}"
  then:
    - set:
        branch_result: active
  else:
    - set:
        branch_result: inactive
```

- `then` 必填
- `else` 可选
- 条件可以写 `${...}`，也可以直接写裸表达式

### `foreach`

遍历数组：

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

- `foreach` 表达式必须最终解析成数组
- `as` 定义循环变量名
- 循环体里可以直接访问 `role`

## `extract`

`extract` 只能出现在 `request`、`query_db`、`query_redis` 中，格式是“变量名 -> 表达式”：

```yaml
extract:
  status_code: response.status
  user_name: response.json.name
  first_role: response.json.roles[0]
```

提取后的值会写入运行时变量，可以在后续步骤里直接使用。

::: warning
`extract` 里的右值应当写原始表达式，例如 `response.status`。当前实现不会自动去掉 `${...}` 外层包装。
:::

## `assert`

断言只能写在 `request`、`query_db`、`query_redis` 的 `assert:` 数组里，每项只能包含一个操作符。

```yaml
assert:
  - eq: [response.status, 200]
  - contains: [response.body, "ok"]
  - not_empty: [response.json]
```

### 支持的断言操作符

| 操作符 | 参数个数 | 语义 |
| --- | --- | --- |
| `eq` | 2 | 相等 |
| `ne` | 2 | 不相等 |
| `contains` | 2 | 字符串包含 / 数组包含成员 / 对象包含键 |
| `not_empty` | 1 | 不是 `null`、空字符串、空数组、空对象 |
| `exists` | 1 | 不是 `null` |
| `gt` / `ge` / `lt` / `le` | 2 | 优先按数字比较，失败后退化成字符串比较 |

### 断言示例

```yaml
assert:
  - eq: [response.status, 200]
  - contains: [response.headers.content-type, application/json]
  - not_empty: [response.json.id]
  - exists: [response.json]
  - gt: [result.row_count, 0]
```

## Mock 路由 DSL

Mock 路由和 case 不是同一种 DSL，但它们现在共用同一套表达式、模板和断言语义。

文件位置：

```text
.testrunner/mocks/routes/**/*.yaml
```

### 可用上下文

| 对象 | 说明 |
| --- | --- |
| `request` | 当前进入 mock 的请求，包含 `method`、`path`、`query`、`headers`、`body`、`json` |
| `route` | 当前命中的 mock 路由，包含 `method`、`path`、`priority` |
| `env` | 当前环境配置 |
| `project` | 项目信息 |
| `data` | `.testrunner/data/` 自动加载的数据树 |
| `vars` | mock 路由运行时变量 |

### 顶层字段

- `method` / `path`：基础路由匹配
- `priority`：可选，数值越大越优先
- `when`：可选，请求命中条件，语法与 case 的 `assert` 一致
- `extract`：可选，把请求上下文提取为变量
- `steps`：可选，目前支持 `set` 和 `if`
- `respond`：可选，定义动态响应

### 示例

```yaml
method: POST
path: /sms/send
priority: 10
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

### 兼容性与限制

- 老的静态写法 `status + headers + body/body_file` 仍然可用
- `respond.status`、`respond.headers`、`respond.body`、`respond.body_file` 都支持 `${...}` / <code v-pre>{{ ... }}</code>
- `steps` 目前不支持 `request`、`sql`、`redis`、`query_db`、`query_redis`、`foreach`
