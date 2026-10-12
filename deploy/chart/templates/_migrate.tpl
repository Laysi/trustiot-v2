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
the same name in postgres.serviceRolesSecret. Argument: the service name.
*/}}
{{- define "trustiot.migrate.appRole" -}}
{{- printf "%s_app" (replace "-" "_" .) -}}
{{- end -}}

{{/*
Where the Job mounts postgres.serviceRolesSecret: one file per login role, named after it
(TRUSTIOT__POSTGRES__ROLE_PASSWORDS_DIR).
*/}}
{{- define "trustiot.migrate.rolePasswordsDir" -}}
/etc/trustiot/role-passwords
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
{{- if and (include "trustiot.migrate.enabled" .) (include "trustiot.postgres.clients" . | fromYaml).services (not $v.postgres.serviceRolesSecret.name) -}}
{{- fail "postgres.serviceRolesSecret.name is required while a service lists postgres under mesh.calls: the migration Job sets each service role's password from it" -}}
{{- end -}}
{{- if include "trustiot.migrate.enabled" . -}}
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
this adds the CA file, the migration accounts, the service roles' password directory, the
collector's account and the waits. Credentials come from Secrets only.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.migrate.env" -}}
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
{{- if $v.postgres.serviceRolesSecret.name -}}
{{- $env = append $env (dict "name" "TRUSTIOT__POSTGRES__ROLE_PASSWORDS_DIR" "value" (include "trustiot.migrate.rolePasswordsDir" .)) -}}
{{- /* The roles of the enabled services that use Postgres: each must have a password, or the
Job stops before any service starts with a role it cannot log in as. */ -}}
{{- $required := list -}}
{{- range (include "trustiot.postgres.clients" . | fromYaml).services -}}
{{- $required = append $required (include "trustiot.migrate.appRole" .) -}}
{{- end -}}
{{- if $required -}}
{{- $env = append $env (dict "name" "TRUSTIOT__POSTGRES__REQUIRED_PASSWORDS" "value" (join "," $required)) -}}
{{- end -}}
{{- end -}}
{{- if eq $v.clickhouse.mode "bundled" -}}
{{- with $v.clickhouse.bundled -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__USERNAME" "value" .adminUser) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" .adminSecret.name "key" .adminSecret.key))) -}}
{{- end -}}
{{- else -}}
{{- with $v.clickhouse.external.migrationSecret -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__USERNAME" "valueFrom" (dict "secretKeyRef" (dict "name" .name "key" .usernameKey))) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" .name "key" .passwordKey))) -}}
{{- end -}}
{{- end -}}
{{- with $v.observability.collector -}}
{{- if .enabled -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__COLLECTOR_USERNAME" "value" .clickhouse.user) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CLICKHOUSE__COLLECTOR_PASSWORD" "valueFrom" (dict "secretKeyRef" (dict "name" .clickhouse.passwordSecret.name "key" .clickhouse.passwordSecret.key))) -}}
{{- end -}}
{{- end -}}
{{- $env = append $env (dict "name" "TRUSTIOT__WAIT__DATABASES" "value" $v.migrate.wait.databases) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__WAIT__LOCK" "value" $v.migrate.wait.lock) -}}
{{- toYaml $env -}}
{{- end -}}

{{/*
The Job's spec without its name. "trustiot.migrate.jobName" hashes it, so the Job gets a new name
(and runs again) exactly when its image, settings, Secret references, revision or the platform
settings change; an upgrade that changes none of them leaves the completed Job as it is. The Job
object is immutable, and a new name also avoids patching it.
*/}}
{{- define "trustiot.migrate.jobSpec" -}}
{{- $v := .Values -}}
{{- $m := $v.migrate -}}
{{- $ctx := dict "root" . "name" "migrate" -}}
{{- $postgresCa := include "trustiot.postgres.caSecret" . | fromYaml -}}
{{- $clickhouseCa := and (eq $v.clickhouse.mode "external") $v.clickhouse.external.caSecret.name -}}
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
      trustiot.io/migrate-revision: {{ $m.revision | toString | quote }}
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
          {{- if $v.postgres.serviceRolesSecret.name }}
          - name: role-passwords
            mountPath: {{ include "trustiot.migrate.rolePasswordsDir" . }}
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
      {{- if $v.postgres.serviceRolesSecret.name }}
      - name: role-passwords
        secret:
          secretName: {{ $v.postgres.serviceRolesSecret.name }}
          {{- /* 0444, read-only: the container runs only the Job, under a UID the platform may
          assign (no fsGroup), so the files stay readable to it whatever that UID is. */}}
          defaultMode: 292
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

{{/* The volume the init container mounts: a short-lived token and the cluster's CA certificate. */}}
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
