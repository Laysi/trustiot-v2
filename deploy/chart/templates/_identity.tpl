{{/*
Internal identity helpers (decision 27, Laysi/trustiot-v2#61; keys generated in the cluster since
Laysi/trustiot-v2#404, docs/architecture/secrets.md, 「內部身分簽章金鑰」).
*/}}

{{/*
The identity service's keys, as YAML {secret, secretKey, configMap, configMapKey, generation,
revokeBefore, propagationSeconds, legacy}; use with fromYaml.
- secret: the Secret templates/passwords.yaml declares empty while the identity service is enabled
  ("trustiot.filledSecretObjects"), which the identity pod's init container fills with the private
  keys (secretKey, a JSON document) and only the identity service mounts;
- configMap: the ConfigMap templates/internal-identity.yaml always declares empty, which the init
  container fills with their public keys (configMapKey, a JWK Set) and every service mounts;
- generation, revokeBefore: internalIdentity.keyGeneration and revokeBeforeGeneration as exact
  decimal digits; the render fails when the generation is below the revocation, since no key
  could sign then;
- legacy: true while internalIdentity.signingKeySecret names a key a person made before
  Laysi/trustiot-v2#404, which the init container imports.
*/}}
{{- define "trustiot.identity.keys" -}}
{{- $v := .Values.internalIdentity -}}
{{- $generation := include "trustiot.decimal" (dict "value" $v.keyGeneration "key" "internalIdentity.keyGeneration") -}}
{{- if not (regexMatch "^[0-9]{1,9}$" $generation) -}}
{{- fail (printf "internalIdentity.keyGeneration: %q must render as 1 to 9 decimal digits" $generation) -}}
{{- end -}}
{{- $revoke := include "trustiot.decimal" (dict "value" $v.revokeBeforeGeneration "key" "internalIdentity.revokeBeforeGeneration") -}}
{{- if not (regexMatch "^[0-9]{1,9}$" $revoke) -}}
{{- fail (printf "internalIdentity.revokeBeforeGeneration: %q must render as 1 to 9 decimal digits" $revoke) -}}
{{- end -}}
{{- if lt (int64 $generation) 1 -}}
{{- fail "internalIdentity.keyGeneration must be at least 1" -}}
{{- end -}}
{{- if lt (int64 $generation) (int64 $revoke) -}}
{{- fail (printf "internalIdentity.keyGeneration (%s) must not be below internalIdentity.revokeBeforeGeneration (%s): every key below the revocation is removed, so the new key needs a generation at least as high" $generation $revoke) -}}
{{- end -}}
{{- $propagation := int64 $v.keyPropagationSeconds -}}
{{- if or (lt $propagation 120) (gt $propagation 86400) -}}
{{- fail "internalIdentity.keyPropagationSeconds must be between 120 and 86400" -}}
{{- end -}}
{{- if and $v.signingKeySecret.name (not $v.publicKeys) -}}
{{- fail "internalIdentity.signingKeySecret needs internalIdentity.publicKeys: the init container imports that key under the key id it has there (README.md, 「內部身分」)" -}}
{{- end -}}
{{- if and $v.publicKeys (not $v.signingKeySecret.name) -}}
{{- fail "internalIdentity.publicKeys is only for moving over a signing key a person made before Laysi/trustiot-v2#404, together with internalIdentity.signingKeySecret; the public keys are published in the cluster now, so remove it (README.md, 「內部身分」)" -}}
{{- end -}}
{{- $keys := dict "generation" $generation "revokeBefore" $revoke "propagationSeconds" (printf "%d" $propagation) -}}
{{- $_ := set $keys "secret" (include "trustiot.componentName" (dict "root" . "name" "identity-signing-keys")) -}}
{{- $_ := set $keys "secretKey" "signing-keys.json" -}}
{{- $_ := set $keys "configMap" (include "trustiot.componentName" (dict "root" . "name" "internal-identity-keys")) -}}
{{- $_ := set $keys "configMapKey" "jwks.json" -}}
{{- $_ := set $keys "legacy" (not (empty $v.signingKeySecret.name)) -}}
{{- toYaml $keys -}}
{{- end -}}

{{/* Whether the identity service is enabled, and so whether anything generates keys. */}}
{{- define "trustiot.identity.enabled" -}}
{{- with .Values.services.identity -}}
{{- if .enabled -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/*
The public keys of a key a person made before Laysi/trustiot-v2#404 (internalIdentity.publicKeys)
as a JWK Set, which the init container finds the imported key's key id in. values.schema.json
allows only kid and x per key, so a private key (d) can never be listed; key ids must be unique.
*/}}
{{- define "trustiot.identity.legacyJwks" -}}
{{- $keys := list -}}
{{- $kids := list -}}
{{- range $i, $key := .Values.internalIdentity.publicKeys -}}
{{- if has $key.kid $kids -}}
{{- fail (printf "internalIdentity.publicKeys[%d]: key id %q is listed twice" $i $key.kid) -}}
{{- end -}}
{{- $kids = append $kids $key.kid -}}
{{- $keys = append $keys (dict "kty" "OKP" "crv" "Ed25519" "kid" $key.kid "x" $key.x) -}}
{{- end -}}
{{- dict "keys" $keys | toJson -}}
{{- end -}}

{{/*
Where every service mounts the public keys ConfigMap (the whole ConfigMap, never subPath, so the
kubelet replaces the file when the ConfigMap changes), and the platform setting
TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE names the JWK Set in it.
*/}}
{{- define "trustiot.identity.publicKeysDir" -}}
/etc/trustiot/internal-identity
{{- end -}}

