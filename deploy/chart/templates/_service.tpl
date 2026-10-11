{{/*
Generic TrustIoT service: ServiceAccount, Service and Deployment.
Every template takes dict "root" $ "name" <service> "config" <effective settings>,
where the settings come from "trustiot.serviceConfig".
*/}}

{{- define "trustiot.service.serviceAccount" -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include "trustiot.componentName" . }}
  labels:
    {{- include "trustiot.componentLabels" . | nindent 4 }}
automountServiceAccountToken: {{ .config.automountServiceAccountToken }}
{{- end -}}

{{- define "trustiot.service.service" -}}
apiVersion: v1
kind: Service
metadata:
  name: {{ include "trustiot.componentName" . }}
  labels:
    {{- include "trustiot.componentLabels" . | nindent 4 }}
spec:
  # Always ClusterIP: services, and above all the MQTT broker, are never exposed directly.
  # Outside traffic enters only through the ingress gateway (decisions 6 and 27).
  type: ClusterIP
  selector:
    {{- include "trustiot.selectorLabels" . | nindent 4 }}
  ports:
    {{- range .config.ports }}
    - name: {{ .name }}
      port: {{ .servicePort | default .containerPort }}
      targetPort: {{ .name }}
      protocol: {{ .protocol | default "TCP" }}
      {{- with .appProtocol }}
      appProtocol: {{ . }}
      {{- end }}
    {{- end }}
{{- end -}}

{{/*
Container env: the chart's variables first (OTEL_SERVICE_NAME, SENTRY_DSN, the credentials the
service lists and, for identity, its routes and signing key file), then serviceDefaults.env and
services.<name>.env.
Custom env cannot take over what the chart controls:
- names starting with TRUSTIOT_ plus a letter or digit, SENTRY_ or OTEL_ are reserved, so the
  platform settings from the ConfigMap (envFrom loses to env) and the chart's Secrets stay as set;
- serviceDefaults.env reaches every service, so it must not read Secrets;
- secret-like names (as in values.schema.json) come from valueFrom.secretKeyRef only;
- dependency credentials reach a service only through services.<name>.credentials.
Returns a YAML list; use with fromYamlArray.
*/}}
{{- define "trustiot.service.env" -}}
{{- $name := .name -}}
{{- $config := .config -}}
{{- $values := .root.Values -}}
{{- range $values.serviceDefaults.env -}}
{{- if get (.valueFrom | default dict) "secretKeyRef" -}}
{{- fail (printf "serviceDefaults.env: %s must not read a Secret, because every service gets serviceDefaults.env; set it under services.<name>.env" .name) -}}
{{- end -}}
{{- end -}}
{{- $protected := include "trustiot.dependencySecretRefs" .root | fromYamlArray -}}
{{- range $config.env -}}
{{- if regexMatch "(?i)^(TRUSTIOT_[A-Z0-9]|SENTRY_|OTEL_)" .name -}}
{{- fail (printf "services.%s: env %s: names starting with TRUSTIOT_, SENTRY_ or OTEL_ are set by the chart" $name .name) -}}
{{- end -}}
{{- if regexMatch "(?i)(secret|passw|passphrase|pwd|token|api_?key|(^|_)key$|dsn|private|credential)" .name -}}
{{- $from := .valueFrom | default dict -}}
{{- if or (hasKey . "value") (ne (len (keys $from)) 1) (not (hasKey $from "secretKeyRef")) -}}
{{- fail (printf "services.%s: env %s looks like a secret, so it must come from valueFrom.secretKeyRef only" $name .name) -}}
{{- end -}}
{{- end -}}
{{- with get (.valueFrom | default dict) "secretKeyRef" -}}
{{- if has (printf "%s/%s" .name .key) $protected -}}
{{- fail (printf "services.%s: env reads the dependency credential %s/%s; list the dependency under services.%s.credentials instead" $name .name .key $name) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $env := list (dict "name" "OTEL_SERVICE_NAME" "value" $name) -}}
{{- with $values.observability.sentry -}}
{{- if .enabled -}}
{{- $env = append $env (dict "name" "SENTRY_DSN" "valueFrom" (dict "secretKeyRef" (dict "name" .dsnSecret.name "key" .dsnSecret.key))) -}}
{{- end -}}
{{- end -}}
{{- $credentialEnv := include "trustiot.credentialEnv" (dict "root" .root "name" $name "credentials" ($config.credentials | default list)) | fromYamlArray -}}
{{- $identityEnv := list -}}
{{- if eq $name "identity" -}}
{{- $identityEnv = include "trustiot.identity.env" .root | fromYamlArray -}}
{{- end -}}
{{- $env = concat $env $credentialEnv $identityEnv ($config.env | default list) -}}
{{- $names := list -}}
{{- range $env -}}
{{- $names = append $names .name -}}
{{- end -}}
{{- if ne (len $names) (len (uniq $names)) -}}
{{- fail (printf "services.%s: env names must be unique (%s)" $name (join ", " $names)) -}}
{{- end -}}
{{- toYaml $env -}}
{{- end -}}

