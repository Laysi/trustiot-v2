# 閘道器（trustiot-gw）的記憶體用量與採購規格：lab Pi 實測（2026-10-12 至 2026-10-13）

對應 issue：Laysi/trustiot-v2#76。原始資料在 [data/2026-10-13-76-gateway-memory/](data/2026-10-13-76-gateway-memory/)（Pi 的主機名稱已換成 `<pi>`），結論與建議在[可行性測試結果（2026-10-13）](../feasibility/2026-10-13-gateway-memory.md)。

在 lab 的 Raspberry Pi 4（2 GB）量 OS 基線，再以 Rust 閘道器雛形（Modbus 輪詢、protocol crate 的 `BatchBuilder` 與 CBOR、超過記憶體上限寫磁碟的離線緩衝、MQTT 5、WebSocket 內的 mTLS）量不同設備數與緩衝上限下的尖峰記憶體，包含上傳中斷、配置器與 zram 交換的影響。

## 環境

- **機器**：lab 的 Raspberry Pi 4 Model B Rev 1.4，2 GB（可見 1,844 MiB），Raspberry Pi OS Lite arm64（Debian 13.7），kernel 6.18.50+rpt-rpi-v8，glibc 2.41，systemd 257。zram swap 1.8 GiB（zstd，優先序 100），`vm.swappiness` 60，核心命令列含 `cgroup_disable=memory`。
- **Pi 上的 OS 狀態**：13 個執行中的服務（NetworkManager、wpa\_supplicant、bluetooth、avahi-daemon、ssh、journald、udevd、logind、timesyncd、cron、dbus、getty\@tty1、user\@1000）；649 個套件，含 Laysi/trustiot-v2#74 留下的 `tpm2-tools`、`cryptsetup-bin`、`systemd-cryptsetup`、`tpm-udev` 與 `dtoverlay=tpm-slb9670-ce0`，沒有 TPM 常駐服務，#74 的 LUKS 測試容器與 crypttab 項目已移除。清單在 [baseline-info/](data/2026-10-13-76-gateway-memory/baseline-info/)（2026-10-13）與 [baseline-info-prereboot/](data/2026-10-13-76-gateway-memory/baseline-info-prereboot/)（2026-10-12，#74 重開機之前，套件清單相同）。
- **工作站**：Fedora 44（kernel 7.2.8），podman 5.8.7，proto 0.62.3，Bun 1.4.2，Rust 1.99.0（`.prototools` 與 `rust-toolchain.toml` 的版本），Zig 0.16.0 與 cargo-zigbuild 0.23.4（`.prototools` 的版本），openssl 3.5.9。
- **伺服器端**（工作站，podman 無根）：Mosquitto 2.1.2（`docker.io/library/eclipse-mosquitto@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408`，mTLS 監聽 `require_certificate true`、`use_identity_as_username true`、`tls_version tlsv1.3`，另有只在本機回送位址發布的明文監聽給雲端模擬器）；Modbus 模擬器在 `docker.io/library/debian:bookworm-slim@sha256:a4672c0cb26fbdde88e38fa2dfb6c681942306680e41e4378b28770b6e79ee91` 內（由 glibc 2.36 為目標的二進位檔，Modbus TCP 與 RTU-over-TCP 兩個埠）；WebSocket 轉接（外層 TLS 終結、WebSocket 轉成到 Mosquitto 的 TCP）與雲端模擬器（解碼每批、依（閘道器，串流）追蹤序號高水位、回傳 `TelemetryAck`）是原生程式。埠 18761（轉接）、18762 與 18763（Mosquitto）、18764 與 18765（Modbus）。測試 CA 與所有憑證與金鑰由 `openssl`（ECDSA P-256）在測試時產生、有效 30 天、沒有提交；用戶端金鑰測完已從 Pi 刪除。
- **閘道器雛形**：交叉編譯到 `aarch64-unknown-linux-gnu.2.36`（glibc）與 `aarch64-unknown-linux-musl`，release、LTO thin、`panic=abort`；tokio 1.53.2、rustls 0.23.45（ring 0.17.14）、tokio-rustls 0.26.6、tokio-tungstenite 0.30.0、rumqttc 0.25.1（只用 MQTT 5 封包編解碼）、本 repo 的 protocol crate（commit `b2b9232`，cbor4ii 1.2.3）。程式碼約 1,600 行，依 2026-10-09 的決定不進 repo：工作站 `~/poc-gw/76/`，Pi `~/poc-76/`。三個版本：`gw-glibc`（`smoke-d10`）、`gw-glibc-v2`（加上 `--batching series`；steady、outage-d250 的 c0/c1/c4/c16、alloc 與 caps 群組，`alloc-musl` 用 `gw-musl-v2`）、`gw-glibc-v3`（取樣多了 zram 與 pswpin、pswpout 欄位；big2、swap、outage3 群組與 synth-c512 之後的執行）。`gw-glibc-v3` 的 sha256 為 `6bab86044b134db5…`，`gw-musl-v3` 為 `138c4b8c5d16d16b…`。
- **雛形的設定**（除非另寫）：每台設備 20 個資料點（32 位元浮點，40 個暫存器，一次 FC3），每秒一輪；每 4 台設備一條 Modbus 連線（TCP 與 RTU-over-TCP 輪流）；設備 ID 為 36 字元的 UUID 樣式，資料點鍵 16 字元（如 `measure_point_01`）；批次上限 256 KiB（`MAX_BULK_BYTES`）；每 10 秒結束開啟中的批次；一次送一批、收到 `TelemetryAck` 才送下一批；確認逾時 30 秒；MQTT keepalive 60 秒；重連退避 1 到 10 秒；tokio 預設工作執行緒（4）；WebSocket 讀寫緩衝區預設 128 KiB；組批方式預設為到達順序。
- **鎖**：測量時持有 Pi 上的 `~/e2e.lock`（owner `Laysi/trustiot-v2#76`），時間見「鎖與中斷」。

