{{/*
Ingress gateway helpers (decision 27, Laysi/trustiot-v2#62).
*/}}

{{/*
Name of the Gateway. Istio names the Deployment, Service and ServiceAccount it creates
"<gateway>-istio", so the Gateway name leaves room for that suffix within 63 characters.
*/}}
{{- define "trustiot.ingress.gatewayName" -}}
{{- $suffix := "-ingress" -}}
{{- $prefix := include "trustiot.fullname" . | trunc (int (sub 57 (len $suffix))) | trimSuffix "-" -}}
{{- printf "%s%s" $prefix $suffix -}}
{{- end -}}

{{/*
The listener every route attaches to: plain HTTP on 80 behind Cloudflare Tunnel (cloudflared on
the host forwards to the ClusterIP), HTTPS on 443 when the gateway faces the network directly.
*/}}
{{- define "trustiot.ingress.listener" -}}
{{- if eq .Values.ingress.exposure "tunnel" -}}http{{- else -}}https{{- end -}}
{{- end -}}

{{/*
Port of that listener. Istio's gateway Service forwards each listener port to the same port on
the pod, where Envoy binds it directly (Istio lets the gateway pod bind ports below 1024 with
the sysctl net.ipv4.ip_unprivileged_port_start=0), so the NetworkPolicy admits this number.
*/}}
{{- define "trustiot.ingress.listenerPort" -}}
{{- if eq (include "trustiot.ingress.listener" .) "http" -}}80{{- else -}}443{{- end -}}
{{- end -}}

{{/*
Istio's status port on the gateway pod. Istio fixes it: the kube-gateway injection template
gives the istio-proxy container a startup and a readiness probe, both GET /healthz/ready on
15021, and the generated Service lists it as "status-port" (Istio 1.30.5, checked on the lab
cluster in docs/test-reports/2026-10-11-287-gateway-netpol-ports.md). The gateway pod is not in
the ambient mesh (Istio labels it istio.io/dataplane-mode: none), so the kubelet's probes come
from the host's own address, not from the ambient node source ("trustiot.mesh.hostSource").
*/}}
{{- define "trustiot.ingress.statusPort" -}}
15021
{{- end -}}

{{/*
The ports of the gateway's NetworkPolicy rules: the listener and Istio's status port, both TCP.
Every other port of the pod (Envoy's metrics on 15020 and 15090) stays closed.
Returns a YAML list of NetworkPolicyPort.
*/}}
{{- define "trustiot.ingress.policyPorts" -}}
{{- list
  (dict "port" (int (include "trustiot.ingress.listenerPort" .)) "protocol" "TCP")
  (dict "port" (int (include "trustiot.ingress.statusPort" .)) "protocol" "TCP") | toYaml -}}
{{- end -}}

{{/*
Plain IPv4 address shared with values.schema.json (optionalIpv4Address): four decimal octets without
leading zeros, so no CIDR, no "None" (headless) and no IPv6.
*/}}
{{- define "trustiot.ingress.ipv4Pattern" -}}
^(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}$
{{- end -}}

{{/*
The fixed ClusterIP of the ingress gateway Service behind the tunnel (ingress.tunnel.clusterIP,
Laysi/trustiot-v2#478), or "" when it is not set. cloudflared on the host forwards to this
address, so a recreated Service keeps it. Checked here, not only in values.schema.json, because
the value goes into the strategic-merge patch Istio applies to the Service it generates: with or
without schema validation, only a plain IPv4 address reaches it, and only when exposure is
tunnel (a direct Service would silently ignore it otherwise).
*/}}
{{- define "trustiot.ingress.tunnelClusterIP" -}}
{{- $tunnel := .Values.ingress.tunnel | default dict -}}
{{- $address := "" -}}
{{- if hasKey $tunnel "clusterIP" -}}
{{- $raw := index $tunnel "clusterIP" -}}
{{- /* Not `default`: it would take false or 0 for unset and silently drop the pin. */ -}}
{{- if not (or (kindIs "invalid" $raw) (kindIs "string" $raw)) -}}
{{- fail (printf "ingress.tunnel.clusterIP: must be a string holding one IPv4 address or empty, got %s %v" (kindOf $raw) $raw) -}}
{{- end -}}
{{- $address = $raw | default "" -}}
{{- end -}}
{{- if $address -}}
{{- if ne .Values.ingress.exposure "tunnel" -}}
{{- fail (printf "ingress.tunnel.clusterIP: set only with ingress.exposure tunnel, where cloudflared forwards to the ingress Service's ClusterIP; exposure %q never uses it" (.Values.ingress.exposure | toString)) -}}
{{- end -}}
{{- if not (regexMatch (include "trustiot.ingress.ipv4Pattern" .) $address) -}}
{{- fail (printf "ingress.tunnel.clusterIP: %q must be one IPv4 address in dotted-quad form without leading zeros (no CIDR, no \"None\", no IPv6), from the static band of the cluster's Service range" $address) -}}
{{- end -}}
{{- end -}}
{{- $address -}}
{{- end -}}

{{/*
Exact-path form shared with values.schema.json: segments start with a letter or digit, so no dot
segments, empty segments, trailing slash, percent-encoding, ';' or wildcards.
*/}}
{{- define "trustiot.ingress.exactPathPattern" -}}
^(/[a-z0-9][a-z0-9._-]*)+$
{{- end -}}

