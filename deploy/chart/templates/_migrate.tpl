{{/*
Migration Job helpers (tools/migrate, Laysi/trustiot-v2#60; README.md, "migration Job").

The Job applies every service's Postgres migrations and the ClickHouse DDL with the migration role,
creates the service roles and the collector's ClickHouse account, and holds a Postgres advisory
lock for the whole run, so two runs never overlap. Every service pod waits for it in an init
container ("trustiot.migrate.initContainer"), so no service starts on a schema older than its own.

It is an ordinary release object, not a Helm hook: with bundled Postgres and ClickHouse the
databases are created by the same install, so a pre-install hook would wait for databases that do
not exist yet, and a post-install hook would deadlock with `helm install --wait`, which waits for
services that wait for the Job.
*/}}

{{/*
A whole number from values as exact decimal digits: a values file gives YAML integers to the
template as float64, which toString renders as "1e+06" from 1000000 on (a --set value stays a
string). dict "value" <number or string> "key" <values path, for the error>.
*/}}
{{- define "trustiot.decimal" -}}
{{- $value := .value -}}
{{- if kindIs "string" $value -}}
{{- $value -}}
{{- else if or (kindIs "float64" $value) (kindIs "int" $value) (kindIs "int64" $value) -}}
{{- if ne (float64 (int64 $value)) (float64 $value) -}}
{{- fail (printf "%s: %v is not a whole number" .key $value) -}}
{{- end -}}
{{- printf "%d" (int64 $value) -}}
{{- else -}}
{{- fail (printf "%s: a number or a string of digits, not a %s" .key (kindOf $value)) -}}
{{- end -}}
{{- end -}}

{{/*
migrate.passwordGeneration as the Job reads it: 1 to 9 decimal digits (tools/migrate compares
generations as numbers). Fails the render otherwise, so a value the Job would refuse never reaches
it.
*/}}
{{- define "trustiot.migrate.passwordGeneration" -}}
{{- $text := include "trustiot.decimal" (dict "value" .Values.migrate.passwordGeneration "key" "migrate.passwordGeneration") -}}
{{- if not (regexMatch "^[0-9]{1,9}$" $text) -}}
{{- fail (printf "migrate.passwordGeneration: %q must render as 1 to 9 decimal digits" $text) -}}
{{- end -}}
{{- $text -}}
{{- end -}}

{{/* Whether this release runs the migration Job. */}}
{{- define "trustiot.migrate.enabled" -}}
{{- if .Values.migrate.enabled -}}true{{- end -}}
{{- end -}}

{{/* Image of the Job and of the services' init container, pinned by digest when one is set. */}}
{{- define "trustiot.migrate.image" -}}
{{- $image := .Values.migrate.image -}}
{{- $tag := $image.tag | default .Chart.AppVersion -}}
{{- if $image.digest -}}
{{- printf "%s:%s@%s" $image.repository $tag $image.digest -}}
{{- else -}}
{{- printf "%s:%s" $image.repository $tag -}}
{{- end -}}
{{- end -}}

{{/*
Login role of a service that uses Postgres, as tools/migrate names it (registry.rs,
Service::app_role): the service name with "-" written "_", then "_app". Its password is the key of
the same name in the service's password Secret ("trustiot.migrate.passwordSecrets").
Argument: the service name.
*/}}
{{- define "trustiot.migrate.appRole" -}}
{{- printf "%s_app" (replace "-" "_" .) -}}
{{- end -}}

{{/*
Platform role of a service, as tools/migrate names it (registry.rs, Service::platform_role): the
service name with "-" written "_", then "_platform". Argument: the service name.
*/}}
{{- define "trustiot.migrate.platformRole" -}}
{{- printf "%s_platform" (replace "-" "_" .) -}}
{{- end -}}

