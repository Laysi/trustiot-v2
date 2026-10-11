# nightly 的 k3d 映像匯入確認（單元測試）

對應 issue：Laysi/trustiot-v2#335。

## 環境

- 本機（Fedora，x86_64），bun 依 `.prototools` 釘選；不需要叢集、docker 或 k3d，指令全部以替身執行。
- 分支 `fix/335-k3d-import-check`，基底 main `1ad700f`。
- 測試檔：`scripts/nightly/platform.test.ts` 的 `importImageIntoCluster` 一組。

## 指令

```sh
# 修正前的行為：暫時把 importImageIntoCluster 換成「只執行一次 k3d image import，信任結束碼」
bun test scripts/nightly/platform.test.ts -t importImageIntoCluster
# 修正後
bun test scripts/nightly
bun run --cwd scripts typecheck
```

## 結果

- 修正前（原始輸出：[old-code-test-output.txt](data/2026-10-11-335-k3d-import-check/old-code-test-output.txt)）：8 項中 6 項失敗，其中「k3d 回報成功、映像卻不在 node 上，重試用完仍缺」一項是 `Expected promise that rejects` / `Received promise that resolved`，也就是 nightly 會照常往下部署；「第一次缺、重試後成功」一項只有 1 次匯入，沒有重試。
- 修正後：同一組 8 項全部通過；`bun test scripts/nightly` 與 `bun run --cwd scripts typecheck` 的結果見 PR 的驗證一節。
- 涵蓋：只選本叢集的 server 與 agent 節點（排除 loadbalancer 與別的叢集）；每個節點都有映像時通過並逐節點記錄 `present` 與 `confirmed`；缺映像時重試（`--mode direct`）；重試用完則以「k3d image import succeeded, but image <映像> is missing on node(s) <節點>」失敗；匯入指令本身失敗也重試；`k3d node list` 輸出不可用時失敗。

## 結論

- 修正前 nightly 會繼續部署；修正後會重試，用完則在匯入這一步失敗並指出映像與節點：通過（單元測試）。
- 正常情況的 log 逐節點記錄確認到的映像：通過（單元測試斷言 log 內容）。
- 真叢集上的行為（`crictl inspecti` 在 k3s 節點內可用、`--mode direct` 可匯入）：待測，要等合併後的下一次 nightly 或帶 `smoke` label 的 PR 執行；本機規定不跑 k3d。
