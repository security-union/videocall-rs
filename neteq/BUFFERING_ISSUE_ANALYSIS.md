# NetEq Buffering Issue Analysis & Design

## Problem Statement [[memory:4787288]]

When a peer starts streaming at t=0 and another peer joins much later, neteq buffers ~60 packets instead of fast-forwarding to maintain minimal latency. This violates the expected WebRTC protocol behavior.

## Root Cause Analysis

After comparing our Rust implementation with libwebrtc's neteq, I identified several critical missing components:

### 1. **Missing Future Packet Handling Logic**

**WebRTC Behavior:** 
- Has sophisticated `FuturePacketAvailable()` method that handles packets with timestamps far in the future
- Recognizes when a new peer joins late and adjusts accordingly
- Can trigger fast acceleration or timestamp resets

**Our Implementation:** 
- No special handling for future packets
- Treats late-joining peer packets as normal buffering
- Results in excessive packet accumulation

### 2. **Simplified Decision Logic**

**WebRTC Implementation:**
```cpp
const int high_limit = std::max(target_level_samples, low_limit + 20 * samples_per_ms);
if (buffer_level_samples >= high_limit << 2)      // 4x high limit
  return NetEq::Operation::kFastAccelerate;
if (buffer_level_samples >= high_limit)
  return NetEq::Operation::kAccelerate;
```

**Our Implementation:**
```rust
if current_buffer_ms > target_delay_ms + 20 {     // Simple threshold
    if current_buffer_ms > target_delay_ms + 40 {
        return Ok(Operation::FastAccelerate);
    } else {
        return Ok(Operation::Accelerate);
    }
}
```

### 3. **Missing Buffer Level Filtering**

**WebRTC:** Uses `BufferLevelFilter` to smooth measurements and avoid oscillating decisions.
**Our Code:** Uses raw buffer size measurements, leading to unstable decisions.

### 4. **Incorrect Initial Target Delay**

**WebRTC:** Starts with `kStartDelayMs = 80ms`
**Our Code:** Starts with `0ms`, causing initial buffering issues

### 5. **Missing Timestamp Management**

**WebRTC:** Carefully tracks `target_timestamp` vs `available_timestamp` and handles timestamp jumps.
**Our Code:** No timestamp-based decision making.

## Proposed Solution

### Phase 1: Core Fixes (Immediate)

1. **Fix Initial Target Delay**
   - Add `const kStartDelayMs: u32 = 80;` constant (exact name match with WebRTC)
   - Change DelayManager to start with 80ms instead of 0ms
   - Ensures maintainers can trace back to libwebrtc reference

2. **Implement Buffer Level Filter**
   - Add `BufferLevelFilter` struct to smooth buffer measurements over time
   - Tracks `filtered_current_level()` instead of raw buffer size
   - Uses exponential smoothing to prevent decision oscillation
   - Accounts for time-stretched samples from previous operations
   - Essential for stable acceleration/deceleration decisions

3. **Update Decision Thresholds**
   - Replace simple `target_delay + 20ms` logic with WebRTC's sophisticated calculations:
     - `low_limit = max(target * 3/4, target - 85ms)`
     - `high_limit = max(target, low_limit + 20ms)`
     - Fast accelerate: `buffer >= high_limit << 2` (4x threshold)
     - Normal accelerate: `buffer >= high_limit`
   - **YES, this will trigger acceleration MORE aggressively in the 60-packet scenario:**
     - Current: Fast accelerate at target+40ms (~120ms with 80ms target)
     - New: Fast accelerate at high_limit×4 (~320ms+ with 80ms target)
     - The 60-packet buffer (~1200ms) would immediately trigger fast acceleration

### Phase 2: Advanced Features

4. **Implement Future Packet Logic**
   - Add timestamp-based decision making
   - Handle late-joining peers with timestamp jumps
   - Implement fast-forward for timestamp gaps

5. **Add Timestamp Management**
   - Track target vs available timestamps
   - Detect and handle stream restarts
   - Implement packet "too early" detection

## Implementation Priority

**✅ COMPLETED - Phase 1 (Critical fixes):**
- ✅ Buffer level filtering (prevents current oscillation)
- ✅ Decision threshold fixes (proper acceleration behavior) 
- ✅ Initial delay fix (prevents startup buffering issues)
- ✅ WebRTC-style constants (kStartDelayMs, kDecelerationTargetLevelOffsetMs)

**🔄 TODO - Phase 2 (Next iteration):**
- Future packet handling (fixes late-joining peer issue)
- Timestamp management (robust stream handling)

## Success Criteria

1. **Latency Test:** When peer joins late, buffer should fast-forward to maintain <100ms latency
2. **Stability Test:** No oscillating acceleration/deceleration decisions
3. **Protocol Compliance:** Behavior matches libwebrtc reference implementation

## Notes

This analysis follows the user's rule to challenge assumptions [[memory:4787288]] - the original implementation assumed simple thresholds were sufficient, but WebRTC's complexity exists for good reasons, particularly handling real-world network conditions and late-joining scenarios. 