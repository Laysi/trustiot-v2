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
The paths of hosts.app that keep the cookie check while services.web serves the SPA's static
files there (decision 27): each prefix itself and everything below it. The SPA calls the API
under /api and the login flow lives under /auth, so they are fixed here rather than in the
values (Laysi/trustiot-v2#123).
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.ingress.cookiePrefixes" -}}
{{- list "/api" "/auth" | toYaml -}}
{{- end -}}

{{/*
Whether a path is one of the cookie prefixes or below one, at a segment boundary.
Argument: the path. Returns "true" or "".
*/}}
{{- define "trustiot.ingress.underCookiePrefix" -}}
{{- $path := . -}}
{{- range include "trustiot.ingress.cookiePrefixes" . | fromYamlArray -}}
{{- if or (eq $path .) (hasPrefix (printf "%s/" .) $path) -}}true{{- end -}}
{{- end -}}
{{- end -}}

{{/*
AuthorizationPolicy rules that match each exact public path on its own host, with or without a
port, with any method it does not list (Laysi/trustiot-v2#859). The cookie check uses them so
another method is never exempt, and the DENY policy so the ingress refuses it before any route.
Istio ANDs the fields of one operation, so a rule matches only that host, that path and a method
outside the list.
Argument: the public items of "trustiot.ingress.plan" without the static files.
Returns a YAML list of rules.
*/}}
{{- define "trustiot.ingress.otherMethodRules" -}}
{{- range . }}
- to:
    - operation:
        hosts:
          - {{ .hostname | quote }}
          - {{ printf "%s:*" .hostname | quote }}
        paths:
          - {{ .path | quote }}
        notMethods:
          {{- range .methods }}
          - {{ . | quote }}
          {{- end }}
{{- end }}
{{- end -}}

{{/*
Backend of a route: the Service name and port number of an enabled service. The ports with a
Service and policies of their own fail the render, because the service's Service leaves them out
and only their own callers may reach them: the identity service's token-exchange
(templates/token-exchange.yaml), the certs service's broker-certs, which issues the MQTT
broker's server certificate (templates/certs-broker.yaml, Laysi/trustiot-v2#557), and the AI
gateway's admin, which changes the model roles' models (templates/ai-gateway-admin.yaml,
Laysi/trustiot-v2#607; a route to ai-gateway fails earlier anyway, "trustiot.ingress.plan").
Argument: dict "root" $ "service" <key under services> "port" <port name> "where" <for errors>.
Returns YAML {name, port}; use with fromYaml.
*/}}
{{- define "trustiot.ingress.backend" -}}
{{- $entry := get .root.Values.services .service -}}
{{- if not $entry -}}
{{- fail (printf "%s: unknown service %q" .where .service) -}}
{{- end -}}
{{- if eq .port (get (include "trustiot.mesh.ownPolicyPorts" .root | fromYaml) .service) -}}
{{- fail (printf "%s: port %q of service %q has a Service and policies of its own; the ingress gateway never reaches it" .where .port .service) -}}
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
The public paths (decision 27): key under ingress.publicPaths, host, backend service and methods.
*/}}
{{- define "trustiot.ingress.publicPathTable" -}}
{{- /*
methods: the only methods the exception admits, each the service's own contract; every other
method on that host and path is checked and then denied (templates/ingress.yaml). fixed: the
only path the service serves, so the value may not differ (values.schema.json says the same).
- login: POST, both steps of the email login (services/identity/src/login.rs, LOGIN_PATH).
- healthCheck: GET and HEAD, service-kit's /healthz on the API port (axum's get() also answers HEAD).
- certRenewal: POST, both steps (services/certs/src/renewal.rs, protocol::renewal::RENEWAL_PATH).
- rootCrl: GET and HEAD (services/certs/src/http.rs, protocol::root_crl::ROOT_CRL_PATH).
- mqtt: GET, the WebSocket upgrade, the only method the bridge accepts
  (services/mqtt-bridge/src/handshake.rs).
The SPA's static files ("trustiot.ingress.plan") keep every method: they are not in this table.
*/ -}}
{{- list
  (dict "key" "login" "host" "app" "service" "identity" "methods" (list "POST"))
  (dict "key" "healthCheck" "host" "app" "service" "identity" "methods" (list "GET" "HEAD"))
  (dict "key" "certRenewal" "host" "gateway" "service" "certs" "methods" (list "POST") "fixed" "/certs/renew")
  (dict "key" "rootCrl" "host" "gateway" "service" "certs" "methods" (list "GET" "HEAD") "fixed" "/certs/root-crl")
  (dict "key" "mqtt" "host" "gateway" "service" "mqtt-bridge" "methods" (list "GET")) | toYaml -}}
{{- end -}}