{{/*
The services whose platform role logs in (Laysi/trustiot-v2#65): the role that reads across
tenants (service-kit's PlatformDb), which every outbox consumer needs to read its producers'
events. tools/migrate creates a platform role for every service its registry marks, but a role
logs in only while a Secret is bound to it, and the chart binds one only to the services listed
here: a platform role nobody logs in with stays NOLOGIN. identity is listed because it keeps
the event log (Laysi/trustiot-v2#130) and reads the events it records as its platform role.
tools/migrate/tests/registry.rs checks this list against the registry: every consumer is in it,
and nothing without a platform role is.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.migrate.platformLogins" -}}
{{- list "assets" "certs" "identity" "ingest" "telemetry-query" | toYaml -}}
{{- end -}}

{{/*
The Secrets that hold the generated passwords (docs/architecture/secrets.md, "migration Job 產生的密碼"),
one per consumer, so a node's kubelet can read only the passwords of the pods scheduled on it:
- each enabled service that lists postgres under mesh.calls: "<release>-<service>-password", with
  its login role's password under the key "<service>_app" (role "app"), and, for a service in
  "trustiot.migrate.platformLogins", its platform role's password under "<service>_platform" (role
  "platform") in the same Secret;
- the telemetry-query service, when enabled: its ClickHouse account's password under the key
  "trustiot_telemetry_query" (role "clickhouse") in the same "<release>-telemetry-query-password"
  (services/telemetry-query/README.md; the account is defined by
  services/ingest/migrations/clickhouse/0008_telemetry_query.sql);
- the OpenTelemetry collector, when enabled: "<release>-otel-collector-password", with its
  ClickHouse account's password under the key named after the account.
templates/passwords.yaml declares them without data; the migration Job generates each password and
writes it in (tools/migrate/src/passwords.rs). They are declared whether or not migrate.enabled is
true: with the Job turned off, whatever replaces it fills them (only the cluster tests do that).
Returns a YAML list of {name, key, database (postgres or clickhouse), component, role (app or
platform for postgres, clickhouse for a service's ClickHouse account)}, one per key, so a Secret
with two keys appears twice; use with fromYamlArray, and "trustiot.migrate.passwordSecretObjects"
for the Secrets themselves.
*/}}
{{- define "trustiot.migrate.passwordSecrets" -}}
{{- $secrets := list -}}
{{- $platformLogins := include "trustiot.migrate.platformLogins" . | fromYamlArray -}}
{{- range (include "trustiot.postgres.clients" . | fromYaml).services -}}
{{- $name := include "trustiot.componentName" (dict "root" $ "name" (printf "%s-password" .)) -}}
{{- $secrets = append $secrets (dict "name" $name "key" (include "trustiot.migrate.appRole" .) "database" "postgres" "component" . "role" "app") -}}
{{- if has . $platformLogins -}}
{{- $secrets = append $secrets (dict "name" $name "key" (include "trustiot.migrate.platformRole" .) "database" "postgres" "component" . "role" "platform") -}}
{{- end -}}
{{- end -}}
{{- with get .Values.services "telemetry-query" -}}
{{- if .enabled -}}
{{- $secrets = append $secrets (dict "name" (include "trustiot.componentName" (dict "root" $ "name" "telemetry-query-password")) "key" "trustiot_telemetry_query" "database" "clickhouse" "component" "telemetry-query" "role" "clickhouse") -}}
{{- end -}}
{{- end -}}
{{- with .Values.observability.collector -}}
{{- if .enabled -}}
{{- $secrets = append $secrets (dict "name" (include "trustiot.componentName" (dict "root" $ "name" "otel-collector-password")) "key" .clickhouse.user "database" "clickhouse" "component" "otel-collector") -}}
{{- end -}}
{{- end -}}
{{- toYaml $secrets -}}
{{- end -}}

