# chart：入口 gateway 的 NetworkPolicy 只放行 listener 與 15021（2026-10-11）

對應 issue：Laysi/trustiot-v2#287。原始資料在 [data/2026-10-11-287-gateway-netpol-ports/](data/2026-10-11-287-gateway-netpol-ports/)。

## 目的

1. `tunnel` 模式渲染出的入口 gateway NetworkPolicy，每條規則都帶 `ports`，只有 Gateway 的 listener（80）與 Istio 的狀態埠 15021；`direct` 模式的渲染結果不變。
2. 回歸：同一份測試在 main `e8f180b`（PR Laysi/trustiot-v2#277 合併後）失敗，修正後通過。
3. 確認 Istio 實際用的埠：入口 pod 的 kubelet 探針打哪個埠、Service 把 listener 轉到 pod 的哪個埠、入口 pod 是否在 ambient mesh 內（決定探針的來源位址）。
4. 在 lab 經真正的入口 NetworkPolicy 跑入口 e2e（`E2E_SSH_JUMP`，請求來源與 cloudflared 相同），確認收緊後 cloudflared 的路徑與 kubelet 的探針照常運作，並直接量測哪些埠連得到。

## 環境

- **本機**：x86_64 Fedora（Linux 7.2.8-300.fc45），bun 1.4.2、helm 4.3.0、kubectl 1.36.5、kubeconform，皆由 `.prototools` 鎖定。
- **build host**：`ruisi-wsl`（Ubuntu on WSL2），以 `rcheck.sh` 跑 `bun test deploy/chart`。
- **lab**：`trustiot-mac-m1`（Mac mini M1，aarch64，Fedora Asahi），k3s v1.36.5+k3s1（NetworkPolicy 由 k3s 內建的 kube-router 執行），Istio 1.30.5 ambient（`registry.istio.io/release/proxyv2:1.30.5-distroless`）。
- **受測版本**：分支 `fix/287-gateway-netpol-ports` 的 commit `781a4d0`（以 main `e8f180b` 為基底）；lab 與回歸都在這個版本上跑。之後 rebase 到 main `e472547`（commit `9f34183`，chart 與 `scripts/nightly/` 的內容不變），在 build host 與本機重跑了 chart 與 scripts 的測試。「修正前」是 `git archive e8f180b deploy/chart .prototools .proto rust-toolchain.toml` 取出的 chart，只換上本分支的測試檔（`deploy/chart/tests/chart.test.ts`、`scripts/nightly/`）。
- **lab 的 e2e 設定**：namespace `e2e-287`（Pod Security `restricted`），執行身分是該 namespace 的 ServiceAccount `e2e-287-runner`（RoleBinding `admin`，另加只在該 namespace 有效、允許 Gateway、HTTPRoute、AuthorizationPolicy 的 Role），token 3 小時，kubeconfig 只放在本機暫存目錄，測完刪除；API server 經 `ssh -L 127.0.0.1:16487:127.0.0.1:6443`。身分服務替身由 repo 的 `identity-double/` 在 lab 以 podman 建置：`localhost/trustiot-e2e/identity-double:287@sha256:95216d2ae24e2a50467b7038f0ad82763ff797338d68d869c10c36cc8490696b`。mesh 設定以 `HelmChartConfig istiod` 疊加（[lab-istiod-helmchartconfig.yaml](data/2026-10-11-287-gateway-netpol-ports/lab/lab-istiod-helmchartconfig.yaml)，由 [make-hcc.ts](data/2026-10-11-287-gateway-netpol-ports/scripts/make-hcc.ts.txt) 從 `deploy/istio/istiod-values.yaml` 產生：provider `trustiot-e2e-287` 加上路徑正規化，lab 原有的 provider `identity` 原樣重複寫入）。
- **時間**：台北時間 2026-10-11 11:40–12:20。lab 鎖 UTC 03:56 取得（owner `Laysi/trustiot-v2#287`），04:16 釋放；mesh 設定被改動的期間是 UTC 04:06:58–04:14:41。

## 指令

本機與 build host：

