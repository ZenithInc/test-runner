# 工作流使用说明

workflow 是 `test-runner` 在 case 之上增加的一层编排能力。

如果 case 解决的是“如何验证一个接口/一组步骤”，那么 workflow 解决的是“如何把多个 case 串成一个完整业务流程”。

典型场景：

- 先注册，再登录，再下单
- 先发短信，再根据是否成功进入不同分支
- 某个 case 的副作用需要被后续 case 复用，但最终仍然要统一清理

## 什么时候该用 workflow

推荐先把单个能力沉淀成稳定的 case，再在这些 case 之上编排 workflow。

通常可以这样划分职责：

- **case**：负责一个明确测试单元，内部可以有 `setup / steps / teardown`
- **workflow**：负责多个 case 的顺序、分支、输入输出衔接和 cleanup 策略

如果一个测试场景需要跨多个 case 共享副作用，或者你希望在成功/失败后走不同路径，就应该考虑 workflow。

## 文件位置与命令

workflow 文件放在：

```text
.testrunner/workflows/*.yaml
```

执行命令：

```bash
test-runner test workflow <WORKFLOW_ID> --root /path/to/your-project
```

例如：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker
```

::: tip V1 语义
workflow 不包含在 `test all` 中，需要通过 `test workflow` 单独触发。
:::

## 顶层结构

一个最小 workflow 看起来像这样：

```yaml
name: register login create order
description: optional
vars:
  phone: "13800000000"
steps:
  - run_case:
      id: register
      case: user/register/happy-path
      cleanup: defer

  - run_case:
      id: send-sms
      case: user/send-sms-code/happy-path
      cleanup: defer
      exports:
        sms_code: vars.sms_code

  - if: "${workflow.steps.send-sms.passed}"
    then:
      - run_case:
          id: login
          case: workflow/user/login-after-register
          inputs:
            seed_user: false
            seed_sms: false
            sms_code: "${workflow.steps.send-sms.exports.sms_code}"
          cleanup: defer
```

### 顶层字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | workflow 名称 |
| `description` | string? | 可选描述 |
| `vars` | object | workflow 级变量 |
| `steps` | array | workflow 步骤列表 |

## Step 类型

V1 只支持两类 workflow step：

- `run_case`
- `if / then / else`

### `run_case`

`run_case` 用来执行一个已经存在的 case。

```yaml
- run_case:
    id: login
    case: workflow/user/login-after-register
    cleanup: defer
    inputs:
      seed_user: false
      seed_sms: false
      sms_code: "${workflow.steps.send-sms.exports.sms_code}"
    exports:
      access_token: vars.access_token
      buyer_email: vars.email
```

#### 字段说明

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `id` | string | - | workflow 内唯一的 step ID |
| `case` | string | - | 要执行的 case ID |
| `inputs` | object | `{}` | 注入到该 case `vars` 的输入值 |
| `exports` | object | `{}` | 从该 case 运行时上下文中导出值，供后续 step 使用 |
| `cleanup` | `immediate \| defer \| skip` | `immediate` | 控制该 case 的 `teardown` 何时执行 |

### `if / then / else`

workflow 里的条件分支复用现有表达式引擎。

```yaml
- if: "${workflow.steps.login.passed}"
  then:
    - run_case:
        id: create-order
        case: workflow/order/create-after-login
        inputs:
          access_token: "${workflow.steps.login.exports.access_token}"
  else:
    - run_case:
        id: health-fallback
        case: system/health/smoke
```

说明：

- `then` 必填
- `else` 可选
- 表达式会在 workflow 上下文里求值

## workflow 上下文

workflow 运行时可以访问下面这些对象：

| 路径 | 类型 | 说明 |
| --- | --- | --- |
| `workflow.vars.*` | any | workflow 顶层 `vars` |
| `workflow.steps.<id>.status` | string | step 状态，例如 `passed` / `failed` |
| `workflow.steps.<id>.passed` | bool | step 是否通过 |
| `workflow.steps.<id>.error` | string \| null | 失败时的错误信息 |
| `workflow.steps.<id>.exports.*` | any | `exports` 提取出的值 |
| `env.*` | object | 当前环境信息 |
| `project.*` | object | 项目信息 |
| `data.*` | object | `.testrunner/data/` 加载的数据树 |

例如：

```yaml
inputs:
  sms_code: "${workflow.steps.send-sms.exports.sms_code}"

- if: "${workflow.steps.login.passed}"
```

## `inputs` 和 `exports` 怎么配合

推荐把 workflow 当成“显式传值”的编排层，而不是直接依赖某个 case 的内部细节。

常见模式：

1. 在前一个 case 里通过 `extract` 把值放进 `vars.*`
2. 在 workflow 里通过 `exports` 把需要的值导出来
3. 在后一个 case 里通过 `inputs` 注入进去

例如：

```yaml
- run_case:
    id: send-sms
    case: user/send-sms-code/happy-path
    exports:
      sms_code: vars.sms_code