{{/*
The password Secrets themselves, once each: {name, component}, in the order of
"trustiot.migrate.passwordSecrets". templates/passwords.yaml declares one per entry and the Job's
Role names each once. Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.migrate.passwordSecretObjects" -}}
{{- $objects := list -}}
{{- $seen := list -}}
{{- range include "trustiot.migrate.passwordSecrets" . | fromYamlArray -}}
{{- if not (has .name $seen) -}}
{{- $seen = append $seen .name -}}
{{- $objects = append $objects (dict "name" .name "component" .component) -}}
{{- end -}}
{{- end -}}
{{- toYaml $objects -}}
{{- end -}}

{{/*
Every Secret the chart declares empty for tools/migrate to fill, once each: {name, component}. The
migration Job's password Secrets ("trustiot.migrate.passwordSecretObjects"), then the bundled
ClickHouse's admin password Secret while the chart generates that password
("trustiot.clickhouse.adminPassword"), which the ClickHouse pod's init container fills, then the
identity service's signing keys Secret while the identity service is enabled
("trustiot.identity.keys"), which the identity pod's init container fills, then the certs
service's issuing CA keys Secret while the certs service is enabled ("trustiot.certs.ca"), which
the certs pod's init container fills, then the MQTT broker's accounts Secret while the broker is
enabled ("trustiot.mqttBroker.accountsSecret"), which the broker pod's init container fills.
templates/passwords.yaml declares one per entry. Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.filledSecretObjects" -}}
{{- $objects := include "trustiot.migrate.passwordSecretObjects" . | fromYamlArray -}}
{{- $admin := include "trustiot.clickhouse.adminPassword" . | fromYaml -}}
{{- if $admin.generated -}}
{{- $objects = append $objects (dict "name" $admin.name "component" "clickhouse") -}}
{{- end -}}
{{- if include "trustiot.identity.enabled" . -}}
{{- $keys := include "trustiot.identity.keys" . | fromYaml -}}
{{- $objects = append $objects (dict "name" $keys.secret "component" "identity") -}}
{{- end -}}
{{- if include "trustiot.certs.enabled" . -}}
{{- $ca := include "trustiot.certs.ca" . | fromYaml -}}
{{- $objects = append $objects (dict "name" $ca.secret "component" "certs") -}}
{{- end -}}
{{- if include "trustiot.mqttBroker.enabled" . -}}
{{- $objects = append $objects (dict "name" (include "trustiot.mqttBroker.accountsSecret" .) "component" "mqtt-broker") -}}
{{- end -}}
{{- toYaml $objects -}}
{{- end -}}

{{/*
Whether the migration Job reaches the Kubernetes API: to fill its password Secrets, or to read
the bundled ClickHouse's generated admin password. Its token is mounted only then.
*/}}
{{- define "trustiot.migrate.usesApi" -}}
{{- $admin := include "trustiot.clickhouse.adminPassword" . | fromYaml -}}
{{- if or (include "trustiot.migrate.passwordSecrets" . | fromYamlArray) $admin.generated -}}true{{- end -}}
{{- end -}}

{{/*
One password of a consumer, as {name, key}. Argument: dict "root" $ "component" <service name, or
otel-collector> and optionally "role" "platform" (default: the service's own login role, or the
collector's account). Returns YAML; use with fromYaml.
*/}}
{{- define "trustiot.migrate.passwordSecret" -}}
{{- $found := dict -}}
{{- $role := .role | default "app" -}}
{{- range include "trustiot.migrate.passwordSecrets" .root | fromYamlArray -}}
{{- if and (eq .component $.component) (eq (.role | default "app") $role) -}}
{{- $found = dict "name" .name "key" .key -}}
{{- end -}}
{{- end -}}
{{- if not $found.name -}}
{{- fail (printf "internal: %s has no password Secret for its %s role" .component $role) -}}
{{- end -}}
{{- toYaml $found -}}
{{- end -}}

{{/*
Whether a service's platform role logs in: it uses Postgres and is in
"trustiot.migrate.platformLogins". Argument: dict "root" $ "name" <service name>.
*/}}
{{- define "trustiot.migrate.platformLogin" -}}
{{- range include "trustiot.migrate.passwordSecrets" .root | fromYamlArray -}}
{{- if and (eq .component $.name) (eq (.role | default "") "platform") -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Where the init container finds the projected ServiceAccount token and the cluster's CA.
*/}}
{{- define "trustiot.migrate.kubeDir" -}}
/var/run/secrets/trustiot/kube
{{- end -}}

