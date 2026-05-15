// stp_test: verify that the kernel handles QEMU EC=0x15 misrouting for
// `stp xzr, xzr, [Xn, #N]` on a PROT_NONE lazy region (Pattern 4 fix).
//
// The test mmap(PROT_NONE)s a region, then calls an assembly stub that
// executes `stp xzr, xzr` against it. On QEMU TCG, PROT_NONE pages trigger
// EC=0x15 instead of EC=0x25 for this instruction. The kernel fix must
// demand-page the target and emulate the store without SIGSEGV.
package main

import (
	"fmt"
	"os"
	"syscall"
	"unsafe"
)

// stpXzrXzr stores two zero 64-bit values at dst using stp xzr, xzr, [x0].
// Declared in stp_arm64.s.
func stpXzrXzr(dst uintptr)

// stpXzrXzrOffset stores two zero 64-bit values at dst+offset using
// stp xzr, xzr, [x0, #N] variants. Declared in stp_arm64.s.
func stpXzrXzrOff16(dst uintptr)
func stpXzrXzrOff32(dst uintptr)

func mustMmapNone(size int) []byte {
	b, err := syscall.Mmap(-1, 0, size, syscall.PROT_NONE,
		syscall.MAP_ANON|syscall.MAP_PRIVATE)
	if err != nil {
		fmt.Fprintln(os.Stderr, "stp_test Go: mmap(PROT_NONE) failed:", err)
		os.Exit(1)
	}
	return b
}

func checkZero(name string, b []byte, off, n int) bool {
	for i := off; i < off+n; i++ {
		if b[i] != 0 {
			fmt.Fprintf(os.Stderr, "stp_test Go: %s byte[%d]=0x%02x != 0\n",
				name, i, b[i])
			return false
		}
	}
	return true
}

func main() {
	pass := true

	// Test 1: stp xzr, xzr, [x0]  (offset 0)
	{
		b := mustMmapNone(4096)
		p := uintptr(unsafe.Pointer(&b[0]))
		stpXzrXzr(p)
		if checkZero("offset=0", b, 0, 16) {
			fmt.Println("stp_test Go: [1/3] offset=0 PASSED")
		} else {
			pass = false
		}
		syscall.Munmap(b) //nolint
	}

	// Test 2: stp xzr, xzr, [x0, #16]
	{
		b := mustMmapNone(4096)
		p := uintptr(unsafe.Pointer(&b[0]))
		stpXzrXzrOff16(p)
		if checkZero("offset=16", b, 16, 16) {
			fmt.Println("stp_test Go: [2/3] offset=16 PASSED")
		} else {
			pass = false
		}
		syscall.Munmap(b) //nolint
	}

	// Test 3: stp xzr, xzr, [x0, #32]
	{
		b := mustMmapNone(4096)
		p := uintptr(unsafe.Pointer(&b[0]))
		stpXzrXzrOff32(p)
		if checkZero("offset=32", b, 32, 16) {
			fmt.Println("stp_test Go: [3/3] offset=32 PASSED")
		} else {
			pass = false
		}
		syscall.Munmap(b) //nolint
	}

	if pass {
		fmt.Println("stp_test Go: ALL PASSED")
		os.Exit(0)
	} else {
		fmt.Fprintln(os.Stderr, "stp_test Go: FAILED")
		os.Exit(1)
	}
}
