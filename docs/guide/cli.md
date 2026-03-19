# 命令行使用

当前 CLI 只有两个顶层命令族：

```text
test-runner init
test-runner test api <API_ID>
test-runner test dir <DIR>
test-runner test all
test-runner test workflow <WORKFLOW_ID>
```

## `init`

```bash
test-runner init [OPTIONS]
```

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--root <ROOT>` | `.` | 目标项目根目录，`.testrunner/` 会创建在这个目录下。 |
| `--force` | `false` | 覆盖已生成的模板文件。 |
| `--env-template <local\|ci\|minimal>` | `local` | 选择初始化模板。 |
| `--with-mock <true\|false>` | `true` | 是否生成 Mock 目录和模板文件。 |

### `init` 的实际行为

- 如果目标目录不存在，会直接报错。
- 如果 `.testrunner/` 已存在且没有传 `--force`，CLI 会拒绝覆盖。
- `--with-mock false` 会跳过 `mocks/` 目录及示例路由。
- 当前实现里，`ci` 模板会把默认环境切到 `ci`，并把 `env/ci.yaml` 的示例地址设成 `http://app:3000`。
- 当前实现里，`local` 和 `minimal` 生成的脚手架内容基本一致，`minimal` 还没有额外裁剪规则。

## `test api`

```bash
test-runner test api [OPTIONS] <API_ID>
```

语义很直接：运行所有满足 `case.api == <API_ID>` 的用例。

例如：

```bash
test-runner test api system/health --root sample-projects
```

## `test dir`

```bash
test-runner test dir [OPTIONS] <DIR>
```

`dir` 模式会选中满足任一条件的用例：

- 用例引用的 API ID 以前缀 `<DIR>` 开头
- 用例文件的相对路径以前缀 `<DIR>` 开头

因此 `test dir user/login` 不只依赖 API ID，也能按 `cases/` 下的目录前缀工作。

## `test all`

```bash
test-runner test all [OPTIONS]
```

运行 `.testrunner/cases/` 下发现的全部用例。

> V1 中，workflow 不包含在 `test all` 里，需要通过 `test workflow` 单独运行。

## `test workflow`

```bash
test-runner test workflow [OPTIONS] <WORKFLOW_ID>
```

执行 `.testrunner/workflows/<WORKFLOW_ID>.yaml` 对应的工作流定义。

例如：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker
```

workflow 的 YAML 结构、上下文和 cleanup 策略请看顶部导航里的「工作流」页面。

## `test` 共有参数

下面这些参数适用于 `test api`、`test dir`、`test all`、`test workflow`：

| 参数 | 说明 |
| --- | --- |
| `--root <ROOT>` | 被测项目根目录，默认 `.`。 |
| `--env <ENV>` | 使用 `.testrunner/env/<ENV>.yaml`。 |
| `--tag <TAG>` | 依据标签过滤，可重复传入。`test workflow` 不支持。 |
| `--case <CASE_PATTERN>` | 按用例 ID 或用例名的子串过滤。`test workflow` 不支持。 |
| `--fail-fast` | 首个失败后停止继续调度后续 case。 |
| `--dry-run` | 只打印执行计划，不真正运行。 |
| `--mock` | 强制启用内嵌 Mock 服务。 |
| `--no-mock` | 强制禁用内嵌 Mock 服务。 |
| `--report-format <summary\|json\|junit>` | 控制终端输出格式。 |

### 这些参数的细节

- 多次传入 `--tag` 时，当前实现是“全部匹配”语义，也就是 AND，不是 OR。
- `--case` 会同时匹配用例 ID 和用例名。
- `test workflow` 遇到 `--tag` 或 `--case` 会直接报错，而不是忽略它们。
- `--dry-run` 只展示选中的用例列表，不会写 `.testrunner/reports/last-run.json`。
- `--mock` 和 `--no-mock` 会覆盖 `project.yaml` 的 `mock.enabled`；即使开启了 mock，如果没有任何路由文件，也不会启动内嵌 Mock 服务。
- `--report-format json` 会把完整运行结果打印到 stdout，同时仍然写入 `.testrunner/reports/last-run.json`。
- `--report-format junit` 目前只是预留选项，执行时会直接报错。
- `--fail-fast` 用在 workflow 上时，会在第一个失败的 `run_case` 后停止继续调度后续 step。

## Dry run 输出长什么样

```text
Selected 2 case(s) for all cases in env `local`:
  - user/create-user/happy-path (create-user happy path)
  - user/get-user/smoke (get-user smoke)
```

## 报告与退出码

实际运行结束后，CLI 会写入：

```text
.testrunner/reports/last-run.json
```

如果运行的是 workflow，则会写入：

```text
.testrunner/reports/last-workflow-run.json
```

默认摘要输出类似这样：

```text
Run finished: 2 passed, 0 failed, 2 total (report: /path/to/.testrunner/reports/last-run.json)
  [PASSED] user/create-user/happy-path (12)
  [PASSED] user/get-user/smoke (34)
```

如果任一 case 失败，CLI 会在写完报告后返回非零退出码。

对于 workflow，V1 的默认语义是：只要任意 `run_case` 失败，整个 workflow 最终也会返回非零退出码；但你仍然可以在 YAML 中通过 `if: "${workflow.steps.<id>.passed}"` 显式处理失败分支。

## Mock 的启动规则

内嵌 Mock 服务会在下面两个条件同时满足时启动：

1. 最终的 mock 开关为启用状态
2. `.testrunner/mocks/routes/` 下至少有一条路由

Mock 服务启动后，运行器会把实际监听地址写回 `env.variables.mock_base_url`，方便在 DSL 中引用。
