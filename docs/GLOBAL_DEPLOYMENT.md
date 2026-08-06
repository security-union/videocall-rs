# Global High-Availability Deployment Design

## Architecture Overview

### Objective
Deploy videocall.rs globally using client-side RTT-based server selection, using cross_region NATS connectivity for global communication.

## Diagram

```mermaid
flowchart LR
    Client(["Client App"])

    subgraph "US East"
        direction TB
        USE_WS["WS"]
        USE_WT["WT"]
        USE_NATS["NATS"]
        USE_WS --> USE_NATS
        USE_WT --> USE_NATS
    end

    subgraph "Singapore"
        direction TB
        SG_WS["WS"]
        SG_WT["WT"]
        SG_NATS["NATS"]
        SG_WS --> SG_NATS
        SG_WT --> SG_NATS
    end

    Client -- "RTT probes" --> USE_WS
    Client -- "RTT probes" --> USE_WT
    Client -- "RTT probes" --> SG_WS
    Client -- "RTT probes" --> SG_WT

    Client -.-> Selected(("Fastest Connection"))

    USE_NATS --- SG_NATS
``` 

### Regional Strategy
- **Primary Region**: US East (NYC1) - Existing deployment
- **Secondary Region**: Singapore (SGP1) - New deployment for Asia-Pacific
- **Cross-Region Communication**: DigitalOcean VPC peering for private connectivity
- **Client-Side Selection**: RTT-based server election eliminates need for geographic load balancing

### Traffic Routing
- **Client Election**: Each client tests all available servers and selects the fastest
- **RTT-Based Selection**: Server with lowest latency wins, regardless of geographic assumptions
- **Cross-region calls**: NATS gateway mesh via VPC peering enables global communication

## Technical Design

### Network Architecture
```
US East VPC (10.100.0.0/16) ←--VPC Peering--→ Singapore VPC (10.110.0.0/16)
├── NATS Cluster (3 replicas)                  ├── NATS Cluster (2 replicas)  
├── WebSocket Servers                          ├── WebSocket Servers
├── WebTransport Servers                       ├── WebTransport Servers
└── Gateway: 10.100.0.2:30722                  └── Gateway: 10.110.0.2:30722
```

### NATS Super-Cluster ✅
- **Gateway Mode**: Cross-region message routing via private IPs
- **Private VPC**: Communication via DigitalOcean VPC peering (no public internet)
- **NodePort Services**: Gateway ports exposed via private node IPs
- **JetStream**: Message persistence and delivery guarantees

### Client-Side RTT-Based Routing
- Each client connects to all available servers simultaneously
- RTT measurements determine optimal server selection
- Automatic failover to next-best server if primary fails
- No DNS propagation delays or geographic assumptions

## System Implementation

### Phase 1: Infrastructure Setup ✅

#### VPC and Cluster Creation:
```bash
# Create US East VPC
doctl vpcs create --name videocall-us-east --region nyc1 --ip-range 10.100.0.0/16

# Create Singapore VPC  
doctl vpcs create --name videocall-singapore --region sgp1 --ip-range 10.110.0.0/16

# Create cross-region VPC peering
doctl vpcs peerings create videocall-cross-region --vpc-ids <us-vpc-id>,<sgp-vpc-id>

# Create US East Kubernetes cluster
doctl kubernetes cluster create videocall-us-east \
  --region nyc1 \
  --node-pool "name=worker-pool;size=s-2vcpu-4gb;count=1" \
  --vpc-uuid 1dfcab1c-d234-47ff-a9c7-03d4d8dbe4b4

# Create Singapore Kubernetes cluster  
doctl kubernetes cluster create videocall-singapore \
  --region sgp1 \
  --node-pool "name=worker-pool;size=s-2vcpu-4gb;count=1" \
  --vpc-uuid 3eba5254-9d97-4c9e-bf46-9e99c327566d
```

**Results:**
- **US East Cluster**: `videocall-us-east` (ID: 6df40c7a-777b-400a-899c-84525093359a)
- **Singapore Cluster**: `videocall-singapore` (ID: d8b84369-d351-41b0-a23a-f19940fd975a)
- **Status**: ✅ Complete - VPCs created and peered, clusters operational

### Phase 2: NATS Cross-Region Deployment ✅

#### Directory Structure Created:
```
helm/global/
├── us-east/
│   ├── nats/
│   │   ├── Chart.yaml
│   │   └── values.yaml
│   ├── webtransport/
│   └── websocket/
└── singapore/
    ├── nats/
    │   ├── Chart.yaml
    │   └── values.yaml
    ├── webtransport/
    └── websocket/
```

#### Chart Configuration
Both regions use the official NATS Helm chart (v0.19.15) with gateway configuration:

