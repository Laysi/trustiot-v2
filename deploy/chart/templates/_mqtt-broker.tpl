{{/*
MQTT broker helpers (Mosquitto 2.1, decision 6, Laysi/trustiot-v2#84; README.md, "MQTT broker";
the objects are in templates/mqtt-broker.yaml).
*/}}

{{/* Whether this release runs the broker. */}}
{{- define "trustiot.mqttBroker.enabled" -}}
{{- if .Values.mqttBroker.enabled -}}true{{- end -}}
{{- end -}}

{{/*
Who may connect to the broker, and on which port. The mesh admits (templates/mesh.yaml):
- on the gateway port (8883), mqtt-bridge, which carries the gateways' connections; the broker
  itself checks each gateway's client certificate (TLS 1.3, our CA, the CRL);
- on the service port (1883), the cloud services that hold a broker account: ingest, gateway-ops,
  and certs, dynamic security's administrator. Each logs in with its account and password
  ("trustiot.mqttBroker.accounts").
These are the only services mesh.calls may list as the broker's callers; any other fails the
render ("trustiot.mesh.plan"). tools/migrate/src/mqtt_accounts.rs gives each account holder its
role. Returns YAML {gateway: {port, callers}, service: {port, callers}}; use with fromYaml.
*/}}
{{- define "trustiot.mqtt-broker.callers" -}}
gateway:
  port: 8883
  callers:
    - mqtt-bridge
service:
  port: 1883
  callers:
    - certs
    - gateway-ops
    - ingest
{{- end -}}

{{/*
The services that get a broker account: the enabled account holders ("trustiot.mqtt-broker.callers",
service) that list mqtt-broker in mesh.calls, sorted. The accounts init container creates exactly
these (TRUSTIOT__ACCOUNTS__SERVICES) and removes the account of a service that left the list.
Returns YAML {services: [...]}; use with fromYaml.
*/}}
{{- define "trustiot.mqttBroker.accounts" -}}
{{- $holders := (include "trustiot.mqtt-broker.callers" . | fromYaml).service.callers -}}
{{- $services := list -}}
{{- range $caller, $callees := .Values.mesh.calls | default dict -}}
{{- $entry := get $.Values.services $caller -}}
{{- if and $entry $entry.enabled (has $caller $holders) (kindIs "slice" $callees) (has "mqtt-broker" $callees) -}}
{{- $services = append $services $caller -}}
{{- end -}}
{{- end -}}
{{- dict "services" ($services | sortAlpha) | toYaml -}}
{{- end -}}

{{/*
The Secret the chart declares empty while the broker is enabled ("trustiot.filledSecretObjects"),
which the broker pod's accounts init container fills with one password per account holder, under
the service's name. Only that init container may read or write it (the Role in
templates/mqtt-broker.yaml); each service's pod mounts its own key and no other
("trustiot.mqttBroker.serviceCredentials"; README.md, "MQTT broker", "服務的帳號").
*/}}
{{- define "trustiot.mqttBroker.accountsSecret" -}}
{{- include "trustiot.componentName" (dict "root" . "name" "mqtt-broker-accounts") -}}
{{- end -}}

{{/*
The account holders ("trustiot.mqtt-broker.callers", service) whose program reads the broker
settings, so that the chart hands them their account ("trustiot.mqttBroker.serviceCredentials"):
certs (Laysi/trustiot-v2#633, Laysi/trustiot-v2#634) and ingest (Laysi/trustiot-v2#641,
Laysi/trustiot-v2#642). A service's settings deny unknown fields, so a holder joins this list in
the change that makes its program read them (gateway-ops: Laysi/trustiot-v2#93); until then the
broker keeps its account but the service gets neither the settings nor the password. Fails the render when an entry is not
an account holder. Returns YAML {services: [...]}; use with fromYaml.
*/}}
{{- define "trustiot.mqttBroker.credentialServices" -}}
{{- $services := list "certs" "ingest" -}}
{{- $holders := (include "trustiot.mqtt-broker.callers" . | fromYaml).service.callers -}}
{{- range $services -}}
{{- if not (has . $holders) -}}
{{- fail (printf "trustiot.mqttBroker.credentialServices: %s holds no broker account (trustiot.mqtt-broker.callers)" .) -}}
{{- end -}}
{{- end -}}
{{- dict "services" $services | toYaml -}}
{{- end -}}

