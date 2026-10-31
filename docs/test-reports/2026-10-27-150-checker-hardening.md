# AI 儀表板：檢查服務、沙箱、映像與 nightly 的安全強化

對應 issue：Laysi/trustiot-v2#150（AI 儀表板）。測試日期 2026-10-27（Asia/Taipei）；第二次審查之後的修正（第 7 節）在 2026-10-27 至 2026-10-30 測試，第 2、5 節的更正也在 2026-10-30；第三次審查之後的修正（第 8 節）在 2026-10-30 至 2026-10-31 測試。

這份報告是 Laysi/trustiot-v2#150 安全審查之後，檢查服務（`services/dashboard-check`）、沙箱 runtime（`frontend/sandbox`）、檢查服務的映像（`deploy/images/dashboard-check`）與 nightly 隔離證明（`scripts/nightly/dashboard-check.ts`）的強化證據。每一項都有修正前與修正後的結果。原始輸出在 [data/2026-10-27-150-checker-hardening/](data/2026-10-27-150-checker-hardening/)。

## 環境

- **受測程式碼**：本報告所在的 commit（分支 `feat/150-y-hardening-checker`，從 Laysi/trustiot-v2#150 的整合分支開始）。「修正前」是同一個測試對修正前的程式碼跑的結果。
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

# 探測 script 本身（需要 GNU coreutils 與 getent，在 Debian 容器裡跑；.prototools 釘選的 Bun 1.4.2，映像以 digest 釘選）
docker run --rm --network none -v "$PWD:/w:ro" --tmpfs /tmp:rw,mode=1777 -e HOME=/tmp -w /w \
  oven/bun:1.4.2-debian@sha256:4f6e31d1a54d6a3dd312daef655fc998101b5043d52e12592ac293ef04b9bc73 \
  bun test scripts/nightly/dashboard-check.test.ts -t "probe script"
