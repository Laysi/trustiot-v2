# AI 儀表板：檢查服務、沙箱、映像與 nightly 的安全強化

對應 issue：Laysi/trustiot-v2#150（AI 儀表板）。測試日期 2026-10-27（Asia/Taipei）。

這份報告是 #150 安全審查之後，檢查服務（`services/dashboard-check`）、沙箱 runtime（`frontend/sandbox`）、檢查服務的映像（`deploy/images/dashboard-check`）與 nightly 隔離證明（`scripts/nightly/dashboard-check.ts`）的強化證據。每一項都有修正前與修正後的結果。原始輸出在 [data/2026-10-27-150-checker-hardening/](data/2026-10-27-150-checker-hardening/)。

## 環境

- **受測程式碼**：本報告所在的 commit（分支 `feat/150-y-hardening-checker`，從 #150 的整合分支開始）。「修正前」是同一個測試對修正前的程式碼跑的結果。
- **執行的地方**：開發工作站。容器引擎是 Docker，平台 `linux/arm64`；起容器的測試一次只跑一個。沒有設 `CI`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、zig 0.16.0、cargo-zigbuild 0.23.4。

## 1. 檢查之間的行程清理（PID 1）

`tests/pid1_cleanup.rs` 的主機端（建映像、起容器）只在 Linux 主機上執行（CI）。這次在開發工作站把同一個測試執行檔以 cargo-zigbuild 交叉編譯成 `aarch64-unknown-linux-musl`，用與主機端相同的容器設定直接跑容器內的部分（PID 1 的角色）。

### 指令

```sh
# 交叉編譯測試執行檔（CARGO_ZIGBUILD_ZIG_PATH 指向 .prototools 釘選的 zig）
cargo zigbuild --locked --target aarch64-unknown-linux-musl -p dashboard-check --test pid1_cleanup

# 映像：gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2
# 加上 COPY 進來的測試執行檔（ENTRYPOINT 是它），以測試主機端相同的設定執行：
docker run --name <name> --platform linux/arm64 --read-only --tmpfs /tmp:rw,mode=1777 \
  --user 65532:65532 --cap-drop ALL --security-opt no-new-privileges --network none \
  --env DASHBOARD_CHECK_PID1_TEST_ROLE=init <image>

# 清理的單元測試在 Linux 上（不是 PID 1：以 docker 的 --init 當 PID 1）
docker run --rm --init --platform linux/arm64 --read-only --tmpfs /tmp:rw,mode=1777 \
  --user 65532:65532 --cap-drop ALL --security-opt no-new-privileges --network none \
  <交叉編譯的 dashboard_check 單元測試執行檔> cleanup
```

### 結果

容器裡的 PID 1 起一個行程：它離開 session、起一個每 10 ms 寫一次計數的執行緒，再只結束主執行緒（`SYS_exit`，不是 `exit_group`）。

| | 確認 | 清理的結果 | 清理後 `/proc` 裡除了 PID 1 還有 | 結束碼 |
| --- | --- | --- | --- | --- |
| 修正前 | 該行程 `State: Z (zombie)`，另一個執行緒 `S (sleeping)`，計數 2 → 4 | `Ok(())` | `6 Z (zombie)` | 1 |
| 修正後 | 同上，計數 2 → 4 | `Ok(())` | 沒有 | 0 |

- 審查的說法成立：主執行緒結束、其他執行緒還在跑的行程，在 `/proc/<pid>/status` 顯示為 zombie，計數還在增加，表示它的程式還在執行。修正前的清理把 zombie 當成已經結束，不殺也不算殘留。
- 修正前與修正後的輸出：[pid1-cleanup-before.txt](data/2026-10-27-150-checker-hardening/pid1-cleanup-before.txt)、[pid1-cleanup-after.txt](data/2026-10-27-150-checker-hardening/pid1-cleanup-after.txt)。
- 清理的單元測試在 Linux 上 6 個全過，包括「不是 PID 1 時碰不到對所有行程的訊號」與「列出 `/proc` 裡其他所有行程」。單元測試的建置不含真的 `kill(-1, …)`，只計數。

### 結論

- 服務是 PID 1 時，清理後 PID namespace 裡只剩服務自己，主執行緒已結束的行程也一樣：通過（Linux 容器，交叉編譯的同一個測試）。
- 不是 PID 1 時走不到對所有行程的訊號：通過（單元測試，開發工作站與 Linux 容器）。
- 測試主機端在 Linux 上建映像、起容器的部分：待測，由 CI 的 `cargo test --workspace`（設了 `CI`，沒有容器引擎時失敗）跑。

