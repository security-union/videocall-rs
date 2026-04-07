#!/bin/bash
# Quick test to see what NATS subjects are active

echo "Testing NATS connection and subscriptions..."
echo ""

cargo build -p vcprobe --quiet

echo "=== Subscribing to health.diagnostics.> for 10 seconds ==="
timeout 10 sh -c 'RUST_LOG=debug ./target/debug/vcprobe --nats nats://localhost:4223 --meeting test-meeting-that-does-not-exist 2>&1 | grep -E "(Received message|health)"' &
PID1=$!

echo ""
echo "=== Subscribing to room.jay.> to see what room traffic exists ==="
echo "    (This should show HEARTBEAT, VIDEO, AUDIO if meeting 'jay' is active)"
echo ""

# Wait for test to complete
wait $PID1

echo ""
echo "If you saw 'Received message on subject: health.diagnostics.*' above,"
echo "then health packets ARE being published."
echo ""
echo "If you only saw heartbeats on room.jay.*, then health packets are NOT"
echo "being published to NATS (they might only be processed locally by the server)."