```

2026-10-27 第一次執行時用的是沒有釘選的 `oven/bun:1.3-debian`（Bun 1.3.14），與 `.prototools` 釘選的 Bun 1.4.2 不同。2026-10-30 以上面釘選的映像重跑。

### 結果

- 修正前：新的測試在舊的 `dashboard-check.ts` 上無法載入（沒有 `dnsQuery`、`ZTUNNEL_PORTS` 等），計畫裡沒有任何 ztunnel 埠的探測。
- 修正後：39 個測試 38 個通過，1 個略過（探測 script 本身需要 GNU 工具，開發工作站上略過，設了 `CI` 時必須執行）。同一個 script 測試在 Debian 容器裡通過：2026-10-27 用 Bun 1.3.14（沒有釘選，見上），2026-10-30 用釘選的 Bun 1.4.2 重跑也通過（[round2-nightly-bun-1.4.2.txt](data/2026-10-27-150-checker-hardening/round2-nightly-bun-1.4.2.txt)）。UDP 與 TCP 送出的 bytes 完整到達（含 NUL），讀回第一個回應；連不上的 TCP 埠回報 `silent 3`。

### 結論

- 探測計畫、判定（含 TLS alert 的拒絕）與 script：通過（單元測試）。
- 在真的叢集（Istio ambient、k3s）裡全部沒有回應：待測，由 coordinator 加上 `smoke` 標籤的 nightly 執行。
- 2026-10-31 更新：Laysi/trustiot-v2#884 帶 `smoke` 的 nightly（head `c2b99fb`，run 41209057473）第一次在叢集上拿到結果。DNS 代理埠在 loopback 以 UDP 與 TCP 都回了沒有任何紀錄的錯誤（29 與 31 bytes），不是沒有回應；pod 位址的 15053、15001、15006 都沒有回應，15008 只回 TLS alert。ztunnel 在每個 pod 的 loopback 都開 DNS 代理，轉送的查詢從 pod 自己的網路命名空間出去、被 NetworkPolicy 擋下，所以判定改成「沒有回應，或只回沒有紀錄的 SERVFAIL／REFUSED」，並加上身分服務的 pod 經自己 loopback 上的代理查得到的對照（docs/architecture/ci-release.md 的儀表板檢查第 1、2 步）。

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

檢查服務在頁面碰不到的 isolated world 比對，本來就不受頁面替換內建物件影響（審查 B 的「Checked and sound」）。有風險的是檢視時（Laysi/trustiot-v2#462）在頁面 realm 執行的比對：比對與它呼叫的格式化（內建的 d3-format）、時間、幾何都在呼叫時才查內建（`Intl`、`Math`、`Number`、`String`、`RegExp`、`Array`、`Set`、`Map`），頁面換掉它們就能讓自己的 `tq.text` 與比對算出同一個錯的數字。內建有兩條存取路徑都要擋：物件上的方法（`Math.round`）與裸名字背後的全域繫結（`Math`）。修正：runtime 載入後、頁面 script 執行前，在 sandbox frame 的 realm 凍結這些內建物件與它們原型上的方法，並把指向它們的全域繫結設為不可寫、不可設定（`frontend/sandbox/src/harden.ts`），同時去掉頁面可見的檢查／檢視模式（`tq.mode`）。另外把持續改動的比對加上時間上限與隱藏（見下）。

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
- 更正（2026-10-30）：映像測試是檢查模式。檢查模式的比對在 isolated world 執行，本來就不受頁面的內建物件影響，所以 `builtin-tampering` 在 Chromium 的判定不能證明顯示時的比對免疫。它在 Chromium（V8）證明的是兩件事：凍結後，頁面替換內建物件沒有讓 runtime 壞掉，頁面照常畫完，由 isolated world 判出錯的值；以及既有 d3 頁面在凍結下判定不變。修正前（分支起點 `e397c9d` 的映像）同一頁讓 runtime 自己壞掉（`script_error`：Zod 的 schema 錯誤與 `Math.ceil is not a function`，以及 `not_rendered`），一樣是 `failed`（[round2-builtin-tampering-before.txt](data/2026-10-27-150-checker-hardening/round2-builtin-tampering-before.txt)）。顯示時的比對在頁面的 realm 執行，它的免疫只在 `harden.test.ts` 與上面的對照證明，兩者都跑在 Bun 的 `node:vm`，引擎是 JavaScriptCore，不是 Chromium 的 V8。

### 結論

- runtime 載入之後，頁面除了改 DOM（比對下一次會看到）以外改不了顯示時的比對結果：在 JavaScriptCore 的獨立 realm 通過（`harden` 單元測試，`node:vm`）；在 Chromium 的頁面 realm 跑顯示時的比對：待測，Chromium 裡目前只證明凍結不會讓 runtime 壞掉、合法 d3 頁面照常判定（見上方更正）。顯示時的比對只在頁面自己的文件裡改正與標記，不是強制的邊界；把結果強制套用到顯示是外層頁面的事（Laysi/trustiot-v2#462，`frontend/sandbox/README.md`「保證」）。
- 持續改動有上限、一直改回錯值會隱藏、一直以正確值更新會繼續顯示：通過（檢視模式單元測試）。
- 判定的可信度以 renderer 為限（攻破 renderer 能讓自己這次的檢查通過，但到不了別次、出不了 pod）：寫入 `services/dashboard-check/README.md`「威脅模型」第 5 點與 `docs/architecture/ai-assistant.md`「儀表板」第 4 點。

## 6. 文字標記：畫出來的文字要等於 textContent

文字標記原本只比 `textContent` 等於 `tq.format(值)`。但 `textContent` 相同、畫出來不同的手法有：標記有子元素、被藏起來（`display: none`、`visibility: hidden`、透明色）、`unicode-bidi` 重排數字、有裁切的 `overflow` 把數字切掉（刪節號也是）、`::first-letter`/`::first-line` 把第一個字改小、藏起來或變透明、整個標記 `font-size: 0` 零大小。修正：文字標記除了 `textContent` 相符，還要是單一文字節點、沒有上述遮蔽，否則判 `mismatch`（`frontend/sandbox/src/compare.ts` 的 `concealsText`；需要版面的方框大小與裁切只在有版面的地方檢查，`display: contents` 這種沒有方框但文字照顯示的不算被藏）。

### 指令

```sh
bun test sandbox/test/compare.test.ts   # concealsText 的每條規則，含有/無版面
bun test sandbox/test/fixtures.test.ts  # 單元可判的文字標記樣本
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:hardening-y bun test deploy/images/dashboard-check
```

### 結果

- `compare.test.ts`：新增的規則測試通過——子元素、bidi override、`display: none`、`visibility: hidden`、透明色、`::first-letter` 改字級或變透明都判 `mismatch`；`concealsText` 在有版面時判出裁切（內容超出有 `overflow` 的方框）與零大小方框（`font-size: 0`），沒有版面時這兩項不判（所以單元測試裡正常的標記仍通過），`display: contents` 即使方框讀為 0 也不判。沙箱單元測試全部通過。
- 九個新固定樣本：`text-mark-child`、`text-mark-bidi`、`text-mark-hidden`、`text-mark-transparent` 在 happy-dom（單元）與瀏覽器都判 `mismatch`；`text-mark-first-letter`、`text-mark-first-letter-color`、`text-mark-clipped`、`text-mark-ellipsis`、`text-mark-font-size-zero` 需要真實版面或 `::first-letter`，只在瀏覽器判。
- 映像測試（manifest list `sha256:1c09563da6e717d9da9cacd589333b82e8bba81ba67a704c325c1218eb3696fb`）：54 個全過，0 失敗；43 次檢查，每次 140–5171 ms（中位數 425 ms），容器記憶體最高讀數 96.4 MiB（上限 512Mi）。九個文字標記樣本在真實 Chromium 都判 `mismatch`，既有樣本判定不變。

### 結論

- 文字標記畫出來的文字要等於它的值、是單一文字節點、不被重排／裁切／藏起來：通過（單元 + 瀏覽器固定樣本）。
- 殘留：祖先元素造成的裁切或 bidi 重排、`clip-path`／`mask`、透明度以外的濾鏡與 `mix-blend-mode` 等只影響畫面的手法仍檢查不到（`frontend/sandbox/README.md`「檢查不到的部分」）。透明度、填色與變形在第 7 節補上；這些手法在第 8 節改以允許清單判定。

## 7. 第二輪：第二次審查之後的修正

第二次審查（re-review）之後的修正，同一個分支：`764b8cb`（W1）、`e16a49c`（W2、W3）、`19df7f0`（W7 與顯示時比對的說明更正）、`553d267`（W4）、`0e64d09`（W5、W6），以及本節所在的 commit（W8 的用詞與本報告）。顯示時比對只在頁面自己的文件裡改正與標記，不是強制的邊界；把結果強制套用到顯示由 Laysi/trustiot-v2#462 處理，這一輪只更正文件。

### 指令

```sh
# 沙箱與前端
bun run --cwd frontend api:check && bun run --cwd frontend typecheck && bun run --cwd frontend lint
bun run --cwd frontend test && bun run --cwd frontend build && bun run --cwd frontend check:dist

