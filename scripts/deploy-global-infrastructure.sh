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
    # Deploy ingress controllers FIRST (required for UDP routing and HTTP ingresses)
    "global/us-east/ingress-nginx"
    "global/singapore/ingress-nginx"
    # Then deploy services that depend on ingress
    "global/us-east/websocket"
    "global/us-east/webtransport"
    "global/singapore/websocket"
    "global/singapore/webtransport"
    # Additional services deployed to US East cluster for consolidation
    "global/us-east/engineering-vlog"
    "global/us-east/matomo"
    "global/us-east/rustlemania-ui"
    "global/us-east/rustlemania-ui-staging" 
    "global/us-east/videocall-website"
    # Monitoring infrastructure
    "global/us-east/prometheus"
    "global/us-east/grafana"
    "global/us-east/metrics-api"
)

# Infrastructure components that need to be deployed first
declare -a CERT_MANAGER_COMPONENTS=(
    "cert-manager"
    "cert-manager-issuer"
)

# Certificate resources (deployed after cert-manager)
declare -a CERTIFICATE_FILES=(
    "global/singapore/websocket/certificate.yaml"
    "global/singapore/webtransport/certificate.yaml" 
    "global/us-east/websocket/certificate.yaml"
    "global/us-east/webtransport/certificate.yaml"
    # Additional certificates for consolidated services
    "global/us-east/engineering-vlog/certificate.yaml"
    "global/us-east/matomo/certificate.yaml"
    "global/us-east/rustlemania-ui/certificate.yaml"
    "global/us-east/rustlemania-ui-staging/certificate.yaml"
    "global/us-east/videocall-website/certificate.yaml"
    "global/us-east/grafana/certificate.yaml"
)

# DigitalOcean DNS secret file
declare -r DIGITALOCEAN_SECRET_FILE="${HELM_DIR}/digital-ocean-secret/digitalocean-dns.yaml"

# Region filtering functions
get_charts_for_region() {
    local region="$1"
    case "$region" in
        "all")
            printf '%s\n' "${CHARTS[@]}"
            ;;
        "us-east")
            # Include global/us-east charts and global chart deployments for US East
            printf '%s\n' "${CHARTS[@]}" | grep "global/us-east"
            ;;
        "singapore")
            printf '%s\n' "${CHARTS[@]}" | grep "global/singapore"
            ;;
        *)
            error_exit "Invalid region: $region"
            ;;
    esac
}

get_contexts_for_region() {
    local region="$1"
    case "$region" in
        "all")
            echo "${US_EAST_CONTEXT} ${SINGAPORE_CONTEXT}"
            ;;
        "us-east")
            echo "${US_EAST_CONTEXT}"
            ;;
        "singapore")
            echo "${SINGAPORE_CONTEXT}"
            ;;
        *)
            error_exit "Invalid region: $region"
            ;;
    esac
}

get_certificate_files_for_region() {
    local region="$1"
    case "$region" in
        "all")
            printf '%s\n' "${CERTIFICATE_FILES[@]}"
            ;;
        "us-east")
            printf '%s\n' "${CERTIFICATE_FILES[@]}" | grep "us-east"
            ;;
        "singapore")
            printf '%s\n' "${CERTIFICATE_FILES[@]}" | grep "singapore"
            ;;
        *)
            error_exit "Invalid region: $region"
            ;;
    esac
}

