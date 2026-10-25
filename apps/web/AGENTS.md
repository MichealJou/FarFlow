# Prototype Instructions

Run the local server yourself and open the preview in the browser available to this environment. Do not give the user server-start instructions when you can run it.

Before making substantial visual changes, use the Product Design plugin's `get-context` skill when the visual source is unclear or no longer matches the current goal. When the user gives durable prototype-specific design feedback, preferences, or decisions, record them in `AGENTS.md`.

When implementing from a selected generated mock, treat that image as the source of truth for layout, component anatomy, density, spacing, color, typography, visible content, and hierarchy.

Build app UI in `src/`. Keep `.openai/hosting.json`, `worker/index.js`, `scripts/prepare-sites-build.mjs`, and `tests/sites-worker.test.mjs` intact so the same local prototype can be handed to Sites. Before a Sites handoff, run `npm run build` and `npm run test:sites`; the build must leave `dist/client/index.html`, `dist/server/index.js`, and `dist/.openai/hosting.json`.

## 已确认的产品约束

- 共用业务界面位于 `packages/ui`，当前目录仅保留浏览器启动入口。桌面与浏览器复用同一套 React 组件。
- 用户确认按名字使用 `design/previews/v3-03-浅灰钴蓝.png`；其他十一套主题可切换，偏好属于设备，不进入业务数据和 Git 历史。
- 本地 SQLite 随 Tauri 打包；Git 由桌面访问本机仓库；服务为独立 Rust + Axum + PostgreSQL。不能把 Tauri 引入无界面服务端。
- 真实业务使用正常表单、独立动作、评论附件和真实账户。不得用示例数据、假登录或仅内存保存冒充可用版本。