# 檢查服務，與 Linux 上的清理（交叉編譯，執行方式同第 1 節；單元測試在 docker run --init 後面，不是 PID 1）
cargo test --locked -p dashboard-check
cargo zigbuild test --locked --no-run --target aarch64-unknown-linux-musl -p dashboard-check --lib --test pid1_cleanup

# 映像在 init 行程後面、以 TRUSTIOT__CLEANUP=browser 啟動（chart 的其他設定同第 3 節）
docker run --init ... localhost/trustiot-e2e/dashboard-check:hardening-y
docker run --init --env TRUSTIOT__CLEANUP=browser ... localhost/trustiot-e2e/dashboard-check:hardening-y
docker run --env TRUSTIOT__CLEANUP=browser ... localhost/trustiot-e2e/dashboard-check:hardening-y

# chart、nightly 與 script 的型別檢查
bun test deploy/chart
bun test scripts/nightly
bun run --cwd scripts typecheck
docker run --rm --network none -v "$PWD:/w:ro" --tmpfs /tmp:rw,exec,mode=1777 -e HOME=/tmp -w /w \
  oven/bun:1.4.2-debian@sha256:4f6e31d1a54d6a3dd312daef655fc998101b5043d52e12592ac293ef04b9bc73 \
  bun test scripts/nightly/dashboard-check.test.ts scripts/nightly/pod-pids-limit.test.ts