# Helper functions for chart configurations
get_context_for_chart() {
    case "$1" in
        "global/us-east/websocket"|"global/us-east/webtransport")
            echo "${US_EAST_CONTEXT}"
            ;;
        "global/singapore/websocket"|"global/singapore/webtransport")
            echo "${SINGAPORE_CONTEXT}"
            ;;
        "global/us-east/ingress-nginx")
            echo "${US_EAST_CONTEXT}"
            ;;
        "global/singapore/ingress-nginx")
            echo "${SINGAPORE_CONTEXT}"
            ;;
        # Additional services deployed to US East for consolidation
        "global/us-east/engineering-vlog"|"global/us-east/matomo"|"global/us-east/rustlemania-ui"|"global/us-east/rustlemania-ui-staging"|"global/us-east/videocall-website"|"global/us-east/prometheus"|"global/us-east/grafana")
            echo "${US_EAST_CONTEXT}"
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
        "global/us-east/ingress-nginx")
            echo "ingress-nginx-us-east"
            ;;
        "global/singapore/ingress-nginx")
            echo "ingress-nginx-singapore"
            ;;
        # Additional services deployed to US East for consolidation
        "global/us-east/engineering-vlog")
            echo "engineering-vlog-us-east"
            ;;
        "global/us-east/matomo")
            echo "matomo-us-east"
            ;;
        "global/us-east/rustlemania-ui")
            echo "videocall-ui-us-east"
            ;;
        "global/us-east/rustlemania-ui-staging")
            echo "videocall-staging-ui-us-east"
            ;;
        "global/us-east/videocall-website")
            echo "videocall-website-us-east"
            ;;
        "global/us-east/prometheus")
            echo "prometheus-us-east"
            ;;
        "global/us-east/grafana")
            echo "grafana-us-east"
            ;;
        *)
            echo "unknown"
            ;;
    esac
}

# Command line options
DRY_RUN=false
SKIP_DEPENDENCIES=false
DEPLOY_REGION="all"  # Options: all, us-east, singapore

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
            --region)
                if [[ -n "$2" && "$2" != --* ]]; then
                    case "$2" in
                        all|us-east|singapore)
                            DEPLOY_REGION="$2"
                            shift 2
                            ;;
                        *)
                            error_exit "Invalid region: $2. Must be one of: all, us-east, singapore"
                            ;;
                    esac
                else
                    error_exit "--region requires a value (all, us-east, or singapore)"
                fi
                ;;
            --us-east-only)
                DEPLOY_REGION="us-east"
                shift
                ;;
            --singapore-only)
                DEPLOY_REGION="singapore"
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
    --dry-run                Show what would be deployed without executing
    --skip-dependencies      Skip helm dependency updates
    --region REGION          Deploy to specific region (all, us-east, singapore)
    --us-east-only           Deploy only to US East region (shortcut for --region us-east)
    --singapore-only         Deploy only to Singapore region (shortcut for --region singapore)
    -h, --help              Show this help message