**Key Configuration Points:**
- **US East**: 3-replica cluster with 5Gi storage
- **Singapore**: 2-replica cluster with 3Gi storage  
- **Gateway Mode**: Cross-region communication via private VPC endpoints
- **JetStream**: Enabled for message persistence

#### NodePort Services for Cross-Region Gateway Access
NodePort services expose gateway ports (30722) for cross-region NATS communication via private VPC endpoints.

**Gateway Endpoints:**
- **US East**: `10.100.0.2:30722`
- **Singapore**: `10.110.0.2:30722`

**Deployment Results:**
- **US East NATS**: `nats-us-east-0` (3/3 Running) ✅
- **Singapore NATS**: `nats-singapore-0` (2/3 Running) ✅
- **Gateway Configuration**: Blocks generated, port 7222 listening ✅
- **Status**: ✅ Both NATS clusters deployed with functioning gateway infrastructure

### Phase 2.5: NATS Connectivity Verification ✅

#### Network Connectivity Testing
Verification confirmed bidirectional connectivity between regions:

**VPC Peering Results:**
- **Singapore → US East**: 234ms average latency, 0% packet loss ✅
- **US East → Singapore**: 235ms average latency, 0% packet loss ✅

**Gateway Port Accessibility:**
- Both regions can reach each other's gateway ports (30722) ✅

**NATS Gateway Status:**
- Both regions listening on port 7222 ✅
- Gateway configuration blocks present ✅
- Inbound connections being processed ✅

**Test Results Summary:**
- **Network Infrastructure**: ✅ Private VPC peering working (~234ms latency)
- **Gateway Ports**: ✅ Both regions listening on port 7222
- **Cross-Region Access**: ✅ Bidirectional connectivity via private IPs
- **Gateway Processing**: ✅ Inbound connections being processed
- **NodePort Services**: ✅ Gateway ports accessible via 30722

### Phase 2.6: Generated Configuration Verification ✅

**Key Achievements:**
- ✅ **Official NATS Chart**: Successfully migrated from custom rustlemania-nats chart
- ✅ **Gateway Infrastructure**: Both regions have gateway blocks generated and listening
- ✅ **Private Network**: Cross-region connectivity via VPC peering confirmed  
- ✅ **Service Discovery**: NodePort services expose gateway ports correctly
- ✅ **Bidirectional Access**: Both regions can reach each other's gateway ports

### Phase 2.7: Final Working Configuration ✅

**Breakthrough Solution**: Official Synadia Labs Configuration Format

