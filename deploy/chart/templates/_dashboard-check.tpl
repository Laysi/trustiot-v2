{{/*
Dashboard check helpers (services/dashboard-check, Laysi/trustiot-v2#150). The service runs pages
a model wrote in a headless Chromium without Chromium's own sandbox, so code on such a page may
run as the service's user in its pod (services/dashboard-check/README.md, 「威脅模型」). The chart
therefore gives the pod nothing that leads out of it or to another service:
- its NetworkPolicy has policyTypes [Ingress, Egress] and egress [], so the pod opens no
  connection at all, DNS included (templates/mesh.yaml); ambient DNS capture is off for the pod
  (the annotation ambient.istio.io/dns-capture), so a name lookup goes to the cluster DNS, which
  the NetworkPolicy refuses, never to ztunnel's DNS proxy;
- it calls nobody: mesh.calls must not list it as a caller, it sends no telemetry to the
  collector, and the identity service exchanges no token for it (it verifies no internal
  identity, like ai-gateway);
- only the AI assistant may call it ("trustiot.dashboardCheck.callers"), never the ingress;
- no ServiceAccount token, no Secret, no credential, no platform ConfigMap, no CA or identity key
  mount, no init container (the migration wait's init container needs the API server);
- its /tmp, the only place Chromium writes, is a memory-backed emptyDir;
- the restricted Pod Security settings stay as serviceDefaults sets them, and nothing runs as
  root (neither runAsNonRoot false in the container nor runAsUser 0);
- the service is its container's PID 1, which the cleanup between checks needs to kill every
  other process and empty the pod's shared memory and IPC objects: the chart sets
  TRUSTIOT__CLEANUP=container ("trustiot.dashboardCheck.env"), with which the service refuses
  to start anywhere else, and the pod neither shares its process namespace nor the node's
  processes, IPC or network.
"trustiot.dashboardCheck.checkConfig" refuses values that would undo any of the per-pod parts.
*/}}

{{/* The service's key under services. */}}
{{- define "trustiot.dashboardCheck.name" -}}
dashboard-check
{{- end -}}

{{/*
Whether `.` (a service name) is the dashboard check. Returns "true" or "".
*/}}
{{- define "trustiot.dashboardCheck.is" -}}
{{- if eq (toString .) (include "trustiot.dashboardCheck.name" nil) -}}true{{- end -}}
{{- end -}}

{{/*
The only services that may call the dashboard check (decision 33; docs/architecture/ai-assistant.md,
「儀表板」): the AI assistant, which checks every page before it is published. The check verifies
no internal identity, so the mesh alone keeps everybody else out: listing dashboard-check as the
callee of any other service in mesh.calls fails the render ("trustiot.mesh.plan").
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.dashboardCheck.callers" -}}
{{- list "assistant" | toYaml -}}
{{- end -}}

{{/*
Pod annotations the chart sets on the dashboard check. Istio ambient captures a pod's DNS
(port 53) into ztunnel's DNS proxy by default; with capture off, a lookup goes to the cluster DNS
service, which the pod's NetworkPolicy refuses like every other connection.
Returns YAML {<annotation>: <value>}; use with fromYaml.
*/}}
{{- define "trustiot.dashboardCheck.podAnnotations" -}}
{{- dict "ambient.istio.io/dns-capture" "false" | toYaml -}}
{{- end -}}

