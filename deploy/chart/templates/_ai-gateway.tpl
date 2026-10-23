{{/*
AI gateway helpers (services/ai-gateway, decisions 15 and 28; Laysi/trustiot-v2#536). The gateway
reads its settings from AI_GATEWAY__* variables, not TRUSTIOT__*: it carries no product name
(decision 28), so the platform ConfigMap means nothing to it. The chart hands it the providers,
model roles and limits from values.ai in a ConfigMap of its own (templates/ai-gateway.yaml), and
each provider's key through "trustiot.credentialCatalog" as a secretKeyRef, never as a value.
*/}}

{{/* Whether the AI gateway is enabled. */}}
{{- define "trustiot.aiGateway.enabled" -}}
{{- with get .Values.services "ai-gateway" -}}
{{- if .enabled -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/*
The only services that may call the AI gateway (decision 28; docs/architecture/ai-assistant.md,
「AI 閘道」): the AI assistant, the knowledge base and alerts. The gateway verifies no internal
identity and spends the provider accounts, so the mesh alone keeps everybody else out: listing
ai-gateway as the callee of any other service in mesh.calls fails the render
("trustiot.mesh.plan").
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.aiGateway.callers" -}}
{{- list "alerts" "assistant" "knowledge" | toYaml -}}
{{- end -}}

{{/*
The providers the chart knows, by the name under values.ai.providers: the gateway's protocol for
it and the variable that carries its key into the pod ("trustiot.credentialCatalog", credential
ai). The gateway reads the key from the variable its key_env names (services/ai-gateway/README.md,
「設定」); TRUSTIOT_ names are the chart's, so custom env can neither set nor shadow them.
Returns YAML {<name>: {protocol, keyEnv}}; use with fromYaml.
*/}}
{{- define "trustiot.aiGateway.providers" -}}
anthropic:
  protocol: anthropic-messages
  keyEnv: TRUSTIOT_AI_ANTHROPIC_API_KEY
openai:
  protocol: openai-decisions
  keyEnv: TRUSTIOT_AI_OPENAI_API_KEY
{{- end -}}

{{/*
Name of the ConfigMap with the gateway's settings.
*/}}
{{- define "trustiot.aiGateway.configName" -}}
{{- include "trustiot.componentName" (dict "root" . "name" "ai-gateway") -}}
{{- end -}}

{{/*
A duration of the gateway's settings in milliseconds: one number and one unit (500ms, 30s, 10m,
1h), a form the gateway's parser reads the same way. Fails the render for anything else.
Argument: dict "value" <duration> "key" <values path, for the error>. Use with atoi.
*/}}
{{- define "trustiot.aiGateway.ms" -}}
{{- if not (and (kindIs "string" .value) (regexMatch "^[0-9]{1,7}(ms|s|m|h)$" .value)) -}}
{{- fail (printf "%s must be a duration such as 30s, 500ms, 10m or 1h" .key) -}}
{{- end -}}
{{- $unit := regexFind "(ms|s|m|h)$" .value -}}
{{- mul (atoi (trimSuffix $unit .value)) (get (dict "ms" 1 "s" 1000 "m" 60000 "h" 3600000) $unit) -}}
{{- end -}}

{{/*
A whole number of a role's settings within [min, max], as decimal digits.
Argument: dict "value" <number> "key" <values path> "min" <int> "max" <int>. Use with atoi.
*/}}
{{- define "trustiot.aiGateway.count" -}}
{{- $text := include "trustiot.decimal" (dict "value" .value "key" .key) -}}
{{- if not (regexMatch "^[1-9][0-9]{0,9}$" $text) -}}
{{- fail (printf "%s must be a whole number from %d to %d" .key .min .max) -}}
{{- end -}}
{{- if or (lt (atoi $text) .min) (gt (atoi $text) .max) -}}
{{- fail (printf "%s must be a whole number from %d to %d" .key .min .max) -}}
{{- end -}}
{{- $text -}}
{{- end -}}

