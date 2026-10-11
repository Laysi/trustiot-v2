{{/*
OTLP/HTTP endpoint of the collector, given to every service as
TRUSTIOT__TELEMETRY__OTLP_ENDPOINT. Plain HTTP inside the cluster: ztunnel adds mTLS between pods.
*/}}
{{- define "trustiot.collector.endpoint" -}}
{{- printf "http://%s.%s.svc:4318" (include "trustiot.componentName" (dict "root" . "name" "otel-collector")) .Release.Namespace -}}
{{- end -}}

{{/*
Redaction rules of the collector (decision 22, Laysi/trustiot#110): logs must not hold chat
content, request bodies, credentials or personal data. The services already mask fields by name
(crates/service-kit/src/telemetry/redact.rs); the collector applies the same rules again to every
log, span and metric data point it receives, whatever the sender, and also looks inside values.

  keyPatterns         attributes (resource, scope, log, span, span event, data point) and log body
                      map keys whose names look like credentials, personal data, payloads or chat
                      content: the value is replaced with ****. The same words as service-kit:
                      substrings with or without separators, whole name segments (separators and
                      camelCase), and payload words.
  ignoredKeyPatterns  payload words followed by a descriptor (http.request.body.size,
                      content_type): sizes and types, kept as they are.
  valuePatterns       inside every attribute value, the log body and span names and status
                      messages: each match is replaced with ****, in this order. Only the
                      matching part is replaced, the rest of the value stays readable: the
                      redaction processor (v0.161.0, processor.go maskValue) masks with
                      regexp.ReplaceAllStringFunc, and so does OTTL's replace_pattern
                      (docs/test-reports/data/2026-10-11-73-otel-collector/local-redaction.txt).
Go RE2 syntax. Returns YAML; use with fromYaml.
*/}}
{{- define "trustiot.collector.redactionRules" -}}
keyPatterns:
  - '(?i)(access[-_.]?key|access[-_.]?token|api[-_.]?key|auth[-_.]?token|authorization|bearer|connection[-_.]?string|cookie|credential|database[-_.]?url|id[-_.]?token|passphrase|passwd|password|private[-_.]?key|refresh[-_.]?token|secret|session[-_.]?id|signing[-_.]?key)'
  - '(^|[^A-Za-z0-9])(?i:emails?|jwt|otp|phone|prompts?|tokens?|body|completion|contents?)([^a-z0-9]|$)'
  - '[a-z0-9](Emails?|EMAILS?|Jwt|JWT|Otp|OTP|Phone|PHONE|Prompts?|PROMPTS?|Tokens?|TOKENS?|Body|BODY|Completion|COMPLETION|Contents?|CONTENTS?)([^a-z0-9]|$)'
ignoredKeyPatterns:
  - '(^|[^A-Za-z0-9])(?i:body|completion|contents?)[-_.]?(?i:bytes|count|encoding|len|length|size|type)$'
  - '[a-z0-9](Body|Completion|Contents?)(Bytes|Count|Encoding|Len|Length|Size|Type)$'
valuePatterns:
  # Private keys and URLs with a user and password.
  - '-----BEGIN [A-Z ]*PRIVATE KEY-----'
  - '\b[A-Za-z][A-Za-z0-9+.-]*://[^/\s:@]+:[^/\s@]+@'
  # Authorization header values, JWTs and well-known API key formats.
  - '(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/-]{8,}=*'
  - '\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]*'
  - '\b(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,}|sk-[A-Za-z0-9_-]{20,}|xox[abposr]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16})\b'
  # key=value and key: value pairs whose key names a credential.
  - '(?i)\b(password|passwd|pwd|secret|token|api[-_]?key|access[-_]?key|client[-_]?secret|session[-_]?id)\s*[:=]\s*[^\s&,;]+'
  # Personal data: email addresses, Taiwanese mobile and international phone numbers, and
  # Taiwanese national ID numbers.
  - '[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}'
  - '\b09[0-9]{2}-?[0-9]{3}-?[0-9]{3}\b'
  - '\+[1-9][0-9]{0,3}[ -]?[0-9]{1,4}[ -]?[0-9]{3,4}[ -]?[0-9]{3,4}\b'
  - '\b[A-Z][12][0-9]{8}\b'
{{- end -}}

{{/*
The collector's ClickHouse exporter endpoint: the bundled server's native protocol, or the
external server's HTTPS URL.
*/}}
{{- define "trustiot.collector.clickhouseEndpoint" -}}
{{- if eq .Values.clickhouse.mode "bundled" -}}
{{- printf "tcp://%s.%s.svc:9000?dial_timeout=10s" (include "trustiot.componentName" (dict "root" . "name" "clickhouse")) .Release.Namespace -}}
{{- else -}}
{{- .Values.clickhouse.external.url -}}
{{- end -}}
{{- end -}}