# 映像（修正前的對照用分支起點的 localhost/trustiot-e2e/dashboard-check:e397c9d，加 -t 只跑新的固定樣本）
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:hardening-y bun test deploy/images/dashboard-check
```

### 結果

| 項目 | 修正前 | 修正後 | 測試 |
| --- | --- | --- | --- |
| W1：版面由呼叫端決定，頁面的 CSS 關不掉版面規則（審查的 N2） | 重現：`text-mark-root-zero`（根元素的方框設成 0，內容照常顯示並被裁切）判 `passed`（[round2-root-zero-before.txt](data/2026-10-27-150-checker-hardening/round2-root-zero-before.txt)） | 判 `failed`（`mismatch`） | `compare.test.ts`、`runtime.test.ts`、固定樣本 |
| W2：透明度與填色 | `text-mark-faint` 判 `passed`（[round2-text-and-pie-before.txt](data/2026-10-27-150-checker-hardening/round2-text-and-pie-before.txt)） | 文字的顏色、`-webkit-text-fill-color`、SVG 的 `fill` 乘 `fill-opacity`、各層祖先的 `opacity` 與 `filter: opacity()` 相乘後低於 `MIN_TEXT_ALPHA`（0.1）判 `mismatch`：7 個 | `compare.test.ts`「faint text」、固定樣本 |
| W3：變形 | `text-mark-turned` 與 `wrong-pie-stretched` 判 `passed`（同上） | 文字的轉向超過 `MAX_TEXT_TURN_DEG`（45°）、鏡像、伸縮比超過 `MAX_TEXT_STRETCH`（4）、實際字級（`font-size` 乘縮放，含 `zoom`）小於 `MIN_TEXT_SIZE_PX`（2 px）、字距小於 `MIN_LETTER_SPACING_EM`（−0.2 em）、逐字定位（`x`／`y`／`dx`／`dy` 清單、`rotate`、`textLength`、`textPath`）都判 `mismatch`：8 個；扇形的伸縮比超過 `MAX_SLICE_STRETCH`（1.01）判 `mark_geometry`：3 個。`perspective` 與 `transform-style: preserve-3d` 一律不通過。d3 常見的 −45° 軸標籤（`slanted-labels`）照常通過 | `compare.test.ts`「turned, mirrored and stretched text」、`dom.test.ts`、固定樣本 |
| W4：容器裡不是 PID 1 時拒絕檢查 | 分支起點的映像在 `docker run --init` 後面照常啟動，`whole_container_cleanup: false`（[round2-no-pid1.txt](data/2026-10-27-150-checker-hardening/round2-no-pid1.txt)） | 新設定 `TRUSTIOT__CLEANUP`（預設 `container`，chart 固定設定）：不是 PID 1 就拒絕啟動；`browser` 在 PID 1 或任何容器的跡象下也拒絕，結束碼 1（同一個檔案）。chart 設 `shareProcessNamespace`、`hostPID`、`hostIPC`、`hostNetwork` 或在 env 設 `TRUSTIOT__CLEANUP` 都渲染失敗 | `config.rs`、`cleanup.rs` 的單元測試（開發工作站與 Linux 容器）、`pid1_cleanup`（[round2-pid1-cleanup.txt](data/2026-10-27-150-checker-hardening/round2-pid1-cleanup.txt)）、`chart.test.ts`、映像測試的三種拒絕 |
| W5：ztunnel 的埠與 DNS 對照 | outbound、inbound、HBONE 埠只測 loopback；DNS 代理埠的沉默可能是叢集 DNS 沒有上游 | 三個埠也測 pod 的位址；身分服務的 pod 以 resolver 與直接送給叢集 DNS 的 UDP 查詢都查得到 `example.com.`（回應要是這個查詢的、rcode 0、至少一筆紀錄） | `dashboard-check.test.ts`（計畫、判定、DNS 標頭、失敗案例） |
| W6：kubelet 實際生效的 `podPidsLimit` | 只在建 k3d 叢集時設定，沒有讀回 | `scripts/nightly/pod-pids-limit.ts` 經節點 proxy 讀每個 kubelet 的 `/configz`，不是 16384 或讀不到就失敗並列出節點；smoke 在部署儀表板檢查的型態執行；可以對其他叢集執行（結束碼 0／1／2） | `pod-pids-limit.test.ts`（含以假的 kubectl 執行整個指令） |
| W7：重新比對的上限與檢視模式 | 拿掉 `MAX_RECHECK_DELAY_MS` 時，持續改動的頁面一直不被比對（[round2-recheck-bound-without.txt](data/2026-10-27-150-checker-hardening/round2-recheck-bound-without.txt)）；沒有凍結迭代器原型時，13 個內建物件沒有被凍結 | 持續改動的頁面在上限內被比對、錯值被換掉；每個迭代器原型與 generator、async function 的原型都凍結 | `runtime.test.ts`、`harden.test.ts` |
| W8：用詞 | 「不帶 cookie」、略過條件寫成某一種工作站、第 2、5 節 | 只寫實測的 `SameSite=Lax` session cookie（`SameSite=None` 的 cookie 會帶）；略過條件寫成缺 GNU coreutils 或 getent 的地方；第 2、5 節更正 | — |

- Chromium 的實測（[round2-chromium-probes.txt](data/2026-10-27-150-checker-hardening/round2-chromium-probes.txt)，2026-10-27，映像裡的 `chromium-headless-shell`）：SVG 的 `getScreenCTM()` 回傳 `SVGMatrix`，不是 `DOMMatrix`（以 `DOMMatrix` 的 getter 讀會丟 `Illegal invocation`），所以比對依物件的原型鏈選讀取方式；計算後的 `rotate` 是 `180deg`、`1 1 0 57.2958deg` 這種寫法，`scale` 是 `0.5`，字距是 px，`filter` 保留 `opacity(0.05) blur(1px)`；SVG 的 CTM 含 HTML 祖先的變形與 `zoom`，計算後的 `font-size` 不含 `zoom`，HTML 的 `zoom` 是元素自己的值。
- 新的常數（Laysi/trustiot-v2#694 的調整對象）：`MIN_TEXT_ALPHA` 0.1、`MAX_TEXT_TURN_DEG` 45、`MAX_TEXT_STRETCH` 4、`MIN_TEXT_SIZE_PX` 2、`MIN_LETTER_SPACING_EM` −0.2、`MAX_SLICE_STRETCH` 1.01（`1 + RELATIVE_TOLERANCE`）。`MAX_TEXT_TURN_DEG` 45 會拒絕轉 90° 的直式軸標籤。
- 單元測試：前端 617 個全過；`cargo test -p dashboard-check` 49 個加 `pid1_cleanup`；Linux 容器裡清理與設定的單元測試 12 個全過，`pid1_cleanup` 的 PID 1 部分通過；`bun test deploy/chart` 699 個全過；`bun test scripts/nightly` 432 個通過、1 個略過、15 個失敗，失敗的與第一輪之前相同，是只在本機環境失敗、第一輪之前就有的 TLS 與 openssl 測試；釘選的 Bun 1.4.2 的 Debian 容器裡，`dashboard-check.test.ts` 與 `pod-pids-limit.test.ts` 54 個全過，探測 script 本身也執行並通過（[round2-nightly-bun-1.4.2.txt](data/2026-10-27-150-checker-hardening/round2-nightly-bun-1.4.2.txt)）。那個容器的 `/tmp` 要允許執行：`pod-pids-limit.test.ts` 把假的 kubectl 放在暫存目錄，Docker 的 `--tmpfs` 預設 `noexec` 時那三個測試失敗。
- 映像測試，每個階段都以自己的程式建出 `localhost/trustiot-e2e/dashboard-check:hardening-y`：

| 階段 | 通過 | 檢查次數 | 每次檢查（中位數） | 容器記憶體最高讀數（上限 512Mi） |
| --- | --- | --- | --- | --- |
| W1 | 55 | 44 | 141–5153 ms（422 ms） | 98.5 MiB |
| W2、W3 | 59 | 48 | 142–5190 ms（428 ms） | 99.0 MiB |
| W7 | 59 | 48 | 139–5172 ms（423 ms） | 96.9 MiB |
| W4（最後的程式） | 62 | 48 | 137–5175 ms（424 ms） | 99.6 MiB |

### 結論

- W1、W2、W3、W7：通過（單元測試與真實 Chromium 的固定樣本；修正前的缺口都重現過）。
- W4：通過（設定、Linux 容器裡的 PID 1 與非 PID 1、映像的三種拒絕、chart 的渲染規則）。在叢集裡共用 process namespace 的 pod 拒絕啟動：待測，chart 不讓這個設定渲染出來，叢集裡的對照要另外做。
- W5、W6：計畫、判定與指令通過（單元測試）；在真的叢集（Istio ambient、k3s）裡 pod 位址上的 ztunnel 埠沒有回應、`example.com.` 的對照查得到、`/configz` 讀得到而且是 16384：待測，由加上 `smoke` 標籤的 nightly 執行。
- 顯示時比對在 Chromium（V8）的頁面 realm 的免疫：待測（見第 5 節的更正），檢視頁面在 Laysi/trustiot-v2#462。

## 8. 第三輪：第三次審查之後的修正

第三次審查之後的修正，同一個分支，本節所在的 commit 與它前一個 commit。前兩輪的文字標記規則是一份逐輪加長的拒絕清單，每次審查都再找到一個讓 `textContent` 不變、畫出來卻轉了向、換了順序、被裁掉或藏起來的 CSS 屬性。這一輪改成計算後樣式的允許清單（`frontend/sandbox/src/compare.ts` 的 `PLAIN_PATH` 與 `PLAIN_MARK`）：清單上的屬性在文字標記上，以及會繼承或作用到子孫畫面的、在每一層祖先上，都要是初始值或不改變字形的值，其他值一律判 `mismatch`。清單以外的屬性，逐一對照 Chromium 計算後樣式列出的每個屬性做了決定，寫在同一段程式註解；合法的儀表板（zh-TW 與 en 的文字、d3 的輸出）用不到的值都拒絕。

- 允許清單（T1）：標記與每層祖先的 `writing-mode` 只能是 `horizontal-tb`、`direction` 只能是 `ltr`，`text-orientation`、`offset-path` 維持初始值，`unicode-bidi` 不能是會重排文字的值（標記本身要維持沙箱樣式設定的 `isolate`）；`contain` 不能含 `paint`、`content`、`strict`，`content-visibility` 只能是 `visible`；`clip-path`、遮罩、`opacity()` 以外的 `filter`、`mix-blend-mode`、`-webkit-box-reflect`、取代元素的 `content`、`-webkit-line-clamp`、錨點定位的隱藏、可編輯的文字，以及刪除線等文字裝飾線（祖先的裝飾線也畫過標記的文字）也不行。標記本身的 `-webkit-text-security`、`text-combine-upright`、`text-shadow`、文字描邊、`font-size-adjust`、`text-emphasis-style`、生效中的文字裝飾、`initial-letter` 要維持初始值；`text-indent` 不能是負的，標記不是行內元素時要是 0（它會縮排自己的第一行）。祖先的 `::first-letter`、`::first-line` 也要維持原樣（之前只看標記自己的），`::selection` 只能換不淡的顏色；頁面註冊的自訂 highlight 不支援；標記文字裡的 bidi 控制字元、被拆到兩行的數字（段數多到讀不完也算）、瀏覽器略過不畫的內容（`checkVisibility`）、在會裁切的祖先的 padding box 以外的標記（祖先在畫面上轉向、傾斜或鏡像時讀不出裁切範圍，一律算）都判 `mismatch`；SVG 文字的描邊只有讀得出全透明或沒有寬度時才不算，漸層、圖樣與讀不懂的值都算描邊。絕對定位的方框不能錨定到別的方框；`position-anchor: auto`（較早版本規格的初始值）只在其他方框上允許，以免其他瀏覽器的一般頁面在檢視時被誤判。
- `<use>`（T2）：參照的元素本身是或含有 SVG 文字、`foreignObject` 或文字標記，或在標記之下時，不論 `<use>` 在哪裡（`tq.decor` 之下也一樣）都判 `unmapped`，檢視時隱藏；`href` 與 `xlink:href`、照寫的與百分比解碼後的 id 都比對；參照不是本文件片段（`#id`）的 `<use>` 讀不出畫的是什麼，一律判 `unmapped`。濾鏡的 `feImage` 判為圖片（不支援）。
- 不在原處畫出的 SVG 容器（T3）：`marker`、`pattern`、`clipPath`、`mask`、`symbol`、`defs`、漸層與濾鏡裡的文字標記判 `mismatch`，圖形標記判 `mark_geometry`。讀 `getScreenCTM` 之前，以擷取的 `SVGGraphicsElement` 介面 getter 做 brand check，不靠頁面改得了的 `instanceof` 或原型鏈；`getScreenCTM` 丟例外時當作讀不到，不讓比對中斷。
- 審查以外自己找到的：`<details>` 的 `::details-content` 能把裡面的內容轉向、變淡、模糊、裁切或鏡像，這些不出現在任何元素的計算後樣式；現在把它當成一層祖先套用相同的規則，標記本身是 `<details>` 時判 `mismatch`。`<marquee>` 一直移動內容，文件卻沒有回報任何動畫，比照 SVG 動畫判渲染逾時。捲動容器的 `::scroll-button()` 內容、scroll marker group，以及 `hyphenate-character`、`text-emphasis-style` 畫出的數字，比照其他產生的內容判定。
- 提交前的程式審查又找到的（都已修正，各有單元測試，畫面上的也加進固定樣本）：較早版本規格的 `position-anchor: auto`、SVG 文字以漸層或圖樣描邊、段數多到讀不完的文字、會裁切的祖先的邊框與轉向、祖先的文字裝飾線、縮排到畫面外的 block 標記、參照其他網址的 `<use>`。這些都已寫進上面兩點。
- 文件（T4）：`frontend/sandbox/README.md` 與 `runtime.ts` 改寫成檢視時的隱藏只在頁面自己的文件裡，確實擋下是 Laysi/trustiot-v2#462 的範圍；chart 與 `deploy/host` 的 README 寫明 pod 一直沿用建立時的 pids 上限，kubelet 的設定改變後要重新啟動已經在跑的儀表板檢查 Deployment，而且 `pod-max-pids` 要加進既有的 `kubelet-arg` 清單、不能再寫第二個鍵；第 5、7 節的用詞調整；`frontend/sandbox/README.md`「檢查不到的部分」以概括的說法列出剩下的殘留。