## 指令

準備（沒有持鎖，只在工作站與 `~/poc-76/` 內）：

```sh
# 在 ~/poc-gw/76（含 Cargo.toml、harness/）；環境與位址由 env.json 提供，不進 repo
bun harness/build.ts                    # 主機：relay、cloudsim；modsim 以 x86_64-unknown-linux-gnu.2.36；Pi：gw（glibc、musl）、ballast
bun harness/pki.ts pki                  # 一次性 CA、轉接、broker 與用戶端憑證
bun harness/stack.ts up                 # Mosquitto 與 Modbus 模擬器（podman）、轉接、雲端模擬器
bun harness/deploy.ts v3                # 二進位檔與用戶端憑證到 Pi 的 ~/poc-76
```

量測：每個群組在一個鎖視窗內執行，結束就放鎖。群組內每次執行由 `harness/scenario.ts` 在 Pi 上以 `setsid` 啟動 `bin/gw-glibc-v3`（`oom_score_adj` 設 1000），等 `exit` 檔出現，再取回 `samples.csv`、`summary.json`、`counters.json` 與雲端模擬器的記錄：

```sh
bun harness/matrix.ts base+steadyall   # 基線 24 筆樣本，加 steady-d{1,10,50,100,250} 與三個形狀變體
bun harness/matrix.ts base+outage1     # 基線，加 outage-d250-c{0,1,4,16}
bun harness/matrix.ts alloc            # musl、ARENA_MAX=2、malloc_trim、2 個工作執行緒、批次 16 KiB、WebSocket 8 KiB
bun harness/matrix.ts caps             # c64、逐系列 c16 與 c64、c256
bun harness/matrix.ts big              # 合成讀值填 512 MiB
bun harness/matrix.ts big2             # 合成讀值填 256 MiB：預設、malloc_trim、MALLOC_MMAP_THRESHOLD_=65536
bun harness/matrix.ts swap             # 壓艙 1,250 MiB 加合成讀值填 384 MiB
bun harness/matrix.ts outage3          # 10、50、100 台的中斷
bun harness/analyze.ts                 # 整理成 summary.json
bun harness/export.ts <repo>/docs/test-reports/data/2026-10-13-76-gateway-memory   # 清掉位址後複製資料
```

單次執行等同於在 Pi 上（`~/poc-76`）：

