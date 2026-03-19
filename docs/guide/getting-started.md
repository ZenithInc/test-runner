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
- 如果你要直接验证仓库里的样例，可以使用 `sample-projects/` 提供的 Docker Compose

## 构建 CLI

在仓库根目录执行：

```bash
cargo build -p test-runner
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

## 一个推荐的接入顺序

如果你是第一次给项目接入 `test-runner`，可以按这个顺序推进：

1. 用 `init` 生成基础结构。
2. 先只保留一个最小的健康检查 API 和 smoke case。
3. 用 `test all --dry-run` 确认用例选择结果。
4. 跑通单 API 的 smoke case。
5. 再逐步补上数据库、Redis、Mock 和更复杂的断言。

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