{{/*
Collector configuration. Secrets stay out of it: the exporters read them from environment
variables (${env:...}) that the Deployment maps from Secrets.
  receivers   OTLP/HTTP on 4318 (service-kit sends logs, traces and metrics there).
  processors  memory_limiter first; then redaction (attribute keys and values, log bodies) and,
              for traces, transform/redact (span names and status messages).
  exporters   clickhouse (logs and traces, insert only, create_schema off), the SIEM forward
              when enabled, and nop for metrics until Laysi/trustiot-v2#183 decides their storage.
Returns YAML; use with fromYaml.
*/}}
{{- define "trustiot.collector.config" -}}
{{- $collector := .Values.observability.collector -}}
{{- $forward := $collector.forward -}}
{{- $rules := include "trustiot.collector.redactionRules" . | fromYaml -}}
{{- $ottl := list -}}
{{- range $field := list "name" "status.message" -}}
{{- range $pattern := $rules.valuePatterns -}}
{{- $literal := $pattern | replace "\\" "\\\\" | replace "\"" "\\\"" -}}
{{- $ottl = append $ottl (printf "replace_pattern(%s, \"%s\", \"****\")" $field $literal) -}}
{{- end -}}
{{- end -}}
{{- $clickhouse := dict
  "endpoint" (include "trustiot.collector.clickhouseEndpoint" .)
  "database" "trustiot_logs"
  "username" "${env:CLICKHOUSE_USER}"
  "password" "${env:CLICKHOUSE_PASSWORD}"
  "create_schema" false
  "logs_table_name" "otel_logs"
  "traces_table_name" "otel_traces"
  "async_insert" true
  "compress" "lz4"
  "timeout" "10s"
  "sending_queue" (dict "enabled" true "queue_size" 1000 "batch" (dict "flush_timeout" "2s" "min_size" 2000 "max_size" 10000))
  "retry_on_failure" (dict "enabled" true "initial_interval" "5s" "max_interval" "30s" "max_elapsed_time" "300s")
-}}
{{- if and (eq .Values.clickhouse.mode "external") .Values.clickhouse.external.caSecret.name -}}
{{- $_ := set $clickhouse "tls" (dict "ca_file" "/etc/trustiot/clickhouse-ca/ca.crt") -}}
{{- end -}}
{{- $exporters := dict "clickhouse" $clickhouse "nop" dict -}}
{{- $forwardExporter := "" -}}
{{- if $forward.enabled -}}
{{- $forwardExporter = ternary "otlp_grpc/siem" "otlp_http/siem" (eq $forward.protocol "grpc") -}}
{{- $siem := dict "endpoint" $forward.endpoint "sending_queue" (dict "enabled" true "queue_size" 1000) "retry_on_failure" (dict "enabled" true "max_elapsed_time" "300s") -}}
{{- if $forward.authHeader.name -}}
{{- $_ := set $siem "headers" (dict $forward.authHeader.name "${env:FORWARD_AUTH_HEADER}") -}}
{{- end -}}
{{- if $forward.caSecret.name -}}
{{- $_ := set $siem "tls" (dict "ca_file" "/etc/trustiot/forward-ca/ca.crt") -}}
{{- else if and (eq $forward.protocol "grpc") (hasPrefix "http://" $forward.endpoint) -}}
{{- /* The gRPC exporter uses TLS unless told otherwise, whatever the scheme. */ -}}
{{- $_ := set $siem "tls" (dict "insecure" true) -}}
{{- end -}}
{{- $_ := set $exporters $forwardExporter $siem -}}
{{- end -}}
{{- $pipelines := dict -}}
{{- range $signal := list "logs" "traces" -}}
{{- $processors := list "memory_limiter" "redaction" -}}
{{- if eq $signal "traces" -}}
{{- $processors = append $processors "transform/redact" -}}
{{- end -}}
{{- $to := list "clickhouse" -}}
{{- if and $forward.enabled (has $signal $forward.signals) -}}
{{- $to = append $to $forwardExporter -}}
{{- end -}}
{{- $_ := set $pipelines $signal (dict "receivers" (list "otlp") "processors" $processors "exporters" $to) -}}
{{- end -}}
{{- $_ := set $pipelines "metrics" (dict "receivers" (list "otlp") "processors" (list "memory_limiter") "exporters" (list "nop")) -}}
{{- $config := dict
  "extensions" (dict "health_check" (dict "endpoint" "0.0.0.0:13133"))
  "receivers" (dict "otlp" (dict "protocols" (dict "http" (dict "endpoint" "0.0.0.0:4318"))))
  "processors" (dict
    "memory_limiter" (dict "check_interval" "1s" "limit_percentage" 80 "spike_limit_percentage" 20)
    "redaction" (dict
      "allow_all_keys" true
      "redact_all_types" true
      "blocked_key_patterns" $rules.keyPatterns
      "ignored_key_patterns" $rules.ignoredKeyPatterns
      "blocked_values" $rules.valuePatterns
      "summary" "silent")
    "transform/redact" (dict
      "error_mode" "ignore"
      "trace_statements" (list (dict "context" "span" "statements" $ottl))))
  "exporters" $exporters
  "service" (dict
    "extensions" (list "health_check")
    "pipelines" $pipelines
    "telemetry" (dict "logs" (dict "level" "info" "encoding" "json")))
-}}
{{- toYaml $config -}}
{{- end -}}

{{/*
Container ports and probes of the collector. The Deployment renders them from here, and the
NetworkPolicy derives the probe ports from the same definition ("trustiot.mesh.probePorts"), so
only the health_check port (13133) is open to the node; OTLP (4318) is reachable only through
the mesh.
Returns YAML {ports, probes: {startup, liveness, readiness}}; use with fromYaml.
*/}}
{{- define "trustiot.otel-collector.pod" -}}
ports:
  - name: otlp-http
    containerPort: 4318
    protocol: TCP
  - name: health
    containerPort: 13133
    protocol: TCP
probes:
  startup:
    httpGet:
      path: /
      port: health
    periodSeconds: 2
    failureThreshold: 30
  liveness:
    httpGet:
      path: /
      port: health
    periodSeconds: 10
    timeoutSeconds: 2
    failureThreshold: 3
  readiness:
    httpGet:
      path: /
      port: health
    periodSeconds: 5
    timeoutSeconds: 2
    failureThreshold: 2
{{- end -}}
