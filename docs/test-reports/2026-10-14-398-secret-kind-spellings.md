# check-secret-handling：每一種寫法的 kind 與由樣板動作印出的 Secret（2026-10-14）

對應 issue：Laysi/trustiot-v2#398。

## 環境

- 本機（x86_64，Fedora）：Bun 1.4.2、helm 4.3.0（`.prototools` 釘選，`proto bin helm`），分支 `fix/398-secret-kind-spellings`，以 `origin/main`（`4c8078e`）為基礎。
- 建置主機（檢查佇列 `rcheck.sh`，`CI=1`）：`bun test scripts`、`bun test deploy/chart` 等完整檢查。
- 「修正前」是 `origin/main` 的 `scripts/ci/check-secret-handling.ts`（`git show origin/main:scripts/ci/check-secret-handling.ts`）。

## 指令

```sh
# 每一種寫法：修正前與修正後的檢查各報幾個違規，以及 helm 實際渲染出幾個帶 data 的 Secret
probe=$(mktemp -d)
git show origin/main:scripts/ci/check-secret-handling.ts > "$probe/old-check.ts"
cp docs/test-reports/data/2026-10-14-398-secret-kind-spellings/probe.ts.txt "$probe/probe.ts"
bun "$probe/probe.ts" "$probe/old-check.ts" scripts/ci/check-secret-handling.ts

# 完整檢查（建置主機）
rcheck.sh 398 'bun install --cwd scripts --frozen-lockfile; bun scripts/ci/check-secret-handling.ts; bun test scripts; bun run --cwd scripts typecheck; bun test deploy/chart; bun test deploy/flux; bun scripts/ci/check-workflows.ts'

# 渲染檢查的突變測試：暫時加入一個文字檢查看不到的樣板，確認 chart 測試擋下
rcheck.sh 398 'bun test deploy/chart -t "every optional component on"'
```

`probe.ts.txt` 是 Bun 腳本，存成 `.txt` 是因為原始資料只放文字格式；Bun 不執行 `.txt`，所以先複製成 `.ts`（以上指令重跑的輸出與存下的檔案逐字相同）。

## 結果

### 每一種寫法，修正前後

[probe-before-after.txt](data/2026-10-14-398-secret-kind-spellings/probe-before-after.txt)：每一種寫法各放進一個只有那個樣板的 chart（樣板裡都有一個樣板動作，與 chart 的樣板一樣），各跑一次修正前與修正後的 `checkRepository`，再以 helm 4.3.0 `helm template` 渲染。

| 寫法 | 修正前的違規數 | 修正後的違規數 | helm 渲染出帶 `data` 的 Secret |
| --- | --- | --- | --- |
| `kind : Secret` | 0 | 1 | 1 |
| `kind: !!str Secret` | 0 | 1 | 1 |
| `kind: &k Secret` | 0 | 1 | 1 |
| `"kind": Secret` | 0 | 1 | 1 |
| `kind:` 值在下一行 | 0 | 1 | 1 |
| `kind: \|-` block scalar | 0 | 1 | 1 |
| `kind: >-` block scalar | 0 | 1 | 1 |
| flow 形式 | 0 | 1 | 1 |
| JSON 形式 | 0 | 1 | 1 |
| `{{ "kind: Secret" }}` | 0 | 2 | 1 |
| 樣板裡的 `toYaml (dict ...)` | 0 | 2 | 1 |
| `.tpl` helper 裡的 `toYaml (dict ...)` | 0 | 2 | 1 |
| `toYaml .Values.extraObject` | 0 | 1 | 1 |
| `range .Values.extraObjects` 加 `toYaml` | 0 | 1 | 1 |
| `tpl .Values.something` | 0 | 2 | 1 |
| `"\x6bind": Secret`（跳脫的 key） | 0 | 1 | 1 |
| `? kind`（明確 key） | 0 | 1 | 1 |
| `<<: {kind: Secret}`（merge key） | 0 | 1 | 1 |
| `ki{{ "" }}nd: Secret` | 0 | 1 | 1 |
| `toJson .Values.extraObject` | 0 | 1 | 1 |

