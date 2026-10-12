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
server CA file (TRUSTIOT_POSTGRES_CA_FILE) and, in bundled mode, reach the Postgres pods through
the NetworkPolicy. Declaring the dependency once keeps the two in step.
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
