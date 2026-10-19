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

以真實 API 資料唯讀查詢 main commit 的判斷（不寫任何東西；只有 GET），每個 commit 的輸出存成 data 目錄的 `explain-<commit>.txt`：

```sh
git fetch origin main
: "${GITHUB_TOKEN:?set GITHUB_TOKEN to a token that can read Laysi/trustiot-v2}"
for c in f1c6b93 8534a607c9a6 51fb7f8 66f90ccb961f 385cf22be6ad e944c0baa9a6 86b52f7dfff7 6701c79; do
  bun scripts/ci/changes.ts --explain "$c" > "docs/test-reports/data/2026-10-15-431-skip-main-rerun/explain-$c.txt"
done
```

## 結果

- 建置主機上全部檢查通過（分支 rebase 到 main `4a89f2a` 之後）：typecheck 無錯誤；`bun test scripts` 1049 pass、4 skip、0 fail（1053 個測試、40 個檔案；含 Rust 編譯快取 key 的規則與獨立驗證後的修正）；workflow policy 4 個檔案無違規；actionlint 3 個 workflow 無錯誤；secret handling 422 個檔案無違規。
- `scripts/ci/changes.test.ts` 的「same tree as the merged pull request」與「what the decision reads from GitHub」兩組測試涵蓋：同一棵樹時 PR 中通過的 job 略過、PR 依路徑略過的 job 照原規則（紅燈的 main 仍執行）、找不到綠燈 main commit 時仍只略過已通過的 job；以及每個照原規則執行的情境：樹不同；PR 最後的 head 沒有執行、失敗、等待核准、被取消、還在跑、其中一次執行失敗；只算 ci.yml 以該 head 的 `pull_request` 執行，舊 head 不算；head 不含 main 的 parent、base 改過（三種事件）；parent 沒有自己的 push 執行（一次推多個 commit）、merge commit 有兩個 parent；沒有 PR、PR 未合併、合併成別的 commit、兩個 PR、base 不是本 repo 的 main、head 不合法；ci.yml 沒有或篩選 `pull_request` 觸發；九種 API 讀取失敗；job 在 PR 執行中 skipped、failure、cancelled、不存在或重複；`--explain` 的用法。另檢查決策程式只呼叫 8 個中繼資料端點與 main 的執行清單，不碰 artifact、log、check run。
- `scripts/ci/workflow-conditions.test.ts` 新增：`changes` job 的權限、ci.yml 的 `pull_request` 觸發沒有篩選、依路徑的 job 的 `name` 等於 id 且不重複、這些 job 與它們執行的程式不引用與事件有關的 context 或環境變數（Rust 快取的 `save-if` 除外）。
- Rust 編譯快取：合併改到任何 `Cargo.lock`、`Cargo.toml`、根目錄的 `rust-toolchain(.toml)`、根目錄 `.cargo/`、`ci.yml` 或 `.github/actions/` 時，`rust` 在 main 執行以存快取，其他已通過的 job 仍略過；巢狀的 `.cargo/`、toolchain 檔與其他 workflow 不算；`workflow-conditions.test.ts` 檢查 rust-cache 只有一個步驟、`with` 只有 `cache-bin`、`save-if`，以及工具鏈是確切的版本。`--explain` 印一般的行，不印 `::notice` 前綴。
- 獨立驗證的修正（見下節）各有測試：`cargo-deny` 一律照原規則；main 停過不在 C 歷史裡的 commit（含本機沒有的 commit）、列出的 push 執行不夠早、force push 在 GitHub 建立 P 前一小時內或之後（含 PR 執行晚了許多小時才建立的情況）、P 不是 GitHub 建立並簽章的；job 在較早的 attempt 失敗或被取消、最新 attempt 沒有該 job；執行的 head 分支或 head repository 不是這個 PR 的、head 分支有其他 PR 或查不到 PR、head repository 已刪除；以及驗證者的兩個情境（force push 回 A 後 squash、PR 沒有自己的執行）。
- Mutation 檢查：把 `scripts/ci/tested-tree.ts` 的條件（最初 12 個，加上獨立驗證後新增的 9 個，與 Codex 檢查後改成以 P 的建立時間為起點的條件與 P 的檢查），以及 `changes.ts` 的快取 key 規則（3 處）、`cargo-deny` 的例外與 `--explain` 的輸出逐一改壞，每一個都讓測試失敗（[mutations.txt](data/2026-10-15-431-skip-main-rerun/mutations.txt)）。
- main 上最近 40 次合併（到 `51fb7f8`）中有 34 次符合「main 的 parent 在 PR 最後的 head 裡」與「樹相同」，包括 PR Laysi/trustiot-v2#430 合併成的 `f1c6b93`；其餘 6 次（例如 PR Laysi/trustiot-v2#421 合併成的 `8534a607c9a6`）合併前沒有把 main 併進 PR 分支，這條規則不會用上（[recent-merges.txt](data/2026-10-15-431-skip-main-rerun/recent-merges.txt)）。

