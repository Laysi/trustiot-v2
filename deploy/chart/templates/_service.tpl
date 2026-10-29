{{/*
Generic TrustIoT service: ServiceAccount, Service and Deployment.
Every template takes dict "root" $ "name" <service> "config" <effective settings>,
where the settings come from "trustiot.serviceConfig".
*/}}

{{/*
Container ports of a service: its ports, then the health port (healthPort, named "health"),
where the service serves /healthz and /readyz and nothing else. The Service exposes only ports:
the kubelet is the health port's one client, and no ingress route can name it. Fails when a port
reuses the health port's name or number, or two ports share a name or number.
Argument: dict "name" <service> "config" <effective settings>.
Returns YAML {ports: [{name, containerPort, ...}]}; use with fromYaml.
*/}}
{{- define "trustiot.service.containerPorts" -}}
{{- $health := dict "name" "health" "containerPort" (int .config.healthPort) -}}
{{- $ports := list -}}
{{- range .config.ports -}}
{{- if or (eq .name "health") (eq (int .containerPort) $health.containerPort) -}}
{{- fail (printf "services.%s: ports must not use the name health or the health port %d (healthPort)" $.name $health.containerPort) -}}
{{- end -}}
{{- $ports = append $ports . -}}
{{- end -}}
{{- $ports = append $ports $health -}}
{{- $names := list -}}
{{- $numbers := list -}}
{{- range $ports -}}
{{- $names = append $names .name -}}
{{- $numbers = append $numbers (int .containerPort) -}}
{{- end -}}
{{- if or (ne (len $names) (len (uniq $names))) (ne (len $numbers) (len (uniq $numbers))) -}}
{{- fail (printf "services.%s: port names and container ports must be unique" .name) -}}
{{- end -}}
{{- dict "ports" $ports | toYaml -}}
{{- end -}}

{{/*
Prefix of a service's own settings (service-kit's env prefix, crates/service-kit/README.md, 「設定」):
TRUSTIOT, except the AI gateway, which carries no product name (decision 28) and reads AI_GATEWAY.
Argument: the service name.
*/}}
{{- define "trustiot.service.envPrefix" -}}
{{- ternary "AI_GATEWAY" "TRUSTIOT" (eq . "ai-gateway") -}}
{{- end -}}

{{/*
Pods a rolling update starts above a service's replicas before it stops old ones (the Deployment's
maxSurge; maxUnavailable is 0, so a single replica rolls without downtime). The Postgres connection
check counts them ("trustiot.postgres.connectionPlan"), so both read it here.
*/}}
{{- define "trustiot.service.maxSurge" -}}
1
{{- end -}}

{{- define "trustiot.service.serviceAccount" -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include "trustiot.componentName" . }}
  labels:
    {{- include "trustiot.componentLabels" . | nindent 4 }}
automountServiceAccountToken: {{ .config.automountServiceAccountToken }}
{{- end -}}

{{- define "trustiot.service.service" -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ include "trustiot.componentName" . }}
  labels:
    {{- include "trustiot.componentLabels" . | nindent 4 }}
spec:
  # Always ClusterIP: services, and above all the MQTT broker, are never exposed directly.
  # Outside traffic enters only through the ingress gateway (decisions 6 and 27).
  type: ClusterIP
  selector:
    {{- include "trustiot.selectorLabels" . | nindent 4 }}
  ports:
    {{- /* The identity service's token exchange, the certs service's MQTT broker certificate
    issuance and the AI gateway's admin API have a Service of their own
    ("trustiot.mesh.ownPolicyPorts"). */}}
    {{- $ownPort := get (include "trustiot.mesh.ownPolicyPorts" . | fromYaml) .name }}
    {{- range .config.ports }}
    {{- if ne .name $ownPort }}
    - name: {{ .name }}
      port: {{ .servicePort | default .containerPort }}
      targetPort: {{ .name }}
      protocol: {{ .protocol | default "TCP" }}
      {{- with .appProtocol }}
      appProtocol: {{ . }}
      {{- end }}
    {{- end }}
    {{- end }}
{{- end -}}

