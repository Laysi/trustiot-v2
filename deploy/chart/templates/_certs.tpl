{{/*
Gateway certificate authority helpers (decision 12, Laysi/trustiot-v2#78; services/certs/README.md,
「CA 階層與私鑰保管」; docs/architecture/secrets.md, 「閘道器憑證 CA」).
*/}}

{{/*
The certs service's CA objects, as YAML {secret, secretKey, requests, certificates, generation,
removeBefore, root, issuing}; use with fromYaml.
- secret: the Secret templates/passwords.yaml declares empty while the certs service is enabled
  ("trustiot.filledSecretObjects"), which the certs pod's init container fills with the issuing CA
  keys (secretKey, a JSON document) and only the certs service's main container mounts;
- requests: the ConfigMap templates/certs-ca.yaml declares empty, which the init container fills
  with each key's certificate signing request for the offline root ceremony;
- certificates: the ConfigMap templates/certs-ca.yaml renders from certificateAuthority
  (root.pem, issuing.pem: public certificates only), which the certs service mounts;
- generation, removeBefore: certificateAuthority.keyGeneration and removeKeysBeforeGeneration as
  exact decimal digits; the render fails when the generation is below the removal, since no key
  could sign then;
- root, issuing: certificateAuthority.rootCertificate (a list of none or one) and
  issuingCertificates, each certificate's DER in base64 on one line, checked by
  "trustiot.certs.checkCertificates"; templates/certs-ca.yaml prints them as PEM.
*/}}
{{- define "trustiot.certs.ca" -}}
{{- $v := .Values.certificateAuthority -}}
{{- $generation := include "trustiot.decimal" (dict "value" $v.keyGeneration "key" "certificateAuthority.keyGeneration") -}}
{{- if not (regexMatch "^[0-9]{1,9}$" $generation) -}}
{{- fail (printf "certificateAuthority.keyGeneration: %q must render as 1 to 9 decimal digits" $generation) -}}
{{- end -}}
{{- $remove := include "trustiot.decimal" (dict "value" $v.removeKeysBeforeGeneration "key" "certificateAuthority.removeKeysBeforeGeneration") -}}
{{- if not (regexMatch "^[0-9]{1,9}$" $remove) -}}
{{- fail (printf "certificateAuthority.removeKeysBeforeGeneration: %q must render as 1 to 9 decimal digits" $remove) -}}
{{- end -}}
{{- if lt (int64 $generation) 1 -}}
{{- fail "certificateAuthority.keyGeneration must be at least 1" -}}
{{- end -}}
{{- if lt (int64 $generation) (int64 $remove) -}}
{{- fail (printf "certificateAuthority.keyGeneration (%s) must not be below certificateAuthority.removeKeysBeforeGeneration (%s): every key below it is removed, so the new key needs a generation at least as high" $generation $remove) -}}
{{- end -}}
{{- $root := ternary (list $v.rootCertificate) (list) (not (empty $v.rootCertificate)) -}}
{{- include "trustiot.certs.checkCertificates" (dict "certificates" $root "key" "certificateAuthority.rootCertificate" "max" 1) -}}
{{- $issuing := $v.issuingCertificates -}}
{{- include "trustiot.certs.checkCertificates" (dict "certificates" $issuing "key" "certificateAuthority.issuingCertificates" "max" 4) -}}
{{- $ca := dict "generation" $generation "removeBefore" $remove -}}
{{- $_ := set $ca "secret" (include "trustiot.componentName" (dict "root" . "name" "certs-ca-keys")) -}}
{{- $_ := set $ca "secretKey" "issuing-ca-keys.json" -}}
{{- $_ := set $ca "requests" (include "trustiot.componentName" (dict "root" . "name" "certs-ca-requests")) -}}
{{- $_ := set $ca "certificates" (include "trustiot.componentName" (dict "root" . "name" "certs-ca-certificates")) -}}
{{- $_ := set $ca "root" $root -}}
{{- $_ := set $ca "issuing" $issuing -}}
{{- toYaml $ca -}}
{{- end -}}

{{/*
Checks the certificates of certificateAuthority: each list entry is one certificate's DER in
base64 on one line (values hold no line breaks, templates/values-check.yaml), and the template
prints only base64 characters from them. Fails unless every entry is base64 that starts like a
DER SEQUENCE whose length takes two bytes ("MII", 256 bytes to 64 KiB): a certificate always
does, while an EC private key (PKCS#8 or SEC 1) is shorter, so a key pasted into the values fails
the render instead of reaching a ConfigMap; at most "max" entries. A "max" of 1 is a single
value (rootCertificate), named without an index in the message.
Argument: dict "certificates" <list> "key" <values path> "max" <int>.
*/}}
{{- define "trustiot.certs.checkCertificates" -}}
{{- if not (kindIs "slice" .certificates) -}}
{{- fail (printf "%s must be a list" .key) -}}
{{- end -}}
{{- if gt (len .certificates) (int .max) -}}
{{- fail (printf "%s holds more than %d certificates" .key (int .max)) -}}
{{- end -}}
{{- range $i, $der := .certificates -}}
{{- if not (and (kindIs "string" $der) (regexMatch "^MII[A-Za-z0-9+/]+={0,2}$" $der) (eq (mod (len $der) 4) 0)) -}}
{{- $name := ternary $.key (printf "%s[%d]" $.key $i) (eq (int $.max) 1) -}}
{{- fail (printf "%s must be one certificate's DER in base64 on one line (services/certs/README.md, 「發簽發 CA 憑證」); a private key never goes into the values" $name) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/* Whether the certs service is enabled, and so whether anything generates CA keys. */}}
{{- define "trustiot.certs.enabled" -}}
{{- with .Values.services.certs -}}
{{- if .enabled -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/* Where the certs service's main container mounts the issuing CA keys Secret. */}}
{{- define "trustiot.certs.keysDir" -}}
/etc/trustiot/certs-ca-keys
{{- end -}}