{{/* Where a service's pod mounts its broker account's password ("trustiot.mqttBroker.serviceCredentials"). */}}
{{- define "trustiot.mqttBroker.accountDir" -}}
/etc/trustiot/mqtt-broker-account
{{- end -}}

{{/*
A service's broker account (Laysi/trustiot-v2#634; README.md, "MQTT broker", "服務的帳號"), for
the service template ("trustiot.service.env", "trustiot.service.deployment"). While the broker is
enabled, the service is in "trustiot.mqttBroker.credentialServices" and it holds an account
("trustiot.mqttBroker.accounts": enabled, an account holder, mqtt-broker listed in mesh.calls),
returns
- env: the service's broker settings, which crates/mqtt-client reads as mqtt.* under the service's
  prefix: <prefix>__MQTT__HOST, the broker Service's DNS name; <prefix>__MQTT__PORT, the service
  port; <prefix>__MQTT__USERNAME, service:<service>; <prefix>__MQTT__PASSWORD_FILE, the file of the
  mount below. Custom env cannot set any <prefix>__MQTT__ name of these services, with the broker
  or without it ("trustiot.service.env"): the account is the in-cluster broker's only;
- volume: the accounts Secret with the service's own key and no other (items), 0440 (root and the
  pod's fsGroup, as for the issuing CA keys; the service needs an fsGroup to read it). Not
  optional: until the broker's accounts init container has written the key (a first install, or
  the upgrade that adds the account), the kubelet keeps the pod waiting for the volume instead of
  starting a service that would exit for want of the file; the key stays in the Secret after that,
  so a broker that is down later keeps no service pod from starting;
- volumeMount: that volume, read-only, as a directory and never through subPath, which the kubelet
  never updates: after a rotation (mqttBroker.passwordGeneration) the kubelet replaces the file,
  and the service reads it again for its next connection, so the pod template carries no
  generation and the service does not restart.
and empty lists otherwise. Argument: dict "root" $ "name" <service>.
Returns YAML {env, volumes, volumeMounts}; use with fromYaml.
*/}}
{{- define "trustiot.mqttBroker.serviceCredentials" -}}
{{- $root := .root -}}
{{- $env := list -}}
{{- $volumes := list -}}
{{- $mounts := list -}}
{{- $wired := has .name (include "trustiot.mqttBroker.credentialServices" $root | fromYaml).services -}}
{{- $holder := has .name (include "trustiot.mqttBroker.accounts" $root | fromYaml).services -}}
{{- if and (include "trustiot.mqttBroker.enabled" $root) $wired $holder -}}
{{- $prefix := include "trustiot.service.envPrefix" .name -}}
{{- $dir := include "trustiot.mqttBroker.accountDir" $root -}}
{{- $port := (include "trustiot.mqtt-broker.callers" $root | fromYaml).service.port -}}
{{- $env = append $env (dict "name" (printf "%s__MQTT__HOST" $prefix) "value" (include "trustiot.mqttBroker.serviceHost" $root)) -}}
{{- $env = append $env (dict "name" (printf "%s__MQTT__PORT" $prefix) "value" (toString (int $port))) -}}
{{- $env = append $env (dict "name" (printf "%s__MQTT__USERNAME" $prefix) "value" (printf "service:%s" .name)) -}}
{{- $env = append $env (dict "name" (printf "%s__MQTT__PASSWORD_FILE" $prefix) "value" (printf "%s/%s" $dir .name)) -}}
{{- $secret := dict "secretName" (include "trustiot.mqttBroker.accountsSecret" $root) "defaultMode" 288 "items" (list (dict "key" .name "path" .name)) -}}
{{- $volumes = append $volumes (dict "name" "mqtt-broker-account" "secret" $secret) -}}
{{- $mounts = append $mounts (dict "name" "mqtt-broker-account" "mountPath" $dir "readOnly" true) -}}
{{- end -}}
{{- dict "env" $env "volumes" $volumes "volumeMounts" $mounts | toYaml -}}
{{- end -}}

