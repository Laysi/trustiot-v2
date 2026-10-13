# nightly 連續三晚的執行紀錄（2026-10-10 至 2026-10-12）

Laysi/trustiot-v2#69 的第一條驗收條件：nightly 連續 3 晚以兩份 values 部署並通過冒煙測試，且冒煙測試失敗時會發出通知（實際驗證過一次）。本報告保存這三晚的 run 結果與原始資料，因為 `nightly-report` artifact 只保留 14 天。日期與時間都是 UTC。

重點先說：2026-10-10 與 2026-10-11 兩晚是排程（`schedule`）run。2026-10-12 那晚的排程 run 到 23:56 UTC 都沒有出現（cron 是 18:17 UTC，前兩晚分別在 21:51 與 21:59 UTC 才開始），所以由人於 23:57 UTC 用 `workflow_dispatch` 手動補跑。第三晚因此不是排程 run，結論一節說明這一點對驗收條件的影響。

## 環境

- 執行位置：GitHub Actions 託管 runner，映像 `ubuntu-24.04-arm`（arm64），runner 版本 2.337.0，runner image provisioner 20261002.596，image release `ubuntu24-arm64/20261004.142`。三晚相同（取自各 run 的 job log）。
- 叢集：k3d，節點映像 `rancher/k3s:v1.36.5-k3s1@sha256:057cfd5a9d51b89f1f0ee139c28d5d5fdd5aa7ddeac2742a0ba90c6dbc0329bb`；kubectl client v1.36.5、server v1.36.5+k3s1。
- Istio（ambient）1.30.5；Gateway API CRD v1.6.1（SHA-256 相符才套用）。
- CloudNativePG 1.30.1：只有第三晚的 workflow 有安裝它（前兩晚的 commit 還沒有 Postgres 的 chart 內容與檢查）。
- 每晚測的是當時 main 的 chart，所以 chart 內容三晚不同，這是 main 前進的結果，不是環境漂移：

| 日期（UTC） | run | 觸發方式 | commit | chart 渲染的物件數（onprem / saas 皆同） |
| --- | --- | --- | --- | --- |
| 2026-10-10 | 38089220300 | 排程 | `a4bfe2b` | 18 |
| 2026-10-11 | 38178596221 | 排程 | `d42a470` | 24 |
| 2026-10-12（跨到 2026-10-13） | 38324812561 | 手動 `workflow_dispatch` | `444ee48` | 28 |

- 時間：

| run | 開始 | 結束 |
| --- | --- | --- |
| 38089220300 | 2026-10-10T21:51:47Z | 2026-10-10T21:58:11Z |
| 38178596221 | 2026-10-11T21:59:43Z | 2026-10-11T22:06:02Z |
| 38324812561 | 2026-10-12T23:57:43Z | 2026-10-13T00:05:32Z |

## 指令

觸發：

```sh
# 排程：.github/workflows/nightly.yml 的 cron（18:17 UTC），第 1、2 晚
# 手動補跑（第 3 晚），在 main 上：
gh workflow run nightly.yml --ref main
```

第 3 晚的排程 run 沒有出現（到 2026-10-12T23:56Z 為止都沒有），協調者在 2026-10-12T23:57:29Z 執行上面的手動指令，產生 run 38324812561。

讀結果：

```sh
gh run view <run id>                                  # smoke、notify 兩個 job 的結論
gh run view <run id> --log                            # Actions job log
gh run download <run id> --name nightly-report        # report.md、report.json、run.log、e2e/ 等
```

通知路徑（失敗時開 issue、通過後留言並關閉）在 2026-10-10 驗證過，指令是在 Actions 頁或以 `gh workflow run nightly.yml --ref main -f simulate_failure=true` 手動觸發（見 Laysi/trustiot-v2#69 的 2026-10-11 留言）。

## 結果

三晚的 smoke 與 notify job 結論都是 success，`report.md` 標題都是 `nightly smoke test: passed`。每份 values 的各項檢查（含 ingress e2e 測試）全部是 `ok`；ingress e2e 都是 `61 passed, 0 failed`。三晚檢查項數不同，因為 nightly 隨 main 加了檢查：第 2 晚多了「Gateway 的 NetworkPolicy 只開 Istio pod 用的 port」，第 3 晚再多了「CloudNativePG Cluster 健康」。

| 晚 | values 檔 | 結果 | 耗時 | 檢查項數 | ingress e2e |
| --- | --- | --- | --- | --- | --- |
| 2026-10-10（run 38089220300） | `values-onprem.yaml` | passed | 1m33s | 10，全 ok | 61 passed, 0 failed（1m03s） |
| 2026-10-10 | `values-saas.yaml` | passed | 3m41s | 10，全 ok | 61 passed, 0 failed（3m19s） |
| 2026-10-11（run 38178596221） | `values-onprem.yaml` | passed | 1m30s | 11，全 ok | 61 passed, 0 failed（59s） |
| 2026-10-11 | `values-saas.yaml` | passed | 3m41s | 11，全 ok | 61 passed, 0 failed（3m19s） |
| 2026-10-12（手動 run 38324812561） | `values-onprem.yaml` | passed | 1m51s | 12，全 ok | 61 passed, 0 failed（1m00s） |
| 2026-10-12（手動） | `values-saas.yaml` | passed | 4m03s | 12，全 ok | 61 passed, 0 failed（3m19s） |