```sh
./bin/gw-glibc-v3 --devices 250 --points 20 --per-endpoint 4 --poll-ms 1000 --duration-secs 300 \
  --upload-interval-secs 10 --buf-mem-bytes 16777216 --drain-secs 180 --tail-secs 15 --min-avail-mb 250 \
  --relay <工作站>:18761 --ca pki/ca.crt --cert pki/gw.crt --key pki/gw.key \
  --modbus-tcp <工作站>:18764 --modbus-rtu <工作站>:18765 --out out/<id> --spill-dir out/<id>/spill
# 其他參數：--batching series、--synthetic-rps 20000（取代 Modbus）、--batch-limit、--ws-buffer、--workers、--trim-on-empty
# 環境變數：MALLOC_ARENA_MAX=2、MALLOC_MMAP_THRESHOLD_=65536
```

上傳中斷由測試程式在指定秒數殺掉工作站上的轉接程式、過了指定時間再啟動（所有連線立即中斷，雛形以 1 到 10 秒的退避重連）。程式自己有安全閥：`MemAvailable` 低於 `--min-avail-mb` 時寫出資料並結束。取樣執行緒每秒讀 `/proc/self/status`、`smaps_rollup`、`/proc/meminfo`、`/proc/vmstat`、`/sys/block/zram0/mm_stat`，並對 `/proc/self/clear_refs` 寫入 `5` 重設峰值，所以 `hwm_interval_kb` 是每秒區間的 `VmHWM`，整次的尖峰是它的最大值。CPU 是 `/proc/self/stat` 的 utime 加 stime（每秒 100 ticks）。

## 結果

### 鎖與中斷

鎖視窗（UTC；每個視窗結束都釋放）：

| 視窗 | 取得 | 釋放 | 內容 |
| --- | --- | --- | --- |
| 1 | 2026-10-12T09:07:00Z | 2026-10-12T09:13:46Z | 開機後（#74 重開機前）的基線（原狀與停掉藍牙、avahi 的對照）；`smoke-d10` |
| 2 | 2026-10-12T10:04:14Z | 2026-10-12T10:11:34Z | 基線取樣時 Pi 的網路中斷約 5 分鐘，視窗作廢，沒有資料 |
| 3 | 2026-10-12T10:11:35Z | 2026-10-13T03:51:12Z | `base+steadyall`；10:12Z 起 Pi 斷電斷網、工作站的有線連線也斷了（約 17 小時），Pi 於 2026-10-13T03:27Z 開機，這個視窗裡 10:12Z 前後的基線取樣作廢，steady 與形狀變體都在 03:30Z 以後執行 |
| 4 | 2026-10-13T03:51:26Z | 2026-10-13T04:15:29Z | 基線（取得後 30 秒起）與 `outage-d250-c{0,1,4,16}` |
| 5 | 2026-10-13T04:15:45Z | 2026-10-13T04:47:07Z | `alloc` |
| 6 | 2026-10-13T07:03:07Z | 2026-10-13T07:34:38Z | `caps` |
| 7 | 2026-10-13T07:34:58Z | 2026-10-13T07:47:00Z | `big` |
| 8 | 2026-10-13T07:47:20Z | 2026-10-13T08:09:56Z | `big2` |
| 9 | 2026-10-13T08:10:08Z | 約 2026-10-13T08:27Z | 第一次 swap 測試：壓艙一半是沒被觸碰的零頁，實際只常駐約 575 MiB（資料 `synth-c384-ballast575`）；收尾時工具誤殺自己的 ssh 連線而卡住，改手動收尾並釋放 |
| 10 | 2026-10-13T08:27:15Z | 2026-10-13T08:33:22Z | 壓艙 1,250 MiB，閘道器的安全閥 100 MiB 先觸發（資料 `swap-ballast1250-c384-floor100`，沒有換出） |
| 11 | 2026-10-13T08:33:59Z | 2026-10-13T08:44:39Z | 安全閥改 50 MiB 的 swap 執行（`swap-ballast1250-c384`） |
| 12 | 2026-10-13T08:46:21Z | 2026-10-13T09:05:33Z | `outage3` |