This script will:
1. Deploy DigitalOcean DNS secret to selected clusters (for Let's Encrypt DNS validation)
2. Deploy cert-manager and certificate issuer (Let's Encrypt)
3. Update helm dependencies for selected charts
4. Deploy SSL certificates for selected endpoints
5. Deploy WebSocket and WebTransport services to selected regions
6. Deploy consolidated services to US East (website, engineering blog, matomo, videocall-ui, videocall-staging-ui, videocall-website)
7. Verify deployments and check certificate status

Contexts used:
- US East: ${US_EAST_CONTEXT}
- Singapore: ${SINGAPORE_CONTEXT}

Available charts:
$(for chart in "${CHARTS[@]}"; do echo "- $chart"; done)

Examples:
    $0                        # Deploy to all regions
    $0 --region us-east       # Deploy only to US East
    $0 --singapore-only       # Deploy only to Singapore
    $0 --dry-run --region singapore  # Show what would be deployed to Singapore
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
    
    # Check kubectl contexts for selected region
    local contexts=($(get_contexts_for_region "${DEPLOY_REGION}"))
    for context in "${contexts[@]}"; do
        if ! kubectl config get-contexts -o name | grep -q "^${context}$"; then
            error_exit "Kubernetes context '${context}' not found"
        fi
        log_info "✓ Context '${context}' is available"
    done
    
    # Check helm charts exist for selected region
    local charts=($(get_charts_for_region "${DEPLOY_REGION}"))
    for chart in "${charts[@]}"; do
        local chart_path="${HELM_DIR}/${chart}"
        
        if [[ ! -f "${chart_path}/Chart.yaml" ]]; then
            error_exit "Chart not found: ${chart_path}/Chart.yaml"
        fi
        log_info "✓ Chart '${chart}' exists"
    done
    
    # Check cert-manager components exist
    for component in "${CERT_MANAGER_COMPONENTS[@]}"; do
        local component_path="${HELM_DIR}/${component}"
        if [[ "$component" == "cert-manager-issuer" ]]; then
            if [[ ! -f "${component_path}/cert-manager-issuer.yaml" ]]; then
                error_exit "Cert-manager issuer not found: ${component_path}/cert-manager-issuer.yaml"
            fi
            log_info "✓ Cert-manager issuer exists"
        fi
    done
    
    # Check certificate files exist for selected region
    local cert_files=($(get_certificate_files_for_region "${DEPLOY_REGION}"))
    for cert_file in "${cert_files[@]}"; do
        local cert_path="${HELM_DIR}/${cert_file}"
        if [[ ! -f "${cert_path}" ]]; then
            error_exit "Certificate file not found: ${cert_path}"
        fi
        log_info "✓ Certificate file '$(basename ${cert_file})' exists"
    done
    
    # Check DigitalOcean DNS secret file exists
    if [[ ! -f "${DIGITALOCEAN_SECRET_FILE}" ]]; then
        error_exit "DigitalOcean DNS secret file not found: ${DIGITALOCEAN_SECRET_FILE}

To create this secret file:
1. Get your DigitalOcean API token from https://cloud.digitalocean.com/account/api/tokens
2. Base64 encode the token: echo -n 'dop_v1_YOUR_TOKEN_HERE' | base64
3. Create the file at ${DIGITALOCEAN_SECRET_FILE} with this content:

apiVersion: v1
kind: Secret
metadata:
  name: digitalocean-dns
type: Opaque
data:
  access-token: YOUR_BASE64_ENCODED_TOKEN_HERE

Replace YOUR_BASE64_ENCODED_TOKEN_HERE with the base64 encoded token from step 2."
    fi
    log_info "✓ DigitalOcean DNS secret file exists"
    
    log_success "All prerequisites validated"
}

# Update helm dependencies
update_dependencies() {
    if [[ "${SKIP_DEPENDENCIES}" == "true" ]]; then
        log_warning "Skipping dependency updates as requested"
        return 0
    fi
    
    log_section "Updating Helm Dependencies"
    
    local charts=($(get_charts_for_region "${DEPLOY_REGION}"))
    
    # Add required helm repositories first
    local repos_added=false
    for chart in "${charts[@]}"; do
        if [[ "${chart}" == "global/us-east/matomo" ]] && [[ "${DRY_RUN}" == "false" ]]; then
            if ! helm repo list | grep -q bitnami; then
                log_info "Adding bitnami helm repository for ${chart}"
                helm repo add bitnami https://charts.bitnami.com/bitnami
                repos_added=true
            fi
        fi
        
        if [[ "${chart}" == "global/us-east/grafana" ]] && [[ "${DRY_RUN}" == "false" ]]; then
            if ! helm repo list | grep -q grafana; then
                log_info "Adding grafana helm repository for ${chart}"
                helm repo add grafana https://grafana.github.io/helm-charts
                repos_added=true
            fi
        fi
        
        if [[ "${chart}" == "global/us-east/prometheus" ]] && [[ "${DRY_RUN}" == "false" ]]; then
            if ! helm repo list | grep -q prometheus-community; then
                log_info "Adding prometheus-community helm repository for ${chart}"
                helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
                repos_added=true
            fi
        fi
    done
    
    # Update repositories if any were added
    if [[ "${repos_added}" == "true" ]] && [[ "${DRY_RUN}" == "false" ]]; then
        log_info "Updating helm repositories"
        helm repo update
    fi
    
    # Update chart dependencies
    for chart in "${charts[@]}"; do
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
    
    # Standard local chart deployment for all regional charts
    if ! (cd "${chart_path}" && helm upgrade --install "${release_name}" . -f values.yaml --timeout 300s); then
        error_exit "Failed to deploy ${chart}"
    fi
    
    log_success "Successfully deployed ${chart}"
}

# Deploy DigitalOcean DNS secret to selected clusters
deploy_digitalocean_secret() {
    log_section "Deploying DigitalOcean DNS Secret"
    
    local contexts=($(get_contexts_for_region "${DEPLOY_REGION}"))
    
    for context in "${contexts[@]}"; do
        log_info "Deploying DigitalOcean DNS secret to ${context}"
        
        if [[ "${DRY_RUN}" == "true" ]]; then
            log_info "[DRY RUN] Would deploy DigitalOcean DNS secret to ${context}"
            continue
        fi
        
        # Switch context
        if ! kubectl config use-context "${context}" >/dev/null 2>&1; then
            error_exit "Failed to switch to context ${context}"
        fi
        
        # Check if secret already exists
        if kubectl get secret digitalocean-dns >/dev/null 2>&1; then
            log_info "DigitalOcean DNS secret already exists in ${context}, skipping"
            continue
        fi
        
        # Deploy the secret
        if ! kubectl apply -f "${DIGITALOCEAN_SECRET_FILE}"; then
            error_exit "Failed to deploy DigitalOcean DNS secret to ${context}"
        fi
        
        log_success "Successfully deployed DigitalOcean DNS secret to ${context}"
    done
}

# Deploy cert-manager infrastructure
deploy_cert_manager() {
    log_section "Deploying Certificate Manager Infrastructure"
    
    local contexts=($(get_contexts_for_region "${DEPLOY_REGION}"))
    
    # Deploy cert-manager to selected clusters
    log_info "Will deploy cert-manager to contexts: ${contexts[*]}"
    
    for component in "${CERT_MANAGER_COMPONENTS[@]}"; do
        local component_path="${HELM_DIR}/${component}"
        log_info "Deploying ${component}"
        
        if [[ "${DRY_RUN}" == "true" ]]; then
            log_info "[DRY RUN] Would deploy cert-manager component: ${component}"
            continue
        fi
        
        if [[ "$component" == "cert-manager" ]]; then
            # Install cert-manager using helm chart
            if ! helm repo list | grep -q jetstack; then
                log_info "Adding jetstack helm repository"
                helm repo add jetstack https://charts.jetstack.io
                helm repo update
            fi
            
            # Deploy to each selected context
            for context in "${contexts[@]}"; do
                log_info "Deploying cert-manager to ${context}"
                
                if [[ "${DRY_RUN}" == "false" ]]; then
                    kubectl config use-context "${context}" >/dev/null 2>&1
                fi
                
                if ! helm upgrade --install cert-manager jetstack/cert-manager \
                    --namespace cert-manager \
                    --create-namespace \
                    --version v1.13.0 \
                    --set installCRDs=true \
                    --timeout 300s; then
                    error_exit "Failed to deploy cert-manager to ${context}"
                fi
            done
        elif [[ "$component" == "cert-manager-issuer" ]]; then
            # Wait for cert-manager to be ready in all selected contexts and deploy issuer
            for context in "${contexts[@]}"; do
                log_info "Waiting for cert-manager to be ready in ${context}..."
                
                if [[ "${DRY_RUN}" == "false" ]]; then
                    kubectl config use-context "${context}" >/dev/null 2>&1
                    kubectl wait --for=condition=ready pod -l app=cert-manager -n cert-manager --timeout=300s
                    kubectl wait --for=condition=ready pod -l app=cainjector -n cert-manager --timeout=300s
                    kubectl wait --for=condition=ready pod -l app=webhook -n cert-manager --timeout=300s
                fi
                
                log_info "Deploying cert-manager issuer to ${context}"
                if ! kubectl apply -f "${component_path}/cert-manager-issuer.yaml"; then
                    error_exit "Failed to deploy cert-manager issuer to ${context}"
                fi
            done
        fi
        
        log_success "Successfully deployed ${component}"
    done
}

# Deploy Certificate resources
deploy_certificates() {
    log_section "Deploying SSL Certificates"

    local cert_files=($(get_certificate_files_for_region "${DEPLOY_REGION}"))

    for cert_file in "${cert_files[@]}"; do
        local cert_path="${HELM_DIR}/${cert_file}"
        local namespace="default"
        local region=""

        # Determine which context to use based on the file path
        if [[ "$cert_file" == *"singapore"* ]]; then
            region="Singapore"
            if [[ "${DRY_RUN}" == "false" ]]; then
                kubectl config use-context "${SINGAPORE_CONTEXT}" >/dev/null 2>&1
            fi
        else
            region="US East"
            if [[ "${DRY_RUN}" == "false" ]]; then
                kubectl config use-context "${US_EAST_CONTEXT}" >/dev/null 2>&1
            fi
        fi

        # Extract certificate name from YAML (first occurrence under metadata)
        local cert_name="$(grep -m1 '^  name:' "${cert_path}" | awk '{print $2}')"
        if [[ -z "${cert_name}" ]]; then
            log_warning "Could not determine certificate name from ${cert_path}. Skipping."
            continue
        fi

        # Skip deployment if certificate already exists and is Ready
        if kubectl get certificate "${cert_name}" -n "${namespace}" >/dev/null 2>&1; then
            local readiness="$(kubectl get certificate "${cert_name}" -n "${namespace}" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || echo "")"
            if [[ "${readiness}" == "True" ]]; then
                log_info "Certificate ${cert_name} already exists and is Ready in ${region}, skipping"
                continue
            else
                log_info "Certificate ${cert_name} exists but not Ready in ${region}, re-applying"
            fi
        else
            log_info "Deploying certificate for ${region}: $(basename ${cert_file})"
        fi

        if [[ "${DRY_RUN}" == "true" ]]; then
            log_info "[DRY RUN] Would deploy certificate: ${cert_path}"
            continue
        fi

        if ! kubectl apply -f "${cert_path}"; then
            error_exit "Failed to deploy certificate: ${cert_path}"
        fi

        log_success "Successfully applied certificate: $(basename ${cert_file})"
    done
}

# Deploy selected charts
deploy_all_charts() {
    log_section "Deploying Selected Charts"
    
    local charts=($(get_charts_for_region "${DEPLOY_REGION}"))
    
    for chart in "${charts[@]}"; do
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
    
    local contexts=($(get_contexts_for_region "${DEPLOY_REGION}"))
    
    for context in "${contexts[@]}"; do
        local region_name=""
        case "$context" in
            "$US_EAST_CONTEXT")
                region_name="US EAST"
                ;;
            "$SINGAPORE_CONTEXT")
                region_name="SINGAPORE"
                ;;
            *)
                region_name=$(echo "$context" | tr '[:lower:]' '[:upper:]')
                ;;
        esac
        
        log_info "Switching to ${region_name} context (${context})"
        kubectl config use-context "${context}" >/dev/null 2>&1
    
    echo ""
        log_info "${region_name} REGION STATUS:"
        
        # Check WebSocket ingress (now uses Ingress instead of LoadBalancer)
        local ws_ingress=$(kubectl get ingress -o name 2>/dev/null | grep websocket || echo "not-found")
        
        # Check WebTransport LoadBalancer based on region
        local wt_service=""
        if [[ "$context" == "$US_EAST_CONTEXT" ]]; then
            wt_service="webtransport-us-east-lb"
        elif [[ "$context" == "$SINGAPORE_CONTEXT" ]]; then
            wt_service="webtransport-singapore-lb"
        fi
        
        local wt_ip="not-applicable"
        if [[ -n "$wt_service" ]]; then
            wt_ip=$(kubectl get service "$wt_service" -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "pending")
        fi
    
        log_info "  WebSocket Ingress: ${ws_ingress}"
        log_info "  WebTransport LoadBalancer IP: ${wt_ip}"
    
        # Check certificate status based on region
        local ws_cert_name=""
        local wt_cert_name=""
        if [[ "$context" == "$US_EAST_CONTEXT" ]]; then
            ws_cert_name="websocket-us-east-tls"
            wt_cert_name="webtransport-us-east-tls"
        elif [[ "$context" == "$SINGAPORE_CONTEXT" ]]; then
            ws_cert_name="websocket-singapore-tls"
            wt_cert_name="webtransport-singapore-tls"
        fi
        
        local ws_cert="not-applicable"
        local wt_cert="not-applicable"
        if [[ -n "$ws_cert_name" ]]; then
            ws_cert=$(kubectl get certificate "$ws_cert_name" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || echo "not-found")
        fi
        if [[ -n "$wt_cert_name" ]]; then
            wt_cert=$(kubectl get certificate "$wt_cert_name" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || echo "not-found")
        fi
        
        log_info "  WebSocket Certificate: ${ws_cert}"
        log_info "  WebTransport Certificate: ${wt_cert}"
        
        # Check additional services deployed to US East for consolidation
        if [[ "$context" == "$US_EAST_CONTEXT" ]]; then
            echo ""
            log_info "  CONSOLIDATED SERVICES STATUS:"
            
            # Check deployments for new services
            local website_deployment=$(kubectl get deployment website-us-east -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            local blog_deployment=$(kubectl get deployment engineering-vlog-us-east -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            local matomo_deployment=$(kubectl get deployment matomo-us-east -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            local ui_deployment=$(kubectl get deployment videocall-ui-us-east -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            local staging_deployment=$(kubectl get deployment videocall-staging-ui-us-east -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            local videocall_website_deployment=$(kubectl get deployment videocall-website-us-east -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            local prometheus_deployment=$(kubectl get deployment prometheus-us-east-server -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            local grafana_deployment=$(kubectl get deployment grafana-us-east -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
            
            log_info "    Website: ${website_deployment} replicas ready"
            log_info "    Engineering Blog: ${blog_deployment} replicas ready"
            log_info "    Matomo: ${matomo_deployment} replicas ready"
            log_info "    Videocall UI: ${ui_deployment} replicas ready"
            log_info "    Videocall Staging UI: ${staging_deployment} replicas ready"
            log_info "    Videocall Website: ${videocall_website_deployment} replicas ready"
            log_info "    Prometheus: ${prometheus_deployment} replicas ready"
            log_info "    Grafana: ${grafana_deployment} replicas ready"
            
            # Check ingresses for new services
            local website_ingress=$(kubectl get ingress website-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "not-found")
            local blog_ingress=$(kubectl get ingress engineering-vlog-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "not-found")
            local matomo_ingress=$(kubectl get ingress matomo-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "not-found")
            local ui_ingress=$(kubectl get ingress videocall-ui-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "not-found")
            local staging_ingress=$(kubectl get ingress videocall-staging-ui-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "not-found")
            local videocall_website_ingress=$(kubectl get ingress videocall-website-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "not-found")
            local grafana_ingress=$(kubectl get ingress grafana-us-east -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || echo "not-found")
            
            echo ""
            log_info "  INGRESS IP ADDRESSES:"
            log_info "    Website: ${website_ingress}"
            log_info "    Engineering Blog: ${blog_ingress}"
            log_info "    Matomo: ${matomo_ingress}"
            log_info "    Videocall UI: ${ui_ingress}"
            log_info "    Videocall Staging UI: ${staging_ingress}"
            log_info "    Videocall Website: ${videocall_website_ingress}"
            log_info "    Grafana: ${grafana_ingress}"
        fi
    
    echo ""
    done
    
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
    log_info "Deploy region: ${DEPLOY_REGION}"
    
    validate_prerequisites
    deploy_digitalocean_secret
    deploy_cert_manager
    update_dependencies
    deploy_certificates
    deploy_all_charts
    verify_deployments
    
    echo ""
    log_success "Global infrastructure deployment completed successfully!"
    log_info "Log file saved: ${LOG_FILE}"
}

# Run main function with all arguments
main "$@" 