{{/* Where the certs service's main container mounts the CA certificates ConfigMap. */}}
{{- define "trustiot.certs.certificatesDir" -}}
/etc/trustiot/certs-ca
{{- end -}}

{{/*
The port of the Service <release>-certs-broker (templates/certs-broker.yaml), which the MQTT
broker's certificate CronJob calls for the broker's server certificate: always 8082, whatever
container port broker-certs has, so the address
http://<release>-certs-broker.<namespace>.svc:8082 never changes. The Service forwards to the
container port by name; the policies match the container port.
*/}}
{{- define "trustiot.certs.brokerServicePort" -}}
8082
{{- end -}}

{{/*
Env entries only the certs service gets, set by the chart (custom env cannot repeat a name):
TRUSTIOT__CA__KEYS_FILE, TRUSTIOT__CA__ROOT_CERTIFICATE_FILE and TRUSTIOT__CA__CERTIFICATES_FILE,
the files of the mounts above, which the service reads again when they change; and for the MQTT
broker's server certificate (Laysi/trustiot-v2#557) TRUSTIOT__BROKER__LISTEN_ADDR, the
broker-certs port of services.certs.ports, so the service listens where the Service and the
policies point ("trustiot.mesh.certsPorts"), and, while the broker runs,
TRUSTIOT__BROKER__SERVICE_NAME, the broker Service's name <release>-mqtt-broker.<namespace>.svc,
which the certificate carries after mqtt-broker.trustiot.internal (deploy/chart/README.md,
"伺服器憑證").
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.certs.env" -}}
{{- $ca := include "trustiot.certs.ca" . | fromYaml -}}
{{- $certificates := include "trustiot.certs.certificatesDir" . -}}
{{- $env := list (dict "name" "TRUSTIOT__CA__KEYS_FILE" "value" (printf "%s/%s" (include "trustiot.certs.keysDir" .) $ca.secretKey)) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CA__ROOT_CERTIFICATE_FILE" "value" (printf "%s/root.pem" $certificates)) -}}
{{- $env = append $env (dict "name" "TRUSTIOT__CA__CERTIFICATES_FILE" "value" (printf "%s/issuing.pem" $certificates)) -}}
{{- $ports := include "trustiot.mesh.certsPorts" . | fromYaml -}}
{{- if $ports.broker -}}
{{- $env = append $env (dict "name" "TRUSTIOT__BROKER__LISTEN_ADDR" "value" (printf "0.0.0.0:%d" (int $ports.broker))) -}}
{{- end -}}
{{- if .Values.mqttBroker.enabled -}}
{{- $env = append $env (dict "name" "TRUSTIOT__BROKER__SERVICE_NAME" "value" (include "trustiot.mqttBroker.serviceHost" .)) -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}

{{/*
Init container of the certs pod: `migrate certs-ca-keys` (tools/migrate/src/ca_keys.rs) makes
sure the Secret holds an ECDSA P-256 issuing CA key of certificateAuthority.keyGeneration,
generating one with the operating system's CSPRNG when it holds none, holds older generations
only, or is not the object it filled; removes the keys of a generation below
removeKeysBeforeGeneration; and writes each key's certificate signing request into the requests
ConfigMap, for the offline root ceremony. It runs before every start of a certs pod, before
wait-for-migrations, since it needs no database. It authenticates with a projected token of the
certs service's ServiceAccount, which only init containers mount (the service's own container has
none), and the Role in templates/certs-ca.yaml lets that ServiceAccount get and patch this one
Secret and this one ConfigMap.
Argument: dict "root" $ "config" <the certs service's effective settings>.
*/}}
{{- define "trustiot.certs.initContainer" -}}
{{- $root := .root -}}
{{- $ca := include "trustiot.certs.ca" $root | fromYaml -}}
{{- $dir := include "trustiot.migrate.kubeDir" $root -}}
name: ca-keys
image: {{ include "trustiot.migrate.image" $root }}
imagePullPolicy: {{ $root.Values.migrate.image.pullPolicy }}
args:
  - certs-ca-keys
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
    value: {{ $ca.secret }}
  - name: TRUSTIOT__KEYS__CONFIG_MAP
    value: {{ $ca.requests }}
  - name: TRUSTIOT__KEYS__GENERATION
    value: {{ $ca.generation | quote }}
  - name: TRUSTIOT__KEYS__REMOVE_BEFORE_GENERATION
    value: {{ $ca.removeBefore | quote }}
resources:
  {{- toYaml $root.Values.migrate.resources | nindent 2 }}
securityContext:
  {{- toYaml .config.securityContext | nindent 2 }}
volumeMounts:
  - name: migrate-kube-api
    mountPath: {{ $dir }}
    readOnly: true
{{- end -}}
