# nightly 的失敗診斷不再含 Pod spec 與容器 log（2026-10-15）

對應 issue：Laysi/trustiot-v2#410。做法與理由在 [CI 與發佈](../architecture/ci-release.md) 的「失敗時怎麼辦」之下的「診斷收什麼」；原始資料在 [data/2026-10-15-410-nightly-diagnostics/](data/2026-10-15-410-nightly-diagnostics/)。

## 環境

- **受測程式碼**：分支 `fix/410-nightly-diagnostics`，從 main `24bbd5e` 開出。
- **單元測試**：建置主機（x86_64 Linux），經檢查佇列執行，`CI=1`；bun 1.4.2、helm、kubeconform、actionlint 由 `.prototools` 鎖定。
- **真叢集的範例**：lab 的 Mac mini M1（arm64，k3s），在一個拋棄式命名空間 `e2e-410` 建四個測試用 Pod，跑完刪除命名空間並釋放 lab 鎖。命名空間以外的資料沒有讀取（見下方「真叢集的範例」）。
- **nightly 本身只在 CI 跑**（k3d、arm64 runner）。診斷只在失敗時執行，所以由單元測試與上面的範例涵蓋；PR 是否帶 `smoke` label 由協調者決定。

## 指令

在 repo 根目錄，經檢查佇列：

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun test deploy/chart
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
```

診斷的單元測試：`bun test scripts/nightly/diagnostics.test.ts`；與安裝失敗證據共用的部分：`bun test deploy/chart/tests/e2e/install-evidence.test.ts`。

真叢集的範例：以 `lab-pods.yaml`（[data](data/2026-10-15-410-nightly-diagnostics/lab-pods.yaml)，環境變數、annotation、log 與終止訊息的值是 `__ENV__` 這類佔位符，執行時換成隨機值）在 `e2e-410` 建 Pod，再以 `kubectl` 指向那個命名空間，呼叫 `gatherDiagnostics("e2e-410", ...)`（`scripts/nightly/diagnostics.ts`），最後逐一檢查隨機值有沒有出現在輸出。

## 結果

### 單元測試

| 項目 | 結果 |
| --- | --- |
| `bun test scripts` | 851 通過、2 略過（需要 podman 與 arm64 的既有測試）、0 失敗；其中新的 `diagnostics.test.ts` 21 個 |
| `bun test deploy/chart` | 372 通過、0 失敗；其中 `install-evidence.test.ts` 新增 URL 與探針輸出、保留結尾的測試 |
| `bun run --cwd scripts typecheck` | 通過 |
| workflow policy、secret handling | 通過（401 個檔案，無違規） |

指令與輸出都經檢查佇列（`rcheck.sh 410 '...'`，建置主機，退出碼 0）。第一次執行有一個失敗：我寫的 URL 測試的預期字串少算了查詢字串吃掉的冒號，改測試後第二次全部通過。

### 單元測試證明了什麼

`scripts/nightly/diagnostics.test.ts` 的模擬叢集同時有：記憶體不足被殺的服務（`OOMKilled`、exit 137、重啟 4 次）、映像拉不下來的服務（`ErrImagePull`，message 內有帶使用者資訊與 token 的 URL）、起不來的 Postgres Cluster、沒有 programmed 的 Gateway、探針失敗的事件（HTTP 503 與指令探針輸出）。每個 Pod、Cluster、事件還帶著不能出現的值，以 `crypto.randomUUID()` 在執行時產生：環境變數值、annotation、終止訊息、Secret 資料、探針輸出、URL 裡的 token 與使用者、Cluster spec 的值。

- 這些值一個都不在診斷文字、Markdown 報告、`failureIssueBody`（issue 內文）與 `report.json` 裡。
- 診斷只呼叫 `kubectl get`（沒有 `logs`、`describe`、`exec`、`jsonpath`）；Secret 只以 `--output name` 列名稱。
- nightly 的程式碼（`scripts/nightly/*.ts`）不含 `"logs"`、`"describe"`；`smoke.ts` 裡每個解析 `--output json` 的 `kubectl get` 都走 quiet 的 `kubectlJson`，所以 Pod spec 不進 `run.log`（artifact）。
- 仍看得出為什麼沒就緒：Deployment、ReplicaSet、Pod 的 conditions、容器狀態、重啟次數、上一次終止的 exit code 與 reason、Cluster 的 phase 與原因、Gateway 與 listener 的 conditions、其他命名空間的 Pod 與 Warning 事件、事件依時間由舊到新。

變異測試（改壞程式碼，測試要失敗；還原後通過）：

| 改壞的地方 | 失敗的測試 |
| --- | --- |
| 容器狀態多印 `terminated.message` | 3 個：「never contain …」、「show an image …」、「carry what the diagnostics show and none of the planted values」 |
| `probeMessage` 一律保留原文 | 6 個：「never contain …」、事件順序與探針、`probeMessage` 的三個單元（兩個在 `diagnostics.test.ts`、一個在 `install-evidence.test.ts`）、報告與 issue 內文 |
| `smoke.ts` 一處 JSON 讀取改回會寫進日誌的 `kubectl(` | 「keeps the output of a JSON read out of the run log」 |

### 真叢集的範例

lab 的 k3s 上，`e2e-410` 命名空間的四個 Pod：

| Pod | 設定 | 預期 |
| --- | --- | --- |
| `bad-image` | 映像 `registry.invalid/...`，拉不到 | `ImagePullBackOff` |
| `exits-nonzero` | 印一行 log、把一行寫進終止訊息、`exit 3` | `CrashLoopBackOff`，exit 3 |
| `out-of-memory` | 記憶體上限 16Mi，要配置約 40 MB | `OOMKilled`，exit 137 |
| `never-ready` | readiness 指令探針印一個值後失敗 | Ready=False，Unhealthy 事件 |

四個 Pod 都有一個環境變數 `PLAIN_VALUE`，值是隨機字串；`bad-image` 另有隨機值的 annotation。診斷輸出見 [lab-diagnostics.txt](data/2026-10-15-410-nightly-diagnostics/lab-diagnostics.txt)（lab 節點名稱以 `<lab-node>` 遮蔽），其中最有用的幾段：

```
### Pods
Pod/bad-image: phase=Pending conditions: PodReadyToStartContainers=True; Initialized=True; Ready=False (ContainersNotReady: containers with unready status: [assets]); ...
  assets: waiting (ImagePullBackOff: Back-off pulling image "registry.invalid/trustiot/assets:nightly": ErrImagePull: failed to pull and unpack image "registry.invalid/trustiot/assets:nightly": failed to resolve reference "registry.inval... (68 characters cut) ...istry.invalid/v2/trustiot/assets/manifests/nightly": dial tcp: lookup registry.invalid: no such host), ready=false, restarts=0
Pod/exits-nonzero: phase=Running conditions: ...; Ready=False (ContainersNotReady: containers with unready status: [identity]); ...
  identity: terminated (exit 3, Error), ready=false, restarts=3; last exit 3, Error
Pod/never-ready: phase=Running conditions: ...; Ready=False (ContainersNotReady: containers with unready status: [dashboard]); ...
  dashboard: running, ready=false, restarts=0
Pod/out-of-memory: phase=Running conditions: ...; Ready=False (ContainersNotReady: containers with unready status: [ingest]); ...
  ingest: terminated (exit 137, OOMKilled), ready=false, restarts=3; last exit 137, OOMKilled

### Events
2026-10-14T17:13:06Z Warning Failed Pod/bad-image x3: Failed to pull image "registry.invalid/trustiot/assets:nightly": ... (52 characters cut) ...istry.invalid/v2/trustiot/assets/manifests/nightly": dial tcp: lookup registry.invalid: no such host
2026-10-14T17:13:08Z Warning BackOff Pod/exits-nonzero x3: Back-off restarting failed container identity in pod exits-nonzero_e2e-410(...)
2026-10-14T17:13:10Z Warning BackOff Pod/out-of-memory x3: Back-off restarting failed container ingest in pod out-of-memory_e2e-410(...)
2026-10-14T17:13:12Z Warning Unhealthy Pod/never-ready x10: Readiness probe failed: (probe output not shown)
```

- 沒有任何一個隨機值（環境變數、annotation、log、終止訊息、指令探針的輸出）出現在輸出：腳本逐一比對，五個都是 `absent`。
- 發現並修正一個可用性問題：第一版把過長的欄位只留開頭 300 字元，映像拉取的錯誤是一長串，原因（`no such host`）在最後，被切掉了；改為留開頭 200 與結尾 100 字元。這個版本的輸出就是上面的。
- 指令探針的 message 是指令印出的內容（那個隨機值），診斷只留 `Readiness probe failed: (probe output not shown)`；要知道為什麼探針失敗，得到叢集上看，這是為了不收應用程式輸出而接受的代價。
- 事件裡的 `Successfully assigned <命名空間>/<pod> to <節點>` 帶節點名稱。nightly 的節點是 k3d 的 `k3d-nightly-server-0`，不是祕密；這次 lab 的節點名稱是主機別名，所以報告裡遮蔽了。
- 這次執行為了不讀其他命名空間與節點，把「所有命名空間」與「節點」的讀取以空清單回應（腳本在 kubectl 呼叫處攔截），所以「其他命名空間」與「Nodes」兩節是 `(none)`；它們的格式由單元測試涵蓋。實際送出的 kubectl 呼叫見 [lab-kubectl-calls.txt](data/2026-10-15-410-nightly-diagnostics/lab-kubectl-calls.txt)：全是 `kubectl get`。
- 叢集沒有 CloudNativePG 的 CRD，該節顯示 `(not available: ... doesn't have a resource type "clusters")`，其他節照常。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| nightly 的診斷不含 Pod spec 的環境變數值與容器 log 的內容，有單元測試以模擬輸出證明 | 通過：`diagnostics.test.ts`，含變異測試；真叢集的範例也沒有出現植入的值 |
| 失敗時仍看得出哪個元件、為什麼沒有就緒，有範例：conditions、事件、重啟次數與原因 | 通過：上方範例與單元測試（conditions、事件、重啟次數、上一次終止的 reason 與 exit code、`OOMKilled`、映像拉取錯誤） |
| `docs/architecture/ci-release.md` 寫明診斷收什麼、不收什麼與理由 | 通過：「失敗時怎麼辦」之下的「診斷收什麼」 |

未做：nightly 在 k3d 上實際失敗一次來看診斷（那只在 CI 跑；診斷的收集程式與 lab 的範例相同，kubectl 的呼叫方式是 `ctx.kubectl` 加 `KUBECONFIG`）。若要在 CI 看到，PR 帶 `smoke` label 後以 `workflow_dispatch` 的 `simulate_failure` 跑一次。
