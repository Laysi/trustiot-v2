# 閘道器經入口取根 CRL

Laysi/trustiot-v2#831（Laysi/trustiot-v2#560 的子項）。

## 環境

- 建置主機（aarch64 Linux VM）：certs 與 protocol 的 clippy、certs 的測試、`bun test deploy/chart`、`bun test scripts`（含 `scripts/nightly` 與 `scripts/demo`）、`bun test deploy/flux`、`bun run --cwd scripts typecheck`、`cargo run -p openapi -- --check`，以及整個 workspace（agent 除外）的 fmt、clippy 與測試。
- 建置主機（x86_64 Linux）：最後一次 `cargo test --workspace`。第一次在 aarch64 上跑時，`outbox` 的一個容器測試在主機上與同時跑的另一個工作搶到同一個連接埠（podman 回 `Address already in use`），與本變更無關，cargo 也因此停在那裡；改到一次只跑一個工作的 x86_64 主機重跑，全部通過。
- 需要容器的測試都設 `CI=1`：certs 的服務測試（拋棄式 Postgres）沒有容器時失敗，不會略過。
- 工具版本依 `.prototools` 與 `rust-toolchain.toml`（Rust 1.99.0、Bun 1.4.2、Helm 4.3.0）。
- 分支 `feat/831-certs-root-crl-endpoint`，以 `origin/main` 的 `1d19df2a` 為基底。
- 測試用的根、根 CRL 都在測試執行時產生（Rust 測試以 rcgen，nightly 的單元測試以 OpenSSL），沒有固定的金鑰檔。
- 部署後的冒煙測試（nightly 的 `certs.ts`）只在 CI 的 k3d 叢集跑，要等 PR 帶 `smoke` label；工作站不跑 Kubernetes。

## 指令

```sh
# certs 端點（第一次跑在加入 nightly 之前，附 --nocapture 看每一步）
cargo clippy --locked -p certs -p protocol --all-targets -- -D warnings
CI=1 cargo test --locked -p certs --test root_crl_endpoint -- --nocapture

# chart、展示環境的 tunnel、Flux、型別
bun install --cwd scripts --frozen-lockfile
bun test deploy/chart && bun test scripts/demo && bun test deploy/flux && bun run --cwd scripts typecheck

# nightly 的冒煙檢查（不需要叢集的部分）
bun test scripts/nightly && bun run --cwd scripts typecheck

# 最後的整組檢查
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked -p certs
cargo run --locked -p openapi -- --check
bun test deploy/chart && bun test scripts && bun test deploy/flux && bun run --cwd scripts typecheck
CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast
```

## 結果

### certs 的端點（`services/certs/tests/root_crl_endpoint.rs`）

3 個測試全部通過。`--nocapture` 印出的每一步：

```text
no root CRL: GET and HEAD 404 "no root CRL is published\n", no-store
root CRL 1 adopted: GET 200 application/x-pem-file, 386 bytes, the file's DER; HEAD 200 without a body
a GET announcing a 1 MiB body it never sends, with a cookie, a forged identity, a query and a range: the same 200 at once
root CRL 2 adopted: served instead of 1
6 other methods: 405, Allow: GET,HEAD, no body
path variants, the renewal and the /internal files: 404 from the gateway routes
after a new root without a root CRL of its own: 404 again
root CRL 1: GET 200 (386 bytes, the file's DER), HEAD 200, POST 405
broker port: /certs/root-crl 404
```

最後兩行是真的服務（容器）：在 API 埠上，寫入根 CRL 檔之後不重啟就從 404 變成 200；broker 埠沒有這條路徑。

### chart 與入口

| 指令 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 714 pass，0 fail |
| `bun test deploy/flux` | 78 pass，0 fail |
| `bun test scripts`（含 nightly、demo） | 1,739 pass，9 skip，0 fail |
| `bun run --cwd scripts typecheck` | 沒有錯誤 |

chart 測試的證據：

