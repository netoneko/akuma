// Repro for docs/archive/AMD64_CRUSH_SIGSEGV_STACK_GROWTH_DURING_SPAWN.md.
//
// Crush (a static Go binary) crashes at startup, in a git repo, inside the Go runtime's
// copystack/memmove, while other goroutines run `git` through os/exec. This program has just
// that shape: N goroutines spawn a child in a loop (Go's os/exec -> clone(CLONE_VM|CLONE_VFORK))
// while the main goroutine repeatedly forces a large goroutine-stack growth.
//
//	GOOS=linux GOARCH=amd64 CGO_ENABLED=0 go build -o gofork .
//	./gofork [-spawners 4] [-rounds 200] [-frame 262144] [-child /bin/true]
//
// Exit 0 and "OK" = survived. A SIGSEGV / "fatal error: fault" from the Go runtime = reproduced.
package main

import (
	"flag"
	"fmt"
	"os"
	"os/exec"
	"sync"
	"sync/atomic"
)

var sink atomic.Uint64

// grow recurses with a big frame, so each call deepens the stack and forces copystack.
//
//go:noinline
func grow(depth int, frame int) uint64 {
	var buf [32 << 10]byte
	for i := 0; i < len(buf); i += 4096 {
		buf[i] = byte(depth)
	}
	if depth == 0 {
		return uint64(buf[0])
	}
	return grow(depth-1, frame) + uint64(buf[(depth*7)%len(buf)])
}

func main() {
	spawners := flag.Int("spawners", 4, "goroutines spawning children")
	rounds := flag.Int("rounds", 200, "stack-growth rounds on the main goroutine")
	depth := flag.Int("depth", 48, "recursion depth per round (x32 KiB frames ~ 1.5 MiB at 48)")
	child := flag.String("child", "/bin/true", "child to spawn")
	flag.Parse()

	var stop atomic.Bool
	var wg sync.WaitGroup
	for i := 0; i < *spawners; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for !stop.Load() {
				if err := exec.Command(*child).Run(); err != nil {
					fmt.Fprintln(os.Stderr, "spawn:", err)
					return
				}
			}
		}()
	}
	for r := 0; r < *rounds; r++ {
		// A fresh goroutine each round starts with a small stack and must grow it.
		done := make(chan uint64)
		go func() { done <- grow(*depth, 0) }()
		sink.Add(<-done)
	}
	stop.Store(true)
	wg.Wait()
	fmt.Println("OK")
}