{{/*
Backend of a route: the Service name and port number of an enabled service.
Argument: dict "root" $ "service" <key under services> "port" <port name> "where" <for errors>.
Returns YAML {name, port}; use with fromYaml.
*/}}
{{- define "trustiot.ingress.backend" -}}
{{- $entry := get .root.Values.services .service -}}
{{- if not $entry -}}
{{- fail (printf "%s: unknown service %q" .where .service) -}}
{{- end -}}
{{- $config := include "trustiot.serviceConfig" (dict "root" .root "name" .service "values" $entry) | fromYaml -}}
{{- $number := 0 -}}
{{- range $config.ports -}}
{{- if eq .name $.port -}}
{{- $number = int (.servicePort | default .containerPort) -}}
{{- end -}}
{{- end -}}
{{- if not $number -}}
{{- fail (printf "%s: service %q has no port named %q" .where .service .port) -}}
{{- end -}}
{{- dict "name" (include "trustiot.componentName" (dict "root" .root "name" .service)) "port" $number | toYaml -}}
{{- end -}}

{{/*
Routing plan of the ingress gateway. Every rule gets the same filter except for the header
strip, so all the safety rules live here:
- public paths (the only cookie-free paths, decision 27) come from a fixed table of host and
  backend; each is one exact path and is exempt only while its backend is enabled, so an
  exemption never falls through to another route;
- cookie routes come from ingress.routes and must point at an enabled service other than
  ai-gateway, which cannot verify the internal identity the cookie check adds;
- nothing is routed under /internal, and a host never has the same path twice.
Returns YAML {public: [...], routes: [...]}; each item has host, hostname, path, match, service
(the key under services), backend and, for public paths, key. Use with fromYaml.
*/}}
{{- define "trustiot.ingress.plan" -}}
{{- $v := .Values -}}
{{- $hostnames := dict "app" $v.hosts.app "gateway" $v.hosts.gateway "ota" $v.hosts.ota -}}
{{- $table := list
  (dict "key" "login" "host" "app" "service" "identity")
  (dict "key" "healthCheck" "host" "app" "service" "identity")
  (dict "key" "certRenewal" "host" "gateway" "service" "certs")
  (dict "key" "mqtt" "host" "gateway" "service" "mqtt-bridge") -}}
{{- $exact := include "trustiot.ingress.exactPathPattern" . -}}
{{- $seen := list -}}
{{- $public := list -}}
{{- range $table -}}
{{- $path := get $v.ingress.publicPaths .key -}}
{{- $where := printf "ingress.publicPaths.%s" .key -}}
{{- if not (regexMatch $exact ($path | toString)) -}}
{{- fail (printf "%s: %q must be one exact lower-case path without '.', '..', '//', '%%', ';', wildcards or a trailing '/'" $where $path) -}}
{{- end -}}
{{- if hasPrefix "/internal" $path -}}
{{- fail (printf "%s: paths under /internal are never routed (decision 27)" $where) -}}
{{- end -}}
{{- $id := printf "%s %s" .host $path -}}
{{- if has $id $seen -}}
{{- fail (printf "%s: %s is listed twice for hosts.%s" $where $path .host) -}}
{{- end -}}
{{- $seen = append $seen $id -}}
{{- $entry := get $v.services .service -}}
{{- if and $entry $entry.enabled -}}
{{- $backend := include "trustiot.ingress.backend" (dict "root" $ "service" .service "port" "http" "where" $where) | fromYaml -}}
{{- $public = append $public (dict "key" .key "host" .host "hostname" (get $hostnames .host) "path" $path "match" "Exact" "service" .service "backend" $backend) -}}
{{- end -}}
{{- end -}}
{{- $routes := list -}}
{{- range $i, $route := $v.ingress.routes -}}
{{- $where := printf "ingress.routes[%d]" $i -}}
{{- if not (hasKey $hostnames $route.host) -}}
{{- fail (printf "%s: host must be app, gateway or ota" $where) -}}
{{- end -}}
{{- if not (has $route.match (list "PathPrefix" "Exact")) -}}
{{- fail (printf "%s: match must be PathPrefix or Exact" $where) -}}
{{- end -}}
{{- if not (or (eq $route.path "/") (regexMatch $exact ($route.path | toString))) -}}
{{- fail (printf "%s: %q must be '/' or a lower-case path without '.', '..', '//', '%%', ';', wildcards or a trailing '/'" $where $route.path) -}}
{{- end -}}
{{- if hasPrefix "/internal" $route.path -}}
{{- fail (printf "%s: paths under /internal are never routed (decision 27)" $where) -}}
{{- end -}}
{{- $id := printf "%s %s" $route.host $route.path -}}
{{- if has $id $seen -}}
{{- fail (printf "%s: %s is already routed for hosts.%s" $where $route.path $route.host) -}}
{{- end -}}
{{- $seen = append $seen $id -}}
{{- if eq $route.service "ai-gateway" -}}
{{- fail (printf "%s: ai-gateway takes no internal identity tokens and serves only other services (decision 28), so it gets no cookie route" $where) -}}
{{- end -}}
{{- $entry := get $v.services $route.service -}}
{{- if not (and $entry $entry.enabled) -}}
{{- fail (printf "%s: service %q is not enabled" $where $route.service) -}}
{{- end -}}
{{- $backend := include "trustiot.ingress.backend" (dict "root" $ "service" $route.service "port" ($route.port | default "http") "where" $where) | fromYaml -}}
{{- $routes = append $routes (dict "host" $route.host "hostname" (get $hostnames $route.host) "path" $route.path "match" $route.match "service" $route.service "backend" $backend) -}}
{{- end -}}
{{- dict "public" $public "routes" $routes | toYaml -}}
{{- end -}}