Pi 的重開機與中斷：（1）2026-10-12T09:14Z 前後 #74 為 LUKS 開機解鎖重開機（視窗 1 在它之前結束）；（2）2026-10-12T10:12Z 至 2026-10-13T03:27Z 斷電斷網（協調者的記錄：journal 只有一次開機，首筆時間是 fake-hwclock 的 2026-10-12T10:12Z 前後），視窗 2 與視窗 3 的前半作廢，作廢的資料沒有使用；（3）2026-10-14T03:04Z 前後再重開機，發生在所有視窗結束之後，不影響資料。作廢的樣本留在本機，不提交（`baseline-suspect-interrupted-run` 等）。

### OS 基線

各 24 筆樣本，間隔 5 秒（原始資料：[baseline-stock-after-reboot/](data/2026-10-13-76-gateway-memory/baseline-stock-after-reboot/)、[baseline-stock-prereboot/](data/2026-10-13-76-gateway-memory/baseline-stock-prereboot/)、[baseline-trimmed-prereboot/](data/2026-10-13-76-gateway-memory/baseline-trimmed-prereboot/)）：

| 狀態 | 已用（MemTotal 減 MemAvailable，中位數，kB） | 範圍 | MemAvailable（kB） | Slab（kB） | AnonPages（kB） | 分頁快取（kB） | 使用者空間 PSS 合計（kB） |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 2026-10-13T03:52Z，開機後約 24 分鐘（量測的基線） | 163,396 | 162,876 到 163,984 | 1,725,524 | 43,512 | 25,412 | 199,588 | 67,230 |
| 2026-10-12T09:08Z 起，#74 重開機前 | 163,944 | 163,008 到 164,948 | 1,724,968 | 44,132 | 25,276 | 148,648 | 66,955 |
| 同上，停掉 bluetooth、avahi-daemon、avahi-daemon.socket、udisks2 後 | 163,280 | 162,356 到 170,276 | 1,725,684 | 44,292 | 24,308 | 148,788 | 63,570 |

`free -m`（量測的基線）：total 1,844，used 159，free 1,516，buff/cache 232，available 1,685，Swap 1,843 用 0；`/sys/block/zram0/mm_stat` 使用量 0 附近（原始頁數 4096、20,480 bytes 的管理開銷）。

### 各次執行

欄位：常駐 RSS 是上線且（若有）中斷前（t 大於 20 秒）的 `VmRSS` 中位數；尖峰是整次執行每秒區間 `VmHWM` 的最大值；補送後 RSS 是最後 10 秒的中位數；最低 MemAvailable 是整次的最小值；單核 CPU 是整次的平均（100% 等於一顆核心）；未送達是結束時產生的讀值減去雲端模擬器收到的讀值（串流序號零缺號、零重複是另外檢查的，見結論）。所有 RSS 與 PSS 單位為 MiB。