```sh
bun test deploy/chart/tests/chart.test.ts -t "NetworkPolicy"          # 入口與服務的 NetworkPolicy 測試
bun test scripts/nightly/manifests.test.ts -t "gateway ports"        # smoke 新檢查的單元測試
bun run --cwd scripts typecheck
bun test scripts
bun test deploy/chart/tests/e2e                                      # 通道復原的單元測試（build host 上已知失敗，在本機跑）
bash rcheck.sh 287 'bun test deploy/chart 2>&1 | tail -30'            # build host

# 修正前：main e8f180b 的 chart，換上本分支的測試
mkdir before && git archive e8f180b deploy/chart .prototools .proto rust-toolchain.toml | tar -x -C before
cp deploy/chart/tests/chart.test.ts before/deploy/chart/tests/ && cp -r scripts/nightly before/scripts/
(cd before && bun test ./deploy/chart/tests/chart.test.ts -t "NetworkPolicy")
(cd before && bun test ./scripts/nightly/manifests.test.ts -t "gateway ports")

# 修正前後逐一比較兩種型態（不加、加 smoke、加 smoke 與 e2e 覆寫檔）渲染出的所有物件
bun diff-render.ts before/deploy/chart deploy/chart
```

`diff-render.ts` 見 [scripts/diff-render.ts.txt](data/2026-10-11-287-gateway-netpol-ports/scripts/diff-render.ts.txt)。

lab（取得鎖之後；準備步驟同 `deploy/chart/tests/e2e/README.md`）：

```sh
# 入口 e2e，SaaS 型態，經 ssh -L 由 lab 主機連入口的 ClusterIP（會經過入口的 NetworkPolicy）
KUBECONFIG=<暫存>/config E2E_NAMESPACE=e2e-287 E2E_SHAPE=saas E2E_AUTHZ_PROVIDER=trustiot-e2e-287 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity-double:287@sha256:95216d2ae24e2a50467b7038f0ad82763ff797338d68d869c10c36cc8490696b \
E2E_IMAGE_PULL_POLICY=Never E2E_SSH_JUMP=trustiot-mac-m1 E2E_KEEP=1 E2E_REPORT_DIR=<資料目錄> \
bun test --timeout 600000 --reporter=junit --reporter-outfile=<資料目錄>/e2e-saas.junit.xml ./deploy/chart/tests/e2e/ingress.e2e.ts

# 保留 release，從 lab 主機連入口 pod 的每個埠（scripts/probe-ports.sh.txt）
bash probe-ports.sh "<標籤>"
# 以 smoke 的新檢查比對實際的 Deployment、Service 與 release（scripts/live-gateway-ports.ts.txt）
KUBECONFIG=<暫存>/config bun live-gateway-ports.ts <repo>
# 加一個不在 mesh 的 pod 當 allowedSources，從它連每個埠（scripts/np-probe-pod.yaml、scripts/pod-probe.ts.txt）
kubectl apply -f np-probe-pod.yaml
helm upgrade trustiot deploy/chart -n e2e-287 --reuse-values --force-conflicts \
  --set-json 'ingress.tunnel.allowedSources=["10.42.0.1/32","10.42.0.60/32"]' --wait
KUBECONFIG=<暫存>/config bun pod-probe.ts "<標籤>"
# 修正前的政策形狀：拿掉規則的 ports，再連一次
kubectl -n e2e-287 patch networkpolicy trustiot-ingress --type=json -p '[{"op":"remove","path":"/spec/ingress/0/ports"}]'
```

Istio 的入口樣板：`sudo k3s kubectl -n istio-system get cm istio-sidecar-injector -o jsonpath='{.data.config}'` 的 `kube-gateway` 一段。

## 結果

### 1. 渲染結果與回歸測試

| 版本 | 結果 | 記錄 |
| --- | --- | --- |
| main `e8f180b` + 新 chart 測試（`-t "NetworkPolicy"`） | 7 個中 2 個失敗：改寫的 tunnel／direct 精確比對與新加的 `saas: every rule ... admits only ...`；`onprem` 那一個通過（`direct` 本來就限埠） | [chart-test-on-main.log](data/2026-10-11-287-gateway-netpol-ports/chart-test-on-main.log) |
| 分支（同上） | 7 個全部通過 | [chart-test-on-branch.log](data/2026-10-11-287-gateway-netpol-ports/chart-test-on-branch.log) |
| main `e8f180b` + 新 smoke 單元測試（`-t "gateway ports"`） | 6 個中 1 個失敗：`values-saas.yaml` 的政策 `admitted` 為 `"all"` | [nightly-gateway-ports-on-main.log](data/2026-10-11-287-gateway-netpol-ports/nightly-gateway-ports-on-main.log) |
| 分支（同上） | 6 個全部通過 | [nightly-gateway-ports-on-branch.log](data/2026-10-11-287-gateway-netpol-ports/nightly-gateway-ports-on-branch.log) |
| build host，`bun test deploy/chart` | 202 個中 197 個通過；失敗的 5 個都是 `tunnel-recovery.test.ts`（WSL 上的已知問題），本機跑 `bun test deploy/chart/tests/e2e` 22 個全部通過 | [rcheck-bun-test-deploy-chart.log](data/2026-10-11-287-gateway-netpol-ports/rcheck-bun-test-deploy-chart.log) |
| build host，`chart.test.ts` 與 `raw-client.test.ts` | 187 個全部通過 | [rcheck-chart-test-only.log](data/2026-10-11-287-gateway-netpol-ports/rcheck-chart-test-only.log) |
| 本機，`bun test scripts`、`bun run --cwd scripts typecheck` | 159 個全部通過；型別檢查無錯誤 | 本機 |
| rebase 到 main `e472547` 後：build host 的 `chart.test.ts` 與 `raw-client.test.ts`、`bun test deploy/chart`；本機的 `bun test scripts`、`bun test deploy/chart/tests/e2e`、型別檢查、`check-workflows.ts` | build host 187 個全部通過、`deploy/chart` 202 個中只有同樣 5 個 `tunnel-recovery.test.ts` 失敗；本機 249 個與 22 個全部通過，型別檢查與 workflow 規則無錯誤 | [rcheck-after-rebase.log](data/2026-10-11-287-gateway-netpol-ports/rcheck-after-rebase.log) |