### 指令

```sh
# 沙箱與前端
bun run --cwd frontend api:check && bun run --cwd frontend typecheck && bun run --cwd frontend lint
bun run --cwd frontend test && bun run --cwd frontend build && bun run --cwd frontend check:dist

# chart 與主機安裝程式（README 的改動）
bun test deploy/chart
bun install --cwd deploy/host --frozen-lockfile && bun run --cwd deploy/host test

# 映像：以 scripts/nightly/service-image.ts 的 buildServiceImage 建出 localhost/trustiot-e2e/dashboard-check:hardening-y
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:hardening-y bun test deploy/images/dashboard-check

# 修正前的對照：第二輪的沙箱（git archive 234435e 的 frontend，以 bun tools/build-sandbox.ts 建出）
# 覆蓋這一輪映像的 /srv/www/sandbox/（檢查服務的執行檔這一輪沒有改），標成另一個標籤
DASHBOARD_CHECK_IMAGE=<那個標籤> bun test deploy/images/dashboard-check

# Chromium 的實測：探測頁在映像的 chromium-headless-shell 以 --dump-dom 執行（設定見資料檔的開頭）
```

### 結果

修正前是第二輪的沙箱，修正後是這一輪的沙箱，都在真實 Chromium（映像）裡判定（[round3-before.txt](data/2026-10-27-150-checker-hardening/round3-before.txt)、[round3-before-final.txt](data/2026-10-27-150-checker-hardening/round3-before-final.txt)、[round3-after.txt](data/2026-10-27-150-checker-hardening/round3-after.txt)）。審查的每個案例在修正前都重現。

