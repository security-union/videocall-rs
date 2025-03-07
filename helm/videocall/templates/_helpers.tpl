{{/*
Expand the name of the chart.
*/}}
{{- define "videocall.name" -}}
{{- default .Chart.Name .Values.global.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "videocall.fullname" -}}
{{- if .Values.global.fullnameOverride }}
{{- .Values.global.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.global.nameOverride }}
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
Common labels
*/}}
{{- define "videocall.labels" -}}
helm.sh/chart: {{ include "videocall.chart" . }}
{{ include "videocall.selectorLabels" . }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "videocall.selectorLabels" -}}
app.kubernetes.io/name: {{ include "videocall.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Component-specific labels
*/}}
{{- define "videocall.componentLabels" -}}
{{ include "videocall.labels" . }}
app.kubernetes.io/component: {{ .component }}
{{- end }}

{{/*
Component-specific selector labels
*/}}
{{- define "videocall.componentSelectorLabels" -}}
{{ include "videocall.selectorLabels" . }}
app.kubernetes.io/component: {{ .component }}
{{- end }}

{{/*
Create the name of a component
*/}}
{{- define "videocall.componentName" -}}
{{- printf "%s-%s" (include "videocall.fullname" .root) .component | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "videocall.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "videocall.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Get the UI URL
*/}}
{{- define "videocall.uiUrl" -}}
{{- if .Values.global.urls.ui -}}
{{- .Values.global.urls.ui -}}
{{- else -}}
ui.{{ .Values.global.domain }}
{{- end -}}
{{- end -}}

{{/*
Get the API URL
*/}}
{{- define "videocall.apiUrl" -}}
{{- if .Values.global.urls.api -}}
{{- .Values.global.urls.api -}}
{{- else -}}
api.{{ .Values.global.domain }}
{{- end -}}
{{- end -}}

{{/*
Get the Transport URL
*/}}
{{- define "videocall.transportUrl" -}}
{{- if .Values.global.urls.transport -}}
{{- .Values.global.urls.transport -}}
{{- else -}}
transport.{{ .Values.global.domain }}
{{- end -}}
{{- end -}} 