import { defineConfig } from 'vitepress'

const repositoryName = process.env.GITHUB_REPOSITORY?.split('/')[1]
const shouldUseProjectPagesBase =
  repositoryName !== undefined && !repositoryName.toLowerCase().endsWith('.github.io')
const base =
  process.env.DOCS_BASE_PATH ??
  (process.env.GITHUB_ACTIONS === 'true' && repositoryName
    ? shouldUseProjectPagesBase
      ? `/${repositoryName}/`
      : '/'
    : '/')

export default defineConfig({
  base,
  title: 'test-runner',
  description: '面向内部 AI / Agent 生成与执行闭环的 HTTP 集成测试 CLI 文档',
  lang: 'zh-CN',
  lastUpdated: true,
  themeConfig: {
    nav: [
      { text: '快速开始', link: '/guide/getting-started' },
      { text: '命令行', link: '/guide/cli' },
      { text: 'AI / Agent', link: '/guide/schema' },
      { text: 'DSL', link: '/guide/dsl' },
      {
        text: '专题',
        items: [
          { text: '环境 DSL', link: '/guide/environment-dsl' },
          { text: 'Callback', link: '/guide/callbacks' }
        ]
      },
      { text: '工作流', link: '/workflow/' },
      { text: '示例', link: '/guide/examples' }
    ],
    sidebar: {
      '/guide/': [
        {
          text: '用户 Guide',
          items: [
            { text: '快速开始', link: '/guide/getting-started' },
            { text: '命令行使用', link: '/guide/cli' },
            { text: '面向 AI / Agent 的生成与校验', link: '/guide/schema' },
            { text: '项目结构', link: '/guide/project-structure' },
            { text: '配置文件', link: '/guide/configuration' },
            { text: '环境 DSL', link: '/guide/environment-dsl' },
            { text: 'DSL 语法', link: '/guide/dsl' },
            { text: 'Callback', link: '/guide/callbacks' },
            { text: '示例与最佳实践', link: '/guide/examples' },
            { text: '工作流使用', link: '/workflow/' }
          ]
        }
      ],
      '/workflow/': [
        {
          text: '工作流',
          items: [
            { text: '工作流使用说明', link: '/workflow/' }
          ]
        }
      ]
    },
    outline: [2, 3],
    search: {
      provider: 'local'
    },
    docFooter: {
      prev: '上一页',
      next: '下一页'
    },
    footer: {
      message: 'Built with VitePress',
      copyright: 'Copyright © 2026 test-runner'
    }
  }
})
