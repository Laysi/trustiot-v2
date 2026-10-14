{{/*
Server configuration of the bundled ClickHouse, mounted as /etc/clickhouse-server/config.d (it
replaces the image's docker_related_config.xml). Listening, ports and logging only: access
entities are never configured in files (decision 14).
*/}}
{{- define "trustiot.clickhouse.serverConfig" -}}
<clickhouse>
    <!-- Listen on every address; listen_try tolerates a cluster without IPv6 or IPv4. -->
    <listen_host>::</listen_host>
    <listen_host>0.0.0.0</listen_host>
    <listen_try>1</listen_try>
    <!-- Only HTTP (8123) and the native protocol (9000): no MySQL or PostgreSQL wire
         protocol, and no interserver port on a single node. -->
    <mysql_port remove="remove"/>
    <postgresql_port remove="remove"/>
    <interserver_http_port remove="remove"/>
    <!-- Log to the container's stdout only; the root filesystem is read-only. -->
    <logger>
        <level>information</level>
        <console>1</console>
        <log remove="remove"/>
        <errorlog remove="remove"/>
    </logger>
</clickhouse>
{{- end -}}

{{/*
Container ports and probes of the bundled ClickHouse. The StatefulSet renders them from here, and
the NetworkPolicy derives the probe ports from the same definition ("trustiot.mesh.probePorts").
Returns YAML {ports, probes: {startup, liveness, readiness}}; use with fromYaml.
*/}}
{{- define "trustiot.clickhouse.pod" -}}
ports:
  - name: http
    containerPort: 8123
    protocol: TCP
  - name: native
    containerPort: 9000
    protocol: TCP
probes:
  startup:
    httpGet:
      path: /ping
      port: http
    periodSeconds: 5
    failureThreshold: 60
  liveness:
    httpGet:
      path: /ping
      port: http
    periodSeconds: 10
    timeoutSeconds: 3
    failureThreshold: 6
  readiness:
    httpGet:
      path: /ping
      port: http
    periodSeconds: 5
    timeoutSeconds: 3
    failureThreshold: 3
{{- end -}}

{{/*
The bundled ClickHouse's bootstrap admin password (Laysi/trustiot-v2#403), as YAML {name, key,
generated}; use with fromYaml. By default the Secret "<release>-clickhouse-admin-password", which
templates/passwords.yaml declares without data and the init container of the ClickHouse pod fills
("trustiot.clickhouse.adminInitContainer"), with the password under the key named after the
account; generated is true. With clickhouse.bundled.adminSecret.name set (installations from
before Laysi/trustiot-v2#403, until they move over; README.md, "叢集內的 ClickHouse"), that
existing Secret and key. An empty map when ClickHouse is not bundled.
*/}}
{{- define "trustiot.clickhouse.adminPassword" -}}
{{- if eq .Values.clickhouse.mode "bundled" -}}
{{- $ch := .Values.clickhouse.bundled -}}
{{- if $ch.adminSecret.name -}}
{{- toYaml (dict "name" $ch.adminSecret.name "key" $ch.adminSecret.key "generated" false) -}}
{{- else -}}
{{- toYaml (dict "name" (include "trustiot.componentName" (dict "root" . "name" "clickhouse-admin-password")) "key" $ch.adminUser "generated" true) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
clickhouse.bundled.adminPasswordGeneration as the init container reads it: 1 to 9 decimal digits
(tools/migrate compares generations as numbers). Fails the render otherwise.
*/}}
{{- define "trustiot.clickhouse.adminPasswordGeneration" -}}
{{- $text := include "trustiot.decimal" (dict "value" .Values.clickhouse.bundled.adminPasswordGeneration "key" "clickhouse.bundled.adminPasswordGeneration") -}}
{{- if not (regexMatch "^[0-9]{1,9}$" $text) -}}
{{- fail (printf "clickhouse.bundled.adminPasswordGeneration: %q must render as 1 to 9 decimal digits" $text) -}}
{{- end -}}
{{- $text -}}
{{- end -}}

{{/*
Where the server's entrypoint reads the bootstrap admin password (CLICKHOUSE_PASSWORD_FILE, the
file "password" in it): an in-memory emptyDir that the init container fills, or the existing
Secret of clickhouse.bundled.adminSecret. Only the ClickHouse pod mounts it.
*/}}
{{- define "trustiot.clickhouse.adminPasswordDir" -}}
/etc/trustiot/clickhouse-admin
{{- end -}}

{{/*
Init container of the ClickHouse pod while the chart generates the admin password:
`migrate clickhouse-admin-password` (tools/migrate/src/clickhouse_admin.rs) makes sure the Secret
holds a password of clickhouse.bundled.adminPasswordGeneration, generating one with the operating
system's CSPRNG when it holds none, holds an older generation or is not the object it filled, and
writes the password into the in-memory volume the server reads it from. It runs before every start
of the pod, so the server always starts with the password the Secret holds; the migration Job reads
it from the Secret. It authenticates with a projected token of ClickHouse's ServiceAccount, which
only this container mounts, and the Role in clickhouse.yaml lets that ServiceAccount get and patch
this one Secret. It runs the migration image (arm64 only, like the Job).
*/}}
{{- define "trustiot.clickhouse.adminInitContainer" -}}
{{- $admin := include "trustiot.clickhouse.adminPassword" . | fromYaml -}}
{{- $dir := include "trustiot.migrate.kubeDir" . -}}
name: admin-password
image: {{ include "trustiot.migrate.image" . }}
imagePullPolicy: {{ .Values.migrate.image.pullPolicy }}
args:
  - clickhouse-admin-password
env:
  - name: TRUSTIOT__ADMIN__API_URL
    value: https://kubernetes.default.svc
  - name: TRUSTIOT__ADMIN__NAMESPACE
    valueFrom:
      fieldRef:
        fieldPath: metadata.namespace
  - name: TRUSTIOT__ADMIN__TOKEN_FILE
    value: {{ printf "%s/token" $dir }}
  - name: TRUSTIOT__ADMIN__CA_FILE
    value: {{ printf "%s/ca.crt" $dir }}
  - name: TRUSTIOT__ADMIN__SECRET
    value: {{ $admin.name }}
  - name: TRUSTIOT__ADMIN__ACCOUNT
    value: {{ $admin.key }}
  - name: TRUSTIOT__ADMIN__GENERATION
    value: {{ include "trustiot.clickhouse.adminPasswordGeneration" . | quote }}
  - name: TRUSTIOT__ADMIN__PASSWORD_FILE
    value: {{ printf "%s/password" (include "trustiot.clickhouse.adminPasswordDir" .) }}
resources:
  {{- toYaml .Values.migrate.resources | nindent 2 }}
securityContext:
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities:
    drop: [ALL]
volumeMounts:
  - name: admin-password
    mountPath: {{ include "trustiot.clickhouse.adminPasswordDir" . }}
  - name: migrate-kube-api
    mountPath: {{ $dir }}
    readOnly: true
{{- end -}}
