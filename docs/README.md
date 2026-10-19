# TrustIoT v2 文件

TrustIoT 將以 Rust 從零重建，不沿用舊架構。本目錄是重建的決策、範圍與里程碑，轉寫自 2026-10-10 確認的重建計畫確認稿；自此以本目錄為準，變更一律經 git 提交。

## 閱讀約定

- 文中的 `#N`（例如 [#139](https://github.com/Laysi/trustiot/issues/139)）一律指舊 repo `Laysi/trustiot` 的 issue 或 PR；本 repo 的 issue 寫成 `Laysi/trustiot-v2#N`。
- 「決策 N」指 [架構決策總表](architecture/decisions.md) 的第 N 列。
- A1–M8 指 [功能重建清單](requirements/rebuild-checklist.md) 的項目；MS0–MS6 指 [里程碑](roadmap/milestones.md)。兩者分開編號，避免清單的 M1–M8 與里程碑混淆。
- 日期一律寫絕對日期（YYYY-MM-DD）。

## 目錄

| 文件 | 內容 |
| --- | --- |
| [架構決策總表](architecture/decisions.md) | 32 項已定案的架構決策與理由 |
| [部署與可搬移性](architecture/deployment.md) | 系統架構、服務切分、流量路徑、可搬移規則 |
| [專案目錄](architecture/repository-layout.md) | repo 的目錄結構與各目錄職責 |
| [CI 與發佈](architecture/ci-release.md) | GitHub Actions 三個 workflow 與 GitOps 部署 |
| [密鑰與權限](architecture/secrets.md) | 密鑰的規則、注入方式與清冊；一次性 token 的規則（M3）；公開 repo 的資訊規則 |
| [密鑰輪替 runbook](runbooks/secrets.md) | 每項密鑰的輪替步驟與外洩處理 |
| [migration Job runbook](runbooks/migrations.md) | 既有叢集補 owner 的 CREATEROLE、收回 `postgres` 與 `template1` 的 PUBLIC 權限（叢集內與外部 Postgres）、一個 Postgres 叢集一個 TrustIoT 資料庫、服務資料庫密碼的輪替、Job 停止或服務等不到 Job 時的處理 |
| [AI 助理與 AI 閘道](architecture/ai-assistant.md) | 主 agent 加工具、分流、模型角色、AI 閘道的原生協定轉送與帳本、安全與題庫 |
| [知識與文件庫（RAG）](architecture/rag.md) | 文件與影音的檢索流程與去 AWS 化調整 |
| [現場脈絡與知識圖（Apache AGE）](architecture/field-context.md) | 機台、生產紀錄、AI 記憶與 Cypher 護欄 |
| [觀測：日誌、看板、錯誤追蹤](architecture/observability.md) | 日誌、Grafana、外部監控、Sentry |
| [AI 警報規則](architecture/alert-rules.md) | AI 自動撰寫並啟用的警報規則與安全護欄 |
| [方案與權益](product/plans.md) | 四級方案的權益與升級牆 |
| [功能重建清單](requirements/rebuild-checklist.md) | A1–M8 各項的「重建必須」，即 issue 的驗收條件來源 |
| [舊系統功能盤點](requirements/legacy-inventory.md) | 舊系統每一項功能與現況 |
| [不帶進新系統的東西](requirements/dropped.md) | 重建時捨棄的功能與元件 |
| [現場硬限制](requirements/field-constraints.md) | 現場的物理事實，新系統一樣會撞到 |
| [里程碑](roadmap/milestones.md) | MS0–MS6 的內容與完成條件、可行性測試進度 |
| [MS0 完成條件與測試](roadmap/ms0-acceptance.md) | MS0 每個完成條件由哪些自動測試證明、在 CI、nightly 或 lab 執行 |
| [風險與待定事項](roadmap/risks.md) | 重建期間的風險與尚未決定的事 |
| [可行性測試結果](feasibility/2026-10-09-cloud-poc.md) | 2026-10-09 第一批雲端 PoC 的實測結果 |
| [可行性測試結果（2026-10-10）](feasibility/2026-10-10-edge-poc.md) | MS0 前的 Istio ambient 與 Cloudflare Tunnel 實測結果 |
| [可行性測試結果（2026-10-12）](feasibility/2026-10-12-tpm-poc.md) | MS1 前在 lab 的閘道器實測 TPM：mTLS、換發挑戰簽章與 LUKS 自動解鎖 |
| [可行性測試結果（2026-10-13）](feasibility/2026-10-13-double-tls-gateway.md) | MS1 前在 lab 的閘道器上量的雙層 TLS（WebSocket 內包 mTLS）CPU 與延遲 |
| [可行性測試結果（2026-10-13）](feasibility/2026-10-13-gateway-memory.md) | lab 的閘道器的記憶體用量、採購規格與 G1 記憶體上限的建議 |
| [測試報告](test-reports/README.md) | 驗收證據、e2e 與效能測試報告的存放與命名規則 |
| [資料來源](references.md) | 決策引用的外部文件 |

## 背景與範圍

- **為何整個重做**：
  - 前端以外全部統一用 Rust。
  - 舊架構有結構性問題：設定下發鏈有競態與非原子性問題（[#139](https://github.com/Laysi/trustiot/issues/139)、[#97](https://github.com/Laysi/trustiot/issues/97)、[#84](https://github.com/Laysi/trustiot/issues/84)）；Greengrass 的限制（[#178](https://github.com/Laysi/trustiot/issues/178)、[#157](https://github.com/Laysi/trustiot/issues/157)）；租戶隔離與注入漏洞（[#101](https://github.com/Laysi/trustiot/issues/101)、[#102](https://github.com/Laysi/trustiot/issues/102)、[#103](https://github.com/Laysi/trustiot/issues/103)）。
  - 要能提供大型企業純地端部署。
- **清單的寫法**：先盤點舊系統的每一項功能（[舊系統功能盤點](requirements/legacy-inventory.md)），再依功能列出重建要求；不描述舊模組，舊系統踩過的坑轉成驗收條件。
- **盤點範圍**：租戶與裝置後端、雲端資料面與基礎設施、AI agent、前端、邊緣與模擬，共 5 個領域，依據為 2026-10-09 盤點時的 HEAD，即 2026-09-09 的 commit `69f2525`。
- **目前狀態**：2026-10-10 決策與 A1–M8 清單全部確認，轉寫進本目錄；接下來依本目錄建立 GitHub milestone（MS0–MS6）與 issue，工作追蹤在組織 Project「TrustIoT v2」。舊 issue 已於 2026-10-09 關閉：114 張以 not planned 關閉，需求與教訓已整理進本目錄（[#176](https://github.com/Laysi/trustiot/issues/176) 與子卡 [#146](https://github.com/Laysi/trustiot/issues/146)–[#149](https://github.com/Laysi/trustiot/issues/149) 的需求以 J2 為準；[#5](https://github.com/Laysi/trustiot/issues/5)、[#17](https://github.com/Laysi/trustiot/issues/17)、[#59](https://github.com/Laysi/trustiot/issues/59)、[#86](https://github.com/Laysi/trustiot/issues/86)、[#87](https://github.com/Laysi/trustiot/issues/87) 是舊架構專用的工作，不再適用），已修好的 [#13](https://github.com/Laysi/trustiot/issues/13)、[#35](https://github.com/Laysi/trustiot/issues/35)、[#96](https://github.com/Laysi/trustiot/issues/96)、[#111](https://github.com/Laysi/trustiot/issues/111)、[#171](https://github.com/Laysi/trustiot/issues/171) 以 completed 關閉；只留下舊 repo 的 CI 與工具 issue [#174](https://github.com/Laysi/trustiot/issues/174)、[#181](https://github.com/Laysi/trustiot/issues/181)。每張關閉的 issue 都留了 comment，寫明對應到確認稿的哪一節，章節名稱可對照本目錄的文件標題。
