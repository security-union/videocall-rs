#!/bin/bash

# Production deployment script for videocall infrastructure
# Deploys WebSocket and WebTransport services to both Singapore and US East regions
#
# Usage: ./scripts/deploy-global-infrastructure.sh [--dry-run] [--skip-dependencies]
#
# Options:
#   --dry-run            Show what would be deployed without executing
#   --skip-dependencies  Skip helm dependency updates
#
# Authors: videocall-rs team
# Version: 1.0.0

set -euo pipefail

# Configuration
declare -r SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
declare -r PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
declare -r LOG_FILE="${PROJECT_ROOT}/deployment-$(date +%Y%m%d-%H%M%S).log"

# Kubernetes contexts
declare -r US_EAST_CONTEXT="do-nyc1-videocall-us-east"
declare -r SINGAPORE_CONTEXT="do-sgp1-videocall-singapore"

# Chart directories
declare -r HELM_DIR="${PROJECT_ROOT}/helm"
declare -a CHARTS=(
    "global/us-east/websocket"
    "global/us-east/webtransport"
    "global/singapore/websocket"
    "global/singapore/webtransport"
)

# Helper functions for chart configurations
get_context_for_chart() {
    case "$1" in
        "global/us-east/websocket"|"global/us-east/webtransport")
            echo "${US_EAST_CONTEXT}"
            ;;
        "global/singapore/websocket"|"global/singapore/webtransport")
            echo "${SINGAPORE_CONTEXT}"
            ;;
        *)
            echo "unknown"
            ;;
    esac
}

get_release_name_for_chart() {
    case "$1" in
        "global/us-east/websocket")
            echo "websocket-us-east"
            ;;
        "global/us-east/webtransport")
            echo "webtransport-us-east"
            ;;
        "global/singapore/websocket")
            echo "websocket-singapore"
            ;;
        "global/singapore/webtransport")
            echo "webtransport-singapore"
            ;;
        *)
            echo "unknown"
            ;;
    esac
}

# Command line options
DRY_RUN=false
SKIP_DEPENDENCIES=false

# Colors for output
declare -r RED='\033[0;31m'
declare -r GREEN='\033[0;32m'
declare -r YELLOW='\033[1;33m'
declare -r BLUE='\033[0;34m'
declare -r PURPLE='\033[0;35m'
declare -r NC='\033[0m' # No Color

# Logging functions
log() {
    local level="$1"
    shift
    local message="$*"
    local timestamp=$(date '+%Y-%m-%d %H:%M:%S')
    echo -e "${timestamp} [${level}] ${message}" | tee -a "${LOG_FILE}"
}

log_info() {
    log "INFO" "${BLUE}$*${NC}"
}

log_success() {
    log "SUCCESS" "${GREEN}$*${NC}"
}

log_warning() {
    log "WARNING" "${YELLOW}$*${NC}"
}

log_error() {
    log "ERROR" "${RED}$*${NC}"
}

log_section() {
    echo ""
    log "SECTION" "${PURPLE}=== $* ===${NC}"
    echo ""
}

# Error handling
error_exit() {
    log_error "$1"
    exit 1
}

# Parse command line arguments
parse_args() {
    while [[ $# -gt 0 ]]; do
        case $1 in
            --dry-run)
                DRY_RUN=true
                shift
                ;;
            --skip-dependencies)
                SKIP_DEPENDENCIES=true
                shift
                ;;
            -h|--help)
                show_help
                exit 0
                ;;
            *)
                error_exit "Unknown option: $1"
                ;;
        esac
    done
}

show_help() {
    cat << EOF
Production deployment script for videocall infrastructure

Usage: $0 [OPTIONS]

Options:
    --dry-run             Show what would be deployed without executing
    --skip-dependencies   Skip helm dependency updates
    -h, --help           Show this help message

This script will:
1. Update helm dependencies for all charts
2. Switch to appropriate Kubernetes contexts
3. Deploy WebSocket and WebTransport services to both regions
4. Verify deployments and collect load balancer IPs

Contexts used:
- US East: ${US_EAST_CONTEXT}
- Singapore: ${SINGAPORE_CONTEXT}

Charts deployed:
$(printf '- %s\n' "${CHARTS[@]}")
EOF
}

# Validation functions
validate_prerequisites() {
    log_section "Validating Prerequisites"
    
    # Check required tools
    local tools=("kubectl" "helm")
    for tool in "${tools[@]}"; do
        if ! command -v "$tool" &> /dev/null; then
            error_exit "$tool is not installed or not in PATH"
        fi
        log_info "✓ $tool is available"
    done
    
    # Check kubectl contexts
    local contexts=("${US_EAST_CONTEXT}" "${SINGAPORE_CONTEXT}")
    for context in "${contexts[@]}"; do
        if ! kubectl config get-contexts -o name | grep -q "^${context}$"; then
            error_exit "Kubernetes context '${context}' not found"
        fi
        log_info "✓ Context '${context}' is available"
    done
    
    # Check helm charts exist
    for chart in "${CHARTS[@]}"; do
        local chart_path="${HELM_DIR}/${chart}"
        if [[ ! -f "${chart_path}/Chart.yaml" ]]; then
            error_exit "Chart not found: ${chart_path}/Chart.yaml"
        fi
        log_info "✓ Chart '${chart}' exists"
    done
    
    log_success "All prerequisites validated"
}

