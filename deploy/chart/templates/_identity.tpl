{{/*
Internal identity helpers (decision 27, Laysi/trustiot-v2#61).
*/}}

{{/*
The identity service's public keys as the JWK Set every service reads from the platform setting
TRUSTIOT_INTERNAL_IDENTITY_KEYS. values.schema.json allows only kid and x per key, so a private
key (d) can never reach the ConfigMap; key ids must be unique. Empty when no key is listed.
*/}}
{{- define "trustiot.internalIdentity.jwks" -}}
{{- $keys := list -}}
{{- $kids := list -}}
{{- range $i, $key := .Values.internalIdentity.publicKeys -}}
{{- if has $key.kid $kids -}}
{{- fail (printf "internalIdentity.publicKeys[%d]: key id %q is listed twice" $i $key.kid) -}}
{{- end -}}
{{- $kids = append $kids $key.kid -}}
{{- $keys = append $keys (dict "kty" "OKP" "crv" "Ed25519" "kid" $key.kid "x" $key.x) -}}
{{- end -}}
{{- if $keys -}}
{{- dict "keys" $keys | toJson -}}
{{- end -}}
{{- end -}}

{{/*
Where the identity service's signing key Secret is mounted, and the file inside it.
*/}}
{{- define "trustiot.identity.signingKeyDir" -}}
/etc/trustiot/identity-signing-key
{{- end -}}

{{- define "trustiot.identity.signingKeyFile" -}}
signing-key.pem
{{- end -}}

{{/*
Env entries only the identity service gets, set by the chart (the TRUSTIOT_ names are reserved,
so custom env cannot replace them):
- TRUSTIOT_INGRESS_ROUTES: the ingress gateway's cookie routes as JSON, [{host, path, match,
  service}] with the real host names, so the identity service addresses each token to the service
  the gateway routes the request to;
- TRUSTIOT_IDENTITY_SIGNING_KEY_FILE: the mounted private key, when its Secret is set.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.identity.env" -}}
{{- $plan := include "trustiot.ingress.plan" . | fromYaml -}}
{{- $routes := list -}}
{{- range $plan.routes | default list -}}
{{- $routes = append $routes (dict "host" .hostname "path" .path "match" .match "service" .service) -}}
{{- end -}}
{{- $env := list (dict "name" "TRUSTIOT_INGRESS_ROUTES" "value" ($routes | toJson)) -}}
{{- if .Values.internalIdentity.signingKeySecret.name -}}
{{- $file := printf "%s/%s" (include "trustiot.identity.signingKeyDir" .) (include "trustiot.identity.signingKeyFile" .) -}}
{{- $env = append $env (dict "name" "TRUSTIOT_IDENTITY_SIGNING_KEY_FILE" "value" $file) -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}
