# Gauge

一個數值相對於門檻的位置：線性量表，填色依狀態（正常、警告、嚴重），軌道上標出警告與嚴重區段。

新增（儀表板）。選線性而不是半圓儀表：同樣寬度下刻度更準、容易並排，也不必畫指針。

## 使用者提供

- `label`、`value`、`unit`、`min`、`max`。
- `thresholds`：`{ warning, critical }`；狀態由元件依門檻計算，不由使用者指定顏色。
- `source`：數值是按鈕，點開 SourcePopover。

## 規則

- 軌道 8px、`muted`；警告區 `status-warning-soft`、嚴重區 `status-critical-soft`；填色 `status-normal`、`status-warning` 或 `status-critical`。
- 門檻刻度 2px 狀態色，標籤「警告 32」「嚴重 34」分別靠在刻度左右兩側，不會重疊。
- 角色 `meter`，`aria-valuetext` 包含數值、狀態與兩個門檻；標題列同時顯示 StatusBadge。
- 下面一行寫量程「量程 20–40 °C」。