{{- define "trustiot.service.deployment" -}}
{{- $config := .config -}}
{{- $preStop := int $config.preStopSleepSeconds -}}
{{- /* Kubernetes counts the preStop sleep in the grace period, so add it to the drain time. */ -}}
{{- $grace := add $preStop (int $config.drainSeconds) -}}
{{- if not $config.probes.liveness -}}
{{- fail (printf "services.%s: probes.liveness is required (docs/architecture/deployment.md)" .name) -}}
{{- end -}}
{{- if not $config.probes.readiness -}}
{{- fail (printf "services.%s: probes.readiness is required (docs/architecture/deployment.md)" .name) -}}
{{- end -}}
{{- $portNames := list -}}
{{- $portNumbers := list -}}
{{- range $config.ports -}}
{{- $portNames = append $portNames .name -}}
{{- $portNumbers = append $portNumbers (int .containerPort) -}}
{{- end -}}
{{- if or (ne (len $portNames) (len (uniq $portNames))) (ne (len $portNumbers) (len (uniq $portNumbers))) -}}
{{- fail (printf "services.%s: port names and container ports must be unique" .name) -}}
{{- end -}}
{{- range $kind, $probe := $config.probes -}}
{{- range $handler := list "httpGet" "tcpSocket" "grpc" -}}
{{- $port := (get $probe $handler | default dict).port -}}
{{- if and (kindIs "string" $port) (not (has $port $portNames)) -}}
{{- fail (printf "services.%s: probes.%s uses port %q, which is not in ports" $.name $kind $port) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $env := include "trustiot.service.env" . | fromYamlArray -}}
{{- $caMounts := include "trustiot.caMounts" .root | fromYamlArray -}}
{{- /* The internal identity signing key reaches the identity service only (decision 27). */ -}}
{{- $signingKey := dict -}}
{{- if and (eq .name "identity") .root.Values.internalIdentity.signingKeySecret.name -}}
{{- $signingKey = .root.Values.internalIdentity.signingKeySecret -}}
{{- end -}}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ include "trustiot.componentName" . }}
  labels:
    {{- include "trustiot.componentLabels" . | nindent 4 }}
spec:
  replicas: {{ $config.replicas }}
  revisionHistoryLimit: 3
  strategy:
    type: RollingUpdate
    rollingUpdate:
      # Start the new pod before stopping the old one, so a single replica rolls without downtime.
      maxUnavailable: 0
      maxSurge: 1
  selector:
    matchLabels:
      {{- include "trustiot.selectorLabels" . | nindent 6 }}
  template:
    metadata:
      {{- /* The chart's own keys win over podLabels and podAnnotations (sprig merge keeps the
      first value), so the selector labels and the checksum cannot be overridden. */}}
      labels:
        {{- $labels := include "trustiot.componentLabels" . | fromYaml }}
        {{- /* Istio ambient: ztunnel adds mTLS to every connection of this pod (decision 27). */}}
        {{- $_ := set $labels "istio.io/dataplane-mode" "ambient" }}
        {{- toYaml (merge $labels ($config.podLabels | default dict)) | nindent 8 }}
      annotations:
        {{- /* Restart pods when the platform settings they read through envFrom change. */}}
        {{- $checksum := include (print .root.Template.BasePath "/platform-config.yaml") .root | sha256sum }}
        {{- $annotations := dict "checksum/platform-config" $checksum }}
        {{- toYaml (merge $annotations ($config.podAnnotations | default dict)) | nindent 8 }}
    spec:
      serviceAccountName: {{ include "trustiot.componentName" . }}
      automountServiceAccountToken: {{ $config.automountServiceAccountToken }}
      enableServiceLinks: false
      terminationGracePeriodSeconds: {{ $grace }}
      securityContext:
        {{- toYaml $config.podSecurityContext | nindent 8 }}
      {{- with .root.Values.global.imagePullSecrets }}
      imagePullSecrets:
        {{- range . }}
        - name: {{ . }}
        {{- end }}
      {{- end }}
      {{- with $config.nodeSelector }}
      nodeSelector:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $config.tolerations }}
      tolerations:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with $config.affinity }}
      affinity:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      containers:
        - name: {{ .name }}
          image: {{ include "trustiot.serviceImage" (dict "root" .root "name" .name "image" $config.image) }}
          imagePullPolicy: {{ $config.image.pullPolicy }}
          ports:
            {{- range $config.ports }}
            - name: {{ .name }}
              containerPort: {{ .containerPort }}
              protocol: {{ .protocol | default "TCP" }}
            {{- end }}
          envFrom:
            - configMapRef:
                name: {{ include "trustiot.platformConfigName" .root }}
          env:
            {{- toYaml $env | nindent 12 }}
          {{- with $config.probes.startup }}
          startupProbe:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          livenessProbe:
            {{- toYaml $config.probes.liveness | nindent 12 }}
          readinessProbe:
            {{- toYaml $config.probes.readiness | nindent 12 }}
          {{- if gt $preStop 0 }}
          lifecycle:
            preStop:
              sleep:
                seconds: {{ $preStop }}
          {{- end }}
          resources:
            {{- toYaml $config.resources | nindent 12 }}
          securityContext:
            {{- toYaml $config.securityContext | nindent 12 }}
          volumeMounts:
            - name: tmp
              mountPath: /tmp
            {{- range $caMounts }}
            - name: {{ .volume }}
              mountPath: {{ .dir }}
              readOnly: true
            {{- end }}
            {{- if $signingKey }}
            - name: identity-signing-key
              mountPath: {{ include "trustiot.identity.signingKeyDir" .root }}
              readOnly: true
            {{- end }}
      volumes:
        - name: tmp
          emptyDir:
            sizeLimit: {{ $config.tmpSizeLimit }}
        {{- range $caMounts }}
        - name: {{ .volume }}
          secret:
            secretName: {{ .secret }}
            items:
              - key: {{ .key }}
                path: ca.crt
        {{- end }}
        {{- if $signingKey }}
        - name: identity-signing-key
          secret:
            secretName: {{ $signingKey.name }}
            items:
              - key: {{ $signingKey.key }}
                path: {{ include "trustiot.identity.signingKeyFile" .root }}
        {{- end }}
{{- end -}}