| 固定樣本 | 審查項目 | 修正前 | 修正後 |
| --- | --- | --- | --- |
| `text-mark-writing-mode` | F1 | `passed`：直式與 sideways 的 `writing-mode`（標記自己的或祖先的，HTML 與 SVG 文字）把數字轉 90° 而通過 | 6 個標記判 `mismatch`，橫式的照常通過 |
| `text-mark-motion-path` | F1 | `passed`：`offset-path` 把數字翻轉而通過 | 3 個 `mismatch` |
| `text-mark-rtl` | F2 | `passed`：負值在 `direction: rtl` 下負號跑到數字後面 | 4 個 `mismatch`，保持隔離的照常通過 |
| `text-mark-contained` | F3、F8 | `passed`：`contain: paint`、`strict` 裁掉數字，`content-visibility`、關起來的 `<details>`、錨點定位讓數字不畫 | 7 個 `mismatch`，只有 layout 與 style containment 的照常通過 |
| `text-mark-glyphs` | F8 | 只有負的 `text-indent` 判 `mismatch`；`-webkit-text-security`、強調符號、描邊、`font-size-adjust`、`text-fit`、刪除線、`content`、`initial-letter`、`text-combine-upright`、拆行、祖先的 `::first-letter`／`::first-line`、祖先的刪除線、縮排到畫面外的 block 標記都通過 | 15 個 `mismatch`，強調符號畫的「8」另判 `literal_number` |
| `text-mark-copies` | F8 的同類 | `passed`：`text-shadow`、`-webkit-box-reflect`、`drop-shadow`、祖先的模糊與倒影、`mix-blend-mode` | 6 個 `mismatch`，只以 `filter: opacity()` 稍微變淡的照常通過 |
| `use-copies-text` | F4 | `passed`：真的標記被 SVG 裁掉，`tq.decor` 下的 `<use>` 畫出翻轉的副本；另一個標記由 `feImage` 再畫一次 | 被裁掉的標記 `mismatch`、`<use>` 判 `unmapped`、`feImage` 判不支援的圖片 |
| `text-mark-unrendered` | F6 | HTTP 500（`internal`）：比對丟例外 | 判定：文字標記 6 個 `mismatch`、圖形標記 `mark_geometry`，容器外的照常通過 |
| `text-mark-highlighted` | 自己找到的 | `passed`：`::selection` 與自訂 highlight 把數字的第一個字變透明 | 該標記 `mismatch`，highlight 判不支援的產生內容 |
| `scroll-pseudo-content` | 自己找到的 | `passed`：scroll marker 與 `::scroll-button()` 畫出數字 | 不支援的產生內容與 `literal_number`「777」 |
| `text-mark-details-content` | 自己找到的 | 只有 SVG 文字（`getScreenCTM` 含 `::details-content` 的變形）判 `mismatch`；HTML 的標記被轉向、變淡、模糊、裁切、鏡像都通過 | 8 個 `mismatch`，在 summary 裡的照常通過 |
| `marquee` | 自己找到的 | `passed`：內容移出畫面 | 渲染逾時 |
| `view-transition` | 對照 | 渲染逾時（既有的動畫規則已經擋下） | 不變 |
| `text-mark-plain-styles` | 誤判對照 | `passed` | `passed`：layout containment 加 `overflow: hidden` 的卡片、等寬數字、大寫標籤、段落縮排、負值、`filter: opacity()`、打開的 `<details>` 裡可捲動的表格、被裁切的 SVG 裡的文字都照常通過 |

