#!/usr/bin/env bash
set -euo pipefail

# PR Preview Slot Manager for HCL K3s
# Checks slot usage and optionally undeploys a slot.
#
# Prerequisites:
#   - kubectl configured for the HCL K3s daily-deployment cluster.
#     The kubeconfig is read from $KUBECONFIG (defaults to ~/vc-k3s-config.yaml).
#   - gh CLI installed and authenticated to github01.hclpnp.com.
#     Run `gh auth login --hostname github01.hclpnp.com` if not yet set up.

KUBECONFIG="${KUBECONFIG:-$HOME/vc-k3s-config.yaml}"
export KUBECONFIG

GH_HOST="github01.hclpnp.com"
GH_REPO="labs-projects/videocall"
PREVIEW_INFRA_NS="preview-infra"
URL_PATTERN="pr%s.preview.videocall.fnxlabs.com"

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

List HCL PR Preview deployment slots and their status.

Options:
  --undeploy SLOT   Undeploy the specified slot number
  -h, --help        Show this help message

Examples:
  $(basename "$0")              # List all active slots
  $(basename "$0") --undeploy 2 # Undeploy slot 2
EOF
    exit 0
}

undeploy_slot=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --undeploy)
            undeploy_slot="$2"
            shift 2
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage
            ;;
    esac
done

EXPECTED_NODE="videocall.videocall.fnxlabs.com"

check_cluster() {
    # Verify kubectl can reach the cluster and exactly the expected node is present.
    local nodes
    if ! nodes=$(kubectl get nodes --no-headers -o custom-columns=NAME:.metadata.name 2>/dev/null); then
        echo "Error: Unable to reach the Kubernetes cluster." >&2
        echo "  KUBECONFIG=${KUBECONFIG}" >&2
        echo "  Do you have the right kubeconfig? Expected the HCL K3s daily-deployment cluster." >&2
        exit 1
    fi

    local node_count
    node_count=$(echo "$nodes" | grep -c . || true)

    if [[ "$node_count" -ne 1 ]] || [[ "$(echo "$nodes" | tr -d '[:space:]')" != "$EXPECTED_NODE" ]]; then
        echo "Error: Expected exactly one node ($EXPECTED_NODE) but found:" >&2
        echo "$nodes" >&2
        echo "  KUBECONFIG=${KUBECONFIG}" >&2
        echo "  Do you have the right kubeconfig? Expected the HCL K3s daily-deployment cluster." >&2
        exit 1
    fi

    # Verify the preview-infra namespace exists.
    if ! kubectl get namespace "$PREVIEW_INFRA_NS" &>/dev/null; then
        echo "Error: Namespace '$PREVIEW_INFRA_NS' does not exist." >&2
        echo "  KUBECONFIG=${KUBECONFIG}" >&2
        echo "  Do you have the right kubeconfig? Expected the HCL K3s daily-deployment cluster." >&2
        exit 1
    fi
}

list_slots() {
    local namespaces
    namespaces=$(kubectl get namespaces -l app=preview -o json 2>/dev/null)

    local count
    count=$(echo "$namespaces" | jq '.items | length')

    if [[ "$count" -eq 0 ]]; then
        echo "No active PR preview slots."
        return
    fi

    printf "\n%-6s %-8s %-50s %-20s %s\n" "SLOT" "PR#" "URL" "AUTHOR" "TITLE"
    printf "%-6s %-8s %-50s %-20s %s\n" "----" "---" "---" "------" "-----"

    echo "$namespaces" | jq -r '.items[] | [
        (.metadata.labels.slot // "?"),
        (.metadata.labels.pr // "?"),
        .metadata.name
    ] | @tsv' | sort -t$'\t' -k1 -n | while IFS=$'\t' read -r slot pr _ns; do
        local url
        url=$(printf "$URL_PATTERN" "$slot")

        local title="" author=""
        if [[ "$pr" != "?" ]]; then
            local pr_info
            if pr_info=$(GH_HOST="$GH_HOST" gh pr view "$pr" --repo "$GH_REPO" --json title,author 2>/dev/null); then
                title=$(echo "$pr_info" | jq -r '.title // ""')
                author=$(echo "$pr_info" | jq -r '.author.login // ""')
            else
                title="(unable to fetch)"
                author="?"
            fi
        fi

        local link="https://$url"
        printf "%-6s %-8s %-50s %-20s %s\n" "$slot" "#$pr" "$link" "$author" "$title"
    done

    echo ""
    echo "Slots in use: $count"
}

do_undeploy() {
    local slot="$1"
    local ns="preview-slot-${slot}"

    # Verify the namespace exists
    if ! kubectl get namespace "$ns" &>/dev/null; then
        echo "Error: Namespace '$ns' does not exist. Slot $slot is not active." >&2
        exit 1
    fi

    # Show what we're about to undeploy
    local pr
    pr=$(kubectl get namespace "$ns" -o jsonpath='{.metadata.labels.pr}' 2>/dev/null || echo "?")
    echo "Undeploying slot $slot (PR #$pr, namespace: $ns)"

    # Confirm
    read -rp "Are you sure? This will delete the namespace and drop the database. [y/N] " confirm
    if [[ "$confirm" != [yY] ]]; then
        echo "Aborted."
        exit 0
    fi

    echo "Deleting namespace $ns..."
    kubectl delete namespace "$ns" --wait=false

    echo "Dropping database preview_slot_${slot}..."
    local pg_pod
    pg_pod=$(kubectl get pods -n "$PREVIEW_INFRA_NS" -l app=postgres -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)

    if [[ -n "$pg_pod" ]]; then
        kubectl exec -n "$PREVIEW_INFRA_NS" "$pg_pod" -- \
            psql -U postgres -c "DROP DATABASE IF EXISTS preview_slot_${slot};" 2>/dev/null \
            && echo "Database dropped." \
            || echo "Warning: Failed to drop database. You may need to clean it up manually."
    else
        echo "Warning: No postgres pod found in $PREVIEW_INFRA_NS. Database not dropped."
    fi

    echo "Slot $slot undeployed."
}

check_cluster

if [[ -n "$undeploy_slot" ]]; then
    do_undeploy "$undeploy_slot"
else
    list_slots
fi
