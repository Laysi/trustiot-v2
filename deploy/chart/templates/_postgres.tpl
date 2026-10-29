{{/*
Postgres helpers (decisions 21 and 29, checklist M3 and M8, Laysi/trustiot-v2#58).

bundled: a CloudNativePG Cluster in the release namespace runs our image with Apache AGE
(deploy/images/postgres). The operator and its CRDs are a cluster-wide platform component,
installed once per cluster like Istio (deploy/cloudnative-pg/README.md), never by this chart.
external: a Postgres with Apache AGE that runs elsewhere (postgres.external).
In both modes services connect with verify-full and trust only the CA in their CA file.
*/}}

{{/*
Name of the bundled Cluster and of everything CloudNativePG derives from it: the Services
<name>-rw, -ro and -r, the Secrets <name>-ca, -server, -app and -replication, the pods <name>-<n>.
CloudNativePG refuses a Cluster name longer than 50 characters. A longer name keeps the start of
the release's name and adds 8 characters of its SHA-256 before "-postgres", so two releases in one
namespace whose names start alike still get different Clusters.
*/}}
{{- define "trustiot.postgres.clusterName" -}}
{{- $suffix := "-postgres" -}}
{{- $full := include "trustiot.fullname" . -}}
{{- if le (add (len $full) (len $suffix)) 50 -}}
{{- printf "%s%s" $full $suffix -}}
{{- else -}}
{{- $hash := sha256sum $full | trunc 8 -}}
{{- $prefix := $full | trunc (int (sub 50 (add (len $suffix) 9))) | trimSuffix "-" -}}
{{- printf "%s-%s%s" $prefix $hash $suffix -}}
{{- end -}}
{{- end -}}

{{/*
PostgreSQL major version of deploy/images/postgres. CloudNativePG reads the major version from the
image catalog, because the image's tag is the TrustIoT version, not a PostgreSQL version. A
major upgrade is a deliberate change of the image and this number together (chart test).
*/}}
{{- define "trustiot.postgres.majorVersion" -}}
18
{{- end -}}

{{/*
Apache AGE version in deploy/images/postgres (AGE_VERSION in its Containerfile, chart test).
CloudNativePG installs the extension at this version and runs ALTER EXTENSION ... UPDATE when it
changes, as a superuser, so no TrustIoT role needs superuser for it.
*/}}
{{- define "trustiot.postgres.ageVersion" -}}
1.8.0
{{- end -}}

{{/* The database that every service uses, created at bootstrap. */}}
{{- define "trustiot.postgres.database" -}}
trustiot
{{- end -}}

{{/*
Owner of the database and the migration role: a login role with CREATEROLE (granted at bootstrap,
templates/postgres.yaml) and without superuser or BYPASSRLS. CloudNativePG generates its password
into the Secret <cluster>-app. Only the migration Job (Laysi/trustiot-v2#60) may use it; no
service may read that Secret ("trustiot.dependencySecretNames").
*/}}
{{- define "trustiot.postgres.owner" -}}
trustiot_owner
{{- end -}}

{{/* PostgreSQL's port, on the Cluster's Services and pods. */}}
{{- define "trustiot.postgres.port" -}}
5432
{{- end -}}

{{/*
Port of CloudNativePG's instance manager in every Postgres pod: the kubelet's probes and the
operator's status checks (HTTPS).
*/}}
{{- define "trustiot.postgres.statusPort" -}}
8000
{{- end -}}

{{/*
Host name services connect to: the read-write Service of the bundled Cluster, which always points
at the primary. CloudNativePG's server certificate lists it, so verify-full matches it.
*/}}
{{- define "trustiot.postgres.host" -}}
{{- if eq .Values.postgres.mode "external" -}}
{{- .Values.postgres.external.host -}}
{{- else -}}
{{- printf "%s-rw.%s.svc" (include "trustiot.postgres.clusterName" .) .Release.Namespace -}}
{{- end -}}
{{- end -}}