## 2. nightly 隔離證明：ztunnel 在 pod 裡的埠

`scripts/nightly/dashboard-check.ts` 從儀表板檢查的 pod 多探測 ztunnel 在 pod 網路命名空間裡可能聽的埠：以 UDP 與 TCP 向 DNS 代理埠 15053（loopback 與 pod 位址）查叢集外的名稱，以及經 15001、15006、15008（loopback）連叢集外的位址，全部要沒有回應（15008 只回 TLS alert 也算拒絕）；身分服務的 pod 以同樣送 bytes 的方式向叢集 DNS 查詢，要有回應，作為對照。

### 指令

```sh
bun test scripts/nightly/dashboard-check.test.ts

# 探測 script 本身（需要 GNU coreutils 與 getent，在 Debian 容器裡跑）
docker run --rm --network none -v "$PWD:/w:ro" --tmpfs /tmp:rw,mode=1777 -e HOME=/tmp -w /w \
  oven/bun:1.3-debian bun test scripts/nightly/dashboard-check.test.ts -t "probe script"
```

### 結果

- 修正前：新的測試在舊的 `dashboard-check.ts` 上無法載入（沒有 `dnsQuery`、`ZTUNNEL_PORTS` 等），計畫裡沒有任何 ztunnel 埠的探測。
- 修正後：39 個測試 38 個通過，1 個略過（探測 script 本身需要 GNU 工具，開發工作站上略過，設了 `CI` 時必須執行）。同一個 script 測試在 Debian 容器（Bun 1.3.14）裡通過：UDP 與 TCP 送出的 bytes 完整到達（含 NUL），讀回第一個回應；連不上的 TCP 埠回報 `silent 3`。

### 結論

- 探測計畫、判定（含 TLS alert 的拒絕）與 script：通過（單元測試）。
- 在真的叢集（Istio ambient、k3s）裡全部沒有回應：待測，由 coordinator 加上 `smoke` 標籤的 nightly 執行。

## 3. 映像的 Debian 套件改走 https

`deploy/images/dashboard-check/Containerfile` 的 snapshot 來源改成 `https://snapshot.debian.org/...`，apt 以基底映像（以 digest 釘選）的 CA bundle 驗證伺服器憑證（`Acquire::https::CaInfo`）。`Check-Valid-Until "false"` 保留（snapshot 的 Release 檔保留原本的有效期限）。

### 指令

```sh
# Containerfile 的靜態規則（不建映像；DASHBOARD_CHECK_IMAGE 指向自己的標籤，所以不會重建 :image-test）
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:hardening-y bun test deploy/images/dashboard-check -t "Containerfile"

# 以 scripts/nightly/service-image.ts 的 buildServiceImage 建出 localhost/trustiot-e2e/dashboard-check:hardening-y，再跑整個映像測試
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:hardening-y bun test deploy/images/dashboard-check
```

### 結果

- 修正前：靜態規則「takes every package from the pinned Debian snapshot over https」失敗（Containerfile 是 `URIs: http://snapshot.debian.org/...`）。
- 修正後：靜態規則 6 個全過；映像建置的 apt 輸出每一筆都是 `https://snapshot.debian.org/...`（共下載 157 MB），解開的 `/rootfs` 與修正前建出的層相同（Docker 的快取命中），表示拿到的是同一批套件。
- 映像測試（manifest list `sha256:ceb0ff53a1a0fb07172d6d47c54854a74cde6ff55b25d65a07a2ff53d2da0143`，也含第 1 節的清理）：44 個全過，0 失敗；33 次檢查，每次 152–5167 ms（中位數 407 ms），容器記憶體最高讀數 167.3 MiB（上限 512Mi）；「檢查後沒有留下瀏覽器行程」通過，服務以 PID 1 的清理在真的 Chromium 之後執行。

### 結論

- 套件只經 https 從 snapshot 伺服器取得、映像測試全部通過：通過。
- 沒有另外釘選每個 `.deb` 的 SHA-256：Renovate 更新 Chromium 的 pull request 會需要手動補上每個套件的雜湊，所以不做（`deploy/images/dashboard-check/README.md`）。

## 4. 檢查之間經檔案留下的東西