前 15 列是 issue「重現」列出的寫法，後 5 列是同一類的其他寫法。每一列修正前都通過、helm 都渲染出帶 `data` 的 Secret；修正後全部被擋。`scripts/ci/check-secret-handling.test.ts` 的 `templates: every way to write or print a Secret` 把這些寫法（以及更多：大小寫不同的 key、`!!binary`、alias 當 key、同一物件兩個 kind、經 anchor 套用的 kind、`List`、`.Files.Get`、計算出的 helper 名稱、印出 values 的 helper、`templates/` 下其他副檔名的檔案、看起來像引用的頂層 Secret、拿掉動作後無法解析的樣板）各寫成一個必須被擋的測試；chart 自己的樣板（含 migration Job 的空密碼 Secret）、縮排在 key 底下的輸出、宣告完整 manifest 的 helper、以名稱引用 Secret 是通過的對照組。

### 完整檢查（建置主機）

[checks.txt](data/2026-10-14-398-secret-kind-spellings/checks.txt)：

- `bun scripts/ci/check-secret-handling.ts`：288 個檔案、6 個容器建置檔，沒有違規。
- `bun test scripts`：750 個測試，0 失敗（2 個 skip 是既有的 podman e2e 與需要 arm64 的測試）。
- `bun run --cwd scripts typecheck`：通過。
- `bun test deploy/chart`：316 個測試，0 失敗。新的「with every optional component on, in every mode, the only Secrets rendered are the migration Job's empty password Secrets」通過（saas 與 onprem 兩種 shape，各以 bundled 與 external 兩種 mode 渲染，共 4 次）；新的四個換行案例（略過 schema 驗證，值、CR、map key 的 LS、清單項目的 NEL）都讓 `helm template` 失敗，訊息指出欄位。
- `bun test deploy/flux`：19 個測試，0 失敗（SaaS 的 instance values 沒有換行，通過新的檢查）。
- `bun scripts/ci/check-workflows.ts`：沒有違規。

### 渲染檢查的突變測試

[render-check-mutation.txt](data/2026-10-14-398-secret-kind-spellings/render-check-mutation.txt)：暫時加入一個只在 `observability.collector.forward.enabled` 開啟時渲染的樣板，在 annotation 的值裡以 `b64dec` 解出第二份文件（一個帶 `data` 的 Secret）。文字檢查看不到這種刻意的混淆（`check-secret-handling.ts` 沒有違規），chart 測試的渲染檢查在第一次渲染就擋下（`checkRenderedManifests` 回報 Secret）。測試後已刪除這個檔案。（同樣以 `printf` 與含換行的字串拼出的寫法，現在文字檢查就會擋下：動作裡的字串不能含換行。）

## 結論

- 「重現」列出的每一種寫法都讓 `bun scripts/ci/check-secret-handling.ts` 失敗，並各有一個回歸測試；修正前這些寫法都通過（上表）：**通過**。
- chart 現有的樣板（含 migration Job 的空密碼 Secret）仍然通過，`bun test deploy/chart` 與 `bun test scripts` 通過：**通過**。chart 只改了一處：`trustiot.migrate.passwordSecrets` 回傳的 map key 由 `kind` 改名為 `database`，因為動作裡的字串不能再寫出 kind key。
- 以渲染結果為準的檢查：放在 `deploy/chart/tests/chart.test.ts`，由 CI 的 `helm` job 執行；`scripts/ci/check-secret-handling.ts` 變動時也會觸發（`scripts/ci/changes.ts` 的規則與測試）；開關從 `values.yaml` 的每個布林 `enabled` 與 `values.schema.json` 的每個 `mode` 讀出，涵蓋每個可選元件都開啟的設定：**通過**（CI 上的執行待 PR 開出後確認）。
- `docs/architecture/secrets.md` 的規則說明與實作一致（同一個變更更新）：**通過**。
- 印在一行之中的值：values 的字串、map key 與清單項目不能含換行，chart 渲染時就拒絕（`templates/values-check.yaml`，略過 schema 驗證也一樣）；動作裡的字串不能含換行，也不讀 `.Files`。所以印進一行的內容開不出新的一行或文件。
- 已知的界線（寫在 `docs/architecture/secrets.md`）：刻意在渲染時才拼出內容的樣板（例如 `b64dec` 一段編碼過的文字），文字檢查看不到，由渲染檢查在它會渲染的設定下擋下（上方突變測試）。