修正前後逐一比較渲染出的所有物件（[render-diff.txt](data/2026-10-11-287-gateway-netpol-ports/render-diff.txt)）：SaaS 三種疊加都只有 `NetworkPolicy/trustiot-ingress` 不同，規則從 `from: 10.42.0.1/32`（不限埠）變成同一個來源加上 `ports: [80/TCP, 15021/TCP]`；地端三種疊加沒有任何物件不同，Gateway 的 listener 改由 `trustiot.ingress.listenerPort` 渲染後內容也相同。

### 2. Istio 實際用的埠（lab，Istio 1.30.5）

[live-gateway-ports.txt](data/2026-10-11-287-gateway-netpol-ports/lab/live-gateway-ports.txt) 是 Istio 為 `e2e-287` 的 Gateway 產生的物件，[istio-1.30.5-kube-gateway-template.txt](data/2026-10-11-287-gateway-netpol-ports/lab/istio-1.30.5-kube-gateway-template.txt) 是產生它們的樣板：

| 項目 | 實際值 | 對 NetworkPolicy 的意義 |
| --- | --- | --- |
| `istio-proxy` 的探針 | startup 與 readiness 都是 `GET /healthz/ready`，埠 15021；沒有 liveness | kubelet 只連 15021 |
| container ports | 15020（`metrics`）、15021（`status-port`）、15090（`http-envoy-prom`） | 15020、15090 只是指標，沒有用戶端需要從外面連 |
| Service | `status-port` 15021→15021、`http` 80→80，`type: ClusterIP` | cloudflared 連 ClusterIP:80，到 pod 的也是 80 |
| pod 的 sysctl | `net.ipv4.ip_unprivileged_port_start=0` | Envoy 以非 root 直接聽 80，不轉到其他埠 |
| pod 的 label | `istio.io/dataplane-mode: none` | 入口 pod 不在 ambient mesh 內，kubelet 的探針不會被 istio-cni SNAT 成 `169.254.7.127`，來源是主機自己的位址 |

樣板裡 Service 的 `ports` 是 Istio 依 Gateway 的 listener 加上 `status-port` 產生，沒有 `targetPort`，所以 pod 上的埠就是 listener 的埠。smoke 的新檢查（`gatewayPorts`）對這組實際物件與分支的 release：`needed: [80, 15021]`、`admitted: [80, 15021]`，沒有問題。

### 3. 入口 e2e（SaaS，經 `E2E_SSH_JUMP`）

UTC 04:07:16–04:08:17，61 個全部通過（[e2e-saas.log](data/2026-10-11-287-gateway-netpol-ports/lab/e2e-saas.log)、[e2e-saas.junit.xml](data/2026-10-11-287-gateway-netpol-ports/lab/e2e-saas.junit.xml)、[ingress-e2e-saas.json](data/2026-10-11-287-gateway-netpol-ports/lab/ingress-e2e-saas.json)）。請求由 lab 主機經入口的 ClusterIP 進來，與 cloudflared 相同；兩個入口 pod 都就緒，`helm install --wait` 與 `Programmed` 都通過，所以 kubelet 的 15021 探針在新的政策下照常（[objects.txt](data/2026-10-11-287-gateway-netpol-ports/lab/objects.txt)）。地端型態（`direct`）的渲染結果與修正前完全相同，沒有在 lab 重跑。

### 4. 哪些埠連得到

