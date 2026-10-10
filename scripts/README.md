# scripts

Bun + TypeScript 腳本：本地一鍵啟動（M6）、種子資料與 CI 檢查。Bun 版本鎖在根目錄 `.prototools`。不是 Cargo workspace 成員。

- `ci/check-workflows.ts`：CI 的 `workflow-policy` 檢查。每個 job 要明列 `permissions`，action 與 reusable workflow 要釘 40 字元 commit SHA，容器映像要釘 digest，不得使用 `pull_request_target` 與 `workflow_run`；規則與理由見 [CI 與發佈](../docs/architecture/ci-release.md)。執行：`bun scripts/ci/check-workflows.ts`。

腳本只用 Bun 內建的 API，執行時不需要安裝套件。`package.json` 只列型別檢查與測試用的開發相依（`typescript`、`@types/bun`），版本鎖在 `bun.lock`。以下指令都在 repo 根目錄執行：

- 安裝：`bun install --cwd scripts --frozen-lockfile`
- 型別檢查：`bun run --cwd scripts typecheck`
- 測試：`bun run --cwd scripts test`；單一測試：`bun run --cwd scripts test -t "<test-name-filter>"`