{{/*
Routing plan of the ingress gateway. Every rule gets the same filter except for the header
strip, so all the safety rules live here:
- public paths (the only cookie-free paths, decision 27) come from a fixed table of host,
  backend and methods; each is one exact path and is exempt only while its backend is enabled,
  so an exemption never falls through to another route, and only for its methods;
- cookie routes come from ingress.routes and must point at an enabled service other than
  ai-gateway, which cannot verify the internal identity the cookie check adds;
- nothing is routed under /internal, and a host never has the same path twice;
- while services.web is enabled, the SPA's static files are one more public entry, every path on
  hosts.app (PathPrefix /, key spa, static true); cookie routes on hosts.app must then be under
  the cookie prefixes, and hosts.gateway and hosts.ota must be other names, so the exemption
  never covers one of their routes.
Returns YAML {public: [...], routes: [...]}; each item has host, hostname, path, match, service
(the key under services), backend and, for public paths, key and methods (none for the static
files, which admit every method). Use with fromYaml.
*/}}
{{- define "trustiot.ingress.plan" -}}
{{- $v := .Values -}}
{{- $hostnames := dict "app" $v.hosts.app "gateway" $v.hosts.gateway "ota" $v.hosts.ota -}}
{{- $table := include "trustiot.ingress.publicPathTable" . | fromYamlArray -}}
{{- $exact := include "trustiot.ingress.exactPathPattern" . -}}
{{- $seen := list -}}
{{- $public := list -}}
{{- range $table -}}
{{- $path := get $v.ingress.publicPaths .key -}}
{{- $where := printf "ingress.publicPaths.%s" .key -}}
{{- if not (regexMatch $exact ($path | toString)) -}}
{{- fail (printf "%s: %q must be one exact lower-case path without '.', '..', '//', '%%', ';', wildcards or a trailing '/'" $where $path) -}}
{{- end -}}
{{- if not .methods -}}
{{- fail (printf "%s: the chart's table lists no methods for it; an empty list would refuse every method" $where) -}}
{{- end -}}
{{- if and .fixed (ne $path .fixed) -}}
{{- fail (printf "%s: must be %s, the only path the %s service serves it on; another value would route a path the service does not know" $where .fixed .service) -}}
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
{{- $public = append $public (dict "key" .key "host" .host "hostname" (get $hostnames .host) "path" $path "match" "Exact" "methods" .methods "service" .service "backend" $backend) -}}
{{- end -}}
{{- end -}}
{{- $web := get $v.services "web" -}}
{{- $static := and $web $web.enabled -}}
{{- if $static -}}
{{- range $host := list "gateway" "ota" -}}
{{- if eq (get $hostnames $host) $v.hosts.app -}}
{{- fail (printf "hosts.%s: must differ from hosts.app while services.web is enabled, because every path on hosts.app outside %s skips the cookie check" $host (include "trustiot.ingress.cookiePrefixes" $ | fromYamlArray | join " and ")) -}}
{{- end -}}
{{- end -}}
{{- $backend := include "trustiot.ingress.backend" (dict "root" $ "service" "web" "port" "http" "where" "services.web") | fromYaml -}}
{{- $public = append $public (dict "key" "spa" "host" "app" "hostname" $v.hosts.app "path" "/" "match" "PathPrefix" "service" "web" "backend" $backend "static" true) -}}
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
{{- if and $static (eq $route.host "app") (not (include "trustiot.ingress.underCookiePrefix" $route.path)) -}}
{{- fail (printf "%s: while services.web is enabled, every path on hosts.app outside %s is the SPA's static files, so a cookie route there must be one of them or below one" $where (include "trustiot.ingress.cookiePrefixes" $ | fromYamlArray | join " and ")) -}}
{{- end -}}
{{- $id := printf "%s %s" $route.host $route.path -}}
{{- if has $id $seen -}}
{{- fail (printf "%s: %s is already routed for hosts.%s" $where $route.path $route.host) -}}
{{- end -}}
{{- $seen = append $seen $id -}}
{{- if eq $route.service "ai-gateway" -}}
{{- fail (printf "%s: ai-gateway takes no internal identity tokens and serves only other services (decision 28), so it gets no cookie route; its admin port is the identity service's alone (templates/ai-gateway-admin.yaml)" $where) -}}
{{- end -}}
{{- if include "trustiot.dashboardCheck.is" $route.service -}}
{{- fail (printf "%s: dashboard-check serves only the AI assistant, inside the mesh, so it gets no route" $where) -}}
{{- end -}}
{{- if eq $route.service "web" -}}
{{- fail (printf "%s: web serves only the SPA's static files, which need neither the session cookie nor an internal identity, so it gets no cookie route" $where) -}}
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