{{/*
The broker's server certificate (Laysi/trustiot-v2#535; README.md, "MQTT broker", "伺服器憑證"):
a Secret the chart declares empty while the broker is enabled ("trustiot.filledSecretObjects"),
which the CronJob of templates/mqtt-broker-certificate.yaml fills with tls.crt (the certificate
and the issuing CA's) and tls.key, both PEM. Only the broker pod mounts it, whole, so a renewed
certificate reaches the pod without a restart; only the CronJob's ServiceAccount may read or
write it.
*/}}
{{- define "trustiot.mqttBroker.tlsSecret" -}}
{{- include "trustiot.componentName" (dict "root" . "name" "mqtt-broker-tls") -}}
{{- end -}}

{{/*
The broker Service's DNS name, <release>-mqtt-broker.<namespace>.svc: the certs service puts it
in the server certificate after mqtt-broker.trustiot.internal (its TRUSTIOT__BROKER__SERVICE_NAME,
"trustiot.certs.env"), and the certificate CronJob accepts a certificate only with these names.
*/}}
{{- define "trustiot.mqttBroker.serviceHost" -}}
{{- printf "%s.%s.svc" (include "trustiot.componentName" (dict "root" . "name" "mqtt-broker")) .Release.Namespace -}}
{{- end -}}

{{/*
mqttBroker.certificateGeneration as the certificate CronJob reads it: 1 to 9 decimal digits, at
least 1 (tools/migrate compares generations as numbers). Fails the render otherwise.
*/}}
{{- define "trustiot.mqttBroker.certificateGeneration" -}}
{{- $text := include "trustiot.decimal" (dict "value" .Values.mqttBroker.certificateGeneration "key" "mqttBroker.certificateGeneration") -}}
{{- if not (regexMatch "^[1-9][0-9]{0,8}$" $text) -}}
{{- fail (printf "mqttBroker.certificateGeneration: %q must render as 1 to 9 decimal digits, at least 1" $text) -}}
{{- end -}}
{{- $text -}}
{{- end -}}

{{/*
mqttBroker.certificateSchedule as the certificate CronJob's schedule, in Etc/UTC: @hourly, or five
cron fields whose minute is a comma-separated list of the wildcard, the wildcard with a step of 1
to 59, or minutes from 0 to 59, and whose other four fields are the wildcard, so it runs at least
every hour and the API server accepts it. No ranges, whose order a pattern cannot check.
values.schema.json has the same pattern; this check also holds under --skip-schema-validation.
Fails the render otherwise.
*/}}
{{- define "trustiot.mqttBroker.certificateSchedule" -}}
{{- $schedule := toString .Values.mqttBroker.certificateSchedule -}}
{{- $item := `(\*(/([1-9]|[1-5][0-9]))?|[0-5]?[0-9])` -}}
{{- if not (regexMatch (printf `^(@hourly|%s(,%s)* \* \* \* \*)$` $item $item) $schedule) -}}
{{- fail (printf "mqttBroker.certificateSchedule: %q must be @hourly, or five cron fields whose minute is a comma-separated list of *, */<step> (1 to 59) or minutes from 0 to 59 and whose other four fields are *" $schedule) -}}
{{- end -}}
{{- $schedule -}}
{{- end -}}

{{/* Where the pod mounts the server certificate Secret. */}}
{{- define "trustiot.mqttBroker.tlsDir" -}}
/etc/trustiot/mqtt-broker-tls
{{- end -}}

