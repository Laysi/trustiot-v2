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