{{/* Where the identity service's main container mounts its signing keys Secret. */}}
{{- define "trustiot.identity.signingKeysDir" -}}
/etc/trustiot/identity-signing-keys
{{- end -}}

{{/* Where the init container mounts the Secret of a key a person made before #404. */}}
{{- define "trustiot.identity.legacyKeyDir" -}}
/etc/trustiot/legacy-identity-signing-key
{{- end -}}

{{/*
Env entries only the identity service gets, set by the chart (the TRUSTIOT_ names are reserved,
so custom env cannot replace them):
- TRUSTIOT_INGRESS_ROUTES: the ingress gateway's cookie routes as JSON, [{host, path, match,
  service}] with the real host names, so the identity service addresses each token to the service
  the gateway routes the request to;
- TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE: the signing keys file the init container fills;
- TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION: it never signs with a key of a lower generation,
  even before the kubelet updates the file after a revocation;
- TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS: how long its keys and the published public keys may
  disagree before it logs an ERROR.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.identity.env" -}}
{{- $plan := include "trustiot.ingress.plan" . | fromYaml -}}
{{- $keys := include "trustiot.identity.keys" . | fromYaml -}}
{{- $routes := list -}}
{{- range $plan.routes | default list -}}
{{- $routes = append $routes (dict "host" .hostname "path" .path "match" .match "service" .service) -}}
{{- end -}}
{{- $env := list (dict "name" "TRUSTIOT_INGRESS_ROUTES" "value" ($routes | toJson)) -}}
{{- $env = append $env (dict "name" "TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE" "value" (printf "%s/%s" (include "trustiot.identity.signingKeysDir" .) $keys.secretKey)) -}}
{{- $env = append $env (dict "name" "TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION" "value" $keys.revokeBefore) -}}
{{- $env = append $env (dict "name" "TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS" "value" $keys.propagationSeconds) -}}
{{- toYaml $env -}}
{{- end -}}

{{/*
Init container of the identity pod: `migrate identity-signing-keys`
(tools/migrate/src/signing_keys.rs) makes sure the Secret holds a key of
internalIdentity.keyGeneration, generating one with the operating system's CSPRNG when it holds
none, holds an older generation only, or is not the object it filled; publishes every key's public
key in the ConfigMap, and only then lets a new key sign keyPropagationSeconds later; removes the
keys of a generation below revokeBeforeGeneration at once, and the old keys once their last token
has expired. It runs before every start of an identity pod, before wait-for-migrations, since it
needs no database. It authenticates with a projected token of the identity service's
ServiceAccount, which only init containers mount (the service's own container has none), and the
Role in templates/internal-identity.yaml lets that ServiceAccount get and patch this one Secret and
this one ConfigMap. While internalIdentity.signingKeySecret names a key a person made, it mounts
that Secret, which no other container does, and imports the key once.
Argument: dict "root" $ "config" <the identity service's effective settings>.
*/}}
{{- define "trustiot.identity.initContainer" -}}
{{- $root := .root -}}
{{- $keys := include "trustiot.identity.keys" $root | fromYaml -}}
{{- $dir := include "trustiot.migrate.kubeDir" $root -}}
name: signing-keys
image: {{ include "trustiot.migrate.image" $root }}
imagePullPolicy: {{ $root.Values.migrate.image.pullPolicy }}
args:
  - identity-signing-keys
env:
  - name: TRUSTIOT__KEYS__API_URL
    value: https://kubernetes.default.svc
  - name: TRUSTIOT__KEYS__NAMESPACE
    valueFrom:
      fieldRef:
        fieldPath: metadata.namespace
  - name: TRUSTIOT__KEYS__TOKEN_FILE
    value: {{ printf "%s/token" $dir }}
  - name: TRUSTIOT__KEYS__CA_FILE
    value: {{ printf "%s/ca.crt" $dir }}
  - name: TRUSTIOT__KEYS__SECRET
    value: {{ $keys.secret }}
  - name: TRUSTIOT__KEYS__CONFIG_MAP
    value: {{ $keys.configMap }}
  - name: TRUSTIOT__KEYS__GENERATION
    value: {{ $keys.generation | quote }}
  - name: TRUSTIOT__KEYS__REVOKE_BEFORE_GENERATION
    value: {{ $keys.revokeBefore | quote }}
  - name: TRUSTIOT__KEYS__PROPAGATION_SECONDS
    value: {{ $keys.propagationSeconds | quote }}
  {{- if $keys.legacy }}
  - name: TRUSTIOT__KEYS__LEGACY_KEY_FILE
    value: {{ printf "%s/signing-key.pem" (include "trustiot.identity.legacyKeyDir" $root) }}
  - name: TRUSTIOT__KEYS__LEGACY_PUBLIC_KEYS
    value: {{ include "trustiot.identity.legacyJwks" $root | quote }}
  {{- end }}
resources:
  {{- toYaml $root.Values.migrate.resources | nindent 2 }}
securityContext:
  {{- toYaml .config.securityContext | nindent 2 }}
volumeMounts:
  - name: migrate-kube-api
    mountPath: {{ $dir }}
    readOnly: true
  {{- if $keys.legacy }}
  - name: legacy-identity-signing-key
    mountPath: {{ include "trustiot.identity.legacyKeyDir" $root }}
    readOnly: true
  {{- end }}
{{- end -}}
