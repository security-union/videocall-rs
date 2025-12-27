{{/*
Expand the name of the chart.
*/}}
{{- define "videocall.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "videocall.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "videocall.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a component-specific fullname.
Usage: include "videocall.componentFullname" (dict "root" $ "component" "ui")
This creates names like: videocall-ui, videocall-websocket, etc.
*/}}
{{- define "videocall.componentFullname" -}}
{{- $componentConfig := index .root.Values .component }}
{{- if $componentConfig.fullnameOverride }}
{{- $componentConfig.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else if $componentConfig.nameOverride }}
{{- $componentConfig.nameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" (include "videocall.fullname" .root) .component | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}

{{/*
Common labels - includes component label for proper resource grouping.
Usage: include "videocall.labels" (dict "root" $ "component" "ui")
*/}}
{{- define "videocall.labels" -}}
helm.sh/chart: {{ include "videocall.chart" .root }}
{{ include "videocall.selectorLabels" . }}
{{- if .root.Chart.AppVersion }}
app.kubernetes.io/version: {{ .root.Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .root.Release.Service }}
{{- end }}

{{/*
Selector labels - CRITICAL: includes component label to prevent selector collisions.
This ensures each service only routes to its own component's pods.
Usage: include "videocall.selectorLabels" (dict "root" $ "component" "ui")

Example output:
  app.kubernetes.io/name: videocall
  app.kubernetes.io/instance: my-release
  app.kubernetes.io/component: ui
*/}}
{{- define "videocall.selectorLabels" -}}
app.kubernetes.io/name: {{ include "videocall.name" .root }}
app.kubernetes.io/instance: {{ .root.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end }}

{{/*
Create the name of the service account to use.
*/}}
{{- define "videocall.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "videocall.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Generate the config.js content for the UI component.
This is injected as a ConfigMap and mounted into the UI container.
*/}}
{{- define "videocall.ui.configjs" -}}
window.CONFIG = {
  API_BASE_URL: {{ .Values.ui.runtimeConfig.apiBaseUrl | quote }},
  WS_URL: {{ .Values.ui.runtimeConfig.wsUrl | quote }},
  WEB_TRANSPORT_HOST: {{ .Values.ui.runtimeConfig.webTransportHost | quote }},
  OAUTH_ENABLED: {{ .Values.ui.runtimeConfig.oauthEnabled | quote }},
  E2EE_ENABLED: {{ .Values.ui.runtimeConfig.e2eeEnabled | quote }},
  WEB_TRANSPORT_ENABLED: {{ .Values.ui.runtimeConfig.webTransportEnabled | quote }},
  USERS_ALLOWED_TO_STREAM: {{ .Values.ui.runtimeConfig.usersAllowedToStream | quote }},
  SERVER_ELECTION_PERIOD_MS: {{ .Values.ui.runtimeConfig.serverElectionPeriodMs }},
  AUDIO_BITRATE_KBPS: {{ .Values.ui.runtimeConfig.audioBitrateKbps }},
  VIDEO_BITRATE_KBPS: {{ .Values.ui.runtimeConfig.videoBitrateKbps }},
  SCREEN_BITRATE_KBPS: {{ .Values.ui.runtimeConfig.screenBitrateKbps }}
};
{{- end }}