{{/*
mqttBroker.passwordGeneration as the init container reads it: 1 to 9 decimal digits
(tools/migrate compares generations as numbers). Fails the render otherwise.
*/}}
{{- define "trustiot.mqttBroker.passwordGeneration" -}}
{{- $text := include "trustiot.decimal" (dict "value" .Values.mqttBroker.passwordGeneration "key" "mqttBroker.passwordGeneration") -}}
{{- if not (regexMatch "^[0-9]{1,9}$" $text) -}}
{{- fail (printf "mqttBroker.passwordGeneration: %q must render as 1 to 9 decimal digits" $text) -}}
{{- end -}}
{{- $text -}}
{{- end -}}

{{/*
The broker's memory_limit in bytes: three quarters of mqttBroker.resources.limits.memory, so the
broker refuses new messages before the kernel kills it. Fails unless the limit is a whole number
of Mi or Gi.
*/}}
{{- define "trustiot.mqttBroker.memoryLimit" -}}
{{- $limit := toString (.Values.mqttBroker.resources.limits.memory | default "") -}}
{{- if not (regexMatch "^[1-9][0-9]{0,5}(Mi|Gi)$" $limit) -}}
{{- fail (printf "mqttBroker.resources.limits.memory: %q must be a whole number of Mi or Gi; the broker's memory_limit is three quarters of it" $limit) -}}
{{- end -}}
{{- $number := atoi (regexReplaceAll "(Mi|Gi)$" $limit "") -}}
{{- $unit := ternary 1073741824 1048576 (hasSuffix "Gi" $limit) -}}
{{- div (mul (mul $number $unit) 3) 4 -}}
{{- end -}}

{{/*
The broker's configuration, mounted as /mosquitto/config/mosquitto.conf. The text up to
memory_limit is files/mosquitto/mosquitto.conf word for word, which
tools/migrate/tests/mqtt_broker.rs runs in the pinned image; the chart tests fail when the two
differ. Templates never read chart files (scripts/ci/check-secret-handling.ts), hence the copy.
*/}}
{{- define "trustiot.mqttBroker.config" -}}
# Mosquitto 2.1 configuration of the chart's MQTT broker (Laysi/trustiot-v2#84; README.md,
# "MQTT broker"). templates/_mqtt-broker.tpl carries the same text, because chart templates never
# read chart files (scripts/ci/check-secret-handling.ts), and appends memory_limit from
# mqttBroker.resources.limits.memory; the chart test fails when the two differ.
# tools/migrate/tests/mqtt_broker.rs runs this file in the pinned image. Every path below is a
# mount of the broker's pod (templates/mqtt-broker.yaml):
#   /mosquitto/data                  the persistent volume: dynamic security, sessions and
#                                    messages, and the CA and CRL files
#   /mosquitto/data/trust/current    the CA and CRL files, written by the trust sidecar
#                                    (`migrate mqtt-broker-trust`)
#   /mosquitto/listeners             the gateway listener (gateway-listener.conf), written by the
#                                    trust init container or sidecar once the server certificate
#                                    is usable; empty until then
#   /etc/trustiot/mqtt-broker-tls    the Secret with the server certificate (tls.crt, tls.key)
#   /run/mosquitto                   the probes' Unix socket

# Logging: stdout only (the root filesystem is read-only). Connection messages stay on: they are
# the record of which gateway connected when.
log_dest stdout
log_type error
log_type warning
log_type notice
log_type information
log_timestamp true
log_timestamp_format %Y-%m-%dT%H:%M:%SZ
connection_messages true

# No anonymous clients, except on the probes' socket (its listener below).
allow_anonymous false

# Dynamic security: every client and role. Its file lives on the persistent volume; the init
# container `migrate mqtt-broker-accounts` writes it before every start, so Mosquitto never
# creates its own default file (which would hold an admin account and a client allowed on `#`).
plugin /usr/lib/mosquitto_dynamic_security.so
plugin_opt_config_file /mosquitto/data/dynamic-security.json