{{/*
Settings the Job cannot run without, checked when the chart renders, so a missing Secret name
fails `helm install` instead of a Job that fails later.
*/}}
{{- define "trustiot.migrate.check" -}}
{{- $v := .Values -}}
{{- range $reserved := list "migrate" "migrate-status" -}}
{{- if hasKey $v.services $reserved -}}
{{- fail (printf "services.%s: the name belongs to the migration Job" $reserved) -}}
{{- end -}}
{{- end -}}
{{- if include "trustiot.migrate.enabled" . -}}
{{- /* The Job's image carries every service's migrations, and only a change of its own spec runs
it again: a service that uses Postgres must be the same release as the Job, or it would start on
the schema of another release without waiting for anything (decision 4: services are released
together). */ -}}
{{- $migrateTag := $v.migrate.image.tag | default $.Chart.AppVersion -}}
{{- range (include "trustiot.postgres.clients" . | fromYaml).services -}}
{{- $own := (get $v.services .).image | default dict -}}
{{- $tag := $own.tag | default $v.serviceDefaults.image.tag | default $.Chart.AppVersion -}}
{{- if ne $tag $migrateTag -}}
{{- fail (printf "services.%s: image tag %s differs from migrate.image.tag %s: a service that uses Postgres must come from the same release as the migration Job, which holds its migrations and runs again only when its own spec changes. Set migrate.image.tag (and migrate.image.digest, for a pinned image) to the release of the service, or the service tag to the release of the Job" . $tag $migrateTag) -}}
{{- end -}}
{{- end -}}
{{- if eq $v.postgres.mode "external" -}}
{{- if not $v.postgres.external.migrationSecret.name -}}
{{- fail "postgres.external.migrationSecret.name is required in external mode while migrate.enabled is true: the migration Job logs in with it" -}}
{{- end -}}
{{- if not $v.postgres.external.caSecret.name -}}
{{- fail "postgres.external.caSecret.name is required while migrate.enabled is true: the migration Job verifies the server with it" -}}
{{- end -}}
{{- end -}}
{{- if eq $v.clickhouse.mode "external" -}}
{{- if not $v.clickhouse.external.migrationSecret.name -}}
{{- fail "clickhouse.external.migrationSecret.name is required in external mode while migrate.enabled is true: the migration Job logs in with it" -}}
{{- end -}}
{{- if not $v.clickhouse.external.caSecret.name -}}
{{- fail "clickhouse.external.caSecret.name is required while migrate.enabled is true: the migration Job trusts only that CA, never the public roots (for a publicly trusted server, put its root CA there)" -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Env of the Job's container: where the databases are comes from the platform settings (envFrom);
this adds the CA file, the migration accounts, the password Secrets it fills (names only, never a
value) and the waits. Credentials come from Secrets only.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.migrate.env" -}}
{{- $v := .Values -}}
{{- $env := include "trustiot.migrate.postgresEnv" . | fromYamlArray -}}
{{- if eq $v.clickhouse.mode "bundled" -}}
{{- $admin := include "trustiot.clickhouse.adminPassword" . | fromYaml -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__USERNAME" "value" $v.clickhouse.bundled.adminUser) -}}
{{- if $admin.generated -}}
{{- /* The Job reads the generated password from its Secret through the API (get only, by name,
in migrate.yaml's Role) when it is about to use ClickHouse, and again while ClickHouse refuses it:
an environment variable would keep the value of the moment the pod started, which a rotation can
replace (tools/migrate/src/clickhouse_admin.rs). The key is the account. */ -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__PASSWORD_SECRET" "value" $admin.name) -}}
{{- else -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" $admin.name "key" $admin.key))) -}}
{{- end -}}
{{- else -}}
{{- with $v.clickhouse.external.migrationSecret -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__USERNAME" "valueFrom" (dict "secretKeyRef" (dict "name" .name "key" .usernameKey))) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" .name "key" .passwordKey))) -}}
{{- end -}}
{{- end -}}
{{- include "trustiot.migrate.apiEnv" (dict "root" . "env" $env) -}}
{{- end -}}

{{/*
The Postgres part of the Job's env: the CA file and the migration role's credentials, from
Secrets only. Shared with the platform administrator bootstrap (templates/platform-admin-bootstrap.yaml),
which connects exactly as the Job does. Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.migrate.postgresEnv" -}}
{{- $v := .Values -}}
{{- $env := list -}}
{{- $postgresCa := include "trustiot.postgres.caSecret" . | fromYaml -}}
{{- if $postgresCa.name -}}
{{- $env = append $env (dict "name" "TRUSTIOT_POSTGRES_CA_FILE" "value" "/etc/trustiot/postgres-ca/ca.crt") -}}
{{- end -}}
{{- if eq $v.postgres.mode "bundled" -}}
{{- /* The database owner; CloudNativePG generates its password into <cluster>-app. */ -}}
{{- $app := printf "%s-app" (include "trustiot.postgres.clusterName" .) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__POSTGRES__USERNAME" "valueFrom" (dict "secretKeyRef" (dict "name" $app "key" "username"))) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__POSTGRES__PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" $app "key" "password"))) -}}
{{- else -}}
{{- with $v.postgres.external.migrationSecret -}}
{{- $env = append $env (dict "name" "TRUSTIOT__POSTGRES__USERNAME" "valueFrom" (dict "secretKeyRef" (dict "name" .name "key" .usernameKey))) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__POSTGRES__PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" .name "key" .passwordKey))) -}}
{{- end -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}

{{/*
The rest of the Job's env after the databases' credentials: how it reaches the Secrets it fills,
and the waits. Argument: dict "root" $ "env" <the env so far>. Returns the whole env as a YAML
list.
*/}}
{{- define "trustiot.migrate.apiEnv" -}}
{{- $env := .env -}}
{{- with .root -}}
{{- $v := .Values -}}
{{- /* The password Secrets the Job fills, as key=Secret pairs, and how it reaches them and the
bundled ClickHouse's generated admin Secret: the Kubernetes API with its own projected token
("trustiot.migrate.kubeApiVolume"), whose Role in migrate.yaml allows get and patch on exactly the
Secrets it fills, and get on the admin Secret. passwordGeneration renews every
password (a rotation); a new value also renames the Job, so the services restart with the new
passwords. */ -}}
{{- $secrets := include "trustiot.migrate.passwordSecrets" . | fromYamlArray -}}
{{- if include "trustiot.migrate.usesApi" . -}}
{{- $dir := include "trustiot.migrate.kubeDir" . -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__API_URL" "value" "https://kubernetes.default.svc") -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__NAMESPACE" "valueFrom" (dict "fieldRef" (dict "fieldPath" "metadata.namespace"))) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__TOKEN_FILE" "value" (printf "%s/token" $dir)) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__CA_FILE" "value" (printf "%s/ca.crt" $dir)) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__GENERATION" "value" (include "trustiot.migrate.passwordGeneration" .)) -}}
{{- $postgres := list -}}
{{- range $secrets -}}
{{- if eq .database "postgres" -}}
{{- $postgres = append $postgres (printf "%s=%s" .key .name) -}}
{{- else if eq .component "telemetry-query" -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY" "value" (printf "%s=%s" .key .name)) -}}
{{- else -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR" "value" (printf "%s=%s" .key .name)) -}}
{{- end -}}
{{- end -}}
{{- if $postgres -}}
{{- $env = append $env (dict "name" "TRUSTIOT__PASSWORDS__POSTGRES" "value" (join "," $postgres)) -}}
{{- end -}}
{{- end -}}
{{- $env = append $env (dict "name" "TRUSTIOT__WAIT__DATABASES" "value" $v.migrate.wait.databases) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__WAIT__LOCK" "value" $v.migrate.wait.lock) -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}

{{/*
The Job's spec without its name. "trustiot.migrate.jobName" hashes it, so the Job gets a new name
(and runs again) exactly when its image, settings, Secret references, revision, password
generation, the platform settings or the image of a service that uses Postgres change; an upgrade
that changes none of them leaves the completed Job as it is. The Job object is immutable, and a
new name also avoids patching it.
*/}}
{{- define "trustiot.migrate.jobSpec" -}}
{{- $v := .Values -}}
{{- $m := $v.migrate -}}
{{- $ctx := dict "root" . "name" "migrate" -}}
{{- $postgresCa := include "trustiot.postgres.caSecret" . | fromYaml -}}
{{- $clickhouseCa := and (eq $v.clickhouse.mode "external") $v.clickhouse.external.caSecret.name -}}
{{- $usesApi := include "trustiot.migrate.usesApi" . -}}
# One pod at a time, and a retry only after the previous pod has fully stopped; the advisory lock
# in Postgres keeps two runs apart even across Jobs.
parallelism: 1
completions: 1
podReplacementPolicy: Failed
backoffLimit: {{ $m.backoffLimit }}
activeDeadlineSeconds: {{ $m.activeDeadlineSeconds }}
template:
  metadata:
    labels:
      {{- $labels := include "trustiot.componentLabels" $ctx | fromYaml }}
      {{- /* In the mesh, so the bundled ClickHouse admits it by its ServiceAccount. */}}
      {{- $_ := set $labels "istio.io/dataplane-mode" "ambient" }}
      {{- toYaml $labels | nindent 6 }}
    annotations:
      {{- /* The platform settings say where the databases are; a change runs the Job again. */}}
      checksum/platform-config: {{ include (print .Template.BasePath "/platform-config.yaml") . | sha256sum }}
      trustiot.io/migrate-revision: {{ include "trustiot.decimal" (dict "value" $m.revision "key" "migrate.revision") | quote }}
      {{- /* The image of every service that uses Postgres: a new service image (a digest-only
      hotfix too) runs the Job again, and the service waits for that run instead of starting on
      the old Job's result. The tag rule in "trustiot.migrate.check" keeps the tags equal; a new
      digest still needs the migration image of the same release (README.md, "migration Job"). */}}
      {{- $images := list }}
      {{- range (include "trustiot.postgres.clients" . | fromYaml).services }}
      {{- $config := include "trustiot.serviceConfig" (dict "root" $ "name" . "values" (get $v.services .)) | fromYaml }}
      {{- $images = append $images (printf "%s=%s" . (include "trustiot.serviceImage" (dict "root" $ "name" . "image" $config.image))) }}
      {{- end }}
      trustiot.io/service-images: {{ join "," $images | quote }}
  spec:
    serviceAccountName: {{ include "trustiot.componentName" $ctx }}
    automountServiceAccountToken: false
    enableServiceLinks: false
    restartPolicy: Never
    securityContext:
      {{- toYaml $v.serviceDefaults.podSecurityContext | nindent 6 }}
    {{- with $v.global.imagePullSecrets }}
    imagePullSecrets:
      {{- range . }}
      - name: {{ . }}
      {{- end }}
    {{- end }}
    {{- with $m.nodeSelector }}
    nodeSelector:
      {{- toYaml . | nindent 6 }}
    {{- end }}
    {{- with $m.tolerations }}
    tolerations:
      {{- toYaml . | nindent 6 }}
    {{- end }}
    containers:
      - name: migrate
        image: {{ include "trustiot.migrate.image" . }}
        imagePullPolicy: {{ $m.image.pullPolicy }}
        args:
          - run
        envFrom:
          - configMapRef:
              name: {{ include "trustiot.platformConfigName" . }}
        env:
          {{- include "trustiot.migrate.env" . | fromYamlArray | toYaml | nindent 10 }}
        resources:
          {{- toYaml $m.resources | nindent 10 }}
        securityContext:
          {{- toYaml $v.serviceDefaults.securityContext | nindent 10 }}
        volumeMounts:
          {{- if $postgresCa.name }}
          - name: postgres-ca
            mountPath: /etc/trustiot/postgres-ca
            readOnly: true
          {{- end }}
          {{- if $clickhouseCa }}
          - name: clickhouse-ca
            mountPath: /etc/trustiot/clickhouse-ca
            readOnly: true
          {{- end }}
          {{- if $usesApi }}
          {{- /* The token that fills the password Secrets and reads the generated ClickHouse admin
          password: only this container, and only while there is one of those. */}}
          - name: migrate-kube-api
            mountPath: {{ include "trustiot.migrate.kubeDir" . }}
            readOnly: true
          {{- end }}
    volumes:
      {{- if $postgresCa.name }}
      - name: postgres-ca
        secret:
          secretName: {{ $postgresCa.name }}
          items:
            - key: {{ $postgresCa.key }}
              path: ca.crt
      {{- end }}
      {{- if $clickhouseCa }}
      - name: clickhouse-ca
        secret:
          secretName: {{ $v.clickhouse.external.caSecret.name }}
          items:
            - key: {{ $v.clickhouse.external.caSecret.key }}
              path: ca.crt
      {{- end }}
      {{- if $usesApi }}
      - {{ include "trustiot.migrate.kubeApiVolume" . | indent 8 | trim }}
      {{- end }}
{{- end -}}

{{/*
Name of the Job: "<release>-migrate-" and the first 8 hex digits of the SHA-256 of its spec,
within 63 characters (the Job controller copies it into a pod label).
*/}}
{{- define "trustiot.migrate.jobName" -}}
{{- $hash := include "trustiot.migrate.jobSpec" . | sha256sum | trunc 8 -}}
{{- include "trustiot.componentName" (dict "root" . "name" (printf "migrate-%s" $hash)) -}}
{{- end -}}

{{/*
Init container of every service pod: `migrate wait-for-job` polls the Kubernetes API until the Job
is complete and fails when the Job has failed, so the pod restarts it and keeps waiting for a fixed
release. It authenticates with a projected token of the service's ServiceAccount, which only this
container mounts (the service's own container still has none), and the Role in migrate.yaml lets
that ServiceAccount read this one Job and nothing else. Every NetworkPolicy of the chart restricts
ingress only (templates/mesh.yaml, the default deny included; a chart test checks that none has
an Egress policy type), so the connection to the API server is not blocked; ztunnel passes it
through, since the API server is outside the mesh.
Argument: dict "root" $ "config" <the service's effective settings>.
*/}}
{{- define "trustiot.migrate.initContainer" -}}
{{- $root := .root -}}
{{- $dir := include "trustiot.migrate.kubeDir" $root -}}
name: wait-for-migrations
image: {{ include "trustiot.migrate.image" $root }}
imagePullPolicy: {{ $root.Values.migrate.image.pullPolicy }}
args:
  - wait-for-job
env:
  - name: TRUSTIOT__KUBE__JOB
    value: {{ include "trustiot.migrate.jobName" $root }}
  - name: TRUSTIOT__KUBE__NAMESPACE
    valueFrom:
      fieldRef:
        fieldPath: metadata.namespace
  - name: TRUSTIOT__KUBE__API_URL
    value: https://kubernetes.default.svc
  - name: TRUSTIOT__KUBE__TOKEN_FILE
    value: {{ printf "%s/token" $dir }}
  - name: TRUSTIOT__KUBE__CA_FILE
    value: {{ printf "%s/ca.crt" $dir }}
resources:
  {{- toYaml $root.Values.migrate.resources | nindent 2 }}
securityContext:
  {{- toYaml .config.securityContext | nindent 2 }}
volumeMounts:
  - name: migrate-kube-api
    mountPath: {{ $dir }}
    readOnly: true
{{- end -}}

{{/*
The volume the init container (and the Job, to fill the password Secrets) mounts: a short-lived
token and the cluster's CA certificate. 0444: the containers run under a UID the platform may
assign (no fsGroup), so the files stay readable whatever that UID is.
*/}}
{{- define "trustiot.migrate.kubeApiVolume" -}}
name: migrate-kube-api
projected:
  defaultMode: 292
  sources:
    - serviceAccountToken:
        path: token
        expirationSeconds: 3600
    - configMap:
        name: kube-root-ca.crt
        items:
          - key: ca.crt
            path: ca.crt
{{- end -}}
