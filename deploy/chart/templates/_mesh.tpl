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
Who may call each enabled service. Callers come from two places only:
- mesh.calls, the list of service-to-service calls (<caller>: [<callee>, ...]); a call whose
  caller or callee is disabled is skipped, so the list can name services before they ship;
- the ingress gateway, for every service it routes to (public paths and ingress.routes) and for
  the identity service, which it calls for the cookie check (ext_authz).
Every other connection is refused. Names that are not keys under services fail the render.
Returns YAML {services: [{name, callers: [service keys], ingress: bool}]}; use with fromYaml.
*/}}
{{- define "trustiot.mesh.plan" -}}
{{- $v := .Values -}}
{{- $calls := $v.mesh.calls | default dict -}}
{{- range $caller, $callees := $calls -}}
{{- if not (hasKey $v.services $caller) -}}
{{- fail (printf "mesh.calls: unknown caller %q; callers and callees are keys under services" $caller) -}}
{{- end -}}
{{- if not (kindIs "slice" $callees) -}}
{{- fail (printf "mesh.calls.%s: must be a list of services" $caller) -}}
{{- end -}}
{{- range $callees -}}
{{- if not (hasKey $v.services (toString .)) -}}
{{- fail (printf "mesh.calls.%s: unknown callee %q; callers and callees are keys under services" $caller (toString .)) -}}
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
{{- $services := list -}}
{{- range $name, $entry := $v.services -}}
{{- if $entry.enabled -}}
{{- $callers := list -}}
{{- range $caller, $callees := $calls -}}
{{- if and (get $v.services $caller).enabled (has $name $callees) -}}
{{- $callers = append $callers $caller -}}
{{- end -}}
{{- end -}}
{{- $services = append $services (dict "name" $name "callers" $callers "ingress" (has $name $ingressCallees)) -}}
{{- end -}}
{{- end -}}
{{- dict "services" $services | toYaml -}}
{{- end -}}
