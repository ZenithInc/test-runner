# Callback

`callback` 让 `test-runner` 可以模拟“第三方稍后主动回调被测系统”的链路。

这套能力可以出现在三个层面：

- case 里直接安排 callback
- mock route 在收到请求后安排 callback
- workflow 把“安排 callback”和“验证 callback 副作用”拆到两个 case 里编排

如果你的业务里有支付通知、短信回执、异步审核、第三方 webhook，这一页就是对应的入口。

## callback step 的基本语义

最小写法如下：

```yaml
- callback:
    after_ms: 120
    request:
      api: callback/payment/status
      body:
        order_no: "{{ order_no }}"
        status: SUCCESS
```

这里有两个重要语义：

- callback step 成功，表示“已成功入队”
- 真正的 HTTP 投递结果，不在这个 step 当下判定，而会出现在最终报告里的 `callbacks` 列表中

也就是说，callback 不是同步 `request` 的变体，而是“先排队，后投递”的异步动作。

## 模式一：在 case 里直接安排 callback

如果你只想在单个 case 里模拟一次异步回调，可以直接使用：

```yaml
name: direct payment callback success
api: callback/payment/status
vars:
  order_no: callback-direct-001
steps:
  - callback:
      after_ms: 100
      request:
        api: callback/payment/status
        body:
          order_no: "{{ order_no }}"
          status: SUCCESS
  - sleep:
      ms: 200
  - query_redis:
      datasource: redis.main
      command: GET
      args:
        - "payment:status:{{ order_no }}"
    assert:
      - eq: ["result.value", "SUCCESS"]
```

这就是 `sample-projects/.testrunner/cases/callback/direct-payment-success.yaml` 的核心思路：

1. 用 `callback` 安排一次异步回调
2. 用 `sleep` 给回调一点落地时间
3. 用 `query_redis` / `query_db` / `request` 去验证副作用已经出现

适合场景：

- 单条 case 就能完成验证
- 你不需要把副作用保留给后续 case
- 你只是想测试“回调来了之后系统会发生什么”

## 模式二：在 mock route 里安排 callback

更贴近真实业务的方式，是让 mock 先充当第三方 provider，再由 mock 主动回调你的系统。

例如：

```yaml
method: POST
path: /payments/create
extract:
  order_no: request.json.order_no
steps:
  - set:
      request_id: "mock-payment-{{ vars.order_no }}"
  - callback:
      after_ms: 120
      request:
        api: callback/payment/status
        body:
          order_no: "{{ vars.order_no }}"
          status: SUCCESS
respond:
  status: 202
  body:
    accepted: true
    request_id: "{{ vars.request_id }}"
```

这对应 `sample-projects/.testrunner/mocks/routes/payments-create.yaml`。  
它表示：

1. 被测服务先调用 mock 的 `/payments/create`
2. mock 同步返回 `202`
3. mock 额外安排一条稍后投递的 callback

然后对应的 case 只需要验证“同步响应 + 异步副作用”：

```yaml
steps:
  - request:
      api: payment/provider/create
      base_url: "${env.variables.service_base_url}"
      body:
        order_no: "{{ order_no }}"
        provider_base_url: "${env.variables.payment_provider_base_url}"
    assert:
      - eq: ["response.status", 202]
  - sleep:
      ms: 300
  - query_redis:
      datasource: redis.main
      command: GET
      args:
        - "payment:status:{{ order_no }}"
    assert:
      - eq: ["result.value", "SUCCESS"]
```

这种方式更适合模拟：

- 第三方同步受理
- 第三方稍后异步通知
- 你的服务在 callback 后更新数据库 / Redis / 状态机

## 模式三：在 workflow 里拆成两个 case

如果你想把“安排 callback”和“验证 callback 结果”拆开，可以直接用 workflow：

```yaml
name: payment callback flow
steps:
  - run_case:
      id: schedule-callback
      case: workflow/payment/schedule-callback
      cleanup: defer
      exports:
        order_no: vars.order_no
        expected_status: vars.callback_status
  - run_case:
      id: verify-callback
      case: workflow/payment/assert-callback
      cleanup: immediate
      inputs:
        order_no: "${workflow.steps.schedule-callback.exports.order_no}"
        expected_status: "${workflow.steps.schedule-callback.exports.expected_status}"
```

这就是 `sample-projects/.testrunner/workflows/payment-callback-flow.yaml` 的核心结构。

它体现了三件事：

- callback 本身仍然是 case DSL，不需要发明 workflow 专属 callback step
- workflow 用 `exports + inputs` 把回调上下文传给后续 case
- `cleanup: defer` 让中间副作用在第二个 case 断言前保持可见

执行命令：

```bash
test-runner test workflow payment-callback-flow --root sample-projects --env docker --no-mock
```

## 报告里怎么看 callback 是否真正成功

callback 的最终投递结果会出现在：

- 终端 summary 输出的 `Callbacks` 小节
- `last-run.json` / `last-workflow-run.json` / `last-workflows-run.json` 里的 `callbacks` 数组

你会看到类似这样的信息：

```json
{
  "callbacks": [
    {
      "id": 1,
      "status": "passed",
      "source": "case:workflow/payment/schedule-callback",
      "url": "http://127.0.0.1:18080/callbacks/payments/status"
    }
  ]
}
```

因此排查 callback 时，一般建议同时看两层：

- 业务断言有没有通过（Redis / DB / HTTP 查询）
- callback 投递本身有没有成功（`callbacks` 报告）

## 实战建议

- callback request 会在“入队时”先完成模板解析，所以依赖的变量应该在 step 执行前就准备好
- 需要观察异步副作用时，优先用 `sleep + query_redis / query_db / request`
- 如果 callback 是直接打到真实服务，而不是通过 mock 间接触发，记得按场景决定是否传 `--no-mock`
- 如果你想保留更多环境侧证据（例如 MySQL 日志），可以把 callback 测试和 [环境 DSL](/guide/environment-dsl) 组合使用

## sample-project 推荐命令

```bash
test-runner test dir callback --root sample-projects --env docker --case direct-payment-success
test-runner test api payment/provider/create --root sample-projects --env docker
test-runner test workflow payment-callback-flow --root sample-projects --env docker --no-mock
```

## 继续阅读

- [DSL 语法](/guide/dsl)
- [环境 DSL](/guide/environment-dsl)
- [工作流使用说明](/workflow/)
