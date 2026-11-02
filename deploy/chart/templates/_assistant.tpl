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
The only callees mesh.calls.assistant may list (Laysi/trustiot-v2#630): the services whose address
"trustiot.assistant.calleeSettings" sets, and postgres for the conversations. The assistant's tools
run on what a model writes, so every service it may call is one the model reaches as the asking
user: the mesh admits the assistant there ("trustiot.mesh.plan") and the identity service
exchanges its tokens for that audience ("trustiot.mesh.onBehalfCalls"). A new tool's service is
added to "trustiot.assistant.calleeSettings", together with README.md's 「AI 助理」 and
「呼叫矩陣」. Returns a sorted YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.assistant.callees" -}}
{{- $names := list "postgres" -}}
{{- range $callee, $_ := include "trustiot.assistant.calleeSettings" . | fromYaml -}}
{{- $names = append $names $callee -}}
{{- end -}}
{{- $names | sortAlpha | toYaml -}}
{{- end -}}

{{/*
Refuses values under which the assistant could call a service it has no business with, or could
not start (Laysi/trustiot-v2#630; README.md, 「AI 助理」):
- mesh.calls.assistant lists a callee outside "trustiot.assistant.callees", whether or not the
  assistant or that service is enabled;
- the assistant is enabled, but services.ai-gateway is not, or mesh.calls.assistant does not list
  ai-gateway: it would get no TRUSTIOT__AI_GATEWAY__URL and restart forever
  (services/assistant/README.md, 「設定」).
A mesh.calls.assistant that is not a list is left to "trustiot.mesh.plan", which refuses it.
"trustiot.mesh.plan" and "trustiot.mesh.onBehalfCalls" both run it, so neither the mesh policies
nor TRUSTIOT_ON_BEHALF_CALLS can come out of values it refuses. Argument: the root context.
Renders nothing.
*/}}
{{- define "trustiot.assistant.checkCalls" -}}
{{- $v := .Values -}}
{{- $listed := get ($v.mesh.calls | default dict) "assistant" -}}
{{- if or (kindIs "slice" $listed) (kindIs "invalid" $listed) -}}
{{- $names := list -}}
{{- range $listed | default list -}}
{{- $names = append $names (toString .) -}}
{{- end -}}
{{- $allowed := include "trustiot.assistant.callees" $ | fromYamlArray -}}
{{- range $names -}}
{{- if not (has . $allowed) -}}
{{- fail (printf "mesh.calls.assistant: the assistant may not call %q; it may call only %s. Its tools run on what a model writes, so every callee listed here is a service the model reaches as the asking user (deploy/chart/README.md, \"AI 助理\"). A new tool's service goes into \"trustiot.assistant.calleeSettings\" (templates/_assistant.tpl), together with README.md's \"AI 助理\" and \"呼叫矩陣\"" . (join ", " $allowed)) -}}
{{- end -}}
{{- end -}}
{{- $assistant := get $v.services "assistant" -}}
{{- if and $assistant $assistant.enabled -}}
{{- if not (include "trustiot.aiGateway.enabled" $) -}}
{{- fail "services.assistant: the assistant calls its models through the AI gateway and does not start without it (services/assistant/README.md, \"設定\"): enable services.ai-gateway (with ai) or disable the assistant" -}}
{{- end -}}
{{- if not (has "ai-gateway" $names) -}}
{{- fail "mesh.calls.assistant: ai-gateway is missing, so the assistant gets no TRUSTIOT__AI_GATEWAY__URL and does not start (services/assistant/README.md, \"設定\"): list ai-gateway or disable the assistant" -}}
{{- end -}}
{{- end -}}
{{- end -}}
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