{{/*
Container env: the chart's variables first (OTEL_SERVICE_NAME, the collector endpoint, SENTRY_DSN,
the Postgres CA file, login role, password and pool size for a service that uses Postgres, the
credentials the service lists, for identity, its routes and signing key settings, for certs, its CA
files, for mqtt-bridge, the broker's address, for a service that gets its broker account, the
broker's address and the account ("trustiot.mqttBroker.serviceCredentials"), for assistant, the
addresses of the services it calls, and for ai-gateway, its admin API's listen address),
then serviceDefaults.env and services.<name>.env.
Custom env cannot take over what the chart controls:
- names starting with TRUSTIOT_ plus a letter or digit, SENTRY_ or OTEL_ are reserved, so the
  platform settings from the ConfigMap (envFrom loses to env) and the chart's Secrets stay as set;
- while the collector is enabled, TRUSTIOT__TELEMETRY__OTLP_ENDPOINT is reserved too, so every
  service's telemetry goes through the collector's redaction;
- serviceDefaults.env reaches every service, so it must not read Secrets;
- secret-like names (as in values.schema.json) come from valueFrom.secretKeyRef only;
- dependency credentials reach a service only through services.<name>.credentials;
- the Secrets an in-cluster component creates for itself (the bundled Postgres's CA key, server key
  and owner password) and the password Secrets the migration Job fills (each service's and the
  collector's) are out of reach in every key: a service gets its own database password only
  through the chart;
- for a service that uses Postgres, <prefix>__DATABASE__USERNAME and <prefix>__DATABASE__PASSWORD
  are the chart's, and so are <prefix>__DATABASE__PLATFORM_USERNAME and
  <prefix>__DATABASE__PLATFORM_PASSWORD (set for a service whose platform role logs in) and
  <prefix>__DATABASE__MAX_CONNECTIONS (services.<name>.database.maxConnections, which the
  Postgres connection check counts), where <prefix> is the service's settings prefix
  ("trustiot.service.envPrefix"); a service whose prefix is not TRUSTIOT does not read the
  platform ConfigMap's TRUSTIOT_POSTGRES_*, so it also gets <prefix>_POSTGRES_HOST, _PORT,
  _DATABASE and _SSLMODE ("trustiot.postgres.connection"), which are the chart's too, like
  <prefix>_POSTGRES_CA_FILE;
- for mqtt-bridge, TRUSTIOT__BROKER__ADDRESS is the chart's ("trustiot.mqttBridge.env"): the bridge
  relays the gateways' connections to the in-cluster broker only, even while it is off;
- for a service whose program reads the broker settings ("trustiot.mqttBroker.credentialServices",
  certs and ingest), every <prefix>__MQTT__ name is the chart's, with the broker or without it: its broker
  account is the in-cluster broker's, and only the chart hands it the password file;
- for assistant, the addresses of the AI gateway, the identity service's token exchange,
  telemetry-query and assets are the chart's ("trustiot.assistant.env"), derived from
  mesh.calls.assistant, even while unset: an address in env would point it at a service the mesh
  does not let it call;
- for telemetry-query and ingest, TRUSTIOT__CLICKHOUSE__USERNAME and TRUSTIOT__CLICKHOUSE__PASSWORD
  are the chart's: the service's ClickHouse account and the password the migration Job generates
  for it;
- for ai-gateway, whose settings carry the prefix AI_GATEWAY ("trustiot.service.envPrefix"), the
  names of its platform settings and settings file (AI_GATEWAY_ plus a letter or digit), its
  providers and roles (AI_GATEWAY__PROVIDERS__*, AI_GATEWAY__ROLES__*) and every other variable of
  its ConfigMap are the chart's ("trustiot.aiGateway.config"), so the providers' key variables,
  endpoints and models come from values.ai only; container env would win over the ConfigMap. Its
  admin API's settings (AI_GATEWAY__ADMIN__*) are the chart's too ("trustiot.aiGateway.env"):
  the admin API listens on the pod's address only while the identity service, the one caller its
  policies admit, is deployed (templates/ai-gateway-admin.yaml, Laysi/trustiot-v2#607), and on
  loopback otherwise.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.service.env" -}}
{{- $name := .name -}}
{{- $config := .config -}}
{{- $values := .root.Values -}}
{{- $prefix := include "trustiot.service.envPrefix" .name -}}
{{- $assistantNames := list -}}
{{- if eq $name "assistant" -}}
{{- $assistantNames = include "trustiot.assistant.reserved" .root | fromYamlArray -}}
{{- end -}}
{{- $aiGatewayNames := list -}}
{{- if eq $name "ai-gateway" -}}
{{- $aiGatewayNames = keys (include "trustiot.aiGateway.config" .root | fromYaml).data -}}
{{- end -}}
{{- $brokerClient := has $name (include "trustiot.mqttBroker.credentialServices" .root | fromYaml).services -}}
{{- $dbEnv := list -}}
{{- if has $name (include "trustiot.postgres.clients" .root | fromYaml).services -}}
{{- /* The service's own Postgres login role and its password, which the migration Job generates
into the service's own Secret and sets. */ -}}
{{- $secret := include "trustiot.migrate.passwordSecret" (dict "root" .root "component" $name) | fromYaml -}}
{{- $dbEnv = append $dbEnv (dict "name" (printf "%s__DATABASE__USERNAME" $prefix) "value" $secret.key) -}}
{{- $dbEnv = append $dbEnv (dict "name" (printf "%s__DATABASE__PASSWORD" $prefix) "valueFrom" (dict "secretKeyRef" (dict "name" $secret.name "key" $secret.key))) -}}
{{- /* The size of its main pool, which the connection check counts with its other pools
("trustiot.postgres.connectionPlan", Laysi/trustiot-v2#575). */ -}}
{{- $poolSize := include "trustiot.postgres.poolSize" (dict "root" .root "name" $name "config" $config) -}}
{{- $dbEnv = append $dbEnv (dict "name" (printf "%s__DATABASE__MAX_CONNECTIONS" $prefix) "value" $poolSize) -}}
{{- if ne $prefix "TRUSTIOT" -}}
{{- /* Where Postgres is, under the service's own prefix: the platform ConfigMap names it
TRUSTIOT_POSTGRES_*, which such a service does not read. */ -}}
{{- $postgres := include "trustiot.postgres.connection" .root | fromYaml -}}
{{- range $setting, $field := dict "HOST" "host" "PORT" "port" "DATABASE" "database" "SSLMODE" "sslMode" -}}
{{- $dbEnv = append $dbEnv (dict "name" (printf "%s_POSTGRES_%s" $prefix $setting) "value" (get $postgres $field)) -}}
{{- end -}}
{{- end -}}
{{- /* Its platform role too, when it logs in ("trustiot.migrate.platformLogins"): the role an
outbox consumer reads its producers' events with. */ -}}
{{- if include "trustiot.migrate.platformLogin" (dict "root" .root "name" $name) -}}
{{- $platform := include "trustiot.migrate.passwordSecret" (dict "root" .root "component" $name "role" "platform") | fromYaml -}}
{{- $dbEnv = append $dbEnv (dict "name" (printf "%s__DATABASE__PLATFORM_USERNAME" $prefix) "value" $platform.key) -}}
{{- $dbEnv = append $dbEnv (dict "name" (printf "%s__DATABASE__PLATFORM_PASSWORD" $prefix) "valueFrom" (dict "secretKeyRef" (dict "name" $platform.name "key" $platform.key))) -}}
{{- end -}}
{{- end -}}
{{- $clickhouseEnv := list -}}
{{- if has $name (list "telemetry-query" "ingest") -}}
{{- /* Its own ClickHouse account: the migration Job generates its password into the service's
Secret and sets it. telemetry-query's creates the tenants' read-only accounts
(services/ingest/migrations/clickhouse/0008_telemetry_query.sql); ingest's holds the role
trustiot_ingest alone, which the Job gives it (Laysi/trustiot-v2#642). */ -}}
{{- $account := include "trustiot.migrate.passwordSecret" (dict "root" .root "component" $name "role" "clickhouse") | fromYaml -}}
{{- $clickhouseEnv = append $clickhouseEnv (dict "name" "TRUSTIOT__CLICKHOUSE__USERNAME" "value" $account.key) -}}
{{- $clickhouseEnv = append $clickhouseEnv (dict "name" "TRUSTIOT__CLICKHOUSE__PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" $account.name "key" $account.key))) -}}
{{- end -}}
{{- range $values.serviceDefaults.env -}}
{{- if get (.valueFrom | default dict) "secretKeyRef" -}}
{{- fail (printf "serviceDefaults.env: %s must not read a Secret, because every service gets serviceDefaults.env; set it under services.<name>.env" .name) -}}
{{- end -}}
{{- end -}}
{{- $protected := include "trustiot.dependencySecretRefs" .root | fromYamlArray -}}
{{- $protectedSecrets := include "trustiot.dependencySecretNames" .root | fromYamlArray -}}
{{- range $config.env -}}
{{- if regexMatch "(?i)^(TRUSTIOT_[A-Z0-9]|SENTRY_|OTEL_)" .name -}}
{{- fail (printf "services.%s: env %s: names starting with TRUSTIOT_, SENTRY_ or OTEL_ are set by the chart" $name .name) -}}
{{- end -}}
{{- if and $values.observability.collector.enabled (eq (upper .name) (printf "%s__TELEMETRY__OTLP_ENDPOINT" $prefix)) -}}
{{- fail (printf "services.%s: env %s is set by the chart: it points at the OpenTelemetry collector (observability.collector)" $name .name) -}}
{{- end -}}
{{- if and (eq $name "mqtt-bridge") (regexMatch "(?i)^TRUSTIOT__BROKER__ADDRESS$" .name) -}}
{{- fail (printf "services.mqtt-bridge: env %s is set by the chart: the bridge relays only to the in-cluster broker's gateway port (mqttBroker.enabled)" .name) -}}
{{- end -}}
{{- if and $brokerClient (regexMatch (printf "(?i)^%s__MQTT__" $prefix) .name) -}}
{{- fail (printf "services.%s: env %s is set by the chart: the service's broker account is the in-cluster broker's (mqttBroker.enabled, mqtt-broker in mesh.calls.%s), and only the chart hands it the password" $name .name $name) -}}
{{- end -}}
{{- if and (eq $name "assistant") (has (upper .name) $assistantNames) -}}
{{- fail (printf "services.assistant: env %s is set by the chart from mesh.calls.assistant: the assistant calls only the services the mesh lets it call" .name) -}}
{{- end -}}
{{- if eq $name "ai-gateway" -}}
{{- $reserved := or (regexMatch "(?i)^AI_GATEWAY_[A-Z0-9]" .name) (regexMatch "(?i)^AI_GATEWAY__(PROVIDERS|ROLES)__" .name) (has (upper .name) $aiGatewayNames) -}}
{{- if $reserved -}}
{{- fail (printf "services.ai-gateway: env %s is set by the chart from values.ai (the providers, their key variables and endpoints, the model roles and the gateway's time limits)" .name) -}}
{{- end -}}
{{- if regexMatch "(?i)^AI_GATEWAY__ADMIN__" .name -}}
{{- fail (printf "services.ai-gateway: env %s is set by the chart: the admin API changes every tenant's models, so it listens beyond loopback only for the identity service, behind its own Service and policies (templates/ai-gateway-admin.yaml, Laysi/trustiot-v2#607)" .name) -}}
{{- end -}}
{{- end -}}
{{- if and $dbEnv (or (regexMatch (printf "(?i)^%s__DATABASE__(PLATFORM_)?(USERNAME|PASSWORD)$" $prefix) .name) (regexMatch (printf "(?i)^%s_POSTGRES_" $prefix) .name)) -}}
{{- fail (printf "services.%s: env %s is set by the chart: where Postgres is, the service's login roles and the passwords the migration Job generates for them" $name .name) -}}
{{- end -}}
{{- if and $dbEnv (regexMatch (printf "(?i)^%s__DATABASE__MAX_CONNECTIONS$" $prefix) .name) -}}
{{- fail (printf "services.%s: env %s is set by the chart from services.%s.database.maxConnections, which the Postgres connection check counts" $name .name $name) -}}
{{- end -}}
{{- if and $clickhouseEnv (regexMatch "(?i)^TRUSTIOT__CLICKHOUSE__(USERNAME|PASSWORD)$" .name) -}}
{{- fail (printf "services.%s: env %s is set by the chart: the service's ClickHouse account and the password the migration Job generates for it" $name .name) -}}
{{- end -}}
{{- if regexMatch "(?i)(secret|passw|passphrase|pwd|token|api_?key|(^|_)key$|dsn|private|credential)" .name -}}
{{- $from := .valueFrom | default dict -}}
{{- if or (hasKey . "value") (ne (len (keys $from)) 1) (not (hasKey $from "secretKeyRef")) -}}
{{- fail (printf "services.%s: env %s looks like a secret, so it must come from valueFrom.secretKeyRef only" $name .name) -}}
{{- end -}}
{{- end -}}
{{- with get (.valueFrom | default dict) "secretKeyRef" -}}
{{- if has (printf "%s/%s" .name .key) $protected -}}
{{- fail (printf "services.%s: env reads the dependency credential %s/%s; list the dependency under services.%s.credentials instead" $name .name .key $name) -}}
{{- end -}}
{{- if has .name $protectedSecrets -}}
{{- fail (printf "services.%s: env reads Secret %s, whose keys reach services only through the chart (the bundled Postgres's own Secrets, the passwords generated in the cluster, the migration accounts)" $name .name) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $env := list (dict "name" "OTEL_SERVICE_NAME" "value" $name) -}}
{{- if $values.observability.collector.enabled -}}
{{- /* service-kit exports logs, traces and metrics only when its own setting names the endpoint,
under the service's own prefix. */ -}}
{{- $env = append $env (dict "name" (printf "%s__TELEMETRY__OTLP_ENDPOINT" $prefix) "value" (include "trustiot.collector.endpoint" .root)) -}}
{{- end -}}
{{- with $values.observability.sentry -}}
{{- if .enabled -}}
{{- $env = append $env (dict "name" "SENTRY_DSN" "valueFrom" (dict "secretKeyRef" (dict "name" .dsnSecret.name "key" .dsnSecret.key))) -}}
{{- end -}}
{{- end -}}
{{- /* The Postgres server's CA, only where the service template mounts it ("trustiot.caMounts"). */ -}}
{{- range include "trustiot.caMounts" (dict "root" .root "name" $name) | fromYamlArray -}}
{{- if eq .volume "postgres-ca" -}}
{{- $env = append $env (dict "name" (printf "%s_POSTGRES_CA_FILE" $prefix) "value" (printf "%s/ca.crt" .dir)) -}}
{{- end -}}
{{- end -}}
{{- $credentialEnv := include "trustiot.credentialEnv" (dict "root" .root "name" $name "credentials" ($config.credentials | default list)) | fromYamlArray -}}
{{- $identityEnv := list -}}
{{- if eq $name "identity" -}}
{{- $identityEnv = include "trustiot.identity.env" .root | fromYamlArray -}}
{{- end -}}
{{- $certsEnv := list -}}
{{- if eq $name "certs" -}}
{{- $certsEnv = include "trustiot.certs.env" .root | fromYamlArray -}}
{{- end -}}
{{- $mqttBridgeEnv := list -}}
{{- if eq $name "mqtt-bridge" -}}
{{- $mqttBridgeEnv = include "trustiot.mqttBridge.env" .root | fromYamlArray -}}
{{- end -}}
{{- $brokerEnv := (include "trustiot.mqttBroker.serviceCredentials" (dict "root" .root "name" $name) | fromYaml).env -}}
{{- $assistantEnv := list -}}
{{- if eq $name "assistant" -}}
{{- $assistantEnv = include "trustiot.assistant.env" .root | fromYamlArray -}}
{{- end -}}
{{- $aiGatewayEnv := list -}}
{{- if eq $name "ai-gateway" -}}
{{- $aiGatewayEnv = include "trustiot.aiGateway.env" .root | fromYamlArray -}}
{{- end -}}
{{- $env = concat $env $dbEnv $clickhouseEnv $credentialEnv $identityEnv $certsEnv $mqttBridgeEnv $brokerEnv $assistantEnv $aiGatewayEnv ($config.env | default list) -}}
{{- $names := list -}}
{{- range $env -}}
{{- $names = append $names .name -}}
{{- end -}}
{{- if ne (len $names) (len (uniq $names)) -}}
{{- fail (printf "services.%s: env names must be unique (%s)" $name (join ", " $names)) -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}

{{- define "trustiot.service.deployment" -}}
{{- $config := .config -}}
{{- $preStop := int $config.preStopSleepSeconds -}}
{{- /* Kubernetes counts the preStop sleep in the grace period, so add it to the drain time. */ -}}
{{- $grace := add $preStop (int $config.drainSeconds) -}}
{{- if not $config.probes.liveness -}}
{{- fail (printf "services.%s: probes.liveness is required (docs/architecture/deployment.md)" .name) -}}
{{- end -}}
{{- if not $config.probes.readiness -}}
{{- fail (printf "services.%s: probes.readiness is required (docs/architecture/deployment.md)" .name) -}}
{{- end -}}
{{- $containerPorts := (include "trustiot.service.containerPorts" . | fromYaml).ports -}}
{{- $portNames := list -}}
{{- range $containerPorts -}}
{{- $portNames = append $portNames .name -}}
{{- end -}}
{{- /* A port with a Service and policies of its own ("trustiot.mesh.ownPolicyPorts") is never
probed, by name or by number: the node source reaches every probe port without mTLS or
AuthorizationPolicy, so a probe there would open it to every process on the node. */ -}}
{{- $ownPort := get (include "trustiot.mesh.ownPolicyPorts" . | fromYaml) .name -}}
{{- $ownNumber := 0 -}}
{{- range $containerPorts -}}
{{- if and $ownPort (eq .name $ownPort) -}}
{{- $ownNumber = int .containerPort -}}
{{- end -}}
{{- end -}}
{{- range $kind, $probe := $config.probes -}}
{{- range $handler := list "httpGet" "tcpSocket" "grpc" -}}
{{- $port := (get $probe $handler | default dict).port -}}
{{- if and (kindIs "string" $port) (not (has $port $portNames)) -}}
{{- fail (printf "services.%s: probes.%s uses port %q, which is not in ports" $.name $kind $port) -}}
{{- end -}}
{{- $own := "" -}}
{{- if and $ownPort (kindIs "string" $port) (eq $port $ownPort) -}}
{{- $own = printf "%q" $port -}}
{{- else if and $ownNumber (or (kindIs "float64" $port) (kindIs "int" $port) (kindIs "int64" $port)) -}}
{{- if eq (int $port) $ownNumber -}}
{{- $own = printf "%d, the port %s" $ownNumber $ownPort -}}
{{- end -}}
{{- end -}}
{{- if $own -}}
{{- fail (printf "services.%s: probes.%s uses port %s, which has a Service and policies of its own: the node reaches a pod's probe ports without mTLS or AuthorizationPolicy, so every process on the node could call it; probe the health port (README.md, 「節點來源」)" $.name $kind $own) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $env := include "trustiot.service.env" . | fromYamlArray -}}
{{- $caMounts := include "trustiot.caMounts" (dict "root" .root "name" .name) | fromYamlArray -}}
{{- /* The service's own key of the MQTT broker's accounts Secret, while it gets its account
(_mqtt-broker.tpl). */ -}}
{{- $brokerAccount := include "trustiot.mqttBroker.serviceCredentials" (dict "root" .root "name" .name) | fromYaml -}}
{{- $migrate := include "trustiot.migrate.enabled" .root -}}
{{- /* The internal identity keys (decision 27, _identity.tpl): while the identity service is
enabled, every service mounts the public keys ConfigMap; only the identity service mounts the
signing keys Secret, which its init container fills with a token that only init containers mount. */ -}}
{{- $keys := include "trustiot.identity.keys" .root | fromYaml -}}
{{- $identity := eq .name "identity" -}}
{{- $publicKeys := include "trustiot.identity.enabled" .root -}}
{{- if and $identity $config.automountServiceAccountToken -}}
{{- fail "services.identity.automountServiceAccountToken must be false: the service reads and signs with the internal identity private key, and a token in its container could write a key of its own into the Secret and the public keys ConfigMap (docs/architecture/secrets.md, 「內部身分簽章金鑰」)" -}}
{{- end -}}
{{- /* The gateway certificate authority (_certs.tpl): only the certs service mounts the issuing
CA keys Secret, which its init container fills with a token that only init containers mount, and
the CA certificates ConfigMap. */ -}}
{{- $certs := eq .name "certs" -}}
{{- $ca := dict -}}
{{- if $certs -}}
{{- $ca = include "trustiot.certs.ca" .root | fromYaml -}}
{{- if $config.automountServiceAccountToken -}}
{{- fail "services.certs.automountServiceAccountToken must be false: the service signs with the issuing CA private key, and a token in its container could read the keys Secret or write a request of its own into the requests ConfigMap (services/certs/README.md, 「CA 階層與私鑰保管」)" -}}
{{- end -}}
{{- end -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "trustiot.componentName" . }}
  labels:
    {{- include "trustiot.componentLabels" . | nindent 4 }}
spec:
  replicas: {{ $config.replicas }}
  revisionHistoryLimit: 3
  strategy:
    type: RollingUpdate
    rollingUpdate:
      # Start the new pod before stopping the old one, so a single replica rolls without downtime.
      maxUnavailable: 0
      maxSurge: {{ include "trustiot.service.maxSurge" .root }}
  selector:
    matchLabels:
      {{- include "trustiot.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      {{- /* The chart's own keys win over podLabels and podAnnotations (sprig merge keeps the
      first value), so the selector labels and the checksum cannot be overridden. */}}
      labels:
        {{- $labels := include "trustiot.componentLabels" . | fromYaml }}
        {{- /* Istio ambient: ztunnel adds mTLS to every connection of this pod (decision 27). */}}
        {{- $_ := set $labels "istio.io/dataplane-mode" "ambient" }}
        {{- toYaml (merge $labels ($config.podLabels | default dict)) | nindent 8 }}
      annotations:
        {{- /* Restart pods when the platform settings they read through envFrom change. */}}
        {{- $checksum := include (print .root.Template.BasePath "/platform-config.yaml") .root | sha256sum }}
        {{- $annotations := dict "checksum/platform-config" $checksum }}
        {{- /* The AI gateway reads its settings at startup only (templates/ai-gateway.yaml). */}}
        {{- if eq .name "ai-gateway" }}
        {{- $_ := set $annotations "checksum/ai-gateway-config" (include (print .root.Template.BasePath "/ai-gateway.yaml") .root | sha256sum) }}
        {{- end }}
        {{- toYaml (merge $annotations ($config.podAnnotations | default dict)) | nindent 8 }}
    spec:
      serviceAccountName: {{ include "trustiot.componentName" . }}
      automountServiceAccountToken: {{ $config.automountServiceAccountToken }}
      enableServiceLinks: false
      terminationGracePeriodSeconds: {{ $grace }}
      securityContext:
        {{- toYaml $config.podSecurityContext | nindent 8 }}
      {{- with .root.Values.global.imagePullSecrets }}
      imagePullSecrets:
        {{- range . }}
        - name: {{ . }}
        {{- end }}
      {{- end }}
      {{- with $config.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $config.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $config.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- if or $identity $certs $migrate }}
      initContainers:
        {{- if $identity }}
        {{- /* Generates, publishes and rotates the signing keys (_identity.tpl); needs no database. */}}
        - {{ include "trustiot.identity.initContainer" (dict "root" .root "config" $config) | indent 10 | trim }}
        {{- end }}
        {{- if $certs }}
        {{- /* Generates the issuing CA keys and their requests (_certs.tpl); needs no database. */}}
        - {{ include "trustiot.certs.initContainer" (dict "root" .root "config" $config) | indent 10 | trim }}
        {{- end }}
        {{- if $migrate }}
        {{- /* Holds the pod until the migration Job is complete (_migrate.tpl). */}}
        - {{ include "trustiot.migrate.initContainer" (dict "root" .root "config" $config) | indent 10 | trim }}
        {{- end }}
      {{- end }}
      containers:
        - name: {{ .name }}
          image: {{ include "trustiot.serviceImage" (dict "root" .root "name" .name "image" $config.image) }}
          imagePullPolicy: {{ $config.image.pullPolicy }}
          ports:
            {{- range $containerPorts }}
            - name: {{ .name }}
              containerPort: {{ .containerPort }}
              protocol: {{ .protocol | default "TCP" }}
            {{- end }}
          envFrom:
            - configMapRef:
                name: {{ include "trustiot.platformConfigName" .root }}
            {{- if eq .name "ai-gateway" }}
            - configMapRef:
                name: {{ include "trustiot.aiGateway.configName" .root }}
            {{- end }}
          env:
            {{- toYaml $env | nindent 12 }}
          {{- with $config.probes.startup }}
          startupProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          livenessProbe:
            {{- toYaml $config.probes.liveness | nindent 12 }}
          readinessProbe:
            {{- toYaml $config.probes.readiness | nindent 12 }}
          {{- if gt $preStop 0 }}
          lifecycle:
            preStop:
              sleep:
                seconds: {{ $preStop }}
          {{- end }}
          resources:
            {{- toYaml $config.resources | nindent 12 }}
          securityContext:
            {{- toYaml $config.securityContext | nindent 12 }}
          volumeMounts:
            - name: tmp
              mountPath: /tmp
            {{- range $caMounts }}
            - name: {{ .volume }}
              mountPath: {{ .dir }}
              readOnly: true
            {{- end }}
            {{- if $publicKeys }}
            - name: internal-identity-keys
              mountPath: {{ include "trustiot.identity.publicKeysDir" .root }}
              readOnly: true
            {{- end }}
            {{- if $identity }}
            - name: identity-signing-keys
              mountPath: {{ include "trustiot.identity.signingKeysDir" .root }}
              readOnly: true
            {{- end }}
            {{- if $certs }}
            - name: certs-ca-keys
              mountPath: {{ include "trustiot.certs.keysDir" .root }}
              readOnly: true
            - name: certs-ca-certificates
              mountPath: {{ include "trustiot.certs.certificatesDir" .root }}
              readOnly: true
            {{- end }}
            {{- with $brokerAccount.volumeMounts }}
            {{- toYaml . | nindent 12 }}
            {{- end }}
      volumes:
        - name: tmp
          emptyDir:
            sizeLimit: {{ $config.tmpSizeLimit }}
        {{- if or $identity $certs $migrate }}
        - {{ include "trustiot.migrate.kubeApiVolume" .root | indent 10 | trim }}
        {{- end }}
        {{- range $caMounts }}
        - name: {{ .volume }}
          secret:
            secretName: {{ .secret }}
            items:
              - key: {{ .key }}
                path: ca.crt
        {{- end }}
        {{- /* The whole ConfigMap, never subPath, so the kubelet replaces the file when the init
        container publishes new keys; optional, so a pod starts (unready) before it has any. */}}
        {{- if $publicKeys }}
        - name: internal-identity-keys
          configMap:
            name: {{ $keys.configMap }}
            optional: true
        {{- end }}
        {{- if $identity }}
        - name: identity-signing-keys
          secret:
            secretName: {{ $keys.secret }}
            {{- /* 0440: root and the pod's fsGroup (services.identity.podSecurityContext) only. */}}
            defaultMode: 288
            {{- /* Optional: the Secret has no key before the init container's first run, and the
            kubelet adds the file at its next sync. */}}
            optional: true
            items:
              - key: {{ $keys.secretKey }}
                path: {{ $keys.secretKey }}
        {{- if $keys.legacy }}
        {{- /* A key a person made before Laysi/trustiot-v2#404: only the init container mounts it. */}}
        - name: legacy-identity-signing-key
          secret:
            secretName: {{ .root.Values.internalIdentity.signingKeySecret.name }}
            defaultMode: 288
            items:
              - key: {{ .root.Values.internalIdentity.signingKeySecret.key }}
                path: signing-key.pem
        {{- end }}
        {{- end }}
        {{- if $certs }}
        {{- /* The whole ConfigMap and Secret, never subPath, so the kubelet replaces the files when the
        init container adds a key or an operator sets a certificate; the service reads them again. */}}
        - name: certs-ca-keys
          secret:
            secretName: {{ $ca.secret }}
            {{- /* 0440: root and the pod's fsGroup (services.certs.podSecurityContext) only. */}}
            defaultMode: 288
            {{- /* Optional: the Secret has no key before the init container's first run, and the
            kubelet adds the file at its next sync. */}}
            optional: true
            items:
              - key: {{ $ca.secretKey }}
                path: {{ $ca.secretKey }}
        - name: certs-ca-certificates
          configMap:
            name: {{ $ca.certificates }}
        {{- end }}
        {{- with $brokerAccount.volumes }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
{{- end -}}