## 獨立驗證

2026-10-15 另一個 agent 以 `2d23907` 為對象嘗試推翻信任論證，結論是核心論證成立，另提出六項；協調者以 API 確認了各項的實例。處理方式：

| 項目 | 內容 | 處理 |
| --- | --- | --- |
| F1 | `cargo-deny` 的結果隨時間改變（RustSec 資料庫）。實例：PR Laysi/trustiot-v2#411 的 PR 執行在 2026-10-14T10:04Z 通過 `cargo-deny`，15:54Z 合併成 `8e29d03`，main 的 push 執行的 `cargo-deny` 因 RUSTSEC-2026-0346 到 0349（rustls）失敗，促成 Laysi/trustiot-v2#419 | 修正：`cargo-deny` 從不套用同一棵樹的略過（`TIME_DEPENDENT_JOBS`），在 main 照原規則；有測試 |
| F2 | main 不再為同一棵樹提供第二次獨立的執行。實例：PR Laysi/trustiot-v2#305 的 PR 執行 `rust` 通過，main 上 `1035426` 的 push 執行 `rust` 失敗（`a_slow_database_connect_never_fails_a_check_after_ready`），Laysi/trustiot-v2#343 修了它指出的真的 bug | 是取捨，不改程式；寫進 ci-release.md 的「已知的取捨」與決策 30 |
| F3 | repo 沒有 ruleset 或分支保護，force push main 會讓條件 4 的前提失效（main A→B，PR 測了併進 B，force push 回 A，再 squash 到 A） | 修正（兩種證據都要過，失敗就不略過）：從 GitHub 建立 P 前一小時起（P 必須是 GitHub 建立並簽章的 commit；H 含有 P，所以 PR 的執行測的 merge commit 都在這之後），main 的每個 push 執行都必須在 C 的歷史裡，且列出的執行要早於這個起點；activity API（`contents: read`，不需新權限）在這段期間沒有 force push；有測試。起點最初取自最早的 PR 執行，提交前的 Codex 檢查指出 GitHub 建立執行可能比建 merge commit 晚一小時以上，所以改取自 P |
| F4 | 重跑失敗的 job 直到通過：`jobs?filter=latest` 看不到較早的 attempt | 修正：改用 `filter=all`，每一次 attempt 的該 job 都必須 success，最新 attempt 恰好一個；有測試 |
| F5 | PR 可能沒有自己的執行（以 `GITHUB_TOKEN` 開的 PR，例如 release.yml 的 deploy-pr），另一個同 head、不同 base 的 PR 的執行會被算進來；文件「這個 PR 自己的執行一定在其中」的說法不對 | 修正：只計入 head 分支與 head repository 都是這個 PR 的執行，並要求 `pulls?state=all&head=<owner>:<ref>` 恰好只有這個 PR；改正文件；有測試 |
| F6 | 事件檢查漏了 `github['event_name']`、`github.event_path`、`toJSON(github)`、`github.workflow_ref`、`github.actor`、`github.triggering_actor` 與對應的環境變數，也沒掃 `deploy/host/package.json`、`.json`、`.js`、`.mjs`、Containerfile 與 chart、image 測試 import 的 `scripts/release/`、`scripts/ci/check-secret-handling.ts` | 修正：擴大 pattern（有自我測試）與範圍（目前沒有命中）；文件寫明依路徑的 job 不可依 HEAD 的 parent 或歷史而異，這無法以 grep 檢查 |

另外：rust-cache 的版本不再在測試裡釘住（這條規則只影響 CI 成本，Renovate 升級不該讓 CI 失敗；升級時重新確認它的 README）。驗證者也指出，`f1c6b93` 一個 commit 不足以驗證新規則（那次 PR 執行沒有實際執行任何依路徑的 job）；所以真實資料改以多個 commit 查詢，包括改到 Rust 編譯快取 key 的合併（`66f90ccb961f`、`385cf22be6ad`）與 PR 中 `rust` 通過的合併（`51fb7f8`、`e944c0baa9a6`、`86b52f7dfff7`）。