**從 lab 主機**（[host-port-probe.txt](data/2026-10-11-287-gateway-netpol-ports/lab/host-port-probe.txt)）：修正前與修正後都是 80 回 401、15021 回 200、15020 與 15090 回 200。主機連得到每個埠，與 NetworkPolicy 的內容無關。原因是 Kubernetes NetworkPolicy 的語意：pod 被政策隔離後，仍一律放行來自 pod 所在節點的連線；k3s 的 kube-router 以「來源是本機位址」的規則實作，cni0 的 10.42.0.1 是本機位址。所以在單節點的 SaaS 主機上，cloudflared 與 kubelet 不靠這條規則就連得到入口，這條規則限制的是 `allowedSources` 裡**不是 pod 所在節點**的來源：多節點時其他節點、cloudflared 改跑在其他節點，或 `allowedSources` 寫成比本機更大的網段（schema 允許到 /8）。

**從 `allowedSources` 裡的一個 pod**：為了看到 NetworkPolicy 本身的效果，在 `e2e-287` 建一個不在 mesh 的 pod `np-probe`（10.42.0.60，`curlimages/curl:8.11.1`），以 `helm upgrade --set-json ingress.tunnel.allowedSources=["10.42.0.1/32","10.42.0.60/32"]` 把它列為來源，從它連兩個入口 pod 的每個埠（[pod-port-probe.txt](data/2026-10-11-287-gateway-netpol-ports/lab/pod-port-probe.txt)）：

| 政策 | 80 `/` | 15021 `/healthz/ready` | 15020 `/stats/prometheus` | 15090 `/stats/prometheus` |
| --- | --- | --- | --- | --- |
| 分支（`ports: [80, 15021]`） | 401 | 200 | 連不上（3 秒逾時） | 連不上（3 秒逾時） |
| 修正前的形狀（同樣的來源，規則不帶 `ports`） | 401 | 200 | 200 | 200 |

兩個入口 pod 的結果相同。

### 5. lab 的還原

[lab-restore.txt](data/2026-10-11-287-gateway-netpol-ports/lab/lab-restore.txt)：`helm uninstall` 後刪除 namespace `e2e-287`，刪除 `HelmChartConfig istiod`，helm-controller 以原本的值重裝 istiod；`istio` ConfigMap 的 `mesh` 與 `meshNetworks` 和測試前的備份以 `cmp` 比對**逐位元組相同**（`mesh` 的 sha256 前後都是 `11507d117465e92da63f14204fc2c19837df210957b181730a46a0ce91ae76be`，`meshNetworks` 都是 `7b2c40b5001e85e00a7ee8ebfc7bcfea574e72f8906e1503bc73e5a2cb0eeaed`）。istiod、ztunnel、istio-cni 與 `edge` 的 pod 沒有重啟，`poc` 的 8 個 pod 都在 Running，`edge` 仍 Programmed。替身與 curl 映像（containerd 與 podman）、`~/e2e-287` 都已刪除，`~/e2e.lock` 已釋放；本機的 `ssh -L` 與 kubeconfig 已移除。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `tunnel` 模式渲染出的 gateway NetworkPolicy，每條規則都只放行需要的埠（chart 測試） | 通過：`saas: every rule of the gateway's NetworkPolicy admits only the Gateway's listener and Istio's status port`（兩個來源，每條規則的 `ports` 都等於 Gateway 的 listener 加 15021），`direct` 同一個測試與精確比對都不變；smoke 新檢查的單元測試以 Istio 1.30.5 產生的物件形狀比對兩種型態的 release |
| 回歸驗證：上述測試在 #277 合併後的 main 上失敗，修正後通過 | 通過：main `e8f180b` 上 chart 測試 2 個、smoke 單元測試 1 個失敗，分支全部通過 |
| PR 的 k3d `smoke` job 兩份 values 都通過；tunnel 模式的入口 e2e 若受影響，需在 lab 依規則驗證 | lab：通過，入口 e2e（SaaS，經 NetworkPolicy）61 個全部通過，從 `allowedSources` 內的 pod 量到只有 80 與 15021 連得到。`smoke` job：待測，由 PR 的 CI 驗證（smoke 多了第 7 項「每個 Gateway 的 NetworkPolicy 放行的埠與 Istio 實際用的埠相同」），結果記錄在 PR 與 issue 的實作說明 |
| README 已更新 | 通過：`deploy/chart/README.md` 的 `ingress.exposure`、`ingress.tunnel.allowedSources`、NetworkPolicy 的說明與新的「入口 pod 的埠」；另更新 `docs/architecture/decisions.md` 決策 27 與 `docs/architecture/ci-release.md` 的 smoke 檢查清單 |