{{/*
A whole number from the rate limit values, checked here as well as in values.schema.json so a
render without the schema cannot hand Envoy a fraction, a string or a value out of range.
Argument: dict "value" <value> "min" <int> "max" <int> "where" <for errors>. Returns the number.
*/}}
{{- define "trustiot.ingress.rateLimitCount" -}}
{{- $value := .value -}}
{{- if not (or (kindIs "float64" $value) (kindIs "int64" $value) (kindIs "int" $value)) -}}
{{- fail (printf "%s: must be a whole number from %d to %d, got %s %v" .where (int .min) (int .max) (kindOf $value) $value) -}}
{{- end -}}
{{- $number := int64 $value -}}
{{- if or (ne (float64 $number) (float64 $value)) (lt $number (int64 .min)) (gt $number (int64 .max)) -}}
{{- fail (printf "%s: must be a whole number from %d to %d, got %v" .where (int .min) (int .max) $value) -}}
{{- end -}}
{{- $number -}}
{{- end -}}

{{/*
Per-client rate limit of the ingress gateway (Laysi/trustiot-v2#113; README.md, 「入口的限流」).
ingress.rateLimit.client names the request header that identifies a client. cf-connecting-ip is
the only one so far, and it is trustworthy only with exposure tunnel: the gateway then listens on plain HTTP
inside the host, the NetworkPolicy admits only ingress.tunnel.allowedSources (the host, where
cloudflared runs), and the Cloudflare edge sets CF-Connecting-IP on every request it forwards and
refuses one that already carries it (403, error 1000; docs/feasibility/2026-10-10-edge-poc.md).
With direct exposure every client reaches the gateway and writes any CF-Connecting-IP it likes,
so it is refused there, with or without the schema. X-Forwarded-For never identifies a client: it
is the client's own, and Envoy only appends to it.
Each rule names a public path (resolved through the same table and values as the routes, so it
follows a changed path) or a host, path and match of its own. Its Envoy matchers follow how the
gateway routes the request: Host case-insensitively, with or without a port or a trailing dot;
the path after the mesh's normalization (Envoy normalizes it before any HTTP filter runs), cut at
the query string, case-sensitively, Exact as the whole path and PathPrefix at a segment boundary.
Every value is checked on every render, even with client empty.
Returns YAML {} when client is empty, otherwise {header, maxClients, perClient, rules}; perClient
and each rule have requests and seconds, and each rule also name, authority and path (RE2
patterns). Use with fromYaml.
*/}}
{{- define "trustiot.ingress.rateLimitPlan" -}}
{{- $v := .Values -}}
{{- $limit := $v.ingress.rateLimit | default dict -}}
{{- $client := $limit.client | default "" -}}
{{- if not (has $client (list "" "cf-connecting-ip")) -}}
{{- fail (printf "ingress.rateLimit.client: must be \"\" (no rate limit) or cf-connecting-ip, got %q" ($client | toString)) -}}
{{- end -}}
{{- if and (eq $client "cf-connecting-ip") (ne $v.ingress.exposure "tunnel") -}}
{{- fail (printf "ingress.rateLimit.client: cf-connecting-ip only with ingress.exposure tunnel, where every request comes through the Cloudflare edge, which sets the header; with exposure %q a client sets it to anything it likes (on-prem: Laysi/trustiot-v2#446)" ($v.ingress.exposure | toString)) -}}
{{- end -}}
{{- $maxClients := include "trustiot.ingress.rateLimitCount" (dict "value" $limit.maxClients "min" 1 "max" 100000 "where" "ingress.rateLimit.maxClients") | int64 -}}
{{- $perClient := $limit.perClient | default dict -}}
{{- if not (kindIs "map" $perClient) -}}
{{- fail "ingress.rateLimit.perClient: must be a map with requests and seconds" -}}
{{- end -}}
{{- $bucket := dict
  "requests" (include "trustiot.ingress.rateLimitCount" (dict "value" $perClient.requests "min" 1 "max" 1000000 "where" "ingress.rateLimit.perClient.requests") | int64)
  "seconds" (include "trustiot.ingress.rateLimitCount" (dict "value" $perClient.seconds "min" 1 "max" 3600 "where" "ingress.rateLimit.perClient.seconds") | int64) -}}
{{- $hostnames := dict "app" $v.hosts.app "gateway" $v.hosts.gateway "ota" $v.hosts.ota -}}
{{- $publicHosts := dict -}}
{{- range include "trustiot.ingress.publicPathTable" . | fromYamlArray -}}
{{- $_ := set $publicHosts .key .host -}}
{{- end -}}
{{- $exact := include "trustiot.ingress.exactPathPattern" . -}}
{{- $list := $limit.rules | default list -}}
{{- if not (kindIs "slice" $list) -}}
{{- fail "ingress.rateLimit.rules: must be a list" -}}
{{- end -}}
{{- if gt (len $list) 32 -}}
{{- fail "ingress.rateLimit.rules: at most 32 rules" -}}
{{- end -}}
{{- $names := list -}}
{{- $targets := list -}}
{{- $rules := list -}}
{{- range $i, $rule := $list -}}
{{- $where := printf "ingress.rateLimit.rules[%d]" $i -}}
{{- if not (kindIs "map" $rule) -}}
{{- fail (printf "%s: must be a map" $where) -}}
{{- end -}}
{{- range $field := keys $rule -}}
{{- if not (has $field (list "name" "publicPath" "host" "path" "match" "requests" "seconds")) -}}
{{- fail (printf "%s: unknown field %q" $where $field) -}}
{{- end -}}
{{- end -}}
{{- $name := $rule.name | default "" | toString -}}
{{- if not (regexMatch "^[a-z]([a-z0-9-]{0,30}[a-z0-9])?$" $name) -}}
{{- fail (printf "%s: name %q must be lower-case letters, digits and '-', at most 32 characters" $where $name) -}}
{{- end -}}
{{- if has $name $names -}}
{{- fail (printf "%s: name %q is used twice" $where $name) -}}
{{- end -}}
{{- $names = append $names $name -}}
{{- $host := "" -}}
{{- $path := "" -}}
{{- $match := "" -}}
{{- if hasKey $rule "publicPath" -}}
{{- if or (hasKey $rule "host") (hasKey $rule "path") (hasKey $rule "match") -}}
{{- fail (printf "%s: either publicPath or host, path and match, not both" $where) -}}
{{- end -}}
{{- $public := $rule.publicPath | toString -}}
{{- if not (hasKey $publicHosts $public) -}}
{{- fail (printf "%s: publicPath %q is not one of %s" $where $public (keys $publicHosts | sortAlpha | join ", ")) -}}
{{- end -}}
{{- $host = get $publicHosts $public -}}
{{- $path = get $v.ingress.publicPaths $public | toString -}}
{{- $match = "Exact" -}}
{{- else -}}
{{- $host = $rule.host | default "" | toString -}}
{{- $path = $rule.path | default "" | toString -}}
{{- $match = $rule.match | default "" | toString -}}
{{- end -}}
{{- if not (hasKey $hostnames $host) -}}
{{- fail (printf "%s: host must be app, gateway or ota" $where) -}}
{{- end -}}
{{- if not (has $match (list "PathPrefix" "Exact")) -}}
{{- fail (printf "%s: match must be PathPrefix or Exact" $where) -}}
{{- end -}}
{{- if not (or (eq $path "/") (regexMatch $exact $path)) -}}
{{- fail (printf "%s: path %q must be '/' or a lower-case path without '.', '..', '//', '%%', ';', wildcards or a trailing '/'" $where $path) -}}
{{- end -}}
{{- if hasPrefix "/internal" $path -}}
{{- fail (printf "%s: paths under /internal are never routed (decision 27)" $where) -}}
{{- end -}}
{{- $target := printf "%s %s %s" $host $match $path -}}
{{- if has $target $targets -}}
{{- fail (printf "%s: %s %s on hosts.%s already has a rule" $where $match $path $host) -}}
{{- end -}}
{{- $targets = append $targets $target -}}
{{- $pathPattern := "^/.*$" -}}
{{- if eq $match "Exact" -}}
{{- $pathPattern = printf "^%s(\\?.*)?$" (regexQuoteMeta $path) -}}
{{- else if ne $path "/" -}}
{{- $pathPattern = printf "^%s([/?].*)?$" (regexQuoteMeta $path) -}}
{{- end -}}
{{- $requests := include "trustiot.ingress.rateLimitCount" (dict "value" $rule.requests "min" 1 "max" 1000000 "where" (printf "%s.requests" $where)) | int64 -}}
{{- $seconds := include "trustiot.ingress.rateLimitCount" (dict "value" $rule.seconds "min" 1 "max" 3600 "where" (printf "%s.seconds" $where)) | int64 -}}
{{- $authority := printf "(?i)^%s\\.?(:.*)?$" (regexQuoteMeta (get $hostnames $host)) -}}
{{- $rules = append $rules (dict "name" $name "authority" $authority "path" $pathPattern "requests" $requests "seconds" $seconds) -}}
{{- end -}}
{{- if $client -}}
{{- dict "header" $client "maxClients" $maxClients "perClient" $bucket "rules" $rules | toYaml -}}
{{- else -}}
{{- dict | toYaml -}}
{{- end -}}
{{- end -}}

