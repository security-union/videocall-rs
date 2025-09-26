#!/bin/bash
# Simple convenience script for deploying global infrastructure
# Usage: ./deploy.sh [--restart] [--services SERVICE1,SERVICE2,...] [other options]
#
# Options:
#   --restart              Restart core deployments to pull latest images
#   --services SERVICES    Deploy only specific services (comma-separated)
#                         Available services: websocket, webtransport, ingress-nginx, 
#                         engineering-vlog, matomo, rustlemania-ui, rustlemania-ui-staging,
#                         videocall-website, prometheus, grafana, metrics-api
#   (other)               Pass through to deploy-global-infrastructure.sh
#
# Examples:
#   ./deploy.sh --restart --services websocket,webtransport
#   ./deploy.sh --services metrics-api --region us-east
#   ./deploy.sh --restart                    # Restart all services

set -euo pipefail

# Parse arguments to extract services filter
SERVICES_FILTER=""
RESTART_MODE=false
REMAINING_ARGS=()

while [[ $# -gt 0 ]]; do
    case $1 in
        --restart)
            RESTART_MODE=true
            shift
            ;;
        --services)
            if [[ -n "$2" && "$2" != --* ]]; then
                SERVICES_FILTER="$2"
                REMAINING_ARGS+=("$1" "$2")
                shift 2
            else
                echo "Error: --services requires a value (comma-separated list of services)"
                exit 1
            fi
            ;;
        *)
            REMAINING_ARGS+=("$1")
            shift
            ;;
    esac
done

# Map service names to deployment names for restart functionality
map_service_to_deployments() {
    local service="$1"
    local region="$2"
    
    case "$service" in
        websocket)
            if [[ "$region" == "us-east" ]]; then
                echo "websocket-us-east"
            elif [[ "$region" == "singapore" ]]; then
                echo "websocket-singapore"
            fi
            ;;
        webtransport)
            if [[ "$region" == "us-east" ]]; then
                echo "webtransport-us-east"
            elif [[ "$region" == "singapore" ]]; then
                echo "webtransport-singapore"
            fi
            ;;
        metrics-api)
            if [[ "$region" == "us-east" ]]; then
                echo "metrics-api-us-east"
            fi
            ;;
        engineering-vlog)
            if [[ "$region" == "us-east" ]]; then
                echo "engineering-vlog-us-east"
            fi
            ;;
        rustlemania-ui)
            if [[ "$region" == "us-east" ]]; then
                echo "videocall-ui-us-east"
            fi
            ;;
        rustlemania-ui-staging)
            if [[ "$region" == "us-east" ]]; then
                echo "videocall-staging-ui-us-east"
            fi
            ;;
        videocall-website)
            if [[ "$region" == "us-east" ]]; then
                echo "videocall-website-us-east"
            fi
            ;;
        grafana)
            if [[ "$region" == "us-east" ]]; then
                echo "grafana-us-east"
            fi
            ;;
        prometheus)
            if [[ "$region" == "us-east" ]]; then
                echo "prometheus-us-east-server"
            fi
            ;;
        *)
            echo ""
            ;;
    esac
}

# Filter deployments based on services
get_deployments_to_restart() {
    local region="$1"
    local services_filter="$2"
    
    if [[ -z "$services_filter" ]]; then
        # Return all deployments for the region
        case "$region" in
            "us-east")
                echo "metrics-api-us-east websocket-us-east webtransport-us-east engineering-vlog-us-east videocall-ui-us-east"
                ;;
            "singapore")
                echo "websocket-singapore webtransport-singapore"
                ;;
        esac
        return
    fi
    
    local deployments=()
    IFS=',' read -ra SERVICES_ARRAY <<< "$services_filter"
    for service in "${SERVICES_ARRAY[@]}"; do
        service=$(echo "$service" | xargs) # trim whitespace
        local deployment=$(map_service_to_deployments "$service" "$region")
        if [[ -n "$deployment" ]]; then
            deployments+=("$deployment")
        fi
    done
    
    echo "${deployments[@]}"
}

# Handle restart mode
if [[ "$RESTART_MODE" == "true" ]]; then
    if [[ -n "$SERVICES_FILTER" ]]; then
        echo "🔄 Restarting filtered services: $SERVICES_FILTER"
    else
        echo "🔄 Restarting all core deployments to pull latest images..."
    fi
    
    # US East context
    echo "📍 Switching to US East context..."
    kubectl config use-context do-nyc1-videocall-us-east
    
    echo "🔄 Restarting US East deployments..."
    US_EAST_DEPLOYMENTS=$(get_deployments_to_restart "us-east" "$SERVICES_FILTER")
    if [[ -n "$US_EAST_DEPLOYMENTS" ]]; then
        for deployment in $US_EAST_DEPLOYMENTS; do
            echo "  → Restarting $deployment"
            kubectl rollout restart deployment "$deployment" -n default || echo "⚠️  $deployment not found"
        done
    else
        echo "  → No deployments to restart in US East"
    fi
    
    # Singapore context
    echo "📍 Switching to Singapore context..."
    kubectl config use-context do-sgp1-videocall-singapore
    
    echo "🔄 Restarting Singapore deployments..."
    SINGAPORE_DEPLOYMENTS=$(get_deployments_to_restart "singapore" "$SERVICES_FILTER")
    if [[ -n "$SINGAPORE_DEPLOYMENTS" ]]; then
        for deployment in $SINGAPORE_DEPLOYMENTS; do
            echo "  → Restarting $deployment"
            kubectl rollout restart deployment "$deployment" -n default || echo "⚠️  $deployment not found"
        done
    else
        echo "  → No deployments to restart in Singapore"
    fi
    
    # Switch back to US East
    echo "📍 Switching back to US East context..."
    kubectl config use-context do-nyc1-videocall-us-east
    
    echo "✅ All deployments restarted successfully!"
    # Exit here - do NOT call deploy-global-infrastructure.sh
    exit 0
fi

# Only call deployment script if --restart was NOT provided
exec ./scripts/deploy-global-infrastructure.sh "${REMAINING_ARGS[@]}"