{{/*
Where services find Postgres, as the platform settings name it: TRUSTIOT_POSTGRES_HOST, _PORT,
_DATABASE and _SSLMODE in templates/platform-config.yaml, and <prefix>_POSTGRES_* for a service
whose settings carry another prefix ("trustiot.service.env"). One place, so the two cannot
disagree. bundled: CloudNativePG's read-write Service, whose server certificate names the host.
Returns YAML {host, port, database, sslMode}, every value a string; use with fromYaml.
*/}}
{{- define "trustiot.postgres.connection" -}}
{{- if eq .Values.postgres.mode "external" -}}
{{- with .Values.postgres.external -}}
{{- dict "host" (include "trustiot.postgres.host" $) "port" (toString .port) "database" (toString .database) "sslMode" (toString .sslMode) | toYaml -}}
{{- end -}}
{{- else -}}
{{- dict "host" (include "trustiot.postgres.host" .) "port" (include "trustiot.postgres.port" .) "database" (include "trustiot.postgres.database" .) "sslMode" "verify-full" | toYaml -}}
{{- end -}}
{{- end -}}

{{/*
Secrets that CloudNativePG creates for the bundled Cluster. The server CA (<cluster>-ca) holds the
CA's private key next to its certificate, and <cluster>-app holds the database owner's password,
so no service's custom env may read any of them; services get only the CA certificate, as a file
("trustiot.postgres.caSecret"). Empty in external mode.
Returns a YAML list of names; use with fromYamlArray.
*/}}
{{- define "trustiot.postgres.generatedSecrets" -}}
{{- $names := list -}}
{{- if eq .Values.postgres.mode "bundled" -}}
{{- $cluster := include "trustiot.postgres.clusterName" . -}}
{{- range $suffix := list "ca" "server" "app" "replication" "superuser" -}}
{{- $names = append $names (printf "%s-%s" $cluster $suffix) -}}
{{- end -}}
{{- end -}}
{{- toYaml $names -}}
{{- end -}}

{{/*
The CA certificate that signed the Postgres server certificate, as {name, key} of a Secret, or an
empty name when none is configured. bundled: the certificate in CloudNativePG's generated
<cluster>-ca (only the ca.crt key is mounted, never ca.key); external: postgres.external.caSecret.
Returns YAML; use with fromYaml.
*/}}
{{- define "trustiot.postgres.caSecret" -}}
{{- if eq .Values.postgres.mode "bundled" -}}
{{- dict "name" (printf "%s-ca" (include "trustiot.postgres.clusterName" .)) "key" "ca.crt" | toYaml -}}
{{- else -}}
{{- dict "name" .Values.postgres.external.caSecret.name "key" .Values.postgres.external.caSecret.key | toYaml -}}
{{- end -}}
{{- end -}}

{{/*
Enabled services that use Postgres: the ones that list `postgres` in mesh.calls. Only they get the
server CA file (<prefix>_POSTGRES_CA_FILE, "trustiot.service.env"), a login role with a generated
password, and, in bundled mode, reach the Postgres pods through the NetworkPolicy. Declaring the dependency once keeps the two in step.
Returns YAML {services: [<name>, ...]} in name order; use with fromYaml.
*/}}
{{- define "trustiot.postgres.clients" -}}
{{- $clients := list -}}
{{- range $caller, $callees := (.Values.mesh.calls | default dict) -}}
{{- $entry := get $.Values.services $caller -}}
{{- if and $entry $entry.enabled (kindIs "slice" $callees) (has "postgres" $callees) -}}
{{- $clients = append $clients $caller -}}
{{- end -}}
{{- end -}}
{{- dict "services" (sortAlpha $clients) | toYaml -}}
{{- end -}}

{{/*
pg_hba rules the chart adds in front of CloudNativePG's default `host all all all scram-sha-256`.
CloudNativePG's own rules come first (local peer for the instance manager, certificate
authentication for streaming_replica), so this only reaches client connections over TCP: one
without TLS is refused by the server too, whatever sslmode the client asks for.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.postgres.pgHba" -}}
{{- list "hostnossl all all all reject" | toYaml -}}
{{- end -}}

{{/*
Image of the bundled Cluster: our CloudNativePG operand image with AGE, released with the services
(the tag defaults to Chart.appVersion), pinned by digest when one is set.
*/}}
{{- define "trustiot.postgres.image" -}}
{{- $image := .Values.postgres.bundled.image -}}
{{- $tag := $image.tag | default .Chart.AppVersion -}}
{{- if $image.digest -}}
{{- printf "%s:%s@%s" $image.repository $tag $image.digest -}}
{{- else -}}
{{- printf "%s:%s" $image.repository $tag -}}
{{- end -}}
{{- end -}}