| 執行 | 設備數 | 上限（MiB） | 中斷 | 產生的讀值 | 未送達（結束時） | 常駐 RSS | 尖峰 RSS | 尖峰 PSS | 補送後 RSS | 最低 MemAvailable | 寫進磁碟（MiB） | 單核 CPU（%） | 補送完成 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| smoke-d10 | 10 | 16 | - | 12,200 | 0 | 5.1 | 5.1 | 3.8 | 5.1 | 1672.9 | 0 | 0.4 | 是 |
| steady-d1 | 1 | 16 | - | 3,020 | 0 | 4.4 | 4.5 | 3.2 | 4.5 | 1682.9 | 0 | 0.2 | 是 |
| steady-d10 | 10 | 16 | - | 30,200 | 0 | 5.3 | 5.3 | 4 | 5.3 | 1684.5 | 0 | 0.4 | 是 |
| steady-d50 | 50 | 16 | - | 150,160 | 0 | 8.4 | 8.7 | 7.4 | 8.4 | 1680.3 | 0 | 1.4 | 是 |
| steady-d100 | 100 | 16 | - | 300,080 | 0 | 8.9 | 9.4 | 8.1 | 9.2 | 1679.2 | 0 | 2.6 | 是 |
| steady-d100-p60 | 100 | 16 | - | 726,000 | 0 | 9.2 | 10.2 | 8.9 | 8.9 | 1678.6 | 0 | 6.2 | 是 |
| steady-d250 | 250 | 16 | - | 750,080 | 0 | 8.7 | 9.6 | 8.3 | 8.4 | 1678.5 | 0 | 6.3 | 是 |
| steady-d250-pe1 | 250 | 16 | - | 599,980 | 0 | 9.3 | 9.9 | 8.6 | 9.4 | 1676.5 | 0 | 6.1 | 是 |
| steady-d250-poll5 | 250 | 16 | - | 120,000 | 0 | 8.8 | 9.3 | 8 | 8.9 | 1678.7 | 0 | 1.8 | 是 |
| outage-d10-c16 | 10 | 16 | 240s | 66,200 | 0 | 4.9 | 6.2 | 4.9 | 5.9 | 1677.1 | 0 | 0.5 | 是 |
| outage-d50-c4 | 50 | 4 | 240s | 331,000 | 0 | 6.7 | 12.7 | 11.4 | 11.5 | 1669.6 | 1.3 | 1.3 | 是 |
| outage-d100-c16 | 100 | 16 | 300s | 840,080 | 0 | 8 | 25.5 | 24.2 | 19.3 | 1655.4 | 6.6 | 2.8 | 是 |
| outage-d250-c0 | 250 | 0 | 180s | 1,500,000 | 0 | 9.2 | 11.3 | 9.9 | 11.3 | 1665.1 | 130.4 | 8.2 | 是 |
| outage-d250-c1 | 250 | 1 | 180s | 1,500,000 | 0 | 8.4 | 12.1 | 10.7 | 12.1 | 1663.7 | 89.7 | 7.3 | 是 |
| outage-d250-c4 | 250 | 4 | 180s | 1,500,000 | 0 | 9.2 | 14.6 | 13.2 | 13 | 1660.8 | 84.7 | 7.2 | 是 |
| outage-d250-c16 | 250 | 16 | 180s | 1,500,000 | 0 | 8.8 | 25.6 | 24.3 | 15 | 1656.1 | 64.7 | 7.7 | 是 |
| outage-d250-c16-series | 250 | 16 | 150s | 1,300,000 | 0 | 11.3 | 29.9 | 28.5 | 23.7 | 1651.2 | 0.3 | 4.5 | 是 |
| outage-d250-c64 | 250 | 64 | 240s | 1,650,000 | 0 | 9.5 | 73.8 | 72.4 | 32.8 | 1604 | 42.9 | 8.1 | 是 |
| outage-d250-c64-series | 250 | 64 | 240s | 1,650,080 | 0 | 10.9 | 39.8 | 38.5 | 22.5 | 1639.2 | 0 | 4.5 | 是 |
| outage-d250-c256 | 250 | 256 | 720s | 4,100,080 | 0 | 8.9 | 265.9 | 264.5 | 150.1 | 1403.7 | 60.1 | 9 | 是 |
| alloc-glibc-arena2 | 250 | 16 | 150s | 1,300,080 | 0 | 7.7 | 24.6 | 23.2 | 24.6 | 1646.7 | 52.4 | 8.1 | 是 |
| alloc-glibc-batch16k | 250 | 16 | 150s | 1,300,000 | 942,650 | 16.7 | 25 | 23.6 | 25 | 1647.1 | 88.6 | 5.1 | 否 |
| alloc-glibc-trim | 250 | 16 | 150s | 1,300,000 | 0 | 8.7 | 25.2 | 23.9 | 17.6 | 1653.3 | 51.9 | 7.4 | 是 |
| alloc-glibc-workers2 | 250 | 16 | 150s | 1,300,080 | 0 | 7.2 | 24.5 | 23.1 | 13.3 | 1653.4 | 52.5 | 7 | 是 |
| alloc-glibc-ws8k | 250 | 16 | 150s | 1,300,000 | 0 | 9.6 | 25.3 | 24 | 15.7 | 1655 | 51.7 | 8 | 是 |
| alloc-musl | 250 | 16 | 150s | 1,303,280 | 0 | 13.2 | 43 | 42.4 | 32.1 | 1632.4 | 52.2 | 8.9 | 是 |
| synth-c256 | 250 | 256 | 200s | 6,002,000 | 0 | 7.9 | 324.7 | 323.2 | 114.4 | 1338.4 | 123.4 | 18.9 | 是 |
| synth-c256-mmapthr | 250 | 256 | 200s | 6,002,000 | 0 | 7.1 | 265.7 | 264.4 | 10.7 | 1402.8 | 123.4 | 20.3 | 是 |
| synth-c256-trim | 250 | 256 | 200s | 6,002,000 | 0 | 8.9 | 337.1 | 335.6 | 236.3 | 1331.8 | 123.4 | 19.2 | 是 |
| synth-c384-ballast575 | 250 | 384 | 300s | 8,402,000 | 0 | 9.1 | 498.7 | 497.3 | 478.6 | 569 | 180.3 | 19.4 | 否 |
| synth-c512 | 250 | 512 | 440s | 10,402,000 | 3,050,487 | 9.2 | 588.8 | 587.4 | 577.7 | 1073.3 | 311.4 | 18.6 | 否 |
| swap-ballast1250-c384 | 250 | 384 | 300s | 8,402,000 | 0 | 8.1 | 465.1 | 463.7 | 430 | 66.3 | 180.3 | 19.5 | 否 |
| swap-ballast1250-c384-floor100 | 250 | 384 | 300s | — | — | 8.6 | 307.1 | 305.8 | 299.4 | 102.8 | 0 | 14.6 | 否 |