確認一次檢查寫下的東西，下一次檢查的瀏覽器讀不到：瀏覽器的 `HOME`、XDG 目錄與 fontconfig 快取在哪裡，以及 pod 裡除了工作目錄、`/dev/shm`、`/dev/mqueue` 與 termination log 之外還有沒有寫得了的地方。

### 指令

```sh
# 瀏覽器的 profile 與每個環境變數都在這次檢查自己的目錄裡（清理會刪掉它）
cargo test --locked -p dashboard-check --lib a_browser_gets_the_pipes

# 以服務的使用者列出容器裡寫得了的地方：掛載點、/proc 與 /sys 以外每個寫得了的路徑、keyring 收不收 key。
# 掃描程式是 data 目錄的 writable-scan.c，以 .prototools 釘選的 zig 編成靜態執行檔：
zig cc -target aarch64-linux-musl -static -O2 -o scan/scan writable-scan.c
# 以 chart 的設定（唯讀根檔案系統、/tmp 是唯一的 volume、UID 65532、drop ALL、no-new-privileges、
# 記憶體 512Mi、沒有網路）跑在檢查服務的映像裡：
docker run --rm --platform linux/arm64 --user 65532:65532 --read-only \
  --tmpfs /tmp:rw,mode=1777,size=128m --cap-drop ALL --security-opt no-new-privileges \
  --memory 512m --network none -v "$PWD/scan:/scan:ro" --entrypoint /scan/scan \
  localhost/trustiot-e2e/dashboard-check:hardening-y
```

### 結果

- 瀏覽器的環境變數只有 `HOME`、`TMPDIR`、`XDG_CONFIG_HOME`、`XDG_CACHE_HOME`，值與 `--user-data-dir` 都在這次檢查的目錄裡；沒有設的 `XDG_DATA_HOME` 等預設在 `HOME` 底下。映像的 `/etc/fonts/fonts.conf` 的字型目錄是 `/usr/share/fonts`、`/usr/local/share/fonts`（唯讀）、XDG 資料目錄的 `fonts` 與 `~/.fonts`，快取是 `/var/cache/fontconfig`（映像裡沒有，根檔案系統唯讀所以也建不出來）、`$XDG_CACHE_HOME/fontconfig` 與 `~/.fontconfig`：會寫的都在這次檢查的目錄裡。
- 測試在修正前後的程式上都通過（這一項沒有找到缺口）；把 `XDG_CACHE_HOME` 改指到檢查目錄外面時，測試失敗，指出 `XDG_CACHE_HOME=<工作目錄>/.cache`。
- 掃描（[writable-scan.txt](data/2026-10-27-150-checker-hardening/writable-scan.txt)，程式在 [writable-scan.c](data/2026-10-27-150-checker-hardening/writable-scan.c)）：寫得了的只有 `/tmp`（工作目錄）、`/dev/shm`、`/dev/mqueue`，以及 `/dev` 的裝置檔（`null`、`zero`、`full`、`random`、`urandom`、`tty`、`pts/ptmx`，不存資料）。根檔案系統、`/etc/hosts`、`/etc/hostname`、`/etc/resolv.conf`、`/sys` 與 `/sys/fs/cgroup` 都唯讀。`add_key` 與 `keyctl` 都被拒絕（`Operation not permitted`，容器引擎預設的 seccomp）。
- Kubernetes 另外有 `/dev/termination-log`，清理已經清空它；kubelet 與容器引擎建的 `/etc/hosts`、`/etc/hostname`、`/etc/resolv.conf` 屬於 root、權限 0644，UID 65532 又沒有任何 capability，寫不了（依它們的行為，未在叢集實測，見結論）。

### 結論

- 一次檢查寫下的東西，下一次檢查的瀏覽器讀不到：通過。瀏覽器寫得了的地方只有工作目錄、`/dev/shm`、`/dev/mqueue` 與 termination log，全部由清理清空（第 1 節與既有的清理測試），沒有需要補的缺口。
- 在真的叢集（containerd 的 RuntimeDefault seccomp、kubelet 掛進來的檔案）裡重做同一個掃描：待測。這是同一件事的第二個來源，依驗證分級延後。

## 5. 顯示時的比對對抗惡意頁面