第二輪複查（對修正後的 `9195b0c`）在完成前停止，沒有結果；修正改由協調者逐項審查程式與文件，並以下節的真實資料確認判斷。

## 真實資料

2026-10-19 以這個分支的判斷程式（與 `9195b0c` 相同；之後只併入 main 的文件、移動一行註解）唯讀查詢 8 個 main commit，結果與 2026-10-15 在 `9195b0c` 上查詢的相同。輸出含完整的 job summary。

| main commit | 來源 | 同一棵樹的檢查 | 判斷 | 輸出 |
| --- | --- | --- | --- | --- |
| `51fb7f8` | PR Laysi/trustiot-v2#435 | 適用；PR 執行 38707591081 的 `rust` 通過 | `rust` 略過；其他 job 照路徑規則略過 | [輸出](data/2026-10-15-431-skip-main-rerun/explain-51fb7f8.txt) |
| `e944c0baa9a6` | PR Laysi/trustiot-v2#426 | 適用；`rust`、`helm` 通過 | `rust`、`helm` 略過 | [輸出](data/2026-10-15-431-skip-main-rerun/explain-e944c0baa9a6.txt) |
| `86b52f7dfff7` | PR Laysi/trustiot-v2#416 | 適用；`rust`、`helm` 通過 | `rust`、`helm` 略過 | [輸出](data/2026-10-15-431-skip-main-rerun/explain-86b52f7dfff7.txt) |
| `6701c79` | PR Laysi/trustiot-v2#425 | 適用；五個 job 都通過 | `rust`、`helm`、`host`、`postgres-image` 略過；`cargo-deny` 的結果隨時間改變，照路徑規則執行（`scripts/release/lib.ts` 有變動） | [輸出](data/2026-10-15-431-skip-main-rerun/explain-6701c79.txt) |
| `385cf22be6ad` | PR Laysi/trustiot-v2#428 | 適用；`rust`、`cargo-deny`、`helm` 通過 | `helm` 略過；`rust` 因 `Cargo.lock` 改變 Rust 編譯快取的 key 而執行（main 存新的快取）；`cargo-deny` 照路徑規則執行 | [輸出](data/2026-10-15-431-skip-main-rerun/explain-385cf22be6ad.txt) |
| `66f90ccb961f` | PR Laysi/trustiot-v2#419 | 適用；`rust`、`cargo-deny` 通過 | `rust` 為了存快取執行；`cargo-deny` 照路徑規則執行；`helm` 照路徑規則執行（當時 main 有三個紅燈的 commit，比較基準往前推，範圍內有 `deploy/chart/README.md`） | [輸出](data/2026-10-15-431-skip-main-rerun/explain-66f90ccb961f.txt) |
| `f1c6b93` | PR Laysi/trustiot-v2#430 | 適用，但那次 PR 執行的五個 job 都依路徑略過 | 照路徑規則，五個都略過（新規則沒有作用） | [輸出](data/2026-10-15-431-skip-main-rerun/explain-f1c6b93.txt) |
| `8534a607c9a6` | PR Laysi/trustiot-v2#421 | 不適用：PR 最後的 head 不含 main 的 parent | 照路徑規則：`helm` 執行，其他略過 | [輸出](data/2026-10-15-431-skip-main-rerun/explain-8534a607c9a6.txt) |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| main 的樹等於 PR 已通過的 merge 樹時，PR 實際執行且通過的 job 在 main 上略過，job summary 寫出依據；有測試 | 通過（單元測試） |
| 每個照現有規則執行的情境都有測試（樹不同、head 沒通過、base 無法確認、一次推多個 commit、PR 略過的 job、API 讀取失敗） | 通過（單元測試，見上） |
| 判斷不使用 PR 執行產生的任何輸出；文件寫明理由 | 通過：程式只讀中繼資料（測試檢查呼叫的端點），理由寫在 ci-release.md |
| 以 main 上已有的合併用真實 API 資料唯讀執行判斷，結果是略過並說明依據 | 通過：8 個 main commit（見「真實資料」）；`51fb7f8`、`e944c0baa9a6`、`86b52f7dfff7`、`6701c79` 的 PR 已通過的 job 在 main 上略過，依據寫在輸出的 job summary；`f1c6b93` 沒有可略過的 job，`8534a607c9a6` 照預期不適用 |
| 決策 30 與 ci-release.md 已更新 | 通過 |
