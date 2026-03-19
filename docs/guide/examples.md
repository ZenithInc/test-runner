# 示例与最佳实践

这一页用仓库里已经存在的 `sample-projects/` 作为参考，展示一条最小链路、一条偏 DSL 表达式的下单链路、一条更完整的登录链路，以及一条跨 case 的 workflow。

## 最小健康检查示例

API 定义：

```yaml
# .testrunner/apis/system/health.yaml
name: Health check
method: GET
path: /health
headers:
  accept: application/json
query: {}
```

Case 定义：

```yaml
# .testrunner/cases/system/health/smoke.yaml
name: health smoke
api: system/health
tags:
  - smoke
steps:
  - request:
      api: system/health
      base_url: "${env.variables.service_base_url}"
    assert:
      - eq: [response.status, 200]
      - eq: [response.json.status, ok]
      - eq: [response.json.service, health-service]
      - contains: [response.headers.content-type, application/json]
```

执行命令：

```bash
test-runner test api system/health --root sample-projects --env docker
```

## 一个偏 DSL 表达式的下单链路

`sample-projects/.testrunner/cases/order/create/expression-happy-path.yaml` 会把 `set`、`extract`、`if`、`foreach` 和比较/长度表达式串起来验证：

```yaml
steps:
  - set:
      buyer_email: "  BUYER@example.com "
      coupon_code: SAVE10

  - request:
      api: order/create
      base_url: "${env.variables.service_base_url}"
      body:
        customer:
          name: " DSL Runner "
          email: buyer_email
          tier: vip
        items:
          - sku: SKU-BOOK
            quantity: 2
            unit_price: 4500
          - sku: SKU-PEN
            quantity: 1
            unit_price: 1200
        coupon_code: coupon_code
    extract:
      order_items: response.json.items
      subtotal: response.json.pricing.subtotal
      discount: response.json.pricing.discount
    assert:
      - eq: ["len(response.json.items)", 2]
      - gt: ["subtotal", "discount"]

  - foreach: order_items
    as: item
    steps:
      - request:
          api: system/health
          base_url: "${env.variables.service_base_url}"
        assert:
          - ge: ["item.line_total", "item.unit_price"]
```

这个例子很适合检查 DSL 的表达式求值是否符合预期，而不需要依赖 MySQL、Redis 或外部短信服务。

## 一个更完整的登录链路

`sample-projects/.testrunner/cases/user/login/happy-path.yaml` 展示了一条比较完整的集成测试流程：

1. 用 SQL 清理并插入登录用户
2. 用 Redis 删除旧验证码
3. 调用 `/send-sms-code`
4. 断言验证码已写入 Redis
5. 调用 `/login`
6. 断言 token 已写入 Redis，验证码已被消费
7. 在 `teardown` 里回收 Redis 和数据库数据

片段如下：

```yaml
steps:
  - request:
      api: user/send-sms-code
      base_url: "${env.variables.service_base_url}"
      body:
        phone: "13800000000"
        provider_base_url: "${env.variables.sms_provider_base_url}"
    extract:
      sms_code: response.json.code
    assert:
      - eq: ["response.status", 200]
      - not_empty: ["sms_code"]

  - query_redis:
      datasource: redis.main
      command: GET
      args:
        - sms:code:13800000000
    assert:
      - eq: ["result.value", "sms_code"]

  - request:
      api: user/login
      base_url: "${env.variables.service_base_url}"
      body:
        email: smoke.login@example.com
        password: P@ssw0rd123
        phone: "13800000000"
        sms_code: "{{ sms_code }}"
    extract:
      access_token: response.json.access_token
```

这个例子很适合拿来理解 `setup`、`request`、`query_redis`、`extract`、断言和 `teardown` 如何串起来工作。

这里用到的 `sample-projects/.testrunner/mocks/routes/sms-send.yaml` 现在也是一个动态 Mock 示例：它会读取 `request.json.phone` 和 `request.json.message`，再通过 `when`、`extract`、`if`、`respond` 生成短信服务响应。

## 一个跨 case 的 workflow

`sample-projects/.testrunner/workflows/register-login-create-order.yaml` 展示了一条真正跨 case 的流程：

1. `user/register/happy-path`
2. `user/send-sms-code/happy-path`
3. `workflow/user/login-after-register`
4. `workflow/order/create-after-login`

它验证了：

- register 产生的数据库副作用可以被后续 login 复用
- send-sms 导出的验证码可以通过 `exports + inputs` 传给后续 case
- login 产生的 token 副作用在 create-order 前仍然可见
- workflow 结束后，deferred teardown 会统一清理这些副作用

执行命令：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker
```

如果你要看 workflow YAML 字段和 cleanup 策略的完整说明，请继续阅读顶部导航里的「工作流」页面。

## 启动仓库内样例服务

推荐直接使用 Docker Compose：

```bash
cd sample-projects
docker compose up --build -d
```

然后回到仓库根目录执行：

```bash
test-runner test api system/health --root sample-projects --env docker
test-runner test api order/create --root sample-projects --env docker
test-runner test api user/register --root sample-projects --env docker
test-runner test api user/login --root sample-projects --env docker
test-runner test api user/send-sms-code --root sample-projects --env docker
test-runner test workflow register-login-create-order --root sample-projects --env docker
```

清理环境：

```bash
cd sample-projects
docker compose down -v
```

## 报告文件示例

每次真实执行结束后，最新报告会写到：

```text
sample-projects/.testrunner/reports/last-run.json
```

报告结构大致如下：

```json
{
  "project": "health-service",
  "environment": "docker",
  "target": "all cases",
  "summary": {
    "total": 6,
    "passed": 6,
    "failed": 0,
    "duration_ms": 110
  },
  "cases": [
    {
      "id": "system/health/smoke",
      "status": "passed"
    }
  ]
}
```

## 当前限制

在给团队推广之前，建议先明确这些边界：

- 当前按 case 串行执行，还没有并行调度。
- `report-format=junit` 尚未实现。
- `api.timeout_ms` 目前不会覆盖全局 HTTP 超时。
- Redis `key_prefix` 只是配置字段，不会自动拼接到命令参数。
- `hooks/setup` 和 `hooks/teardown` 只是预留目录，运行器还没有自动执行它们。
- Mock 已支持基于 `when` / `extract` / `set` / `if` / `respond` 的动态响应，但还不支持在 Mock 内执行 `request` / `sql` / `redis` / `query_*`。
- `sql` step 通过分号做简单拆分，复杂脚本需要先验证。
- 数据库和 Redis 会直连真实实例，建议优先使用专用测试库和测试 key 前缀。

## 推荐的落地方式

如果你准备在真实项目里落地，通常最稳妥的做法是：

1. 从 `GET /health` 这种无状态接口开始。
2. 先把 `apis/` 和 `cases/` 的命名约定统一好。
3. 用 `--dry-run` 固化选择规则。
4. 再逐步引入数据库断言、Redis 断言和 Mock。
5. 最后再扩展到登录、注册、消息发送这类跨系统流程。
