# 現場硬限制

這些是現場的物理事實，與架構無關，新系統一樣會撞到。

- Pi 沒有硬體時鐘，斷網開機時時間曾慢約 38 分鐘（[#180](https://github.com/Laysi/trustiot/issues/180)）。
- 現場停電通常連網路一起斷，只存在記憶體的資料會遺失（[#162](https://github.com/Laysi/trustiot/issues/162)）。
- RS485 轉換器會把設備的回應送給所有 TCP 連線（[#183](https://github.com/Laysi/trustiot/issues/183)）。
- 設備可能一直保持連線卻永遠不回應，keepalive 偵測不到（[#159](https://github.com/Laysi/trustiot/issues/159)、[#164](https://github.com/Laysi/trustiot/issues/164)）。
- 曾有設備靜默停擺 2.5 小時，程式外觀卻完全正常（[#151](https://github.com/Laysi/trustiot/issues/151)）。
- 現場轉換器的 port 各不相同（實測過 502、8886，常見還有 4196、8899），不能用固定白名單（[#140](https://github.com/Laysi/trustiot/issues/140)）。
- SLB9670 做 RSA 運算會逾時，只能用 ECC P-256；重刷 SD 卡後 TPM 內的狀態仍在（[#81](https://github.com/Laysi/trustiot/issues/81)）。
- 企業網路常只放行 443，閘道器所有對外連線都要能走 443；舊系統曾因自己的出站白名單漏開 8443 而部署卡死（[#83](https://github.com/Laysi/trustiot/issues/83)）。
- 所有閘道器共用出廠憑證，ID 又可能從標籤或出貨紀錄取得，就可以被冒充（[#118](https://github.com/Laysi/trustiot/issues/118)）。
