# 合併後 main 的樹與 PR 已測過的樹相同時略過重跑：驗收證據（2026-10-15）

Laysi/trustiot-v2#431。規則與信任論證見 [CI 與發佈](../architecture/ci-release.md) 的「依變更路徑決定 job」中「推到 main：同一棵樹已在 PR 上通過」。

## 環境

- 建置主機（x86_64 Linux），經 check queue 執行，工作目錄是分支 `ci/431-skip-main-rerun-tested-tree` 的快照；Bun 1.4.2（`.prototools`）、actionlint（`.prototools`）。
- 本機（工作站，x86_64 Linux）：Bun 1.4.2、git 2.56.0，用於 mutation 檢查與最近合併的 git 比對。
- 單元測試以拋棄式 git repository 模擬 main、PR 分支與 squash 合併，GitHub API 以注入的假資料回答；`githubReads` 的請求以替換的 `fetch` 檢查。

## 指令

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
bun scripts/ci/check-secret-handling.ts
```

最近 40 次合併的 git 比對（每個 PR 編號各做一次，`<N>` 換成編號，`<C>` 換成 main 上合併成的 commit）：

```sh
git fetch origin "refs/pull/<N>/head"
git merge-base --is-ancestor "<C>^" FETCH_HEAD && echo "P in H"
test "$(git rev-parse "<C>^{tree}")" = "$(git rev-parse "FETCH_HEAD^{tree}")" && echo "same tree"
```

以真實 API 資料唯讀查詢一個 main commit 的判斷（不寫任何東西）：

```sh
git fetch origin main
: "${GITHUB_TOKEN:?set GITHUB_TOKEN to a token that can read Laysi/trustiot-v2}"
bun scripts/ci/changes.ts --explain f1c6b93
# A merge whose pull request head lacks main's parent: the same-tree rule must not apply.
bun scripts/ci/changes.ts --explain 8534a607c9a6
```

## 結果

- 建置主機上全部檢查通過（分支併入 main `51fb7f8` 之後）：typecheck 無錯誤；`bun test scripts` 1002 pass、4 skip、0 fail（1006 個測試、40 個檔案）；workflow policy 4 個檔案無違規；actionlint 3 個 workflow 無錯誤；secret handling 419 個檔案無違規。
- `scripts/ci/changes.test.ts` 的「same tree as the merged pull request」與「what the decision reads from GitHub」兩組測試涵蓋：同一棵樹時 PR 中通過的 job 略過、PR 依路徑略過的 job 照原規則（紅燈的 main 仍執行）、找不到綠燈 main commit 時仍只略過已通過的 job；以及每個照原規則執行的情境：樹不同；PR 最後的 head 沒有執行、失敗、等待核准、被取消、還在跑、其中一次執行失敗；只算 ci.yml 以該 head 的 `pull_request` 執行，舊 head 不算；head 不含 main 的 parent、base 改過（三種事件）；parent 沒有自己的 push 執行（一次推多個 commit）、merge commit 有兩個 parent；沒有 PR、PR 未合併、合併成別的 commit、兩個 PR、base 不是本 repo 的 main、head 不合法；ci.yml 沒有或篩選 `pull_request` 觸發；六種 API 讀取失敗；job 在 PR 執行中 skipped、failure、cancelled、不存在或重複；`--explain` 的用法。另檢查決策程式只呼叫 6 個中繼資料端點與 main 的執行清單，不碰 artifact、log、check run。
- `scripts/ci/workflow-conditions.test.ts` 新增：`changes` job 的權限、ci.yml 的 `pull_request` 觸發沒有篩選、依路徑的 job 的 `name` 等於 id 且不重複、這些 job 與它們執行的程式不引用與事件有關的 context 或環境變數（Rust 快取的 `save-if` 除外）。
- Mutation 檢查：把 `scripts/ci/tested-tree.ts` 的 12 個條件逐一改成永遠不成立，每一個都讓測試失敗（[mutations.txt](data/2026-10-15-431-skip-main-rerun/mutations.txt)）。
- main 上最近 40 次合併（到 `51fb7f8`）中有 34 次符合「main 的 parent 在 PR 最後的 head 裡」與「樹相同」，包括 PR Laysi/trustiot-v2#430 合併成的 `f1c6b93`；其餘 6 次（例如 PR Laysi/trustiot-v2#421 合併成的 `8534a607c9a6`）合併前沒有把 main 併進 PR 分支，這條規則不會用上（[recent-merges.txt](data/2026-10-15-431-skip-main-rerun/recent-merges.txt)）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| main 的樹等於 PR 已通過的 merge 樹時，PR 實際執行且通過的 job 在 main 上略過，job summary 寫出依據；有測試 | 通過（單元測試） |
| 每個照現有規則執行的情境都有測試（樹不同、head 沒通過、base 無法確認、一次推多個 commit、PR 略過的 job、API 讀取失敗） | 通過（單元測試，見上） |
| 判斷不使用 PR 執行產生的任何輸出；文件寫明理由 | 通過：程式只讀中繼資料（測試檢查呼叫的端點），理由寫在 ci-release.md |
| 以 `f1c6b93`（PR Laysi/trustiot-v2#430）用真實 API 資料唯讀執行判斷，結果是略過 | 待測：以 token 呼叫 API 不在實作的權限內，由協調者以上方的 `--explain` 指令執行並補上結果（`8534a607c9a6` 應回答不適用）；git 部分（parent 在 head 裡、樹相同）已確認 |
| 決策 30 與 ci-release.md 已更新 | 通過 |