{{/*
The gateway's settings as AI_GATEWAY__* variables (services/ai-gateway/README.md, 「設定」), the
data of its ConfigMap. Only names and limits: never a key (the chart tests check every ConfigMap).
- a provider under ai.providers is configured when its apiKeySecret has a name: its protocol,
  key_env (the variable "trustiot.credentialCatalog" fills from that Secret) and base_url when set
  (https, no user info, query or fragment; empty means the provider's own API);
- every role under ai.roles: its models in fallback order, each <provider>/<model> with a
  configured provider, and the optional limits; the gateway needs at least one role;
- http.request_timeout from ai.requestTimeout, and shutdown.drain_timeout from the service's
  drainSeconds minus 10 s: the kit's 5 s drain delay plus 3 s to cancel late work and flush
  telemetry must fit in drainSeconds after SIGTERM (crates/service-kit/README.md, 「收尾」), with
  2 s to spare.
Fails the render, whether or not schema validation runs, wherever the gateway would refuse to start
with the result (services/ai-gateway/src/config.rs and gateway.rs): services.ai-gateway.credentials
without ai, no role, a role, model or limit out of the gateway's range, a model of a provider
without a key, a role whose queueWait plus the longer of timeouts.response and timeouts.firstToken
does not fit in requestTimeout less the gateway's margin (a tenth of it, at most 2 s), or
drainSeconds outside 11 to 3605. Rendered while the gateway is enabled only.
Argument: the root context. Returns YAML {data: {<variable>: <value>}}; use with fromYaml.
*/}}
{{- define "trustiot.aiGateway.config" -}}
{{- $ai := .Values.ai -}}
{{- $known := include "trustiot.aiGateway.providers" . | fromYaml -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" . "name" "ai-gateway" "values" (get .Values.services "ai-gateway")) | fromYaml -}}
{{- if not (has "ai" ($config.credentials | default list)) -}}
{{- fail "services.ai-gateway.credentials must list ai: the gateway does not start without its provider keys (services/ai-gateway/README.md, 「金鑰與 TLS」)" -}}
{{- end -}}
{{- $data := dict -}}
{{- $configured := list -}}
{{- if not (kindIs "map" $ai.providers) -}}
{{- fail "ai.providers must be a map of providers" -}}
{{- end -}}
{{- /* An https URL with a host name, an optional port and path; nothing the gateway's URL check refuses. */ -}}
{{- $baseUrl := "^https://[A-Za-z0-9]([A-Za-z0-9.-]{0,251}[A-Za-z0-9])?(:(6553[0-5]|655[0-2][0-9]|65[0-4][0-9]{2}|6[0-4][0-9]{3}|[1-5][0-9]{4}|[1-9][0-9]{0,3}))?(/[!$-;=@-Z_a-z~]*)?$" -}}
{{- range $name, $provider := $ai.providers -}}
{{- $spec := get $known $name -}}
{{- if not $spec -}}
{{- fail (printf "ai.providers.%s: unknown provider; the chart knows %s" $name (keys $known | sortAlpha | join ", ")) -}}
{{- end -}}
{{- if $provider.apiKeySecret.name -}}
{{- $prefix := printf "AI_GATEWAY__PROVIDERS__%s__" (upper $name) -}}
{{- $_ := set $data (printf "%sPROTOCOL" $prefix) $spec.protocol -}}
{{- $_ := set $data (printf "%sKEY_ENV" $prefix) $spec.keyEnv -}}
{{- with $provider.baseUrl -}}
{{- if not (and (kindIs "string" .) (le (len .) 2048) (regexMatch $baseUrl .)) -}}
{{- fail (printf "ai.providers.%s.baseUrl must be an https URL with a host name and no user info, query or fragment (the gateway sends the key there)" $name) -}}
{{- end -}}
{{- $_ := set $data (printf "%sBASE_URL" $prefix) . -}}
{{- end -}}
{{- $configured = append $configured $name -}}
{{- end -}}
{{- end -}}
{{- if not (and (kindIs "map" $ai.roles) $ai.roles) -}}
{{- fail "ai.roles: the AI gateway needs at least one model role, <role>: {models: [<provider>/<model>, ...]} (services/ai-gateway/README.md, 「設定」)" -}}
{{- end -}}
{{- /* The gateway's limits (services/ai-gateway/src/config.rs) and its defaults (crates/ai-clients). */ -}}
{{- $hour := 3600000 -}}
{{- $timeoutNames := dict "connect" "CONNECT" "firstToken" "FIRST_TOKEN" "streamIdle" "STREAM_IDLE" "streamTotal" "STREAM_TOTAL" "response" "RESPONSE" -}}
{{- $timeoutLimits := dict "connect" $hour "firstToken" $hour "streamIdle" $hour "streamTotal" (mul 24 $hour) "response" $hour -}}
{{- $minBytes := 1024 -}}
{{- $maxBytes := 268435456 -}}
{{- $firstAttempts := dict -}}
{{- range $role, $settings := $ai.roles -}}
{{- if not (regexMatch "^[a-z][a-z0-9_]{0,31}$" $role) -}}
{{- fail (printf "ai.roles.%s: a role name is 1 to 32 lower-case letters, digits or _, starting with a letter" $role) -}}
{{- end -}}
{{- if not (kindIs "map" $settings) -}}
{{- fail (printf "ai.roles.%s must be a map with models" $role) -}}
{{- end -}}
{{- range $key := keys $settings -}}
{{- if not (has $key (list "models" "maxConcurrent" "queueWait" "timeouts" "maxResponseBytes" "maxEventBytes")) -}}
{{- fail (printf "ai.roles.%s.%s: unknown setting" $role $key) -}}
{{- end -}}
{{- end -}}
{{- $models := $settings.models -}}
{{- if not (and (kindIs "slice" $models) $models (le (len $models) 8)) -}}
{{- fail (printf "ai.roles.%s.models must list 1 to 8 models, the first choice first" $role) -}}
{{- end -}}
{{- range $models -}}
{{- /* Printable ASCII without space or comma: the variable is a comma-separated list. */ -}}
{{- if not (and (kindIs "string" .) (regexMatch "^[a-z][a-z0-9_]{0,31}/[!-+\\-.-~]{1,256}$" .)) -}}
{{- fail (printf "ai.roles.%s.models: each model is <provider>/<model id>, the id 1 to 256 printable ASCII characters without spaces or commas" $role) -}}
{{- end -}}
{{- $provider := index (splitList "/" .) 0 -}}
{{- if not (has $provider $configured) -}}
{{- fail (printf "ai.roles.%s.models: %s needs ai.providers.%s.apiKeySecret.name, the Secret with that provider's key" $role . $provider) -}}
{{- end -}}
{{- end -}}
{{- if ne (len $models) (len (uniq $models)) -}}
{{- fail (printf "ai.roles.%s.models lists a model twice" $role) -}}
{{- end -}}
{{- $prefix := printf "AI_GATEWAY__ROLES__%s__" (upper $role) -}}
{{- $_ := set $data (printf "%sMODELS" $prefix) (join "," $models) -}}
{{- if hasKey $settings "maxConcurrent" -}}
{{- $_ := set $data (printf "%sMAX_CONCURRENT" $prefix) (include "trustiot.aiGateway.count" (dict "value" $settings.maxConcurrent "key" (printf "ai.roles.%s.maxConcurrent" $role) "min" 1 "max" 10000)) -}}
{{- end -}}
{{- $responseBytes := 33554432 -}}
{{- if hasKey $settings "maxResponseBytes" -}}
{{- $text := include "trustiot.aiGateway.count" (dict "value" $settings.maxResponseBytes "key" (printf "ai.roles.%s.maxResponseBytes" $role) "min" $minBytes "max" $maxBytes) -}}
{{- $responseBytes = atoi $text -}}
{{- $_ := set $data (printf "%sMAX_RESPONSE_BYTES" $prefix) $text -}}
{{- end -}}
{{- $eventBytes := 4194304 -}}
{{- if hasKey $settings "maxEventBytes" -}}
{{- $text := include "trustiot.aiGateway.count" (dict "value" $settings.maxEventBytes "key" (printf "ai.roles.%s.maxEventBytes" $role) "min" $minBytes "max" $maxBytes) -}}
{{- $eventBytes = atoi $text -}}
{{- $_ := set $data (printf "%sMAX_EVENT_BYTES" $prefix) $text -}}
{{- end -}}
{{- if gt $eventBytes $responseBytes -}}
{{- fail (printf "ai.roles.%s.maxEventBytes (%d) must not exceed maxResponseBytes (%d; 32 MiB unless set)" $role $eventBytes $responseBytes) -}}
{{- end -}}
{{- $queueWait := 0 -}}
{{- if hasKey $settings "queueWait" -}}
{{- $queueWait = include "trustiot.aiGateway.ms" (dict "value" $settings.queueWait "key" (printf "ai.roles.%s.queueWait" $role)) | atoi -}}
{{- if gt $queueWait $hour -}}
{{- fail (printf "ai.roles.%s.queueWait must be at most 1h" $role) -}}
{{- end -}}
{{- $_ := set $data (printf "%sQUEUE_WAIT" $prefix) $settings.queueWait -}}
{{- end -}}
{{- $timeouts := dict "response" 300000 "firstToken" 60000 -}}
{{- if hasKey $settings "timeouts" -}}
{{- if not (kindIs "map" $settings.timeouts) -}}
{{- fail (printf "ai.roles.%s.timeouts must be a map" $role) -}}
{{- end -}}
{{- range $key, $value := $settings.timeouts -}}
{{- if not (hasKey $timeoutNames $key) -}}
{{- fail (printf "ai.roles.%s.timeouts.%s: unknown time limit; the gateway has %s" $role $key (keys $timeoutNames | sortAlpha | join ", ")) -}}
{{- end -}}
{{- $ms := include "trustiot.aiGateway.ms" (dict "value" $value "key" (printf "ai.roles.%s.timeouts.%s" $role $key)) | atoi -}}
{{- if or (eq $ms 0) (gt $ms (get $timeoutLimits $key)) -}}
{{- fail (printf "ai.roles.%s.timeouts.%s must be more than 0 and at most %s" $role $key (ternary "24h" "1h" (eq $key "streamTotal"))) -}}
{{- end -}}
{{- $_ := set $timeouts $key $ms -}}
{{- $_ := set $data (printf "%sTIMEOUTS__%s" $prefix (get $timeoutNames $key)) $value -}}
{{- end -}}
{{- end -}}
{{- $_ := set $firstAttempts $role (add $queueWait (max (get $timeouts "response") (get $timeouts "firstToken"))) -}}
{{- end -}}
{{- $requestTimeout := include "trustiot.aiGateway.ms" (dict "value" $ai.requestTimeout "key" "ai.requestTimeout") | atoi -}}
{{- if or (eq $requestTimeout 0) (gt $requestTimeout $hour) -}}
{{- fail "ai.requestTimeout must be more than 0 and at most 1h" -}}
{{- end -}}
{{- /* The gateway keeps a tenth of the request timeout, at most 2 s, to answer in the caller's
protocol before the kit's bare 504; rounded up, so the chart is never laxer than the gateway. */ -}}
{{- $budget := sub $requestTimeout (min 2000 (div (add $requestTimeout 9) 10)) -}}
{{- range $role, $firstAttempt := $firstAttempts -}}
{{- if gt (int64 $firstAttempt) (int64 $budget) -}}
{{- fail (printf "ai.roles.%s: queueWait plus the longer of timeouts.response (5m unless set) and timeouts.firstToken (1m unless set) is %d ms, more than the %d ms ai.requestTimeout leaves (the gateway keeps a tenth of it, at most 2 s); raise ai.requestTimeout or lower the role's limits" $role (int64 $firstAttempt) (int64 $budget)) -}}
{{- end -}}
{{- end -}}
{{- $_ := set $data "AI_GATEWAY__HTTP__REQUEST_TIMEOUT" $ai.requestTimeout -}}
{{- $drain := int $config.drainSeconds -}}
{{- if or (lt $drain 11) (gt $drain 3605) -}}
{{- fail "services.ai-gateway.drainSeconds must be 11 to 3605: the chart gives the gateway drainSeconds - 10 s to let streams in flight end (shutdown.drain_timeout, at most 3595 s with the kit's 5 s drain delay)" -}}
{{- end -}}
{{- $_ := set $data "AI_GATEWAY__SHUTDOWN__DRAIN_TIMEOUT" (printf "%ds" (sub $drain 10)) -}}
{{- dict "data" $data | toYaml -}}
{{- end -}}
