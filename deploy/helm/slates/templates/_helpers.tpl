{{/*
The chart's names and labels, and the fleet facts every template derives from `.Values.replicas`:
the node list, the fault tolerance, and the shared manifest (docs/cli.md "Deploying a fleet").
*/}}

{{- define "slates.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* The release name is the StatefulSet's name and every pod's prefix: pod N is `<fullname>-N`. */}}
{{- define "slates.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "slates.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "slates.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "slates.selectorLabels" -}}
app.kubernetes.io/name: {{ include "slates.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Derived: the fault tolerance f = ⌊(replicas − 1) / 2⌋ (integer division), so 2f + 1 ≤ replicas: a write
commits at f + 1 acknowledgements of 2f + 1 candidates and the fleet keeps a commit quorum through f deaths
(design §4.8; the manifest's `f`). One replica is f = 0, the laptop degenerate (R8).
*/}}
{{- define "slates.f" -}}
{{- $replicas := int .Values.replicas -}}
{{- if lt $replicas 1 -}}
{{- fail "replicas must be at least 1" -}}
{{- end -}}
{{- div (sub $replicas 1) 2 -}}
{{- end -}}

{{/* Pod N's name. */}}
{{- define "slates.podName" -}}
{{- printf "%s-%d" (include "slates.fullname" .root) (int .index) -}}
{{- end -}}

{{/*
Pod N's advertised address: its per-pod DNS name under the headless Service, and its base UDP port —
Derived: basePort + 2N (one block of two ports per node, `slates_server::deploy`).
*/}}
{{- define "slates.podAddress" -}}
{{- $pod := include "slates.podName" . -}}
{{- $port := add (int .root.Values.fleet.basePort) (mul 2 (int .index)) -}}
{{- printf "%s.%s.%s.svc.%s:%d" $pod (include "slates.fullname" .root) .root.Release.Namespace .root.Values.clusterDomain (int $port) -}}
{{- end -}}

{{/* The identity values for pod N, or a refusal naming what the operator must supply. */}}
{{- define "slates.identity" -}}
{{- $pod := include "slates.podName" . -}}
{{- $identity := index .root.Values.certificates $pod -}}
{{- if not $identity -}}
{{- fail (printf "certificates.%s is missing: supply its `certificate` and `key` (base64 DER), one identity per pod (docs/deploy.md); the KIND lane mints self-signed ones with `cargo xtask kind certs`" $pod) -}}
{{- end -}}
{{- if not $identity.certificate -}}
{{- fail (printf "certificates.%s.certificate is missing (base64 DER)" $pod) -}}
{{- end -}}
{{- if not $identity.key -}}
{{- fail (printf "certificates.%s.key is missing (base64 DER)" $pod) -}}
{{- end -}}
{{- end -}}

{{/*
The shared fleet manifest (docs/cli.md), rendered once from `replicas`: every node by name, its per-pod
DNS address and port block, its certificate file beside the manifest, and its key at the one path every
pod mounts its own key at (`../keys/node.key.der`: the pod's Secret alone, selected by the pod's name —
statefulset.yaml). Every pod reads this same file with its own `--node`, and only its own key, so every
node computes the same member ids and socket map (`slates_server::deploy`).
*/}}
{{- define "slates.manifest" -}}
{{- $root := . -}}
{{- $nodes := list -}}
{{- range $index := until (int .Values.replicas) -}}
{{- $pod := include "slates.podName" (dict "root" $root "index" $index) -}}
{{- $node := dict "node" $pod "address" (include "slates.podAddress" (dict "root" $root "index" $index)) "certificate" (printf "%s.crt.der" $pod) "key" "../keys/node.key.der" -}}
{{- $nodes = append $nodes $node -}}
{{- end -}}
{{- $manifest := dict "name" .Values.fleet.name "f" (include "slates.f" . | int) "nodes" $nodes -}}
{{- if .Values.fleet.durability -}}
{{- $_ := set $manifest "durability" .Values.fleet.durability -}}
{{- end -}}
{{- toPrettyJson $manifest -}}
{{- end -}}