- 全部固定樣本：映像測試 76 個全過（62 個固定樣本，每個一次檢查；以這一輪 commit 的建置跑兩次）。同一個測試以修正前的沙箱跑，64 個通過、12 個失敗：失敗的是上表兩個對照以外的 12 個樣本，判定與上表的修正前相同；既有的 48 個樣本、兩個對照與其他測試都通過。合法頁面的判定不變：`correct`、`pie`、`slanted-labels` 等 d3 頁面與 `text-mark-plain-styles` 照常通過。
- Chromium 的實測（[round3-chromium-probes.txt](data/2026-10-27-150-checker-hardening/round3-chromium-probes.txt)，映像裡的 Chromium 156.0.8078.47）：沒有設定樣式的 `::first-letter`、`::first-line`、`::selection`，不繼承的屬性是初始值，繼承的屬性等於元素自己的，block、inline-block、inline 的 HTML 元素與 SVG 的 `g`、`text` 都一樣；所以要求這些 pseudo-element 維持初始值不會拒絕一般頁面，單元測試的 harness 也照這個行為模擬。`getComputedStyle(details, "::details-content")` 讀得到它的變形、透明度、濾鏡與裁切，這一輪的比對判出被它改過的 5 個標記，沒有改過的照常通過。祖先畫的刪除線或底線（連結的底線也是）不出現在標記自己的 `text-decoration-line`，Chromium 只以專有的 `-webkit-text-decorations-in-effect` 反映，inline-block 的標記不承接；比對直接檢查標記與每層祖先的 `text-decoration-line`，不依賴這個專有屬性：探測頁上有祖先裝飾線的 5 個標記都判 `mismatch`（不承接的 inline-block 也算，寧可拒絕），沒有裝飾線的照常通過。
- 映像測試的時間與記憶體（以這一輪的程式建出 `localhost/trustiot-e2e/dashboard-check:hardening-y`，上限 512Mi；第 1 到 6 次是提交前審查最後幾項修正之前的建置，第 7、8 次是這一輪 commit 的建置）：