- run_case:
    id: login
    case: workflow/user/login-after-register
    inputs:
      sms_code: "${workflow.steps.send-sms.exports.sms_code}"
```

这样 workflow 只依赖导出的公共契约，而不是直接耦合前一个 case 的全部运行时上下文。

## cleanup 策略

workflow 复用 case 时，最大的差异点在于 `teardown` 何时执行。

### `immediate`

默认值。该 case 跑完后立刻执行自己的 `teardown`。

适合：

- 后续 step 不依赖这个 case 的副作用
- 你希望尽快回收数据库或 Redis 状态

### `defer`

先不执行 `teardown`，而是把 teardown 加入 workflow 的 compensation stack，等 workflow 主体跑完后再逆序统一执行。

适合：

- 后续 case 需要依赖前一个 case 的副作用
- 你希望在整个流程结束后再统一清理

这是 workflow 最常见、也最关键的能力。

### `skip`

完全跳过该 case 的 `teardown`。

只建议在你非常确定不需要 teardown 时使用。大多数情况下，优先使用 `immediate` 或 `defer`。

## 失败语义（V1）

当前实现里：

- 任意 `run_case` 失败，workflow 总体会标记为失败
- 但你仍然可以在后续 `if` 里根据 `workflow.steps.<id>.passed` 决定走成功分支还是失败分支
- 如果启用 `--fail-fast`，workflow 会在第一个失败的 `run_case` 后停止继续调度后续 step

另外，`test workflow` 不支持：

- `--tag`
- `--case`

传入时会直接报错，而不是被静默忽略。

## sample-project：register -> login -> create order

仓库里的 `sample-projects/` 已经提供了一条可运行的 workflow：

```text
sample-projects/.testrunner/workflows/register-login-create-order.yaml
```

它会依次执行：

1. `user/register/happy-path`
2. `user/send-sms-code/happy-path`
3. `workflow/user/login-after-register`
4. `workflow/order/create-after-login`

这条流程验证了四件事：

- register 产生的用户副作用可以被后续 login 复用
- send-sms 产生的验证码副作用可以被后续 login 复用
- login 产生的 token 副作用在 create-order 前仍然可见
- workflow 结束后，deferred teardown 会把这些副作用统一清理掉

运行方式：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker
```

这里的 `docker` 环境已经可以由 `test-runner` 自动管理；执行命令时会先拉起 Compose、等待 readiness 通过，再运行 workflow，最后收集环境日志并回收容器。

如果你想单独了解这套环境文件的写法和报告结构，请继续阅读 [环境 DSL](/guide/environment-dsl)。

## sample-project：payment callback flow

如果你想看“先安排 callback，再由后续 case 验证副作用”的写法，仓库里还提供了：

```text
sample-projects/.testrunner/workflows/payment-callback-flow.yaml
```

它会依次执行：

1. `workflow/payment/schedule-callback`
2. `workflow/payment/assert-callback`

这条流程刻意没有新增 workflow 原生 step，而是复用 case DSL：

- 第一个 case 用 `callback + sleep` 安排并等待一次第三方回调
- workflow 用 `exports + inputs` 把 `order_no`、`expected_status` 传给下一个 case
- `cleanup: defer` 让 Redis 里的 payment status 在第二个 case 断言前保持可见

如果你想看 callback 在 case / mock / workflow 三个层面的完整对照，请继续阅读 [Callback](/guide/callbacks)。

运行方式：

```bash
test-runner test workflow payment-callback-flow --root sample-projects --env docker --no-mock
```

## Dry run 和报告

和 case 一样，workflow 也支持 `--dry-run`：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker --dry-run
```

实际执行时，workflow 报告会写到：

```text
.testrunner/reports/last-workflow-run.json
```

如果环境文件里配置了日志采集，相关产物会写到：

```text
.testrunner/reports/env/
```

终端摘要大致类似：

```text
==> Running workflow `register-login-create-order` in env `docker`
PASS [1] register -> user/register/happy-path (12ms)
PASS [2] send-sms -> user/send-sms-code/happy-path (18ms)
PASS [3] login -> workflow/user/login-after-register (25ms)
PASS [4] create-order -> workflow/order/create-after-login (10ms)

==> Summary
  Status: PASS
  Steps: 4 passed, 0 failed, 4 total
  Duration: 65ms
  Report: /path/to/.testrunner/reports/last-workflow-run.json
```

## 推荐写法

最后给几个实战建议：

1. 先把 case 稳定下来，再组合 workflow。
2. workflow 之间通过 `exports + inputs` 传值，不要默认依赖 case 的内部实现细节。
3. 只有在后续 step 真的需要前序副作用时，才使用 `cleanup: defer`。
4. 如果一个 case 既做业务动作又做强耦合清理，考虑拆成更适合 workflow 复用的 helper case。
5. 先用 `--dry-run` 看清流程，再跑真实环境。
