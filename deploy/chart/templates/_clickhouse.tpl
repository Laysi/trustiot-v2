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
