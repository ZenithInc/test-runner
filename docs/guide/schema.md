# Schema 与 Agent 校验

如果你的用例、workflow 或配置文件主要由 AI Agent 生成，推荐把 **JSON Schema 校验** 放到真正执行之前。

`test-runner` 现在提供了一个一等命令：

```bash
test-runner schema
```

默认会输出一个 JSON 对象，里面包含当前版本可用的全部 schema 文档：

- `project`
- `environment`
- `datasources`
- `api`
- `case`
- `workflow`
- `mock-route`

## 生成单个 schema

如果你只想让 Agent 读取某一种文件的约束：

```bash
test-runner schema case
test-runner schema workflow
test-runner schema api
```

默认输出到 stdout，适合直接喂给 Agent、验证器或上层编排脚本。

也可以显式写入文件：

```bash
test-runner schema case --output /tmp/case.schema.json
```

## 批量导出 schema 文件

如果你希望在仓库里落一份机器可读的 schema 目录：

```bash
test-runner schema all --output .testrunner/schema
```

它会写出：

```text
.testrunner/schema/project.schema.json
.testrunner/schema/environment.schema.json
.testrunner/schema/datasources.schema.json
.testrunner/schema/api.schema.json
.testrunner/schema/case.schema.json
.testrunner/schema/workflow.schema.json
.testrunner/schema/mock-route.schema.json
```

## 推荐的 Agent 工作流

如果 DSL 由 AI 自动生成，推荐按这个顺序：

1. 调用 `test-runner schema ...` 读取对应 schema
2. 先做本地 JSON Schema 校验
3. 再运行 `test-runner test ... --dry-run`
4. 真正执行时使用 `--report-format json`

这样 Agent 会先拿到**结构约束**，再拿到**选择计划**，最后拿到**运行结果**，比直接靠自然语言猜 DSL 可靠得多。

## Schema 覆盖什么，不覆盖什么

JSON Schema 主要负责：

- 文件顶层字段
- step / workflow step 的允许形状
- 断言操作符与参数个数
- 配置枚举值、对象结构和必填字段

JSON Schema **不负责** 完整表达运行期语义，例如：

- `${...}` / <code v-pre>{{ ... }}</code> / 裸表达式的求值时机
- `extract` 必须写“原始表达式”而不是包裹形式
- `workflow.vars`、`inputs`、`exports` 的作用域和覆盖顺序

这些规则请继续看：

- [DSL 语法](/guide/dsl)
- [工作流使用说明](/workflow/)