| 次序 | 沙箱 | 通過 | 檢查次數 | 每次檢查（中位數） | 容器記憶體最高讀數 |
| --- | --- | --- | --- | --- | --- |
| 1 | 這一輪 | 76 | 62 | 143–5166 ms（426 ms） | 131.2 MiB |
| 2 | 修正前 | 64（12 個失敗） | 62 | 137–5176 ms（423 ms） | 100.9 MiB |
| 3 | 這一輪 | 76 | 62 | 147–5154 ms（417 ms） | 99.9 MiB |
| 4 | 修正前 | 64（12 個失敗） | 62 | 146–5228 ms（429 ms） | 104.7 MiB |
| 5 | 這一輪 | 76 | 62 | 146–5187 ms（427 ms） | 126.7 MiB |
| 6 | 修正前 | 64（12 個失敗） | 62 | 143–5176 ms（421 ms） | 99.2 MiB |
| 7 | 這一輪 | 76 | 62 | 151–5163 ms（429 ms） | 100.7 MiB |
| 8 | 這一輪 | 76 | 62 | 142–5171 ms（422 ms） | 101.1 MiB |

- 每次檢查的時間不變。記憶體最高讀數偶爾出現高峰：這一輪在 99.9 到 131.2 MiB 之間，修正前在 99.2 到 104.7 MiB 之間；把樣本分成三段各自跑時，兩者都在 97 到 100 MiB（[round3-image-runs.txt](data/2026-10-27-150-checker-hardening/round3-image-runs.txt)）。逐一檢查每個固定樣本、記下每個樣本期間的最高讀數時（[round3-fixture-memory.txt](data/2026-10-27-150-checker-hardening/round3-fixture-memory.txt)，兩個時段各跑三次），修正前的沙箱也出現高峰（130.7 與 125.2 MiB），高峰落在不同的樣本上，同一個映像重跑時又沒有出現：高峰不是這一輪的比對才有的，也沒有哪個樣本一定造成高峰。最高的 131.2 MiB 約是上限的四分之一。
- 接近大小上限的頁面（[round3-size-limits.txt](data/2026-10-27-150-checker-hardening/round3-size-limits.txt)，各檢查三次）：9000 個 HTML 文字標記從 1.7 秒變成 2.6 至 2.8 秒，200 層巢狀裡的 9000 個標記從 2.5 秒變成 4.7 至 4.8 秒，9000 個 SVG 文字標記從 1.6 秒變成 2.6 至 2.7 秒；容器記憶體最高讀數從 284.6 MiB 變成 286.5 MiB（修正後的是這一輪 commit 的建置，比修正前晚量，同一個環境）。都在檢查的時間上限（`CHECK_TIME_LIMIT` 15 秒）與記憶體上限（512Mi）之內，判定都是通過。時間是服務整個回應的時間（渲染、比對與清理）。
- 單元測試：前端 646 個全過（沙箱 426 個），`api:check`、型別檢查、lint、建置與 `check:dist` 通過；`bun test deploy/chart` 699 個全過；`deploy/host` 的測試 117 個全過，型別檢查通過；`cargo test -p repo-check` 26 個全過。檢查服務的 Rust 程式這一輪沒有改。

### 結論

- 文字標記的規則是允許清單（T1）：通過（單元測試與真實 Chromium 的固定樣本；審查的 F1、F2、F3、F8 在修正前都重現過）。合法的 d3 頁面與一般樣式不受影響：通過。
- 參照文字或標記、或參照其他網址的 `<use>`（T2）與不在原處畫出的 SVG 容器裡的標記（T3）：通過；後者修正前是 HTTP 500，現在是判定。
- `::details-content`、`<marquee>`、`::selection`、自訂 highlight、捲動容器的 pseudo-element：通過（固定樣本修正前都重現過）。
- 接近大小上限的頁面在負載高的節點上是否仍在時間上限內：待測（效能量測，這次只在開發工作站量過）。
- 文件（T4）：README、`runtime.ts` 與本報告的用詞已改；在真的叢集上改 kubelet 設定後重新啟動儀表板檢查：待測，SaaS 與展示叢集都還沒有開啟儀表板檢查。
- 殘留：允許清單判斷不了的手法、以後新增的 CSS 屬性，與只在 Chromium 以外的瀏覽器生效的樣式，概括列在 `frontend/sandbox/README.md`「檢查不到的部分」。檢視時在 Chromium 以外的瀏覽器的判定：待測，檢視頁面在 Laysi/trustiot-v2#462。
