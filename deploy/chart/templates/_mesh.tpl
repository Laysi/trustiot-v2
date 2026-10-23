{{/*
Service-to-service authorization helpers (decision 27, Laysi/trustiot-v2#63).
*/}}

{{/*
Source address of connections from the node in Istio ambient. istio-cni SNATs every connection
that starts in the node's network namespace and goes to an ambient pod to this link-local
address, and ztunnel passes it straight to the application port, without mTLS or
AuthorizationPolicy. Kubelet probes arrive this way, but so does any other process on the node's
network, such as a hostNetwork pod in another namespace. The NetworkPolicy therefore admits this
source only on the workload's probe ports ("trustiot.mesh.probePorts"). Services probe their
health port, which serves /healthz and /readyz only, so the node never reaches their API
(Laysi/trustiot-v2#286).
*/}}
{{- define "trustiot.mesh.hostSource" -}}
169.254.7.127/32
{{- end -}}

{{/*
Container ports and probes of an enabled service or in-cluster component, as its pod spec
renders them. A service's come from "trustiot.service.containerPorts" (its ports plus the health
port) and its probes, as its Deployment renders them. A component <name> defines
"trustiot.<name>.pod", which its own template also uses for the container's ports and probes, so
the two cannot drift apart.
Argument: dict "root" $ "name" <service or component>.
Returns YAML {ports: [{name, containerPort}], probes: {startup, liveness, readiness}}; use with
fromYaml.
*/}}
{{- define "trustiot.mesh.workloadPod" -}}
{{- $components := include "trustiot.mesh.components" .root | fromYaml -}}
{{- if hasKey $components .name -}}
{{- include (printf "trustiot.%s.pod" .name) .root -}}
{{- else -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" .root "name" .name "values" (get .root.Values.services .name)) | fromYaml -}}
{{- $ports := (include "trustiot.service.containerPorts" (dict "name" .name "config" $config) | fromYaml).ports -}}
{{- dict "ports" $ports "probes" $config.probes | toYaml -}}
{{- end -}}
{{- end -}}

{{/*
TCP ports that the startup, liveness and readiness probes of a service or in-cluster component
connect to: the port of each httpGet, tcpSocket and grpc probe, with named ports resolved to the
container port. exec probes run inside the container and need none. Sorted, without duplicates.
Argument: dict "root" $ "name" <service or component>.
Returns YAML {ports: [<number>, ...]}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.probePorts" -}}
{{- $name := .name -}}
{{- $pod := include "trustiot.mesh.workloadPod" . | fromYaml -}}
{{- $byName := dict -}}
{{- range $pod.ports -}}
{{- $_ := set $byName .name (int .containerPort) -}}
{{- end -}}
{{- $keys := list -}}
{{- range $kind := list "startup" "liveness" "readiness" -}}
{{- $probe := get ($pod.probes | default dict) $kind | default dict -}}
{{- range $handler := list "httpGet" "tcpSocket" "grpc" -}}
{{- with get $probe $handler -}}
{{- $port := .port -}}
{{- if kindIs "string" $port -}}
{{- if not (hasKey $byName $port) -}}
{{- fail (printf "services.%s: probes.%s uses port %q, which is not in ports" $name $kind $port) -}}
{{- end -}}
{{- $port = get $byName $port -}}
{{- else if not (or (kindIs "float64" $port) (kindIs "int" $port) (kindIs "int64" $port)) -}}
{{- fail (printf "services.%s: probes.%s.%s needs a port" $name $kind $handler) -}}
{{- end -}}
{{- /* Zero-padded so that sortAlpha orders the numbers. */ -}}
{{- $keys = append $keys (printf "%05d" (int $port)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $ports := list -}}
{{- range $keys | uniq | sortAlpha -}}
{{- $ports = append $ports (atoi .) -}}
{{- end -}}
{{- dict "ports" $ports | toYaml -}}
{{- end -}}

{{/*
ztunnel's HBONE port: every mesh connection, from a service or the ingress gateway, reaches the
destination pod here (mTLS inside HTTP CONNECT), never on the application port.
*/}}
{{- define "trustiot.mesh.hbonePort" -}}
15008
{{- end -}}

{{/*
Mesh identity of a ServiceAccount in the release namespace, as AuthorizationPolicy principals
spell it (the SPIFFE ID without "spiffe://"). Argument: dict "root" $ "serviceAccount" <name>.
*/}}
{{- define "trustiot.mesh.principal" -}}
{{- printf "%s/ns/%s/sa/%s" .root.Values.mesh.trustDomain .root.Release.Namespace .serviceAccount -}}
{{- end -}}

{{/*
In-cluster components that services call through the mesh, by name, with whether this
deployment runs them. They can be callees in mesh.calls, never callers there; the plan adds the
calls between components itself (the collector writes to ClickHouse, the MQTT broker fetches its
trust files from the certs service). Their pods carry the
component's selector labels ("trustiot.selectorLabels" with the component name) and the
ambient label, and the chart generates their policies like a service's.
Returns YAML {<name>: bool}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.components" -}}
{{- dict "clickhouse" (eq .Values.clickhouse.mode "bundled") "otel-collector" .Values.observability.collector.enabled "mqtt-broker" .Values.mqttBroker.enabled | toYaml -}}
{{- end -}}

{{/*
Dependencies that services list in mesh.calls but reach outside the mesh, so they get no
AuthorizationPolicy or HBONE rule from the plan: Postgres, whose every connection is TLS with
verify-full and whose bundled pods run outside the ambient mesh (templates/postgres.yaml admits its
callers with its own NetworkPolicy). The list also decides which services get the server's CA
("trustiot.postgres.clients"), in external mode too, so a call to it is never skipped as "not
deployed". Returns YAML {<name>: true}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.directCallees" -}}
{{- dict "postgres" true | toYaml -}}
{{- end -}}

{{/*
Calls on behalf of users (decision 27, Laysi/trustiot-v2#106): the calls in mesh.calls from one
enabled service to another, leaving out ai-gateway on either side, which verifies no internal
identity and holds no user's token (decision 28). A service serving a user's request calls
another one only with a token the identity service exchanged for it, and the identity service
exchanges only along this list (TRUSTIOT_ON_BEHALF_CALLS, "trustiot.identity.env"), so the plan
also lets every caller here reach the identity service's token exchange port, and nothing else
of the identity service ("trustiot.mesh.identityPorts", templates/token-exchange.yaml). Calls to in-cluster components, Postgres
and ai-gateway are authorized by the mesh alone.
Returns YAML {calls: {<caller>: [<callee>, ...]}}, callees sorted, callers without a callee left
out; use with fromYaml.
*/}}
{{- define "trustiot.mesh.onBehalfCalls" -}}
{{- $v := .Values -}}
{{- $calls := dict -}}
{{- range $caller, $callees := $v.mesh.calls | default dict -}}
{{- $entry := get $v.services $caller -}}
{{- if and $entry $entry.enabled (ne $caller "ai-gateway") (kindIs "slice" $callees) -}}
{{- $list := list -}}
{{- range $callees -}}
{{- $callee := toString . -}}
{{- $target := get $v.services $callee -}}
{{- if and $target $target.enabled (ne $callee "ai-gateway") -}}
{{- $list = append $list $callee -}}
{{- end -}}
{{- end -}}
{{- if $list -}}
{{- $_ := set $calls $caller ($list | uniq | sortAlpha) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- dict "calls" $calls | toYaml -}}
{{- end -}}

{{/*
The identity service's ports as its AuthorizationPolicy splits them (Laysi/trustiot-v2#106): the
token exchange's port, the container port named token-exchange (the service's
exchange.listen_addr, where it serves POST /internal/token-exchange and nothing else), and its API
ports, every other port in ports (the health port is never one). The ingress gateway and the
services that list identity in mesh.calls reach the API ports only (the identity service's
policy, templates/mesh.yaml); the services that call on behalf of users
("trustiot.mesh.onBehalfCalls") reach the exchange port only (its own Service and policies,
templates/token-exchange.yaml), so they never reach the ext_authz check on the API port, which
turns any session cookie into a token for any service.
Fails when no API port is left: a rule with an empty port list would match every port.
Argument: the root context. Returns YAML {api: [<number>, ...], exchange: <number, 0 when there
is no such port>}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.identityPorts" -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" . "name" "identity" "values" .Values.services.identity) | fromYaml -}}
{{- $api := list -}}
{{- $exchange := 0 -}}
{{- range $config.ports -}}
{{- if eq .name "token-exchange" -}}
{{- $exchange = int .containerPort -}}
{{- else -}}
{{- $api = append $api (int .containerPort) -}}
{{- end -}}
{{- end -}}
{{- if not $api -}}
{{- fail "services.identity.ports: the API port (http) is missing; token-exchange serves the token exchange only" -}}
{{- end -}}
{{- dict "api" $api "exchange" $exchange | toYaml -}}
{{- end -}}

{{/*
The certs service's ports as its AuthorizationPolicy splits them (Laysi/trustiot-v2#557): the
MQTT broker's server certificate issuance, the container port named broker-certs (the service's
broker.listen_addr, where it serves POST /internal/certs/v1/broker-certificate and nothing else),
and its API ports, every other port in ports (the health port is never one). The ingress gateway,
the services that list certs in mesh.calls and the broker's trust sidecar reach the API ports only
(the certs service's policy, templates/mesh.yaml); the MQTT broker's pod reaches the broker-certs
port too (its own Service and policies, templates/certs-broker.yaml), so no other workload can get
a certificate for the broker's name.
Fails when no API port is left: a rule with an empty port list would match every port.
Argument: the root context. Returns YAML {api: [<number>, ...], broker: <number, 0 when there is
no such port>}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.certsPorts" -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" . "name" "certs" "values" .Values.services.certs) | fromYaml -}}
{{- $api := list -}}
{{- $broker := 0 -}}
{{- range $config.ports -}}
{{- if eq .name "broker-certs" -}}
{{- $broker = int .containerPort -}}
{{- else -}}
{{- $api = append $api (int .containerPort) -}}
{{- end -}}
{{- end -}}
{{- if not $api -}}
{{- fail "services.certs.ports: the API port (http) is missing; broker-certs serves the MQTT broker's server certificate only" -}}
{{- end -}}
{{- dict "api" $api "broker" $broker | toYaml -}}
{{- end -}}

{{/*
Who may call each enabled service and in-cluster component. Callers come from these places only:
- mesh.calls, the list of calls (<caller service>: [<callee service or component>, ...]); a
  call whose caller or callee is disabled is skipped, so the list can name services before
  they ship;
- the token exchange: every caller in "trustiot.mesh.onBehalfCalls" may call the identity
  service's token exchange port, and only that port ("trustiot.mesh.identityPorts"), to exchange
  the request's token for one addressed to the callee (the identity service too, when it calls
  another service: it holds the signing keys anyway, so reaching itself grants it nothing). These
  callers are the identity entry's exchangeCallers, not its callers: templates/token-exchange.yaml
  renders their Service and policies;
- the ingress gateway, for every service it routes to (public paths and ingress.routes) and for
  the identity service, which it calls for the cookie check (ext_authz) on its API port;
- the telemetry pipeline, fixed in the chart: every enabled service sends OTLP to the
  OpenTelemetry collector, and the collector writes to the bundled ClickHouse (decision 22);
- the migration Job, fixed in the chart too: it reaches the bundled ClickHouse while
  migrate.enabled is true (tools/migrate);
- the MQTT broker's trust sidecar, fixed in the chart too: it reaches the certs service's API
  ports while both run (templates/mqtt-broker.yaml);
- the MQTT broker's server certificate (Laysi/trustiot-v2#557): the broker's pod reaches the
  certs service's broker-certs port, and only that port ("trustiot.mesh.certsPorts"), while both
  run. It is the certs entry's brokerCallers, not a caller of the API ports' rule:
  templates/certs-broker.yaml renders its Service and policies.
The MQTT broker admits each of its callers on one port only ("trustiot.mqtt-broker.callers"):
its entry also has portRules, [{callers, ports}], and mesh.calls may list it as the callee of
those callers only. mesh.calls may list ai-gateway as the callee of the AI assistant, the knowledge
base and alerts only ("trustiot.aiGateway.callers"), whether or not they are enabled; the ingress
gateway never reaches it, because "trustiot.ingress.plan" refuses every cookie route to ai-gateway
and no public path leads there.
Every other connection is refused. Callers must be keys under services and callees keys under
services, components ("trustiot.mesh.components") or direct callees ("trustiot.mesh.directCallees",
which get no entry here); other names fail the render.
Returns YAML {services: [{name, callers: [service or component keys], ingress: bool}]}; use with
fromYaml. The identity service's entry also has apiPorts (the ports its callers and the ingress
gateway reach), exchangeCallers (the callers on behalf of users, sorted) and exchangePort (the
only port they reach); the render fails when there are such callers but no token-exchange port.
The certs service's entry likewise has apiPorts, brokerCallers (the MQTT broker, while it runs)
and brokerPort; the render fails when the broker runs but there is no broker-certs port.
*/}}
{{- define "trustiot.mesh.plan" -}}
{{- $v := .Values -}}
{{- $components := include "trustiot.mesh.components" . | fromYaml -}}
{{- $direct := include "trustiot.mesh.directCallees" . | fromYaml -}}
{{- range $name := concat (keys $components) (keys $direct) -}}
{{- if hasKey $v.services $name -}}
{{- fail (printf "services.%s: the name belongs to an in-cluster component" $name) -}}
{{- end -}}
{{- end -}}
{{- $calls := $v.mesh.calls | default dict -}}
{{- range $caller, $callees := $calls -}}
{{- if not (hasKey $v.services $caller) -}}
{{- fail (printf "mesh.calls: unknown caller %q; callers are keys under services" $caller) -}}
{{- end -}}
{{- if not (kindIs "slice" $callees) -}}
{{- fail (printf "mesh.calls.%s: must be a list of services" $caller) -}}
{{- end -}}
{{- $brokerCallers := include "trustiot.mqtt-broker.callers" $ | fromYaml -}}
{{- $aiGatewayCallers := include "trustiot.aiGateway.callers" $ | fromYamlArray -}}
{{- range $callees -}}
{{- if and (eq (toString .) "ai-gateway") (not (has $caller $aiGatewayCallers)) -}}
{{- fail (printf "mesh.calls.%s: only %s may call ai-gateway (decision 28): it verifies no internal identity and spends the AI provider accounts, so the mesh alone keeps every other service out (deploy/chart/README.md, \"服務間授權（mesh）\")" $caller (join ", " $aiGatewayCallers)) -}}
{{- end -}}
{{- if eq (toString .) "mqtt-bridge" -}}
{{- fail (printf "mesh.calls.%s: no service may call mqtt-bridge: only the ingress gateway reaches it (decisions 6 and 27), because a caller could set the trusted source-IP header itself and reach the broker's gateway port through it (services/mqtt-bridge/README.md, \"來源 IP\")" $caller) -}}
{{- end -}}
{{- if and (eq (toString .) "mqtt-broker") (not (has $caller (concat $brokerCallers.gateway.callers $brokerCallers.service.callers))) -}}
{{- fail (printf "mesh.calls.%s: the MQTT broker admits only %s on its gateway port and %s on its service port (deploy/chart/README.md, \"MQTT broker\")" $caller (join ", " $brokerCallers.gateway.callers) (join ", " $brokerCallers.service.callers)) -}}
{{- end -}}
{{- if not (or (hasKey $v.services (toString .)) (hasKey $components (toString .)) (hasKey $direct (toString .))) -}}
{{- fail (printf "mesh.calls.%s: unknown callee %q; callees are keys under services or in-cluster components (%s)" $caller (toString .) (concat (keys $components) (keys $direct) | sortAlpha | join ", ")) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $ingressCallees := list -}}
{{- $identity := get $v.services "identity" -}}
{{- if and $identity $identity.enabled -}}
{{- $ingressCallees = append $ingressCallees "identity" -}}
{{- end -}}
{{- $routing := include "trustiot.ingress.plan" . | fromYaml -}}
{{- range concat $routing.public $routing.routes -}}
{{- $ingressCallees = append $ingressCallees .service -}}
{{- end -}}
{{- $enabled := list -}}
{{- range $name, $entry := $v.services -}}
{{- if $entry.enabled -}}
{{- $enabled = append $enabled $name -}}
{{- end -}}
{{- end -}}
{{- range $name, $running := $components -}}
{{- if $running -}}
{{- $enabled = append $enabled $name -}}
{{- end -}}
{{- end -}}
{{- $onBehalf := (include "trustiot.mesh.onBehalfCalls" . | fromYaml).calls -}}
{{- $services := list -}}
{{- range $name := $enabled -}}
{{- $callers := list -}}
{{- range $caller, $callees := $calls -}}
{{- if and (get $v.services $caller).enabled (has $name $callees) -}}
{{- $callers = append $callers $caller -}}
{{- end -}}
{{- end -}}
{{- $planned := dict "name" $name -}}
{{- if eq $name "identity" -}}
{{- /* Callers on behalf of users exchange the request's token on the token exchange's own port
(Laysi/trustiot-v2#106); everyone else reaches the API ports only. */ -}}
{{- $ports := include "trustiot.mesh.identityPorts" $ | fromYaml -}}
{{- $exchangeCallers := keys $onBehalf | sortAlpha -}}
{{- if and $exchangeCallers (not $ports.exchange) -}}
{{- fail (printf "services.identity.ports: the token exchange needs the port named token-exchange (the service's exchange.listen_addr, default 8082), because %s call other services on behalf of users (mesh.calls)" (join ", " $exchangeCallers)) -}}
{{- end -}}
{{- $_ := set $planned "apiPorts" $ports.api -}}
{{- $_ := set $planned "exchangeCallers" $exchangeCallers -}}
{{- $_ := set $planned "exchangePort" $ports.exchange -}}
{{- end -}}
{{- if eq $name "otel-collector" -}}
{{- /* Every service exports its logs, traces and metrics to the collector. */ -}}
{{- range $service, $entry := $v.services -}}
{{- if and $entry.enabled (not (has $service $callers)) -}}
{{- $callers = append $callers $service -}}
{{- end -}}
{{- end -}}
{{- else if and (eq $name "clickhouse") (get $components "otel-collector") -}}
{{- $callers = append $callers "otel-collector" -}}
{{- end -}}
{{- if and (eq $name "clickhouse") $v.migrate.enabled -}}
{{- /* The migration Job applies the DDL and creates the accounts (tools/migrate). */ -}}
{{- $callers = append $callers "migrate" -}}
{{- end -}}
{{- if eq $name "certs" -}}
{{- /* The API ports serve the CA chain, the CRLs and the workstation API; the broker-certs port
the MQTT broker's server certificate, for the broker alone (Laysi/trustiot-v2#557). */ -}}
{{- $ports := include "trustiot.mesh.certsPorts" $ | fromYaml -}}
{{- $brokerCallers := list -}}
{{- if get $components "mqtt-broker" -}}
{{- /* The broker's trust sidecar fetches the CA certificates and CRLs. */ -}}
{{- $callers = append $callers "mqtt-broker" -}}
{{- $brokerCallers = list "mqtt-broker" -}}
{{- if not $ports.broker -}}
{{- fail "services.certs.ports: the MQTT broker's server certificate needs the port named broker-certs (the service's broker.listen_addr, default 8082), because mqttBroker.enabled is true" -}}
{{- end -}}
{{- end -}}
{{- $_ := set $planned "apiPorts" $ports.api -}}
{{- $_ := set $planned "brokerCallers" $brokerCallers -}}
{{- $_ := set $planned "brokerPort" $ports.broker -}}
{{- end -}}
{{- if eq $name "mqtt-broker" -}}
{{- /* Each caller on its own port: mqtt-bridge on the gateway port, the account holders on the
service port. */ -}}
{{- $byPort := include "trustiot.mqtt-broker.callers" $ | fromYaml -}}
{{- $rules := list -}}
{{- range $kind := list "gateway" "service" -}}
{{- $entry := get $byPort $kind -}}
{{- $these := list -}}
{{- range $callers -}}
{{- if has . $entry.callers -}}
{{- $these = append $these . -}}
{{- end -}}
{{- end -}}
{{- if $these -}}
{{- $rules = append $rules (dict "callers" $these "ports" (list $entry.port)) -}}
{{- end -}}
{{- end -}}
{{- $_ := set $planned "portRules" $rules -}}
{{- end -}}
{{- $_ := set $planned "callers" $callers -}}
{{- $_ := set $planned "ingress" (has $name $ingressCallees) -}}
{{- $services = append $services $planned -}}
{{- end -}}
{{- dict "services" $services | toYaml -}}
{{- end -}}