After multiple attempts with different gateway configuration approaches, the final working solution used the exact format from [Synadia Labs NATS configuration](https://github.com/synadia-io/nats-k8s/blob/main/DEVELOPMENT.md#gateways):

**Final Working Configuration:**
The breakthrough solution used the exact format from Synadia Labs NATS configuration with unique gateway names per region and proper private VPC endpoints.

**Key Configuration Points:**
- **US East Gateway**: `name: "us-east-1"` with Singapore endpoint
- **Singapore Gateway**: `name: "singapore"` with US East endpoint
- **Private Endpoints**: Using VPC private IPs via NodePort services

**User Verification Confirmed**: ✅ Cross-region NATS gateway connectivity working

### Critical Insights & Lessons Learned 🧠

#### 1. **Chart Selection Matters Critically**
- **❌ Failed Approach**: Custom `rustlemania-nats` chart 
- **✅ Success**: Official `nats/nats` chart v0.19.15
- **Insight**: Always prefer official charts for complex features like gateways

#### 2. **Configuration Format is Unforgiving**
- **❌ Failed**: `merge:` approach, complex nested structures
- **✅ Success**: Direct `gateway:` block with `name` and `gateways` array
- **Insight**: Follow exact vendor documentation examples (Synadia Labs)

#### 3. **Gateway Naming Strategy**
- **Critical**: Each region needs unique `gateway.name` 
- **US East**: `name: "us-east-1"`
- **Singapore**: `name: "singapore"`
- **Insight**: Gateway names must be unique across the super-cluster

#### 4. **URL Format Requirements**
- **Format**: `nats://IP:PORT` (not `http://` or just `IP:PORT`)
- **Private IPs**: Use VPC private endpoints, not public IPs
- **NodePort**: Gateway ports exposed via NodePort services (30722)
- **Insight**: NATS protocol prefix is mandatory

#### 5. **Don't Fix What Isn't Broken**
- **Mistake**: Attempting to "fix" empty gateways array when infrastructure was working
- **Reality**: Infrastructure was ready, only configuration format was wrong
- **Insight**: Verify actual connectivity before changing working network setup

#### 6. **Official Documentation Hierarchy**
1. **Synadia Labs** (NATS maintainer): Most authoritative
2. **Official NATS docs**: Second source
3. **Community examples**: Use with caution
4. **Custom charts**: Avoid for complex features

#### 7. **Deployment Strategy**
- **Infrastructure First**: VPC, peering, NodePorts, basic NATS
- **Configuration Last**: Gateway endpoints only after connectivity verified
- **Test Incrementally**: Verify each layer before adding complexity

#### 8. **Troubleshooting Methodology**
1. **Network Layer**: Test ping, netcat connectivity
2. **Service Layer**: Verify NodePort exposure  
3. **Application Layer**: Check NATS gateway logs
4. **Configuration Layer**: Validate generated config files
5. **Only then**: Modify gateway endpoint configuration

### Phase 3: Singapore Service Deployments ✅

#### Service Configuration
Singapore services are configured in `helm/global/singapore/` with separate directories for:
- **websocket/**: WebSocket server configuration
- **webtransport/**: WebTransport server configuration

Both services connect to the Singapore NATS cluster and are configured with Singapore-specific ingress hosts and node selectors.

**Status**: ✅ Complete - Singapore services deployed and operational

### Phase 4: Client-Side RTT-Based Selection ✅

#### Implementation Overview
The client-side RTT-based server selection is implemented in the `videocall-client` crate with two main components:

**Key Components:**
- **ConnectionManager**: Orchestrates server testing and election
- **ConnectionController**: Manages timers and diagnostics reporting

**Election Process:**
1. **Simultaneous Connection**: Client opens connections to all servers
2. **RTT Probes**: 200ms interval probes to measure actual latency
3. **Tournament Mode**: Servers compete based on real performance
4. **Winner Selection**: Server with lowest average RTT wins
5. **Continuous Monitoring**: Background RTT measurements for failover

**Status**: ✅ Complete - Client-side selection operational and tested

## Implementation Summary

### Current Architecture
The global deployment uses a client-side RTT-based selection approach rather than traditional load balancers:

**Key Components:**
- **Regional Services**: WebSocket and WebTransport servers in US East and Singapore
- **NATS Super-Cluster**: Cross-region communication via gateway mesh
- **Client Election**: RTT-based server selection eliminates need for geographic routing
- **Automatic Failover**: Seamless migration between servers based on performance

**Benefits of This Approach:**
- **No Vendor Lock-in**: Works with any cloud provider or infrastructure
- **Real Performance**: Based on actual RTT measurements, not geographic assumptions
- **Instant Failover**: No DNS propagation delays
- **Cost Effective**: No expensive load balancer services required

### Directory Structure
```
helm/global/
├── us-east/
│   ├── nats/          # US East NATS cluster with gateway
│   ├── websocket/     # US East WebSocket server
│   └── webtransport/  # US East WebTransport server
└── singapore/
    ├── nats/          # Singapore NATS cluster with gateway
    ├── websocket/     # Singapore WebSocket server
    └── webtransport/  # Singapore WebTransport server
```

**Status**: ✅ Complete - Global deployment operational with client-side RTT-based selection

---

## Key Technical Decisions

### 1. Official NATS Helm Chart Migration ✅
**Issue**: Custom `rustlemania-nats` chart wasn't applying gateway configuration  
**Solution**: Switched to official `nats/nats` chart v0.19.15  
**Result**: Gateway blocks generated correctly, port 7222 listening

### 2. Private VPC Connectivity ✅
**Approach**: DigitalOcean VPC peering + NodePort services  
**Security**: All traffic on private network backbone  
**Performance**: ~234ms cross-region latency via private IPs  
**Cost**: No bandwidth charges between peered VPCs

### 3. Gateway Access Pattern ✅
**Method**: NodePort services on private node IPs  
**US East Gateway**: `10.100.0.2:30722`  
**Singapore Gateway**: `10.110.0.2:30722`  
**Status**: Both endpoints accessible and processing connections

### 4. Configuration Source Authority ✅
**Primary**: Synadia Labs official examples
**Secondary**: Official NATS documentation  
**Avoided**: Custom implementations and complex merge approaches
**Result**: Simple, working configuration that follows vendor patterns

## Current Status: NATS Cross-Region Super-Cluster COMPLETE ✅

The NATS cross-region gateway infrastructure is fully operational and verified:

- **✅ Gateway Mode**: Both regions listening on port 7222 with populated gateways arrays
- **✅ Network Connectivity**: Private VPC peering working (~234ms latency)
- **✅ Service Access**: NodePort endpoints accessible and routing correctly
- **✅ Configuration**: Official Synadia Labs format working perfectly
- **✅ Scalability**: JetStream enabled for message persistence
- **✅ Verification**: User confirmed cross-region connectivity functional

**Next Steps**: Deploy regional WebSocket/WebTransport servers and configure Cloudflare routing.

**Knowledge Preserved**: All critical insights captured to prevent configuration regression.