第 3 晚 `helm uninstall` 那項的 detail：`all objects removed; kept on uninstall as intended, then deleted: cluster.postgresql.cnpg.io/trustiot-postgres, imagecatalog.postgresql.cnpg.io/trustiot-postgres; their volumes were gone after 9 s`，兩份 values 相同。

通知路徑（2026-10-10，本報告沒有重跑；協調者 2026-10-13 以 `gh run view` 與 `gh issue view` 核對）：手動 run 38075061524（`workflow_dispatch`，commit `4d129f2`，`simulate_failure=true`）的 smoke job 失敗、notify job 成功，開立 Laysi/trustiot-v2#257（2026-10-10T18:18:29Z，標籤 `nightly-failure`、`priority:P1`、`area:ci`）；排程 run 38089220300 通過後在 2026-10-10T21:58:29Z 留言，並在 21:58:30Z 以 completed 關閉它。三晚的 notify job 也都是 success。

### 原始資料

位於 [`data/2026-10-13-69-nightly-three-nights/`](data/2026-10-13-69-nightly-three-nights/)，每個 run 一個目錄：

- `38089220300/`、`38178596221/`、`38324812561/`：各含 `report.md`、`report.json`、`run.log`（artifact 內的 run.log，也就是 nightly 腳本自己的指令輸出，節錄版，見下）、`e2e/`（`ingress-e2e-saas.json`、`ingress-e2e-onprem.json`）。
- 刻意不放第三方 manifest（`gateway-api-standard-install.yaml`、`cnpg-*.yaml`、`istiod-nightly.yaml`）：它們是上游 release 檔，其中兩份各超過 1 MB（每檔 1 MB 的規則），其餘不是這次的證據；版本與 SHA-256 已記在上面環境一節。
- 保留的是 artifact 的 `run.log`，而且是節錄：原檔 238 KB、323 KB、491 KB（依 run 順序），含大量 kubectl 輸出的 JSON 傾印（pod 與物件的完整內容），所以刪掉所有以 `"`、`{`、`}`、`[`、`]` 或 `- ` 開頭（前面可有空白）的行，其餘行原樣保留（指令、時間戳、helm 與 bun test 輸出、各項檢查結果）。完整原檔沒有放在 git 裡；14 天內可用 `gh run download <run id> --name nightly-report` 取回。Actions 的 job log（`gh run view --log`）也沒有放，它在 GitHub 上本來就公開可讀。
- 提交前掃描（掃的是節錄前的完整檔案）：未發現 token、私鑰、e-mail 或內部主機名稱；只有 k3d 內的 docker bridge 與 pod、service 位址（`172.18.0.x`、`10.42.x.x`、`10.43.x.x`）與 nightly 腳本建立的假 Secret 值（`nightly-dummy-value`），屬可公開的叢集位址與測試用假值。gitleaks 把完整 `run.log` 裡 pod 註解的 `checksum/*`（設定檔與假 Secret 的 SHA-256）誤判為 generic-api-key；這些行屬於被刪掉的 JSON 傾印，節錄版不含它們。

## 結論

驗收條件 1（nightly 連續 3 晚以兩份 values 部署並通過冒煙測試；失敗時會通知，實際驗證過一次）：

- 連續 3 晚通過：2026-10-10、2026-10-11、2026-10-12 三晚，兩份 values 都 passed，**但第三晚是手動補跑，不是排程 run**。排程 run 在 2026-10-12 沒有出現，原因推測是 GitHub 在高負載時略過，本報告沒有證據可以確認原因。若「連續 3 晚」接受在排程被略過的那晚以手動 run 補上，則此半條通過；若要求三個都是排程 run，則只有 2 晚，需等下一個成功的排程 run 補足。這個判斷留給審查者。
- 失敗通知已實測（2026-10-10，Laysi/trustiot-v2#257）：通過。
- 本報告不涵蓋 GitHub 之後可能再略過排程的問題；若這種情形常發生，是否需要另外處理，也留給審查者決定。

驗收條件 2（Renovate 開出的 PR 保持 SHA 與 digest 釘選）：待測。Renovate GitHub App 尚未安裝（需要組織管理員，Laysi/trustiot-v2#1 待決定清單第 3 項），目前沒有 Renovate 開出的 PR 可以檢查。

因此本報告只支持 Laysi/trustiot-v2#69 的第一條，不足以關閉該 issue。