# Update helm dependencies
update_dependencies() {
    if [[ "${SKIP_DEPENDENCIES}" == "true" ]]; then
        log_warning "Skipping dependency updates as requested"
        return 0
    fi
    
    log_section "Updating Helm Dependencies"
    
    for chart in "${CHARTS[@]}"; do
        local chart_path="${HELM_DIR}/${chart}"
        log_info "Updating dependencies for ${chart}"
        
        if [[ "${DRY_RUN}" == "true" ]]; then
            log_info "[DRY RUN] Would run: helm dependency update in ${chart_path}"
        else
            if ! (cd "${chart_path}" && helm dependency update); then
                error_exit "Failed to update dependencies for ${chart}"
            fi
            log_success "Dependencies updated for ${chart}"
        fi
    done
}

# Deploy a single chart
deploy_chart() {
    local chart="$1"
    local chart_path="${HELM_DIR}/${chart}"
    local context="$(get_context_for_chart "${chart}")"
    local release_name="$(get_release_name_for_chart "${chart}")"
    
    log_info "Deploying ${chart} to context ${context} as release ${release_name}"
    
    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "[DRY RUN] Would run:"
        log_info "  kubectl config use-context ${context}"
        log_info "  helm upgrade --install ${release_name} . -f values.yaml"
        return 0
    fi
    
    # Switch context
    if ! kubectl config use-context "${context}" >/dev/null 2>&1; then
        error_exit "Failed to switch to context ${context}"
    fi
    log_info "Switched to context: ${context}"
    
    # Deploy with helm
    if ! (cd "${chart_path}" && helm upgrade --install "${release_name}" . -f values.yaml --timeout 300s); then
        error_exit "Failed to deploy ${chart}"
    fi
    
    log_success "Successfully deployed ${chart}"
}

# Deploy all charts
deploy_all_charts() {
    log_section "Deploying All Charts"
    
    for chart in "${CHARTS[@]}"; do
        deploy_chart "${chart}"
        echo ""
    done
}

# Verify deployments and get load balancer IPs
verify_deployments() {
    if [[ "${DRY_RUN}" == "true" ]]; then
        log_info "[DRY RUN] Would verify deployments and collect IPs"
        return 0
    fi
    
    log_section "Verifying Deployments and Collecting Load Balancer IPs"
    
    # US East
    log_info "Switching to US East context"
    kubectl config use-context "${US_EAST_CONTEXT}" >/dev/null 2>&1
    
    echo ""
    log_info "US EAST REGION LOAD BALANCER IPs:"
    
    local ws_ip=$(kubectl get service websocket-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "pending")
    local wt_ip=$(kubectl get service webtransport-us-east-lb -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "pending")
    
    log_info "  WebSocket: ${ws_ip}"
    log_info "  WebTransport (QUIC): ${wt_ip}"
    
    # Singapore
    log_info "Switching to Singapore context"
    kubectl config use-context "${SINGAPORE_CONTEXT}" >/dev/null 2>&1
    
    echo ""
    log_info "SINGAPORE REGION LOAD BALANCER IPs:"
    
    local sg_ws_ip=$(kubectl get service websocket-singapore -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "pending")
    local sg_wt_ip=$(kubectl get service webtransport-singapore-lb -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "pending")
    
    log_info "  WebSocket: ${sg_ws_ip}"
    log_info "  WebTransport (QUIC): ${sg_wt_ip}"
    
    echo ""
    log_success "Deployment verification complete"
}

# Cleanup function
cleanup() {
    local exit_code=$?
    if [[ ${exit_code} -ne 0 ]]; then
        log_error "Script failed with exit code ${exit_code}"
        log_info "Check the log file: ${LOG_FILE}"
    fi
    exit ${exit_code}
}

# Main execution
main() {
    parse_args "$@"
    
    trap cleanup EXIT
    
    log_section "Starting Global Infrastructure Deployment"
    log_info "Log file: ${LOG_FILE}"
    log_info "Dry run mode: ${DRY_RUN}"
    log_info "Skip dependencies: ${SKIP_DEPENDENCIES}"
    
    validate_prerequisites
    update_dependencies
    deploy_all_charts
    verify_deployments
    
    echo ""
    log_success "Global infrastructure deployment completed successfully!"
    log_info "Log file saved: ${LOG_FILE}"
}

# Run main function with all arguments
main "$@" 