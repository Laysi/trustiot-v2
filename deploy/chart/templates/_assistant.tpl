{{/*
AI assistant helpers (services/assistant, decision 31; Laysi/trustiot-v2#592).
*/}}

{{/*
The settings that tell the assistant where the services it calls are. The chart derives every
address from mesh.calls.assistant, so the assistant is never pointed at a service the mesh keeps
it from (deploy/chart/README.md, 「AI 助理」):
- TRUSTIOT__AI_GATEWAY__URL, TRUSTIOT__TELEMETRY_QUERY__URL, TRUSTIOT__ASSETS__URL and
  TRUSTIOT__DASHBOARD_CHECK__URL: the Service of ai-gateway, telemetry-query, assets or
  dashboard-check on its port named http, while that service is enabled and listed in
  mesh.calls.assistant;
- TRUSTIOT__IDENTITY__EXCHANGE_URL: the identity service's token exchange
  (<release>-identity-exchange, templates/token-exchange.yaml), while the mesh plan lets the
  assistant reach it, that is while it calls another enabled service on behalf of users
  ("trustiot.mesh.onBehalfCalls") and the identity service runs. The identity service exchanges a
  question's run delegation only for the services in the same list, so an address the chart sets
  is always an audience the exchange grants.
Without an address the assistant leaves the tools of that service out; without the AI gateway's
it does not start (services/assistant/README.md, 「設定」). services.assistant.env cannot set
these names ("trustiot.assistant.reserved").
Argument: the root context. Returns a YAML list of env entries; use with fromYamlArray.
*/}}
{{- define "trustiot.assistant.env" -}}
{{- $v := .Values -}}
{{- $callees := list -}}
{{- with get ($v.mesh.calls | default dict) "assistant" -}}
{{- if kindIs "slice" . -}}
{{- range . -}}
{{- $callees = append $callees (toString .) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $env := list -}}
{{- range $callee, $name := include "trustiot.assistant.calleeSettings" . | fromYaml -}}
{{- $entry := get $v.services $callee -}}
{{- if and $entry $entry.enabled (has $callee $callees) -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" $ "name" $callee "values" $entry) | fromYaml -}}
{{- $port := 0 -}}
{{- range $config.ports -}}
{{- if eq .name "http" -}}
{{- $port = int (.servicePort | default .containerPort) -}}
{{- end -}}
{{- end -}}
{{- if not $port -}}
{{- fail (printf "services.%s.ports: the assistant calls it on the port named http (mesh.calls.assistant), and there is none" $callee) -}}
{{- end -}}
{{- $env = append $env (dict "name" $name "value" (printf "http://%s:%d" (include "trustiot.componentName" (dict "root" $ "name" $callee)) $port)) -}}
{{- end -}}
{{- end -}}
{{- range (include "trustiot.mesh.plan" . | fromYaml).services -}}
{{- if and (eq .name "identity") .exchangePort (has "assistant" (.exchangeCallers | default list)) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__IDENTITY__EXCHANGE_URL" "value" (printf "http://%s:%d" (include "trustiot.componentName" (dict "root" $ "name" "identity-exchange")) (int .exchangePort))) -}}
{{- end -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}

{{/*
The services the assistant calls and the setting that holds each one's address
(services/assistant/README.md, 「設定」). Returns YAML {<service>: <variable>}; use with fromYaml.
*/}}
{{- define "trustiot.assistant.calleeSettings" -}}
{{- dict "ai-gateway" "TRUSTIOT__AI_GATEWAY__URL" "telemetry-query" "TRUSTIOT__TELEMETRY_QUERY__URL" "assets" "TRUSTIOT__ASSETS__URL" "dashboard-check" "TRUSTIOT__DASHBOARD_CHECK__URL" | toYaml -}}
{{- end -}}

{{/*
The variables of "trustiot.assistant.env", which services.assistant.env (and serviceDefaults.env)
cannot set: an address there would point the assistant at a service mesh.calls does not list.
Returns a YAML list of upper-case names; use with fromYamlArray.
*/}}
{{- define "trustiot.assistant.reserved" -}}
{{- $names := list "TRUSTIOT__IDENTITY__EXCHANGE_URL" -}}
{{- range $_, $name := include "trustiot.assistant.calleeSettings" . | fromYaml -}}
{{- $names = append $names $name -}}
{{- end -}}
{{- $names | sortAlpha | toYaml -}}
{{- end -}}
