{{/*
Service-to-service authorization helpers (decision 27, Laysi/trustiot-v2#63).
*/}}

{{/*
Source address of connections from the node in Istio ambient. istio-cni SNATs every connection
that starts in the node's network namespace and goes to an ambient pod to this link-local
address, and ztunnel passes it straight to the application port, without mTLS or
AuthorizationPolicy. Kubelet probes arrive this way, but so does any other process on the node's
network, such as a hostNetwork pod in another namespace. The NetworkPolicy therefore admits this
source only on the workload's probe ports ("trustiot.mesh.probePorts").
*/}}
{{- define "trustiot.mesh.hostSource" -}}
169.254.7.127/32
{{- end -}}

{{/*
Container ports and probes of an enabled service or in-cluster component, as its pod spec
renders them. A component <name> defines "trustiot.<name>.pod", which its own template also
uses for the container's ports and probes, so the two cannot drift apart.
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
{{- dict "ports" $config.ports "probes" $config.probes | toYaml -}}
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
deployment runs them. They can be callees in mesh.calls, never callers. Their pods carry the
component's selector labels ("trustiot.selectorLabels" with the component name) and the
ambient label, and the chart generates their policies like a service's.
Returns YAML {<name>: bool}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.components" -}}
{{- dict "clickhouse" (eq .Values.clickhouse.mode "bundled") | toYaml -}}
{{- end -}}

{{/*
Who may call each enabled service and in-cluster component. Callers come from two places only:
- mesh.calls, the list of calls (<caller service>: [<callee service or component>, ...]); a
  call whose caller or callee is disabled is skipped, so the list can name services before
  they ship;
- the ingress gateway, for every service it routes to (public paths and ingress.routes) and for
  the identity service, which it calls for the cookie check (ext_authz).
Every other connection is refused. Callers must be keys under services and callees keys under
services or components ("trustiot.mesh.components"); other names fail the render.
Returns YAML {services: [{name, callers: [service keys], ingress: bool}]}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.plan" -}}
{{- $v := .Values -}}
{{- $components := include "trustiot.mesh.components" . | fromYaml -}}
{{- range $name, $running := $components -}}
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
{{- range $callees -}}
{{- if not (or (hasKey $v.services (toString .)) (hasKey $components (toString .))) -}}
{{- fail (printf "mesh.calls.%s: unknown callee %q; callees are keys under services or in-cluster components (%s)" $caller (toString .) (keys $components | sortAlpha | join ", ")) -}}
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
{{- $services := list -}}
{{- range $name := $enabled -}}
{{- $callers := list -}}
{{- range $caller, $callees := $calls -}}
{{- if and (get $v.services $caller).enabled (has $name $callees) -}}
{{- $callers = append $callers $caller -}}
{{- end -}}
{{- end -}}
{{- $services = append $services (dict "name" $name "callers" $callers "ingress" (has $name $ingressCallees)) -}}
{{- end -}}
{{- dict "services" $services | toYaml -}}
{{- end -}}