{{/*
Connection budget (Laysi/trustiot-v2#575). The services' roles have no connection limit of their
own, so what all their replicas can open together, old and new ones during a rolling update
included, must fit Postgres' max_connections; "trustiot.postgres.checkConnections" refuses values
where it does not (README.md, 「叢集內的 Postgres」).

Per service that may use Postgres, what one replica opens at most:
  maxConnections  {min, max}: the range of services.<name>.database.maxConnections, which the chart
                  passes as <prefix>__DATABASE__MAX_CONNECTIONS (TRUSTIOT, or AI_GATEWAY for
                  ai-gateway), the size of the service's main pool; the service refuses to start
                  outside this range.
  otherPools      every other pool the replica opens, at its largest, whatever role it logs in as:
                  ai-gateway       none: the settings pool is its only one
                  assets           PLATFORM_CONNECTIONS (its platform role)
                  assistant        none: only the service role's pool (Laysi/trustiot-v2#592)
                  certs            PLATFORM_CONNECTIONS (its platform role)
                  identity         the tenant, login, login send, delegation and event log pools
                                   as identity_app (16), and the event log's pool as
                                   identity_platform, its consumed producers plus 2 (5)
                  ingest           PLATFORM_CONNECTIONS (its platform role; Laysi/trustiot-v2#642)
                  telemetry-query  EVENT_READER_CONNECTIONS (its platform role)
A service that lists postgres under mesh.calls without an entry here fails the render: give it one
when it starts using Postgres. JSON, not YAML, so that each service's Rust test reads this very
text (crates/testkit, `chart::postgres_connection_budget`) and fails when the pools in its code and
these numbers differ.
*/}}
{{- define "trustiot.postgres.connectionBudgets" -}}
{
  "ai-gateway": { "maxConnections": { "min": 2, "max": 32 }, "otherPools": 0 },
  "assets": { "maxConnections": { "min": 2, "max": 100 }, "otherPools": 4 },
  "assistant": { "maxConnections": { "min": 2, "max": 100 }, "otherPools": 0 },
  "certs": { "maxConnections": { "min": 2, "max": 100 }, "otherPools": 4 },
  "identity": { "maxConnections": { "min": 3, "max": 100 }, "otherPools": 21 },
  "ingest": { "maxConnections": { "min": 3, "max": 100 }, "otherPools": 4 },
  "telemetry-query": { "maxConnections": { "min": 2, "max": 100 }, "otherPools": 2 }
}
{{- end -}}

{{/*
Connections of the bundled Postgres that no service's pool may take (README.md, 「叢集內的 Postgres」):
  superuserReserved  superuser_reserved_connections, PostgreSQL's default 3, which neither
                     CloudNativePG 1.30.1 nor these values change: the last 3 are for superusers
  instanceManager    CloudNativePG's instance manager as postgres over the local socket: a pool of
                     at most 3 per database (pkg/management/postgres/pool/pool.go,
                     SetMaxOpenConns(3)) on postgres and trustiot (the AGE extension); template1
                     only at bootstrap, before any service runs
  metricsExporter    cnpg_metrics_exporter, the same kind of pool, on postgres and trustiot
  migrationJob       tools/migrate: its session as the owner, and one login at a time to check a
                     role's password
  platformAdminBootstrap
                     the Job an operator starts from the platform-admin-bootstrap CronJob
                     (templates/platform-admin-bootstrap.yaml): one session as the owner, which
                     may overlap a rollout
JSON like the budgets above.
*/}}
{{- define "trustiot.postgres.reservedConnections" -}}
{
  "superuserReserved": 3,
  "instanceManager": 6,
  "metricsExporter": 6,
  "migrationJob": 2,
  "platformAdminBootstrap": 1
}
{{- end -}}