- 路由清單證明（`PUBLIC_PATHS`）：saas 與 onprem 的 gateway 主機都多了 `/certs/root-crl` → `<release>-certs`，其他主機沒有。
- 新測試「the root CRL is an exact public path of certs on hosts.gateway only」：certs 不需要 cookie 的規則只有 `/certs/renew` 與 `/certs/root-crl` 兩條 Exact，都送到 `certs` 的 8080 埠，主機是 `hosts.gateway`；cookie 檢查對 `/certs/root-crl` 只在 gateway 主機之外（含帶埠號的寫法）套用；`deny-internal` 照舊是 `/internal*`；沒有路由指到 `<release>-certs-broker`。
- 既有的「`/internal` 在入口一律拒絕」與 ext_authz 的路由清單證明照樣通過。
- Flux 的 SaaS 值：gateway 主機的規則依序是換發、根 CRL、MQTT。
- 展示環境的 tunnel：chart 的表多了 `rootCrl gateway`，但 `BYPASS_PATHS` 不含它，留在 Access 後面，等 Laysi/trustiot-v2#563。

### nightly 的冒煙檢查（不需要叢集的部分）

`scripts/nightly/certs.test.ts` 新增與修改的測試都通過，包括：

- 測試根：私鑰只經 stdin 依序交給 `root sign` 與 `root crl --previous-crl none --previous-certificates none`，命令列與紀錄都沒有私鑰，暫存檔刪掉；`root crl` 失敗時私鑰也已丟掉、檔案也刪掉。
- 等待時：Istio 的 404（沒有路由）繼續等，certs 自己的 404 才算；401（cookie 檢查套上了）或 200（沒設定就給出 CRL）立刻失敗；404 沒有 `no-store` 也失敗。
- 設定之後：404 與舊的 CRL 繼續等，等到 DER 相同的那份；`Content-Type`、`no-store`、以測試根驗章與 issuer、`HEAD`、四種方法 405 與 `Allow`、`hosts.app` 與路徑之下拿不到 CRL，每一種被打開的情形都讓檢查失敗（10 種）。
- 以 chart 渲染 smoke 的 values（saas、onprem）：根 CRL 與簽發 CA 憑證在同一次升級進入同一個 ConfigMap，certs 的 Deployment 不變（不重啟）；檢查找出的路由主機就是 `TRUSTIOT_GATEWAY_HOST`。

### 整組檢查

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 沒有差異 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 沒有警告 |
| `CI=1 cargo test --locked -p certs` | 單元測試 55 個與 22 個整合測試檔全部通過（`root_crl_endpoint` 3 個） |
| `cargo run --locked -p openapi -- --check` | `api/openapi.json is up to date`（端點不在規格裡，規格沒有變） |
| `CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast` | 通過，沒有失敗的測試目標（在建置主機（x86_64 Linux）上跑，見下） |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 經入口、不帶 cookie 以 GET 取得目前的根 CRL，內容與 values 設定的相同；其他方法被拒 | chart 測試通過（路由與 cookie 檢查的例外）；服務端的行為由 `root_crl_endpoint.rs` 證明。部署後的冒煙測試已寫好、單元測試通過，實際在叢集跑的紀錄**待測**：要等協調者為 PR 加 `smoke` label 後由 CI 執行。 |
| 還沒有根 CRL 時，回應照 README「端點」寫的契約 | 通過：404、固定本文 `no root CRL is published`、`text/plain; charset=utf-8`、`no-store`（`root_crl_endpoint.rs`，含真的服務）。 |
| mesh 規則只放行這條路徑，`/internal/` 底下照舊不經入口 | 通過（chart 測試，見上）。 |
| 安全審查沒有未處理的 Critical 與 High | 通過：沒有 Critical、High、Medium；兩個 Low 的處理寫在 PR。 |
| 文件已更新；`CI=1 cargo test -p certs`、`bun test deploy/chart`、`bun test scripts/nightly` 通過；測試報告放在 `docs/test-reports/` | 通過（本報告）。 |

延後的進階驗證（另開 P2）：在展示環境實際開放這條路徑、公開路徑的負載與濫用測試、第二個用戶端實作（閘道器端由 Laysi/trustiot-v2#563 實作時再對這份契約驗一次）。
