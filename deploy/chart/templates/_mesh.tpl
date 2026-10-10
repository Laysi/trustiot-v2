{{/*
Service-to-service authorization helpers (decision 27, Laysi/trustiot-v2#63).
*/}}

{{/*
Source address of kubelet probes in Istio ambient: istio-cni SNATs the kubelet's health checks
of mesh pods to this link-local address so ztunnel lets them through, so the NetworkPolicy has to
allow it for probes to pass under default deny.
*/}}
{{- define "trustiot.mesh.kubeletProbeSource" -}}
169.254.7.127/32
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
calls between components itself (the collector writes to ClickHouse). Their pods carry the
component's selector labels ("trustiot.selectorLabels" with the component name) and the
ambient label, and the chart generates their policies like a service's.
Returns YAML {<name>: bool}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.components" -}}
{{- dict "clickhouse" (eq .Values.clickhouse.mode "bundled") "otel-collector" .Values.observability.collector.enabled | toYaml -}}
{{- end -}}

{{/*
Who may call each enabled service and in-cluster component. Callers come from two places only:
- mesh.calls, the list of calls (<caller service>: [<callee service or component>, ...]); a
  call whose caller or callee is disabled is skipped, so the list can name services before
  they ship;
- the ingress gateway, for every service it routes to (public paths and ingress.routes) and for
  the identity service, which it calls for the cookie check (ext_authz);
- the telemetry pipeline, fixed in the chart: every enabled service sends OTLP to the
  OpenTelemetry collector, and the collector writes to the bundled ClickHouse (decision 22).
Every other connection is refused. Callers must be keys under services and callees keys under
services or components ("trustiot.mesh.components"); other names fail the render.
Returns YAML {services: [{name, callers: [service or component keys], ingress: bool}]}; use with
fromYaml.
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
{{- $services = append $services (dict "name" $name "callers" $callers "ingress" (has $name $ingressCallees)) -}}
{{- end -}}
{{- dict "services" $services | toYaml -}}
{{- end -}}