{{/*
The size of one service's main pool: services.<name>.database.maxConnections, within the range in
"trustiot.postgres.connectionBudgets". Fails when the service has no budget or no value, or the
value is out of that range.
Argument: dict "root" $ "name" <service> "config" <effective settings>.
*/}}
{{- define "trustiot.postgres.poolSize" -}}
{{- $budget := get (include "trustiot.postgres.connectionBudgets" .root | fromJson) .name -}}
{{- if not $budget -}}
{{- fail (printf "services.%s lists postgres under mesh.calls, but the chart has no connection budget for it: add the pools it opens to trustiot.postgres.connectionBudgets (templates/_postgres.tpl)" .name) -}}
{{- end -}}
{{- $size := dig "database" "maxConnections" nil .config -}}
{{- if kindIs "invalid" $size -}}
{{- fail (printf "services.%s.database.maxConnections is required: the service uses Postgres (mesh.calls), and the connection check counts its pool" .name) -}}
{{- end -}}
{{- $min := int $budget.maxConnections.min -}}
{{- $max := int $budget.maxConnections.max -}}
{{- if or (ne (toString (int $size)) (toString $size)) (lt (int $size) $min) (gt (int $size) $max) -}}
{{- fail (printf "services.%s.database.maxConnections must be a whole number from %d to %d, the range the service accepts" .name $min $max) -}}
{{- end -}}
{{- int $size -}}
{{- end -}}

{{/*
What the enabled services that use Postgres ("trustiot.postgres.clients") can open together while
all of them roll at once (decision 4: services are released together), and what stays reserved.
Each service: (database.maxConnections + otherPools) per replica, times its replicas plus the pods
a rolling update adds ("trustiot.service.maxSurge").
Returns YAML {services: [{name, perReplica, pods, connections}], servicesTotal, reserved, total};
use with fromYaml.
*/}}
{{- define "trustiot.postgres.connectionPlan" -}}
{{- $budgets := include "trustiot.postgres.connectionBudgets" . | fromJson -}}
{{- $surge := int (include "trustiot.service.maxSurge" .) -}}
{{- $rows := list -}}
{{- $sum := 0 -}}
{{- range $name := (include "trustiot.postgres.clients" . | fromYaml).services -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" $ "name" $name "values" (get $.Values.services $name)) | fromYaml -}}
{{- $pool := int (include "trustiot.postgres.poolSize" (dict "root" $ "name" $name "config" $config)) -}}
{{- $perReplica := add $pool (int (get $budgets $name).otherPools) -}}
{{- $pods := add (int $config.replicas) $surge -}}
{{- $connections := mul $perReplica $pods -}}
{{- $rows = append $rows (dict "name" $name "perReplica" $perReplica "pods" $pods "connections" $connections) -}}
{{- $sum = add $sum $connections -}}
{{- end -}}
{{- $reserved := 0 -}}
{{- range $_, $count := include "trustiot.postgres.reservedConnections" . | fromJson -}}
{{- $reserved = add $reserved (int $count) -}}
{{- end -}}
{{- dict "services" $rows "servicesTotal" $sum "reserved" $reserved "total" (add $sum $reserved) | toYaml -}}
{{- end -}}

{{/*
Refuses values under which the services could open more connections than the bundled Postgres
accepts ("trustiot.postgres.connectionPlan"): a connection past max_connections fails with
SQLSTATE 53300, and a new replica that cannot open its connections never gets ready, so the
rollout stalls. In external mode only each service's pool size is checked: the customer sets that
server's max_connections (README.md says how many it must allow). Renders nothing.
*/}}
{{- define "trustiot.postgres.checkConnections" -}}
{{- $plan := include "trustiot.postgres.connectionPlan" . | fromYaml -}}
{{- if eq .Values.postgres.mode "bundled" -}}
{{- $limit := int .Values.postgres.bundled.parameters.max_connections -}}
{{- if gt (int $plan.total) $limit -}}
{{- $parts := list -}}
{{- range $plan.services -}}
{{- $parts = append $parts (printf "%s %d x %d" .name (int .perReplica) (int .pods)) -}}
{{- end -}}
{{- fail (printf "postgres.bundled.parameters.max_connections is %d, but %d connections are needed: the services can open %d while all of them roll at once (per replica x pods: %s), and %d stay reserved for superusers, CloudNativePG, the migration Job and the platform administrator bootstrap. Raise max_connections within the memory limit, or lower replicas or services.<name>.database.maxConnections (README.md, 「叢集內的 Postgres」)" $limit (int $plan.total) (int $plan.servicesTotal) (join ", " $parts) (int $plan.reserved)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