{{/*
The env the chart sets for the dashboard check, after OTEL_SERVICE_NAME. The cleanup between
checks must cover the whole container (services/dashboard-check/README.md, 「威脅模型」 point 3),
so the service refuses to start unless it is its container's PID 1; env cannot change it.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.dashboardCheck.env" -}}
{{- list (dict "name" "TRUSTIOT__CLEANUP" "value" "container") | toYaml -}}
{{- end -}}

{{/*
Refuses settings of services.dashboard-check that would give its pod a way to the outside, a
secret or more privileges (see the comment at the top of this file). Renders nothing.
Argument: dict "root" $ "config" <effective settings>.
*/}}
{{- define "trustiot.dashboardCheck.checkConfig" -}}
{{- $config := .config -}}
{{- $why := "the pod runs pages a model wrote (services/dashboard-check/README.md, 「威脅模型」)" -}}
{{- /* The template renders none of these pod fields; refusing them keeps it that way should one
ever be passed through. */ -}}
{{- range $field, $what := dict "shareProcessNamespace" "would make another process PID 1, and the service cleans the whole container between checks only as PID 1" "hostPID" "would share the node's processes" "hostIPC" "would share the node's IPC objects" "hostNetwork" "would put the pod on the node's network, past its NetworkPolicy" -}}
{{- if hasKey $config $field -}}
{{- fail (printf "services.dashboard-check.%s must stay unset: it %s, and %s" $field $what $why) -}}
{{- end -}}
{{- end -}}
{{- $chartEnv := list -}}
{{- range include "trustiot.dashboardCheck.env" .root | fromYamlArray -}}
{{- $chartEnv = append $chartEnv (upper .name) -}}
{{- end -}}
{{- if $config.automountServiceAccountToken -}}
{{- fail (printf "services.dashboard-check.automountServiceAccountToken must be false: %s, and a token would let such a page call the Kubernetes API" $why) -}}
{{- end -}}
{{- if $config.credentials -}}
{{- fail (printf "services.dashboard-check.credentials must be empty: %s, which must not reach a Secret" $why) -}}
{{- end -}}
{{- range $config.env -}}
{{- if get (.valueFrom | default dict) "secretKeyRef" -}}
{{- fail (printf "services.dashboard-check: env %s must not read a Secret: %s" .name $why) -}}
{{- end -}}
{{- if regexMatch "(?i)^TRUSTIOT__TELEMETRY__OTLP_ENDPOINT$" .name -}}
{{- fail (printf "services.dashboard-check: env %s: the service sends no telemetry, because its pod has no egress; it logs to stdout only" .name) -}}
{{- end -}}
{{- if has (upper .name) $chartEnv -}}
{{- fail (printf "services.dashboard-check: env %s is set by the chart: the service cleans the whole container between checks, which it can only as the container's PID 1, and refuses to start otherwise" .name) -}}
{{- end -}}
{{- end -}}
{{- $pod := $config.podSecurityContext | default dict -}}
{{- $container := $config.securityContext | default dict -}}
{{- if not $pod.runAsNonRoot -}}
{{- fail (printf "services.dashboard-check.podSecurityContext.runAsNonRoot must be true: %s" $why) -}}
{{- end -}}
{{- if eq (toString (get $container "runAsNonRoot")) "false" -}}
{{- fail (printf "services.dashboard-check.securityContext.runAsNonRoot must not be false, which would override the pod's: %s" $why) -}}
{{- end -}}
{{- range $where, $context := dict "podSecurityContext" $pod "securityContext" $container -}}
{{- if and (hasKey $context "runAsUser") (eq (toString (get $context "runAsUser")) "0") -}}
{{- fail (printf "services.dashboard-check.%s.runAsUser must not be 0 (root): %s" $where $why) -}}
{{- end -}}
{{- end -}}
{{- range $where, $profile := dict "podSecurityContext" $pod.seccompProfile "securityContext" $container.seccompProfile -}}
{{- if and $profile (not (has $profile.type (list "RuntimeDefault" "Localhost"))) -}}
{{- fail (printf "services.dashboard-check.%s.seccompProfile.type must be RuntimeDefault or Localhost: %s, and Chromium runs without its own sandbox" $where $why) -}}
{{- end -}}
{{- end -}}
{{- if not (get ($pod.seccompProfile | default dict) "type") -}}
{{- fail (printf "services.dashboard-check.podSecurityContext.seccompProfile.type must be set (RuntimeDefault): %s" $why) -}}
{{- end -}}
{{- if or $container.allowPrivilegeEscalation $container.privileged (not $container.readOnlyRootFilesystem) -}}
{{- fail (printf "services.dashboard-check.securityContext must keep allowPrivilegeEscalation false, privileged unset and readOnlyRootFilesystem true: %s" $why) -}}
{{- end -}}
{{- $capabilities := $container.capabilities | default dict -}}
{{- if or (not (has "ALL" ($capabilities.drop | default list))) $capabilities.add -}}
{{- fail (printf "services.dashboard-check.securityContext.capabilities must drop ALL and add none: %s" $why) -}}
{{- end -}}
{{- end -}}