說明：

- `smoke-d10` 用 `gw-glibc`（v1）。`alloc-musl` 用 `gw-musl-v2`，其他 alloc 執行與 outage、caps 用 `gw-glibc-v2`，big2、swap、outage3 用 `gw-glibc-v3`。
- `outage-d250-c256` 是用真實的 Modbus 輪詢填 256 MiB（720 秒）；`synth-*` 用合成讀值（`--synthetic-rps 20000`，每秒 1.8 MB 的已編碼批次）取代 Modbus 輪詢，在較短時間填滿。
- `synth-c512` 在 180 秒的補送等待內沒有補完（結束時有 305 萬筆未送，仍在補送中，沒有遺失）。
- `alloc-glibc-batch16k` 的批次上限只有 16 KiB，補送追不上產生速度（結束時 94 萬筆未送、已寫進磁碟的 88.6 MiB 尚未補完）。
- `synth-c384-ballast575` 的壓艙程式有一半是 `calloc` 的零頁，從未被觸碰，所以只常駐約 575 MiB，沒有換出；它當作「記憶體吃緊但還沒換出」的對照，不是 swap 測試。
- `swap-ballast1250-c384-floor100` 因為閘道器的安全閥（`MemAvailable` 低於 100 MiB 即結束，結束碼 3）在核心開始換出之前先觸發而結束，沒有補送。最終的 swap 測試把安全閥降到 50 MiB，壓艙程式的退出門檻降到 30 MiB。
- 「補送完成」為否的五次（`alloc-glibc-batch16k`、`synth-c512`、`synth-c384-ballast575`、`swap-ballast1250-c384`、`swap-ballast1250-c384-floor100`）在 180 秒的補送等待結束時還有未送出的批次。其中 `synth-c384-ballast575` 與 `swap-ballast1250-c384` 的計數檔顯示補送在等待結束後的 15 秒尾段內完成（確認的批次數等於形成的批次數，讀值全數確認），所以表中它們的「未送達」是 0；`alloc-glibc-batch16k` 與 `synth-c512` 到結束仍有未送出的批次；`swap-ballast1250-c384-floor100` 是安全閥中止的執行，沒有計數檔。
- `peak_pss` 與 `peak_rss` 都來自程式本身；PSS 約比 RSS 少 1 到 2 MiB（共用的程式與函式庫頁）。

### 資料量與批次大小

雲端模擬器收到的每筆讀值編碼大小（`cloudsim.log` 的最後一行，位元組比筆數）：

| 執行 | 讀值 | 位元組 | 每筆 |
| --- | --- | --- | --- |
| steady-d10（到達順序） | 30,200 | 703,560 | 23.3 |
| steady-d50（到達順序） | 150,160 | 3,435,855 | 22.9 |
| steady-d100（到達順序） | 300,080 | 11,530,268 | 38.4 |
| steady-d250（到達順序） | 750,080 | 68,409,390 | 91.2 |
| outage-d250-c16（到達順序） | 1,500,000 | 136,687,677 | 91.1 |
| outage-d250-c16-series（逐系列） | 1,300,000 | 29,656,149 | 22.8 |
| outage-d250-c64-series（逐系列） | 1,650,080 | 37,648,004 | 22.8 |

