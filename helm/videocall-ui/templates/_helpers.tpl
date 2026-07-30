{{/*
Expand the name of the chart.
*/}}
{{- define "videocall-ui.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "videocall-ui.fullname" -}}
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
{{- define "videocall-ui.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "videocall-ui.labels" -}}
helm.sh/chart: {{ include "videocall-ui.chart" . }}
{{ include "videocall-ui.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "videocall-ui.selectorLabels" -}}
app.kubernetes.io/name: {{ include "videocall-ui.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Build the CSP connect-src allow-list from runtimeConfig URL fields.
*/}}
{{- define "videocall-ui.cspConnectSrc" -}}
{{- $cfg := default dict .Values.runtimeConfig -}}
{{- $seen := dict -}}
{{- $origins := list -}}
{{- $_ := set $seen "'self'" true -}}
{{- $origins = append $origins "'self'" -}}
{{- range $raw := list (get $cfg "apiBaseUrl") (get $cfg "meetingApiBaseUrl") (get $cfg "searchApiBaseUrl") (get $cfg "jmapBaseUrl") -}}
  {{- $url := trim (toString (default "" $raw)) -}}
  {{- if $url -}}
    {{- $origin := regexFind "^[A-Za-z][A-Za-z0-9+.-]*://[A-Za-z0-9._:-]+" $url -}}
    {{- if and $origin (not (hasKey $seen $origin)) -}}
      {{- $_ := set $seen $origin true -}}
      {{- $origins = append $origins $origin -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- range $field := list "wsUrl" "webTransportHost" -}}
  {{- range $raw := splitList "," (toString (default "" (get $cfg $field))) -}}
    {{- $url := trim $raw -}}
    {{- if $url -}}
      {{- $origin := regexFind "^[A-Za-z][A-Za-z0-9+.-]*://[A-Za-z0-9._:-]+" $url -}}
      {{- if and $origin (not (hasKey $seen $origin)) -}}
        {{- $_ := set $seen $origin true -}}
        {{- $origins = append $origins $origin -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- $oauthEnabled := eq (lower (toString (default "" (get $cfg "oauthEnabled")))) "true" -}}
{{- $oauthFlow := lower (toString (default "" (get $cfg "oauthFlow"))) -}}
{{- if and $oauthEnabled (eq $oauthFlow "pkce") -}}
  {{- $tokenUrl := trim (toString (default "" (get $cfg "oauthTokenUrl"))) -}}
  {{- $issuer := trim (toString (default "" (get $cfg "oauthIssuer"))) -}}
  {{- $provider := lower (toString (default "" (get $cfg "oauthProvider"))) -}}
  {{- $idpUrl := $tokenUrl -}}
  {{- if and (not $idpUrl) (eq $provider "google") -}}
    {{- $idpUrl = "https://oauth2.googleapis.com/token" -}}
  {{- else if and (not $idpUrl) $issuer -}}
    {{- $idpUrl = $issuer -}}
  {{- end -}}
  {{- $origin := regexFind "^[A-Za-z][A-Za-z0-9+.-]*://[A-Za-z0-9._:-]+" $idpUrl -}}
  {{- if and $origin (not (hasKey $seen $origin)) -}}
    {{- $_ := set $seen $origin true -}}
    {{- $origins = append $origins $origin -}}
  {{- end -}}
{{- end -}}
{{- join " " $origins -}}
{{- end }}