# Persistence: sessions, queued and retained messages in SQLite. A flush period of 0 commits the
# changes of each turn of the event loop at the start of the next turn, before the broker waits
# for more traffic: a SIGKILL loses at most the turn it interrupts, not the default 5 seconds.
plugin /usr/lib/mosquitto_persist_sqlite.so
plugin_opt_db_file /mosquitto/data/mosquitto.sqlite3
plugin_opt_flush_period 0
plugin_opt_sync normal

# Limits (README.md, "MQTT broker"). The packet limit is crates/protocol's MIN_MAX_PACKET_BYTES:
# its largest payload (MAX_LARGE_BYTES, 1 MiB) plus 8 KiB for the topic and properties. A client
# that sends a larger packet is disconnected. Each channel's own, smaller limit is checked by the
# side that receives it. No message_size_limit: Mosquitto 2.1 recommends max_packet_size instead.
max_packet_size 1056768
# Per client: QoS 1 messages queued for a client that is offline or slow, beyond those in flight.
max_queued_messages 1000
max_queued_bytes 16777216
max_inflight_messages 20
# Gateways use 60 s (docs/architecture/deployment.md); no client may go without keepalive.
max_keepalive 120
# Sessions of clients that never come back are removed after 30 days.
persistent_client_expiration 30d

# Decision 6: send small packets at once. Without it, Nagle's algorithm holds each CONNACK back
# until the client acknowledges the TLS data sent before it (the session ticket), about 40 ms
# (docs/feasibility/2026-10-12-tpm-poc.md).
set_tcp_nodelay true

# Cloud services (ingest, gateway-ops, and certs as dynamic security's administrator), inside the
# mesh only: ztunnel's mTLS carries the connection, the mesh admits only those services on this
# port, and each logs in with its dynamic security account and password.
listener 1883
protocol mqtt
accept_protocol_versions 5

# The probes: a Unix socket in the pod, no TLS. Anonymous clients may connect here and nothing
# more: they have no role, so dynamic security refuses every publish and subscription.
# max_connections is a listener option: it limits this socket only (Mosquitto 2.1 src/conf.c sets
# it on the current listener; the broker-wide limit would be global_max_connections, not set
# here). tools/migrate/tests/mqtt_broker.rs holds more service connections than that at once.
listener 0 /run/mosquitto/probe.sock
protocol mqtt
accept_protocol_versions 5
listener_allow_anonymous true
max_connections 8

# The gateways' listener (gateway-listener.conf), TLS on 8883: Mosquitto reads every *.conf file
# of this directory at start, and the directory stays empty until the server certificate Secret
# holds a certificate and key that belong together (Laysi/trustiot-v2#535); the trust containers
# empty it again while the Secret is unusable, so a broker that restarts then serves this file's
# other listeners instead of stopping on a certificate it cannot load. After every listener
# of this file, so none of the included listener's options lands on them; only the chart's
# memory_limit, a broker-wide option, follows.
include_dir /mosquitto/listeners
memory_limit {{ include "trustiot.mqttBroker.memoryLimit" . }}
{{- end -}}