### 補送的速度

`synth-c512` 的連線恢復後，每 10 秒確認的讀值約 26,000 到 33,000 筆每秒（每批約 2,800 筆，約 10 批每秒；單核 CPU 約 20% 到 35%）；`alloc-glibc-batch16k` 每批約 175 筆，確認約 6.9 批每秒，連上線期間產生的 5,000 筆每秒都追不上。取樣資料在各自的 `samples.csv`（`acked`、`records_acked` 欄位）。

### zram

`swap-ballast1250-c384`（[資料](data/2026-10-13-76-gateway-memory/swap-ballast1250-c384/)）：壓艙 1,250 MiB（半隨機、半可壓縮），開始時 MemAvailable 391 MiB；閘道器的緩衝每秒增加約 3 MiB 到 384 MiB，t 約 196 秒（MemAvailable 約 116 MiB）核心開始換出（`pswpout`），之後 MemAvailable 回到 150 到 290 MiB；swap 使用最高 708 MiB，`mm_stat` 顯示壓縮後占約 338 MiB 的 RAM（約 2.1 比 1）；閘道器自己的 `VmSwap` 最高 44 MiB；尖峰 RSS 465.1 MiB；最低 MemAvailable 66.3 MiB；中斷 300 秒後恢復連線，840 萬筆讀值全數確認（`counters.json`：`acked` 等於 `formed`，`records_acked` 等於 `produced`）；SSH 全程可用，沒有 OOM。

### 收尾

- 工作站：`bun harness/stack.ts down` 已停止轉接、雲端模擬器與兩個 podman 容器（在工作站重開機之前，容器仍在執行；工作站重開機後確認沒有 `poc76-*` 容器）。
- Pi：`~/poc-76/` 留有二進位檔與輸出（約 17 MB，不含緩衝的磁碟檔）；測試用戶端金鑰、憑證與 CA 已刪除。沒有安裝套件、沒有改任何系統設定；2026-10-12 的對照曾暫停藍牙與 avahi，結束時已啟動回來。

## 結論

對照 Laysi/trustiot-v2#76 的驗收條件：

- 結果檔記錄 OS 基線與各設備數、緩衝量組合下的尖峰記憶體：**通過**。OS 基線見上表；設備數 1、10、50、100、250 的常駐尖峰見各次 steady 執行；上傳中斷的尖峰以 250 台為主，上限 0、1、4、16、64、256 MiB（真實輪詢）加 256、384、512 MiB（合成讀值），另有 10 台、50 台、100 台的中斷。設備數與上限不是完整的交叉：上限只在中斷時有作用，而填滿的速度隨設備數而變（見可行性結果的填滿時間表），所以只以 250 台掃上限，另在較少設備數各測一個上限。
- 結果檔提出採購規格與 G1 記憶體上限的建議值：**通過**，見可行性測試結果的「建議」。這是採購與上限的決定，標記為需要人確認。
- 勾選 docs/roadmap/milestones.md 的記憶體用量項目：**通過**（同一次變更）。

資料完整性：雲端模擬器對每個（閘道器，串流）追蹤序號高水位。所有有完整記錄的執行，`gaps`（序號缺號）都是 0，`dups`（重送的批次）除了 `alloc-glibc-batch16k` 有 1 批之外都是 0；未送達欄在 `alloc-glibc-batch16k` 與 `synth-c512` 以外都是 0，這兩次因補送沒有在等待時間內完成而有仍在緩衝或磁碟內的讀值，不是遺失。`swap-ballast1250-c384-floor100` 因安全閥中止，沒有完整記錄。

與設計的差異與限制：用本機轉接代替 Cloudflare 與 Istio；Modbus 用工作站上的模擬器經 TCP 提供（含 RTU-over-TCP 封裝），沒有真實的 RS485 線速率；TLS 金鑰是軟體金鑰而不是 TPM；沒有心跳、日誌、即時值、設定下發與 OTA 的流量；MQTT 使用 5；設備數與輪詢頻率是假設（docs 沒有現場資料）。這些對結論的影響與預留值見可行性測試結果。
