# 命令行使用

当前 CLI 的顶层命令如下：

```text
test-runner init
test-runner schema [KIND]
test-runner web
test-runner test api <API_ID>
test-runner test dir <DIR>
test-runner test all
test-runner test workflow <WORKFLOW_ID>
test-runner test workflow --all
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

## `web`

```bash
test-runner web [OPTIONS]
```

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--host <HOST>` | `127.0.0.1` | 本地 Web UI 绑定地址。 |
| `--port <PORT>` | `7919` | 本地 Web UI 绑定端口。 |

`web` 会启动一个本地 HTTP 服务，并提供一个简单页面用于：

- 输入目录路径，请后端返回当前路径下的子目录列表，逐级选择 `--root`
- 读取 `.testrunner` 项目元数据，自动填充 env / api / workflow / dir 选项
- 点击执行后，在页面中实时查看 CLI 子进程的 stdout / stderr 输出

它适合本机单用户调试场景，不带认证，也不会替换现有 CLI 行为；页面背后仍然是当前的 `test-runner test ...` 命令。

默认地址是 `127.0.0.1`，也就是只监听本机回环地址；如果你需要从别的机器访问，再显式改成 `0.0.0.0` 或其他网卡地址。

## `schema`

```bash
test-runner schema [KIND] [--output <PATH>]
```

`schema` 用来生成 DSL / 配置文件的 JSON Schema，适合给 AI Agent、编辑器插件或外部校验器使用。

支持的 `KIND`：

- `all`（默认）
- `project`
- `environment`
- `datasources`
- `api`
- `case`
- `workflow`
- `mock-route`

常见用法：

```bash
# 输出全部 schema 的 JSON 对象到 stdout
test-runner schema

# 只输出 case schema
test-runner schema case

# 批量写入一个目录
test-runner schema all --output .testrunner/schema
```

行为细节：

- 不传 `--output` 时，结果打印到 stdout。
- `schema all --output <DIR>` 会把每种 schema 写成单独的 `*.schema.json` 文件。
- `schema <kind> --output <FILE>` 会写单个 schema 文件；如果 `--output` 指向目录，则会自动使用 `<kind>.schema.json`。
- JSON Schema 只描述“文件结构”和“字段约束”；表达式、作用域和运行时语义还要配合 [Schema 与 Agent 校验](/guide/schema)、[DSL 语法](/guide/dsl) 和 [工作流](/workflow/) 文档一起看。

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
test-runner test workflow [OPTIONS] --all
```

执行一个指定 workflow，或者通过 `--all` 一次运行 `.testrunner/workflows/` 下的全部 workflow。

例如：

```bash
test-runner test workflow register-login-create-order --root sample-projects --env docker
test-runner test workflow --all --root sample-projects --env containers --parallel --jobs 2
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
| `--parallel` | 启用并行调度。要求环境使用 `kind: containers`，并具备 slot 隔离能力。 |
| `--jobs <N>` | 指定并行 jobs / slots 数量；可覆盖环境里的 `parallel.slots`。 |
| `--dry-run` | 只打印执行计划，不真正运行。 |
| `--mock` | 强制启用内嵌 Mock 服务。 |
| `--no-mock` | 强制禁用内嵌 Mock 服务。 |
| `--follow-env-logs` | 运行过程中实时跟随 `env/*.yaml` 里声明的环境日志来源，并输出到 `stderr`。 |
| `--report-format <summary\|json>` | 控制终端输出格式。 |

### 这些参数的细节

- 多次传入 `--tag` 时，当前实现是“全部匹配”语义，也就是 AND，不是 OR。
- `--case` 会同时匹配用例 ID 和用例名。
- `test workflow` 遇到 `--tag` 或 `--case` 会直接报错，而不是忽略它们。
- `test workflow --all` 会列出被选中的 workflow 集合；不带 `--all` 时只展示单条 workflow 的 steps。
- `--parallel` 目前只支持 `containers` runtime；`docker_compose` 不支持自动 slot 隔离。
- `--jobs N` 可以单独使用；它本身就会请求并行调度。如果不传，则在并行模式下回退到 `runtime.parallel.slots`。
- `--dry-run` 不会写 `.testrunner/reports/last-run.json`、`last-workflow-run.json` 或 `last-workflows-run.json`。
- `--mock` 和 `--no-mock` 会覆盖 `project.yaml` 的 `mock.enabled`；即使开启了 mock，如果没有任何路由文件，也不会启动内嵌 Mock 服务。
- `--report-format json` 会把完整运行结果打印到 stdout，同时仍然写入对应的报告文件。
- `--follow-env-logs` 不会替代 `logs:` 的 artifact 收集；它只是额外把 live 输出打到 `stderr`，所以和 `--report-format json` 一起用时，`stdout` 里的 JSON 仍然保持机器可读。
- 当前公开可用的终端输出格式只有 `summary` 和 `json`；最近实现已经移除了 JUnit 输出格式。
- `--fail-fast` 用在 workflow 上时，会在第一个失败的 `run_case` 后停止继续调度后续 step。
- 并行模式下，`test api` / `test dir` / `test all` 按 **case** 分 slot；`test workflow --all` 按 **workflow** 分 slot。

## Dry run 输出长什么样

```text
Selected 2 case(s) for all cases in env `local`:
  - user/create-user/happy-path (create-user happy path)
  - user/get-user/smoke (get-user smoke)
```

## 报告与退出码

实际运行 case 后，CLI 会写入：

```text
.testrunner/reports/last-run.json
```

如果运行的是单个 workflow，则会写入：

```text
.testrunner/reports/last-workflow-run.json
```

如果运行的是 `test workflow --all`，则会写入：

```text
.testrunner/reports/last-workflows-run.json
```

默认摘要输出类似这样：

```text
==> Running 2 case(s) for all cases in env `local`
PASS [1/2] user/create-user/happy-path (12ms)
PASS [2/2] user/get-user/smoke (34ms)

==> Summary
  Cases: 2 passed, 0 failed, 2 total
  Duration: 46ms
  Report: /path/to/.testrunner/reports/last-run.json
```

如果这次运行还包含 callback 或环境托管，摘要里还会追加 `Callbacks` / `Environment` 小节，分别汇总回调投递结果和环境启动、readiness、日志采集、回收状态。

如果任一 case 失败，CLI 会在写完报告后返回非零退出码。

对于 workflow，V1 的默认语义是：只要任意 `run_case` 失败，整个 workflow 最终也会返回非零退出码；但你仍然可以在 YAML 中通过 `if: "${workflow.steps.<id>.passed}"` 显式处理失败分支。

对于 `test workflow --all --parallel`，最终退出码按 batch 结果决定：只要任意 workflow 失败，整条命令就会返回非零退出码。

## Mock 的启动规则

内嵌 Mock 服务会在下面两个条件同时满足时启动：

1. 最终的 mock 开关为启用状态
2. `.testrunner/mocks/routes/` 下至少有一条路由

Mock 服务启动后，运行器会把实际监听地址写回 `env.variables.mock_base_url`，方便在 DSL 中引用。