{{/*
The gateway listener, mounted read-only into the trust init container and sidecar as
gateway-listener.conf ("trustiot.mqttBroker.gatewayListenerFile"), which copy it into the
broker's listeners directory once the server certificate Secret is usable
(tools/migrate/src/mqtt_trust.rs). files/mosquitto/gateway-listener.conf word for word, which
tools/migrate/tests/mqtt_broker.rs runs; the chart tests fail when the two differ.
*/}}
{{- define "trustiot.mqttBroker.gatewayListener" -}}
# The gateway listener of the chart's MQTT broker (Laysi/trustiot-v2#84, Laysi/trustiot-v2#535;
# README.md, "MQTT broker"). templates/_mqtt-broker.tpl carries the same text; the chart test
# fails when the two differ. The broker's trust init container and sidecar (`migrate
# mqtt-broker-trust`) copy it into /mosquitto/listeners, which mosquitto.conf includes, only once
# the server certificate Secret holds a certificate and key that belong together: Mosquitto exits
# when a listener's certificate file is missing or does not load, and it adds no listener on
# SIGHUP, so until then the broker runs without this port.
#
# Gateways, through mqtt-bridge only (the mesh admits no other caller on this port): TLS 1.3 with
# a client certificate from our CA, checked against the CRL. The certificate's CN is the
# username and the client id, so a gateway cannot act as, or take over the session of, another
# gateway. The server certificate, and the CA and CRL files, are read at start and on SIGHUP,
# which the trust sidecar sends; a reload does not end connections made before it.
listener 8883
protocol mqtt
accept_protocol_versions 5
certfile /etc/trustiot/mqtt-broker-tls/tls.crt
keyfile /etc/trustiot/mqtt-broker-tls/tls.key
cafile /mosquitto/data/trust/current/ca.pem
crlfile /mosquitto/data/trust/current/crl.pem
require_certificate true
use_identity_as_username true
use_username_as_clientid true
tls_version tlsv1.3
# Decision 6: ChaCha20-Poly1305 first. The gateway's CPU has no AES instructions, so ChaCha20
# costs it 21-34% less CPU (Laysi/trustiot-v2#75). Mosquitto picks the suite in this order, not
# in the client's: without this line it negotiates AES-256-GCM although the gateway lists
# ChaCha20 first (Mosquitto 2.1.2, Laysi/trustiot-v2#86).
ciphers_tls1.3 TLS_CHACHA20_POLY1305_SHA256:TLS_AES_256_GCM_SHA384:TLS_AES_128_GCM_SHA256
{{- end -}}

{{/* Where the trust containers mount the broker's ConfigMap, and the gateway listener in it. */}}
{{- define "trustiot.mqttBroker.configDir" -}}
/etc/trustiot/mqtt-broker-config
{{- end -}}

{{- define "trustiot.mqttBroker.gatewayListenerFile" -}}
{{- printf "%s/gateway-listener.conf" (include "trustiot.mqttBroker.configDir" .) -}}
{{- end -}}

{{/* The broker's listeners directory, an emptyDir that mosquitto.conf includes. */}}
{{- define "trustiot.mqttBroker.listenersDir" -}}
/mosquitto/listeners
{{- end -}}

{{/*
Container ports and probes of the broker. The StatefulSet renders them from here, and the mesh
reads the same definition ("trustiot.mesh.probePorts"): the probes run mosquitto_pub inside the
container against the probe socket (an MQTT connection the broker accepted, not just an open TCP
port, and no TLS handshake that would log an error), so the node reaches no port of the broker.
tools/migrate/tests/mqtt_broker.rs runs the same command (PROBE); the chart tests compare them.
Returns YAML {ports, probes: {startup, liveness, readiness}}; use with fromYaml.
*/}}
{{- define "trustiot.mqtt-broker.pod" -}}
{{- $callers := include "trustiot.mqtt-broker.callers" . | fromYaml -}}
{{- $probe := list "mosquitto_pub" "--unix" "/run/mosquitto/probe.sock" "-V" "5" "-I" "probe:" "-t" "trustiot/probe" "-n" "-q" "0" -}}
ports:
  - name: mqtt-gateways
    containerPort: {{ $callers.gateway.port }}
    protocol: TCP
  - name: mqtt-services
    containerPort: {{ $callers.service.port }}
    protocol: TCP
probes:
  # A restart restores every session and message from SQLite before it accepts connections.
  startup:
    exec:
      command: {{ toJson $probe }}
    periodSeconds: 2
    timeoutSeconds: 5
    failureThreshold: 150
  liveness:
    exec:
      command: {{ toJson $probe }}
    periodSeconds: 30
    timeoutSeconds: 5
    failureThreshold: 4
  readiness:
    exec:
      command: {{ toJson $probe }}
    periodSeconds: 10
    timeoutSeconds: 5
    failureThreshold: 3
{{- end -}}