檢查服務在頁面碰不到的 isolated world 比對，本來就不受頁面替換內建物件影響（審查 B 的「Checked and sound」）。有風險的是檢視時（Laysi/trustiot-v2#462）在頁面 realm 執行的比對：比對與它呼叫的格式化（內建的 d3-format）、時間、幾何都在呼叫時才查內建（`Intl`、`Math`、`Number`、`String`、`RegExp`、`Array`、`Set`、`Map`），頁面換掉它們就能讓自己的 `tq.text` 與比對算出同一個錯的數字。內建有兩條存取路徑都要擋：物件上的方法（`Math.round`）與裸名字背後的全域繫結（`Math`）。修正：runtime 載入後、頁面 script 執行前，在 sandbox frame 的 realm 凍結這些內建物件與它們原型上的方法，並把指向它們的全域繫結設為不可寫、不可設定（`frontend/sandbox/src/harden.ts`），同時去掉頁面可見的檢查／檢視模式（`tq.mode`）。另外把持續改動的比對加上時間上限與 fail-closed（見下）。

### 指令

```sh
# realm 凍結的機制與免疫性，在 node:vm 的獨立 realm（像瀏覽器 frame 一樣有自己的內建物件）
bun test sandbox/test/harden.test.ts
# 檢視模式：一直改回錯值會被隱藏、一直以正確值更新會繼續顯示
bun test sandbox/test/runtime.test.ts -t "view mode"
# 全部沙箱單元測試（既有固定樣本的判定不變）
bun test sandbox
# 真實瀏覽器：映像測試跑每個固定樣本（含 builtin-tampering），並確認既有 d3 頁面在凍結下仍正常
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:hardening-y bun test deploy/images/dashboard-check
```

### 結果

- 凍結前後對照（[harden-before-after.txt](data/2026-10-27-150-checker-hardening/harden-before-after.txt)）：沒有凍結時，頁面換掉 `Intl.NumberFormat`、`Math.round`、`RegExp.prototype.exec` 與全域繫結 `globalThis.Math`、`globalThis.Array`，會讓在呼叫時查內建的比對算出錯的值（`literal` 變成 `null`，數字沒被抓到；`formatted`、`rounded` 都錯），也就能騙過比對；凍結後同樣的替換完全無效（`immune: true`）。
- `harden.test.ts`：7 個全過。在獨立 realm 裡確認：換掉靜態方法或被鎖住的全域繫結在 strict mode 丟例外、換掉原型方法被凍結的 setter 擋下，兩種都到不了比對；指向內建的全域繫結（`Math`、`Array`、`Intl`、`Number`、`parseInt`、`JSON`）設成不可寫不可設定，其他全域與頁面自己新增的全域不受影響；覆寫繼承屬性的合法寫法（`obj.toString`、`err.name`、子類別方法、`for...of`、`Map.set`、在陣列上加自訂屬性）仍可用；全域物件仍可擴充；重複呼叫安全；凍結後 d3 仍能畫比例尺、`nice`、扇形與格式化。
- `runtime.test.ts` 的檢視模式：一直把 runtime 改正的值又改回錯的頁面，在 `MAX_CORRECTION_ROUNDS`（10）輪內整份文件被隱藏，之後不再顯示；一直以正確值重畫（比隱藏門檻更多次）的頁面不會被隱藏、標記也沒有 `data-tq-flag`。全部沙箱單元測試 371 個全過（既有固定樣本的判定不變）。
- 映像測試（manifest list `sha256:75287daeb69eb00429cadf706ef56f6e8dac181e8bef10f41d47e9e55d560078`）：45 個全過，0 失敗；34 次檢查，每次 142–5177 ms（中位數 430 ms），容器記憶體最高讀數 100.5 MiB（上限 512Mi）。新的 `builtin-tampering`（替換上述每個內建物件後畫出錯的標記值與打字的數字）在真實 Chromium 裡被判 `mismatch` 與 `literal_number`；`correct`、`pie` 等既有 d3 頁面在凍結的 realm 下判定不變，表示凍結沒有擋到合法頁面。

### 結論

- runtime 載入之後，頁面除了改 DOM（比對下一次會看到）以外改不了顯示時的比對結果：通過（`harden` 單元測試在真實 realm、`builtin-tampering` 在真實瀏覽器）。
- 持續改動有上限、一直改回錯值會 fail closed 隱藏、一直以正確值更新會繼續顯示：通過（檢視模式單元測試）。
- 判定的可信度以 renderer 為限（攻破 renderer 能讓自己這次的檢查通過，但到不了別次、出不了 pod）：寫入 `services/dashboard-check/README.md`「威脅模型」第 5 點與 `docs/architecture/ai-assistant.md`「儀表板」第 4 點。