{{/*
Envoy's local rate limit filter for the ingress gateway: the typed_config of the EnvoyFilter in
templates/ingress.yaml. Every request gets a descriptor {client: <header value>}, and one
{rule: <name>, client: <header value>} for each rule whose Host and path it matches. A request
without the header is the client "-" (a header_value_match on the header's absence), so leaving
the header out never skips a limit. The descriptors in the configuration leave the client's value
out, so Envoy keeps a token bucket per value it sees, at most maxClients of them per descriptor
(max_dynamic_descriptors; the least recently used goes first). A request takes a token from every
bucket it matches and gets 429 when one is empty. The filter's own bucket, used only when no
descriptor matches, is left out: every request matches {client}.
Argument: the plan from "trustiot.ingress.rateLimitPlan". Returns YAML.
*/}}
{{- define "trustiot.ingress.localRateLimit" -}}
{{- $absent := dict "header_value_match" (dict "descriptor_key" "client" "descriptor_value" "-" "headers" (list (dict "name" .header "present_match" true "invert_match" true))) -}}
{{- $present := dict "request_headers" (dict "header_name" .header "descriptor_key" "client") -}}
{{- $limits := list (dict "actions" (list $present)) (dict "actions" (list $absent)) -}}
{{- $descriptors := list (dict "entries" (list (dict "key" "client")) "token_bucket" (include "trustiot.ingress.tokenBucket" .perClient | fromYaml)) -}}
{{- range .rules -}}
{{- $headers := list (dict "name" ":authority" "string_match" (dict "safe_regex" (dict "regex" .authority))) (dict "name" ":path" "string_match" (dict "safe_regex" (dict "regex" .path))) -}}
{{- $match := dict "header_value_match" (dict "descriptor_key" "rule" "descriptor_value" .name "headers" $headers) -}}
{{- $limits = append $limits (dict "actions" (list $match $present)) -}}
{{- $limits = append $limits (dict "actions" (list $match $absent)) -}}
{{- $descriptors = append $descriptors (dict "entries" (list (dict "key" "rule" "value" .name) (dict "key" "client")) "token_bucket" (include "trustiot.ingress.tokenBucket" . | fromYaml)) -}}
{{- end -}}
{{- $always := dict "numerator" 100 "denominator" "HUNDRED" -}}
{{- dict
  "@type" "type.googleapis.com/envoy.extensions.filters.http.local_ratelimit.v3.LocalRateLimit"
  "stat_prefix" "trustiot_ingress_rate_limit"
  "filter_enabled" (dict "runtime_key" "trustiot_ingress_rate_limit_enabled" "default_value" $always)
  "filter_enforced" (dict "runtime_key" "trustiot_ingress_rate_limit_enforced" "default_value" $always)
  "always_consume_default_token_bucket" false
  "max_dynamic_descriptors" (int64 .maxClients)
  "rate_limits" $limits
  "descriptors" $descriptors | toYaml -}}
{{- end -}}

{{/*
A token bucket that holds `requests` and refills them over `seconds`.
Argument: a map with requests and seconds. Returns YAML.
*/}}
{{- define "trustiot.ingress.tokenBucket" -}}
{{- dict "max_tokens" (int64 .requests) "tokens_per_fill" (int64 .requests) "fill_interval" (printf "%ds" (int64 .seconds)) | toYaml -}}
{{- end -}}
