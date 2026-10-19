# 風險與待定事項

重建期間 stage 仍在運作，舊系統的 P0 問題在新系統上線前都還存在。

- **舊系統風險**：安全 P0 為 [#116](https://github.com/Laysi/trustiot/issues/116)（Cognito 帳密外洩、可繞過驗證碼）、[#100](https://github.com/Laysi/trustiot/issues/100)（session 密鑰寫死，且 cookie 可觸發假登入繞過租戶隔離）；資料遺失 P0 為 [#177](https://github.com/Laysi/trustiot/issues/177)–[#179](https://github.com/Laysi/trustiot/issues/179)。另有 [#37](https://github.com/Laysi/trustiot/issues/37)（git 歷史外洩的 OpenRouter key 未輪替），以及測試用 AI 流程可被登入使用者直接呼叫、不計用量。CI 與部署憑證另有三項：[#158](https://github.com/Laysi/trustiot/issues/158)（stage 部署角色是 AdministratorAccess，任何分支都能 assume）、[#120](https://github.com/Laysi/trustiot/issues/120)（Docker build context 把 Git 忽略的 .env 與金鑰送進 BuildKit）、[#122](https://github.com/Laysi/trustiot/issues/122)（GHCR\_TOKEN 留在 SSM Run Command 紀錄）。已定不處理舊系統：不修補也不止血，接受風險到下線為止；對應的舊 issue 已關閉，這些風險只在這裡追蹤。
- **維運集中風險**：CA 私鑰保管、憑證撤銷與換發、broker 強化、OTA 回滾、Istio 升級都是安全關鍵設施。一開始就自動化並寫 runbook，避免只有 CEO 懂。
- **Istio 的維運成本**：約每 3 個月一版要跟著升級，服務之間多一層流量，除錯較複雜；升級步驟寫進 runbook 並自動化。客戶自有的 k8s 或 OpenShift 要先確認能跑 ambient（OpenShift 要設 global.platform=openshift，ztunnel 與 istio-cni 裝在 kube-system）。
- **Cloudflare 依賴**：SaaS 的網頁、API 與閘道器連線都經 Cloudflare Tunnel。Cloudflare 故障時網頁無法使用，閘道器的資料留在離線緩衝，恢復後補送；備案是改用直接對外的 values（入口 gateway 直接開 443），閘道器只改 DNS，不換連線方式。瀏覽器流量在 Cloudflare 邊緣解密，要列入第三方資料處理說明；閘道器的 MQTT 在內層 mTLS 內，Cloudflare 看不到內容。依現行服務條款，CDN 限制的是傳送影片或比例過高的圖片、音訊與大型檔案，遙測是小訊息，不在這個範圍；Cloudflare 若有異議，改成入口直接對外或加跳板，閘道器同樣只改 DNS。
- **大型企業地端的額外要求**：完全斷網安裝、高可用、版本升級、資安稽核、AD 整合、地端 GPU。等第一個客戶簽約再做，現在只確保架構不擋路。
- **地端延後**（2026-10-18）：SaaS 全部完成前不做地端專用的工作（清單在 Laysi/trustiot-v2#446），地端可能在沒人注意時壞掉，新程式也可能不知不覺依賴 SaaS 才有的東西。靠可搬移的四條規則、chart 對 values-onprem 的測試、nightly 以兩份 values 部署的冒煙測試與 CloudNativePG onprem 形態的 e2e 守住；恢復地端時先以 values-onprem 完整部署一次。
- **展示環境**（2026-10-19，見[里程碑](milestones.md)「展示切片」）：展示環境在與 lab e2e 共用叢集的展示機上，限流、停權、AI 額度、分析護欄與注入題庫都延後，靠 Cloudflare Access 只放行受邀名單與 Anthropic 工作區的用量上限補上；沒有備份，累積的展示資料可能遺失；e2e 或叢集層級元件的變更可能影響展示，要照展示環境的 runbook 先確認。展示用閘道器就是 lab 的閘道器，lab 測試中斷它時，展示圖表會留下缺口。
- **尚未查證**：實際 AWS 帳單。aws-mcp 需要重新登入才能查，去 AWS 化的節省幅度因此只是估算。
- **主機規格（閒置已實測、峰值待測）**：k3s、ClickHouse（時序加日誌）、Qdrant（4096 維向量）、Apache AGE（在 Postgres 內）、Grafana 同在一台單節點，實測整套閒置約 2.7 GB，暫定 8 GB（不含地端 LLM；這是拆成微服務前、也還沒有 Istio ambient 時量的閒置值，MS3 完成時以代表性負載重測峰值）；向量量大時另計，100 萬個 4096 維 chunk 經 int8 約 4 GB。
- **圖的租戶隔離**：每租戶一個 graph 配專屬角色，由資料庫強制；租戶數超過數百時「每租戶一個 schema」會撐不住，要重新評估（見[現場脈絡與知識圖](../architecture/field-context.md)）。
- **第三方資料存放**：SaaS 的向量（Qdrant Cloud）與物件儲存（S3 或 R2）都在第三方雲端；AI 的資料處理者有 Anthropic（主 agent、worker）、Google Vertex AI（影音理解與嵌入，申請零資料保留）、OpenAI（分流與重排的 Decisions API，符合資格時申請零資料保留與資料駐留）；寄信暫用 Resend（2026-10-19 起），收件地址與信件內容（含登入驗證碼）都經過它，主控台也看得到信件內容；隱私與資料處理說明要列出。
- **AI 供應商**（2026-10-18，見[AI 助理與 AI 閘道](../architecture/ai-assistant.md)）：Decisions API 仍是 public beta（2026-10-06 推出），沒有 SLA，分流失敗一律交給主 agent，重排失敗退回向量分數的順序；Gemini 4 Argon 從 2026-12-01 起、Gemini 3.8 Flash 從 2027-01-01 起單價約漲一倍；Claude Opus 5.5 的 thinking 綁定對話，對話歷史只能往後加，備援換模型時快取從零開始。
- **授權與成熟度**：Apache AGE 是 Apache 2.0 授權；它較新（2026-01 才支援 RLS），要自建含 AGE 的 Postgres 映像並跟著 Postgres 升級。
- **單點故障**：單節點 k3s 仍是單點（[#18](https://github.com/Laysi/trustiot/issues/18)）；先靠備份還原（M8）與快速重建；SaaS 上線前（MS4）要完成從零重建的計時演練，並有不跑在同一台主機的外部監控（見[觀測：日誌、看板、錯誤追蹤](../architecture/observability.md)）；高可用等企業客戶需要再做。
- **待決定**：
  - 「觸發異常多」的門檻（回測天數已定為 14 天）。
  - 影音的單檔長度上限、是否列入進階 AI 權益（MS5 前決定，決策 32）。
