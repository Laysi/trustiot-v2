{{/*
Chart name.
*/}}
{{- define "trustiot.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Release-scoped base name: the release name when it already contains the chart name,
otherwise "<release>-<chart>".
*/}}
{{- define "trustiot.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "trustiot.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Labels shared by every object of the release.
*/}}
{{- define "trustiot.labels" -}}
helm.sh/chart: {{ include "trustiot.chart" . }}
app.kubernetes.io/name: {{ include "trustiot.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: trustiot
{{- end -}}

{{/*
Object name of a component: the release prefix is shortened so the "-<component>" suffix
always fits in 63 characters and different components never collapse to the same name.
Argument: dict "root" $ "name" <component>.
*/}}
{{- define "trustiot.componentName" -}}
{{- $suffix := printf "-%s" .name -}}
{{- $prefix := include "trustiot.fullname" .root | trunc (int (sub 63 (len $suffix))) | trimSuffix "-" -}}
{{- printf "%s%s" $prefix $suffix -}}
{{- end -}}

{{/*
Immutable selector labels of a component. Argument: dict "root" $ "name" <component>.
*/}}
{{- define "trustiot.selectorLabels" -}}
app.kubernetes.io/name: {{ include "trustiot.name" .root }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .name }}
{{- end -}}

{{/*
All labels of a component object. Argument: dict "root" $ "name" <component>.
*/}}
{{- define "trustiot.componentLabels" -}}
{{ include "trustiot.labels" .root }}
app.kubernetes.io/component: {{ .name }}
{{- end -}}

{{/*
Name of the ConfigMap with the platform settings every service reads.
*/}}
{{- define "trustiot.platformConfigName" -}}
{{- include "trustiot.componentName" (dict "root" . "name" "platform") -}}
{{- end -}}

{{/*
Recursive merge where "src" wins, including false, 0 and "" (sprig's merge functions
ignore such values). Maps are merged key by key; lists and scalars are replaced.
Mutates "dst". Argument: dict "dst" <map> "src" <map>.
*/}}
{{- define "trustiot.deepMerge" -}}
{{- $dst := .dst -}}
{{- range $key, $value := .src -}}
{{- $current := get $dst $key -}}
{{- if and (kindIs "map" $value) (kindIs "map" $current) -}}
{{- include "trustiot.deepMerge" (dict "dst" $current "src" $value) -}}
{{- else -}}
{{- $_ := set $dst $key $value -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Fails when a string in the values, or a key of a map in them, holds a line break (a YAML reader
also breaks lines at NEL, LS and PS). Templates print values into lines of a manifest, so a value
that could start another line or document could add an object, a Secret with data included
(docs/architecture/secrets.md, Laysi/trustiot-v2#398). Values hold names and settings, never
multi-line text. Argument: dict "path" <dotted path so far, "" at the top> "value" <value>.
*/}}
{{- define "trustiot.values.singleLine" -}}
{{- $break := "[\\r\\n\\x{0085}\\x{2028}\\x{2029}]" -}}
{{- if kindIs "map" .value -}}
{{- range $key, $value := .value -}}
{{- if regexMatch $break $key -}}
{{- fail (printf "%s: a key holds a line break; values are single lines, so that nothing printed from them can start another line or document of a manifest" (default "values" $.path)) -}}
{{- end -}}
{{- $_ := include "trustiot.values.singleLine" (dict "path" (ternary $key (printf "%s.%s" $.path $key) (eq $.path "")) "value" $value) -}}
{{- end -}}
{{- else if kindIs "slice" .value -}}
{{- range $index, $item := .value -}}
{{- $_ := include "trustiot.values.singleLine" (dict "path" (printf "%s[%d]" $.path $index) "value" $item) -}}
{{- end -}}
{{- else if and (kindIs "string" .value) (regexMatch $break .value) -}}
{{- fail (printf "%s: holds a line break; values are single lines, so that nothing printed from them can start another line or document of a manifest" .path) -}}
{{- end -}}
{{- end -}}

{{/*
Effective settings of one service: serviceDefaults overridden by the service entry.
Each probe is replaced as a whole and env is appended to the defaults.
Argument: dict "root" $ "name" <service> "values" <service entry>.
Returns YAML; use with fromYaml.
*/}}
{{- define "trustiot.serviceConfig" -}}
{{- $defaults := deepCopy .root.Values.serviceDefaults -}}
{{- if hasKey $defaults "credentials" -}}
{{- fail "serviceDefaults.credentials is not allowed: list credentials under services.<name>, so only that service gets them" -}}
{{- end -}}
{{- $entry := deepCopy .values -}}
{{- $config := omit $defaults "probes" "env" -}}
{{- $_ := include "trustiot.deepMerge" (dict "dst" $config "src" (omit $entry "enabled" "probes" "env")) -}}
{{- $probes := $defaults.probes -}}
{{- range $kind, $probe := ($entry.probes | default dict) -}}
{{- $_ := set $probes $kind $probe -}}
{{- end -}}
{{- $_ := set $config "probes" $probes -}}
{{- $_ := set $config "env" (concat ($defaults.env | default list) ($entry.env | default list)) -}}
{{- toYaml $config -}}
{{- end -}}

{{/*
Container image reference of a service, pinned by digest when one is set.
Argument: dict "root" $ "name" <service> "image" <effective image settings>.
*/}}
{{- define "trustiot.serviceImage" -}}
{{- $repository := .image.repository | default (printf "%s/%s" .image.registry .name) -}}
{{- $tag := .image.tag | default .root.Chart.AppVersion -}}
{{- if .image.digest -}}
{{- printf "%s:%s@%s" $repository $tag .image.digest -}}
{{- else -}}
{{- printf "%s:%s" $repository $tag -}}
{{- end -}}
{{- end -}}

{{/*
Dependency credentials that services request under services.<name>.credentials, read from the
Secrets named in values. One row per variable:
  credential  the name used in services.<name>.credentials
  env         the variable the service reads
  secret/key  the Secret reference; an empty name means not configured
  available   false when this deployment does not use the dependency (Qdrant in bundled mode)
  owners      the only services allowed to request it, with the reason; empty means any service
  optional    true when a pod must start without the Secret: smtp only, because the identity
              service serves the ext_authz check every request with a cookie needs, so a missing
              relay account must not keep it from starting (services/identity/README.md,
              "寄信（SMTP）"). The platform setting TRUSTIOT_SMTP_AUTH_REQUIRED tells the
              service that the account is expected, so it reports a missing one instead of
              sending without it.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.credentialCatalog" -}}
{{- $v := .Values -}}
{{- $qdrant := $v.qdrant.external.credentialsSecret -}}
{{- $qdrantExternal := eq $v.qdrant.mode "external" -}}
{{- $qdrantOwners := dict "services" (list "knowledge") "reason" "the knowledge service adds the tenant conditions to every Qdrant query (docs/architecture/rag.md)" -}}
{{- $s3 := $v.objectStorage.credentialsSecret -}}
{{- $smtp := $v.smtp.credentialsSecret -}}
{{- $ai := $v.ai -}}
{{- $aiOwners := dict "services" (list "ai-gateway") "reason" "only the AI gateway holds the provider keys (decision 28)" -}}
{{- $anyService := dict "services" (list) "reason" "" -}}
{{- $rows := list -}}
{{- $rows = append $rows (dict "credential" "qdrant-read" "env" "TRUSTIOT_QDRANT_READ_API_KEY" "secret" $qdrant.name "key" $qdrant.readApiKeyKey "available" $qdrantExternal "owners" $qdrantOwners) -}}
{{- $rows = append $rows (dict "credential" "qdrant-write" "env" "TRUSTIOT_QDRANT_WRITE_API_KEY" "secret" $qdrant.name "key" $qdrant.writeApiKeyKey "available" $qdrantExternal "owners" $qdrantOwners) -}}
{{- $rows = append $rows (dict "credential" "object-storage" "env" "TRUSTIOT_S3_ACCESS_KEY_ID" "secret" $s3.name "key" $s3.accessKeyIdKey "available" true "owners" $anyService) -}}
{{- $rows = append $rows (dict "credential" "object-storage" "env" "TRUSTIOT_S3_SECRET_ACCESS_KEY" "secret" $s3.name "key" $s3.secretAccessKeyKey "available" true "owners" $anyService) -}}
{{- $rows = append $rows (dict "credential" "smtp" "env" "TRUSTIOT_SMTP_USERNAME" "secret" $smtp.name "key" $smtp.usernameKey "available" true "owners" $anyService "optional" true) -}}
{{- $rows = append $rows (dict "credential" "smtp" "env" "TRUSTIOT_SMTP_PASSWORD" "secret" $smtp.name "key" $smtp.passwordKey "available" true "owners" $anyService "optional" true) -}}
{{- /* One key per provider of the AI gateway ("trustiot.aiGateway.providers"), each in the variable
the gateway's key_env names for it. */ -}}
{{- range $name, $provider := include "trustiot.aiGateway.providers" . | fromYaml -}}
{{- $secret := (get ($ai.providers | default dict) $name | default dict).apiKeySecret | default dict -}}
{{- $rows = append $rows (dict "credential" "ai" "env" $provider.keyEnv "secret" ($secret.name | default "") "key" ($secret.key | default "api-key") "available" true "owners" $aiOwners) -}}
{{- end -}}
{{- toYaml $rows -}}
{{- end -}}

{{/*
Env entries for the credentials one service requested (services.<name>.credentials), so each
dependency Secret reaches only the services that list it. Fails on an unknown credential or one
the service may not hold; skips credentials whose Secret name is empty or whose dependency this
deployment does not use.
Argument: dict "root" $ "name" <service> "credentials" <list>.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.credentialEnv" -}}
{{- $name := .name -}}
{{- $catalog := include "trustiot.credentialCatalog" .root | fromYamlArray -}}
{{- $env := list -}}
{{- range $credential := .credentials -}}
{{- $rows := list -}}
{{- range $catalog -}}
{{- if eq .credential $credential -}}
{{- $rows = append $rows . -}}
{{- end -}}
{{- end -}}
{{- if not $rows -}}
{{- fail (printf "services.%s: unknown credential %q" $name $credential) -}}
{{- end -}}
{{- range $rows -}}
{{- if and .owners.services (not (has $name .owners.services)) -}}
{{- fail (printf "services.%s: only %s may receive the %s credential: %s" $name (join ", " .owners.services) $credential .owners.reason) -}}
{{- end -}}
{{- if and .available .secret -}}
{{- $ref := dict "name" .secret "key" .key -}}
{{- if .optional -}}
{{- $_ := set $ref "optional" true -}}
{{- end -}}
{{- $env = append $env (dict "name" .env "valueFrom" (dict "secretKeyRef" $ref)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}

{{/*
Every configured dependency credential as "<secret>/<key>", including the Sentry DSN and the
internal identity signing key. Custom env must not read these, so they reach services only
through the chart.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.dependencySecretRefs" -}}
{{- $refs := list -}}
{{- range include "trustiot.credentialCatalog" . | fromYamlArray -}}
{{- if .secret -}}
{{- $refs = append $refs (printf "%s/%s" .secret .key) -}}
{{- end -}}
{{- end -}}
{{- with .Values.observability.sentry.dsnSecret -}}
{{- if .name -}}
{{- $refs = append $refs (printf "%s/%s" .name .key) -}}
{{- end -}}
{{- end -}}
{{- /* The bundled ClickHouse's bootstrap admin password in an existing Secret (adminSecret):
only ClickHouse and the migration Job may use it. The generated one is in
"trustiot.dependencySecretNames". */ -}}
{{- if eq .Values.clickhouse.mode "bundled" -}}
{{- with .Values.clickhouse.bundled.adminSecret -}}
{{- if .name -}}
{{- $refs = append $refs (printf "%s/%s" .name .key) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- /* A signing key a person made before Laysi/trustiot-v2#404: mounted into the identity pod's
init container only. */ -}}
{{- with .Values.internalIdentity.signingKeySecret -}}
{{- if .name -}}
{{- $refs = append $refs (printf "%s/%s" .name .key) -}}
{{- end -}}
{{- end -}}
{{- /* The SIEM's auth header: only the collector. (Its ClickHouse password is in a Secret the
migration Job fills, which "trustiot.dependencySecretNames" protects in every key.) */ -}}
{{- with .Values.observability.collector -}}
{{- if and .enabled .forward.enabled .forward.authHeader.name .forward.authHeader.valueSecret.name -}}
{{- $refs = append $refs (printf "%s/%s" .forward.authHeader.valueSecret.name .forward.authHeader.valueSecret.key) -}}
{{- end -}}
{{- end -}}
{{- toYaml (uniq $refs) -}}
{{- end -}}

{{/*
Secrets whose keys reach services only through the chart, so no custom env may read them in any
key and no Secret field of the values may name them ("trustiot.values.checkSecretRefs"): the
chart's own ("trustiot.chartSecretNames") and the migration accounts of an external Postgres and
ClickHouse (only the migration Job uses those).
Returns a YAML list of names; use with fromYamlArray.
*/}}
{{- define "trustiot.dependencySecretNames" -}}
{{- $names := include "trustiot.chartSecretNames" . | fromYamlArray -}}
{{- range include "trustiot.migrationSecretNames" . | fromYamlArray -}}
{{- $names = append $names . -}}
{{- end -}}
{{- toYaml (uniq $names) -}}
{{- end -}}

{{/*
The Secrets the chart itself creates, declares or has created, each of which the chart hands only
to its own consumers: CloudNativePG's Secrets of the bundled Postgres (the CA's private key, the
server key, the database owner's password), the password Secrets the migration Job fills (each
service's database password and the collector's ClickHouse password; the chart hands each consumer
only its own), the bundled ClickHouse's generated admin password Secret, the internal identity
signing keys Secret (only the identity service mounts it), the certs service's issuing CA keys
(only the certs pod), the MQTT broker's server certificate Secret (only the broker pod), and its
accounts Secret (the broker pod, and each account holder's pod mounts only its own key through
"trustiot.mqttBroker.serviceCredentials").
Returns a YAML list of names; use with fromYamlArray.
*/}}
{{- define "trustiot.chartSecretNames" -}}
{{- $names := include "trustiot.postgres.generatedSecrets" . | fromYamlArray -}}
{{- range include "trustiot.migrate.passwordSecrets" . | fromYamlArray -}}
{{- $names = append $names .name -}}
{{- end -}}
{{- /* The bundled ClickHouse's generated admin password: only its init container and the
migration Job (through the API) may use it. */ -}}
{{- $admin := include "trustiot.clickhouse.adminPassword" . | fromYaml -}}
{{- if $admin.generated -}}
{{- $names = append $names $admin.name -}}
{{- end -}}
{{- /* The identity service's signing keys, whether or not the service is enabled. */ -}}
{{- $names = append $names (include "trustiot.identity.keys" . | fromYaml).secret -}}
{{- /* The certs service's issuing CA keys, whether or not the service is enabled. */ -}}
{{- $names = append $names (include "trustiot.certs.ca" . | fromYaml).secret -}}
{{- /* The MQTT broker's service passwords and server key, whether or not the broker is enabled. */ -}}
{{- $names = append $names (include "trustiot.mqttBroker.accountsSecret" .) -}}
{{- $names = append $names (include "trustiot.mqttBroker.tlsSecret" .) -}}
{{- toYaml (uniq $names) -}}
{{- end -}}

{{/*
The migration accounts of an external Postgres and ClickHouse, while that database is external.
Returns a YAML list of names; use with fromYamlArray.
*/}}
{{- define "trustiot.migrationSecretNames" -}}
{{- $names := list -}}
{{- if eq .Values.postgres.mode "external" -}}
{{- with .Values.postgres.external.migrationSecret.name -}}
{{- $names = append $names . -}}
{{- end -}}
{{- end -}}
{{- if eq .Values.clickhouse.mode "external" -}}
{{- with .Values.clickhouse.external.migrationSecret.name -}}
{{- $names = append $names . -}}
{{- end -}}
{{- end -}}
{{- toYaml $names -}}
{{- end -}}

{{/*
Fails when a Secret field of the values names a protected Secret ("trustiot.dependencySecretNames").
Each field hands its Secret, or one key of it, to a pod or a gateway that the protected Secret's
own rule does not cover: a protected name there would, for example, mount the dynsec
administrator's password (key certs of the broker's accounts Secret) into the collector as a CA, or
send it to an AI provider as an API key. The external migration Secrets are protected themselves,
so their own fields are checked against the chart's Secrets only. A field is checked whenever it
holds a name, whether or not its component is on. global.imagePullSecrets is not among them: only
the kubelet reads those, and no container sees them.
*/}}
{{- define "trustiot.values.checkSecretRefs" -}}
{{- $v := .Values -}}
{{- $fields := list -}}
{{- $fields = append $fields (dict "field" "ingress.direct.tlsSecret" "name" $v.ingress.direct.tlsSecret) -}}
{{- $fields = append $fields (dict "field" "internalIdentity.signingKeySecret.name" "name" $v.internalIdentity.signingKeySecret.name) -}}
{{- $fields = append $fields (dict "field" "postgres.external.caSecret.name" "name" $v.postgres.external.caSecret.name) -}}
{{- $fields = append $fields (dict "field" "clickhouse.bundled.adminSecret.name" "name" $v.clickhouse.bundled.adminSecret.name) -}}
{{- $fields = append $fields (dict "field" "clickhouse.external.caSecret.name" "name" $v.clickhouse.external.caSecret.name) -}}
{{- $fields = append $fields (dict "field" "qdrant.external.credentialsSecret.name" "name" $v.qdrant.external.credentialsSecret.name) -}}
{{- $fields = append $fields (dict "field" "objectStorage.credentialsSecret.name" "name" $v.objectStorage.credentialsSecret.name) -}}
{{- $fields = append $fields (dict "field" "smtp.credentialsSecret.name" "name" $v.smtp.credentialsSecret.name) -}}
{{- range $provider, $settings := $v.ai.providers | default dict -}}
{{- $fields = append $fields (dict "field" (printf "ai.providers.%s.apiKeySecret.name" $provider) "name" ((($settings | default dict).apiKeySecret | default dict).name | default "")) -}}
{{- end -}}
{{- $fields = append $fields (dict "field" "observability.sentry.dsnSecret.name" "name" $v.observability.sentry.dsnSecret.name) -}}
{{- with $v.observability.collector.forward -}}
{{- $fields = append $fields (dict "field" "observability.collector.forward.authHeader.valueSecret.name" "name" .authHeader.valueSecret.name) -}}
{{- $fields = append $fields (dict "field" "observability.collector.forward.caSecret.name" "name" .caSecret.name) -}}
{{- end -}}
{{- $protected := include "trustiot.dependencySecretNames" . | fromYamlArray -}}
{{- range $fields -}}
{{- if and .name (has .name $protected) -}}
{{- fail (printf "%s: Secret %s is one the chart hands only to its own consumers (trustiot.dependencySecretNames in templates/_helpers.tpl); name a Secret of its own here" .field .name) -}}
{{- end -}}
{{- end -}}
{{- $chartOwned := include "trustiot.chartSecretNames" . | fromYamlArray -}}
{{- range $database := list "postgres" "clickhouse" -}}
{{- $name := (get $v $database).external.migrationSecret.name -}}
{{- if and $name (has $name $chartOwned) -}}
{{- fail (printf "%s.external.migrationSecret.name: Secret %s is one the chart hands only to its own consumers (trustiot.chartSecretNames in templates/_helpers.tpl); name a Secret of its own here" $database $name) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
CA certificates of the databases that one service connects to, mounted read-only (they are public
certificates, kept in Secrets): an external ClickHouse's CA into every service, and the Postgres
server's CA into the services that use Postgres ("trustiot.postgres.clients"), from
postgres.external.caSecret or, bundled, the certificate in CloudNativePG's <cluster>-ca.
Argument: dict "root" $ "name" <service>.
Returns a YAML list of {volume, secret, key, dir}; use with fromYamlArray.
*/}}
{{- define "trustiot.caMounts" -}}
{{- $root := .root -}}
{{- $mounts := list -}}
{{- $postgresCa := include "trustiot.postgres.caSecret" $root | fromYaml -}}
{{- if and $postgresCa.name (has .name (include "trustiot.postgres.clients" $root | fromYaml).services) -}}
{{- $mounts = append $mounts (dict "volume" "postgres-ca" "secret" $postgresCa.name "key" $postgresCa.key "dir" "/etc/trustiot/postgres-ca") -}}
{{- end -}}
{{- with $root.Values.clickhouse -}}
{{- if and (eq .mode "external") .external.caSecret.name -}}
{{- $mounts = append $mounts (dict "volume" "clickhouse-ca" "secret" .external.caSecret.name "key" .external.caSecret.key "dir" "/etc/trustiot/clickhouse-ca") -}}
{{- end -}}
{{- end -}}
{{- toYaml $mounts -}}
{{- end -}}

{{/*
Trusted source of the client IP, derived from the ingress exposure so the two can never
disagree (decisions 6 and 27): behind Cloudflare only CF-Connecting-IP is trustworthy; with
direct exposure only the right-most X-Forwarded-For entry, which the ingress gateway appends.
*/}}
{{- define "trustiot.clientIpSource" -}}
{{- if eq .Values.ingress.exposure "tunnel" -}}
cf-connecting-ip
{{- else -}}
x-forwarded-for-rightmost
{{- end -}}
{{- end -}}
