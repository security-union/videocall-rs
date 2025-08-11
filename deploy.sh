#!/bin/bash
# Simple convenience script for deploying global infrastructure
# Usage: ./deploy.sh [--restart]
#
# Options:
#   --restart    Restart core deployments to pull latest images
#   (other)      Pass through to deploy-global-infrastructure.sh

set -euo pipefail

# Handle restart flag independently
if [[ "${1:-}" == "--restart" ]]; then
    echo "🔄 Restarting core deployments to pull latest images..."
    
    # US East context
    echo "📍 Switching to US East context..."
    kubectl config use-context do-nyc1-videocall-us-east
    
    echo "🔄 Restarting US East deployments..."
    kubectl rollout restart deployment metrics-api-us-east -n default
    kubectl rollout restart deployment websocket-us-east -n default  
    kubectl rollout restart deployment webtransport-us-east -n default
    
    # Singapore context
    echo "📍 Switching to Singapore context..."
    kubectl config use-context do-sgp1-videocall-singapore
    
    echo "🔄 Restarting Singapore deployments..."
    kubectl rollout restart deployment websocket-singapore -n default || echo "⚠️  websocket-singapore not found"
    kubectl rollout restart deployment webtransport-singapore -n default || echo "⚠️  webtransport-singapore not found"
    
    # Switch back to US East
    echo "📍 Switching back to US East context..."
    kubectl config use-context do-nyc1-videocall-us-east
    
    echo "✅ All deployments restarted successfully!"
    # Exit here - do NOT call deploy-global-infrastructure.sh
    exit 0
fi

# Only call deployment script if --restart was NOT provided
exec ./scripts/deploy-global-infrastructure.sh "$@" 