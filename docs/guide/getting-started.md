# 快速开始

`test-runner` 是一个用 Rust 编写的 CLI，目标是为以 HTTP 接口为核心的服务提供统一的集成测试入口。

文档里的命令默认写成 `test-runner ...`。如果你当前是在仓库里开发它，可以直接替换成：

```bash
cargo run -p test-runner -- <subcommand>
```

## 准备工作

开始前建议准备好下面几项：

- Rust 工具链，用来构建和运行 CLI
- 一个待测试的 HTTP 服务
- 如果用例里会访问数据库或 Redis，对应的测试实例也需要先就绪
- 如果你要直接验证仓库里的样例，`sample-projects/.testrunner/env/docker.yaml`（Docker Compose 模式）或 `containers.yaml`（Testcontainers 模式）已经可以让 `test-runner` 自动托管容器环境

## 构建 CLI

在仓库根目录执行：

```bash
cargo build -p test-runner
```

## 启动本地 Web UI

如果你希望通过页面来选择路径和参数，而不是每次手写命令，可以直接启动内置 Web UI：

```bash
test-runner web
```

默认监听 `127.0.0.1:7919`。启动后，终端会打印访问地址。

页面会提供下面这些能力：

- 输入一个目录路径，请后端返回该目录下的子目录，逐级选择 `--root`
- 读取 `.testrunner` 项目元数据，自动填充 env / api / workflow / dir 选项
- 点击执行后，实时显示 CLI 子进程的 stdout / stderr 日志

如果你需要换端口或绑定地址：

```bash
test-runner web --host 127.0.0.1 --port 7920
```

## 初始化 `.testrunner/`

在被测项目根目录下生成默认脚手架：

```bash
test-runner init --root /path/to/your-project
```

如果目标目录已经存在 `.testrunner/`，需要显式传入 `--force`：

```bash
test-runner init --root /path/to/your-project --force
```

初始化后会生成 `project.yaml`、环境文件、数据源配置、API 定义、样例用例、数据文件和可选的 Mock 模板。

## 先看执行计划

推荐先跑一次 `--dry-run`，确认 CLI 实际会选中哪些用例：

```bash
test-runner test all --root /path/to/your-project --dry-run
```

它只会打印用例选择结果，不会真正发请求，也不会生成执行报告。

## 执行测试

按 API 运行：

```bash
test-runner test api user/get-user --root /path/to/your-project
```

按目录运行：

```bash
test-runner test dir user --root /path/to/your-project
```

全量运行：

```bash
test-runner test all --root /path/to/your-project
```

如果你的环境使用 `kind: containers`，并且在 `env/*.yaml` 里声明了 `runtime.parallel.slots`，还可以直接启用 slot 并行：

```bash
# case 级并行
test-runner test dir user --root sample-projects --env containers --parallel --jobs 2

# workflow 级并行（一次运行多个 workflow）
test-runner test workflow --all --root sample-projects --env containers --parallel --jobs 2
```

其中：

- `test api` / `test dir` / `test all`：按 **case** 分配 slot
- `test workflow --all`：按 **workflow** 分配 slot
- 单个 workflow 内部的 steps 仍保持串行
- 如果应用通过容器环境变量读取 mock / provider URL，把占位地址显式写到 `runtime.services[*].env`；并行 + 内嵌 mock 时，运行器会自动改写到当前 slot 的实际端口

## 一个推荐的接入顺序

如果你是第一次给项目接入 `test-runner`，可以按这个顺序推进：

1. 用 `init` 生成基础结构。
2. 先只保留一个最小的健康检查 API 和 smoke case。
3. 用 `test all --dry-run` 确认用例选择结果。
4. 跑通单 API 的 smoke case。
5. 再逐步补上数据库、Redis、Mock 和更复杂的断言。

## 下一步推荐阅读

当你已经跑通最小 smoke case 后，通常会继续进入两个高频主题：

- 如果你要让 `test-runner` 自动拉起 Docker Compose 或 Testcontainers 容器、等待服务 ready、采集 MySQL 查询日志 / 慢日志，请继续阅读 [环境 DSL](/guide/environment-dsl)。
- 如果你要模拟“第三方稍后主动回调被测系统”，请继续阅读 [Callback](/guide/callbacks)。

## 预览这份文档站点

仓库根目录已经新增了 VitePress 项目，启动方式如下：

```bash
npm install
npm run docs:dev
```

构建静态站点：

```bash
npm run docs:build
npm run docs:preview
```
