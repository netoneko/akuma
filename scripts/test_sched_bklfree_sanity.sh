#!/bin/bash
# Quick sanity test for the ticket-leak fix - boot with sched_bklfree_el0 enabled

set -e

echo "=== Building kernel with SMP=4 ==="
cargo build --profile release-smp-shared --features smp-shared

echo "=== Starting QEMU with SMP=4 (30 second boot test) ==="
timeout 30 cargo run --profile release-smp-shared --features smp-shared 2>&1 | tee /tmp/smp_test.log &
QEMU_PID=$!

# Wait for boot
echo "Waiting for boot..."
sleep 25

# Check if we're still alive
if kill -0 $QEMU_PID 2>/dev/null; then
    echo "✓ QEMU still running after 25 seconds"
    
    # Check for BKL wedges in the log
    if grep -q "\[BKL\] stuck.*owner=0" /tmp/smp_test.log; then
        echo "✗ TICKET LEAK DETECTED: owner=0 means lock is unowned with waiters spinning"
        echo "This is the signature of the ticket leak!"
        kill $QEMU_PID 2>/dev/null || true
        exit 1
    fi
    
    # Count stuck events
    STUCK_COUNT=$(grep -c "\[BKL\] stuck" /tmp/smp_test.log || true)
    RECOVERED_COUNT=$(grep -c "\[BKL\] RECOVERED" /tmp/smp_test.log || true)
    
    echo "BKL stuck events: $STUCK_COUNT"
    echo "BKL RECOVERED events: $RECOVERED_COUNT"
    
    if [ "$RECOVERED_COUNT" -gt 10 ]; then
        echo "⚠ Warning: Many RECOVERED events suggest ticket leak still present"
    else
        echo "✓ Low RECOVERED count - ticket accounting looks healthy"
    fi
    
    # Clean shutdown
    kill $QEMU_PID 2>/dev/null || true
    wait $QEMU_PID 2>/dev/null || true
    
    echo "=== Test PASSED ==="
    exit 0
else
    echo "✗ QEMU died early - possible crash or deadlock"
    cat /tmp/smp_test.log
    exit 1
fi