{{/*
Where the trust sidecar fetches the CA certificates and CRLs: the certs service's http port, plain
HTTP that ztunnel carries over mTLS ("trustiot.mesh.plan" adds the broker to the certs service's
callers). Empty while the certs service is off: the broker then trusts no gateway.
*/}}
{{- define "trustiot.mqttBroker.certsUrl" -}}
{{- if include "trustiot.certs.enabled" . -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" . "name" "certs" "values" .Values.services.certs) | fromYaml -}}
{{- $port := 0 -}}
{{- range $config.ports -}}
{{- if eq .name "http" -}}
{{- $port = int (.servicePort | default .containerPort) -}}
{{- end -}}
{{- end -}}
{{- if not $port -}}
{{- fail "services.certs.ports: the MQTT broker fetches the CA certificates and CRLs from the port named http" -}}
{{- end -}}
{{- printf "http://%s.%s.svc:%d" (include "trustiot.componentName" (dict "root" . "name" "certs")) .Release.Namespace $port -}}
{{- end -}}
{{- end -}}

{{/*
Settings of `migrate mqtt-broker-trust` (tools/migrate/src/mqtt_trust.rs), for the init container
and the sidecar alike. Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.mqttBroker.trustEnv" -}}
- name: TRUSTIOT__TRUST__CERTS_URL
  value: {{ include "trustiot.mqttBroker.certsUrl" . | quote }}
- name: TRUSTIOT__TRUST__DIR
  value: /mosquitto/data/trust
- name: TRUSTIOT__TRUST__TLS_DIR
  value: {{ include "trustiot.mqttBroker.tlsDir" . }}
- name: TRUSTIOT__TRUST__LISTENERS_DIR
  value: {{ include "trustiot.mqttBroker.listenersDir" . }}
- name: TRUSTIOT__TRUST__GATEWAY_LISTENER
  value: {{ include "trustiot.mqttBroker.gatewayListenerFile" . }}
- name: TRUSTIOT__TRUST__INTERVAL
  value: {{ .Values.mqttBroker.trustInterval | quote }}
{{- end -}}

{{/*
mqtt-bridge's own settings: the broker's gateway port, where it relays each gateway's WebSocket as
TCP (services/mqtt-bridge/README.md). The mesh admits mqtt-bridge on that port only
("trustiot.mqtt-broker.callers"). Empty while the broker is off: the bridge then refuses to start,
because it has nowhere to relay to. Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.mqttBridge.env" -}}
{{- if include "trustiot.mqttBroker.enabled" . -}}
{{- $gateway := (include "trustiot.mqtt-broker.callers" . | fromYaml).gateway -}}
- name: TRUSTIOT__BROKER__ADDRESS
  value: {{ printf "%s.%s.svc:%d" (include "trustiot.componentName" (dict "root" . "name" "mqtt-broker")) .Release.Namespace (int $gateway.port) | quote }}
{{- end -}}
{{- end -}}

{{/* The security context of every container of the broker pod. */}}
{{- define "trustiot.mqttBroker.securityContext" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
capabilities:
  drop: [ALL]
{{- end -}}

{{/*
Init container that keeps the cloud services' broker accounts: `migrate mqtt-broker-accounts`
(tools/migrate/src/mqtt_accounts.rs) makes sure the Secret ("trustiot.mqttBroker.accountsSecret")
holds a password of mqttBroker.passwordGeneration for every account holder, generating one with
the operating system's CSPRNG when it holds none or an older generation, and writes the dynamic
security file on the broker's volume: the release's roles, deny by default, and each account with
only the PBKDF2-SHA512 hash of its password, keeping every gateway client the broker stored. It
authenticates with a projected token of the broker's ServiceAccount, which only this container
mounts, and the Role in templates/mqtt-broker.yaml lets that ServiceAccount get and patch this one
Secret. It runs the migration image (arm64 only, like the Job).
*/}}
{{- define "trustiot.mqttBroker.accountsInitContainer" -}}
{{- $dir := include "trustiot.migrate.kubeDir" . -}}
name: accounts
image: {{ include "trustiot.migrate.image" . }}
imagePullPolicy: {{ .Values.migrate.image.pullPolicy }}
args:
  - mqtt-broker-accounts
env:
  - name: TRUSTIOT__ACCOUNTS__API_URL
    value: https://kubernetes.default.svc
  - name: TRUSTIOT__ACCOUNTS__NAMESPACE
    valueFrom:
      fieldRef:
        fieldPath: metadata.namespace
  - name: TRUSTIOT__ACCOUNTS__TOKEN_FILE
    value: {{ printf "%s/token" $dir }}
  - name: TRUSTIOT__ACCOUNTS__CA_FILE
    value: {{ printf "%s/ca.crt" $dir }}
  - name: TRUSTIOT__ACCOUNTS__SECRET
    value: {{ include "trustiot.mqttBroker.accountsSecret" . }}
  - name: TRUSTIOT__ACCOUNTS__SERVICES
    value: {{ (include "trustiot.mqttBroker.accounts" . | fromYaml).services | join "," | quote }}
  - name: TRUSTIOT__ACCOUNTS__GENERATION
    value: {{ include "trustiot.mqttBroker.passwordGeneration" . | quote }}
  - name: TRUSTIOT__ACCOUNTS__DYNSEC_FILE
    value: /mosquitto/data/dynamic-security.json
resources:
  {{- toYaml .Values.migrate.resources | nindent 2 }}
securityContext:
  {{- include "trustiot.mqttBroker.securityContext" . | nindent 2 }}
volumeMounts:
  - name: data
    mountPath: /mosquitto/data
  - name: migrate-kube-api
    mountPath: {{ $dir }}
    readOnly: true
{{- end -}}

{{/*
`migrate mqtt-broker-trust` (tools/migrate/src/mqtt_trust.rs) as the init container `trust-files`
(with --once: installs the CA certificates and CRLs from the certs service, or a placeholder that
trusts no gateway, and copies the gateway listener into the listeners directory once the server
certificate and key belong together) and as the sidecar `trust` (fetches them again every
mqttBroker.trustInterval, watches the server certificate Secret, and sends the broker SIGHUP after
either changed; when the Secret becomes usable while the broker runs without the gateway port, it
copies the listener and stops the broker once with SIGTERM, so the kubelet starts it again with
the port). Argument: dict "root" $ "name" <container name> "once" <bool>.
*/}}
{{- define "trustiot.mqttBroker.trustContainer" -}}
{{- $root := .root -}}
name: {{ .name }}
image: {{ include "trustiot.migrate.image" $root }}
imagePullPolicy: {{ $root.Values.migrate.image.pullPolicy }}
args:
  - mqtt-broker-trust
  {{- if .once }}
  - --once
  {{- end }}
env:
  {{- include "trustiot.mqttBroker.trustEnv" $root | nindent 2 }}
resources:
  {{- toYaml $root.Values.migrate.resources | nindent 2 }}
securityContext:
  {{- include "trustiot.mqttBroker.securityContext" $root | nindent 2 }}
volumeMounts:
  - name: data
    mountPath: /mosquitto/data
  - name: server-tls
    mountPath: {{ include "trustiot.mqttBroker.tlsDir" $root }}
    readOnly: true
  # The broker's ConfigMap, whole (it holds gateway-listener.conf), and the listeners directory
  # the broker includes, which only these containers write.
  - name: config
    mountPath: {{ include "trustiot.mqttBroker.configDir" $root }}
    readOnly: true
  - name: listeners
    mountPath: {{ include "trustiot.mqttBroker.listenersDir" $root }}
{{- end -}}
