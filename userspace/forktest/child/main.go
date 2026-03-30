package main

import (
	"flag"
	"fmt"
	"os"
	"os/signal"
	"runtime"
	"sync"
	"syscall"
	"time"
)

var (
	mmapTestEnabled = flag.Bool("mmap_test", false, "Enable mmap/munmap stress testing")
	fileIOEnabled   = flag.Bool("file_io", false, "Enable O_APPEND file I/O testing")
	goroutineStress = flag.Bool("goroutine_stress", false, "Enable goroutine/channel/futex stress testing")
	combinedStress  = flag.Bool("combined_stress", false, "Enable all stress modes concurrently")
	durationFlag    = flag.Duration("duration", 0, "How long to run stress tests (e.g. 10s, 1m). 0 = run once")
)

func main() {
	flag.Parse()
	setupSignalHandlers()

	childID := os.Getenv("FORKTEST_CHILD_ID")
	if childID == "" {
		childID = "unknown"
	}
	fmt.Printf("forktest_child %s: Hello from child process!\n", childID)

	var deadline time.Time
	if *durationFlag > 0 {
		deadline = time.Now().Add(*durationFlag)
		fmt.Printf("forktest_child %s: Running for %s.\n", childID, durationFlag.String())
	}

	if *combinedStress {
		runCombinedStress(childID, deadline)
	} else {
		if *mmapTestEnabled {
			runMmapStress(childID, deadline)
		}
		if *fileIOEnabled {
			runFileIOTest(childID, deadline)
		}
		if *goroutineStress {
			runGoroutineStress(childID, deadline)
		}
	}

	os.Exit(0)
}

func setupSignalHandlers() {
	sigChan := make(chan os.Signal, 1)
	signal.Notify(sigChan, syscall.SIGINT, syscall.SIGTERM, syscall.SIGSEGV)

	go func() {
		sig := <-sigChan
		switch sig {
		case syscall.SIGINT, syscall.SIGTERM:
			fmt.Fprintf(os.Stderr, "forktest_child: Received %v, exiting gracefully.\n", sig)
			os.Exit(0)
		case syscall.SIGSEGV:
			fmt.Fprintf(os.Stderr, "forktest_child: Received SIGSEGV, logging fault and exiting.\n")
			os.Exit(139)
		}
	}()
}

// active returns true if the deadline has not been reached (or no deadline is set).
func active(deadline time.Time) bool {
	return deadline.IsZero() || time.Now().Before(deadline)
}

func runMmapStress(childID string, deadline time.Time) {
	fmt.Printf("forktest_child %s: Starting mmap/munmap stress test...\n", childID)
	const allocationSize = 100 * 1024 * 1024
	const numAllocations = 5

	iteration := 0
	for active(deadline) {
		iteration++
		for i := 0; i < numAllocations; i++ {
			fmt.Printf("forktest_child %s: [iter %d] Allocating %d MB (%d/%d)...\n",
				childID, iteration, allocationSize/1024/1024, i+1, numAllocations)
			_ = make([]byte, allocationSize)
			fmt.Printf("forktest_child %s: [iter %d] Triggering GC...\n", childID, iteration)
			runtime.GC()
			time.Sleep(100 * time.Millisecond)
			if !active(deadline) {
				break
			}
		}
		if deadline.IsZero() {
			break // run once
		}
	}
	fmt.Printf("forktest_child %s: Mmap/munmap stress test finished (%d iteration(s)).\n", childID, iteration)
}

func runFileIOTest(childID string, deadline time.Time) {
	fmt.Printf("forktest_child %s: Starting O_APPEND file I/O test...\n", childID)

	iteration := 0
	for active(deadline) {
		iteration++

		tmpFile, err := os.CreateTemp("", fmt.Sprintf("forktest_child_%s_*", childID))
		if err != nil {
			fmt.Fprintf(os.Stderr, "forktest_child %s: Failed to create temp file: %v\n", childID, err)
			return
		}
		tmpPath := tmpFile.Name()
		tmpFile.Close()
		defer os.Remove(tmpPath)

		f, err := os.OpenFile(tmpPath, os.O_WRONLY|os.O_APPEND, 0644)
		if err != nil {
			fmt.Fprintf(os.Stderr, "forktest_child %s: Failed to open temp file with O_APPEND: %v\n", childID, err)
			return
		}

		const numWrites = 10
		for i := 0; i < numWrites; i++ {
			line := fmt.Sprintf("child=%s line=%d data=ABCDEFGHIJ\n", childID, i)
			if _, err := f.WriteString(line); err != nil {
				fmt.Fprintf(os.Stderr, "forktest_child %s: Write failed at line %d: %v\n", childID, i, err)
				f.Close()
				return
			}
		}
		f.Close()

		content, err := os.ReadFile(tmpPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "forktest_child %s: Failed to read back temp file: %v\n", childID, err)
			return
		}

		expected := ""
		for i := 0; i < numWrites; i++ {
			expected += fmt.Sprintf("child=%s line=%d data=ABCDEFGHIJ\n", childID, i)
		}

		if string(content) == expected {
			fmt.Printf("forktest_child %s: [iter %d] O_APPEND file I/O test PASSED (%d writes verified).\n",
				childID, iteration, numWrites)
		} else {
			fmt.Printf("forktest_child %s: [iter %d] O_APPEND file I/O test FAILED: content mismatch.\n",
				childID, iteration)
			fmt.Printf("forktest_child %s: Expected %d bytes, got %d bytes.\n",
				childID, len(expected), len(content))
		}

		if deadline.IsZero() {
			break // run once
		}
	}
	fmt.Printf("forktest_child %s: File I/O test finished (%d iteration(s)).\n", childID, iteration)
}

func runGoroutineStress(childID string, deadline time.Time) {
	fmt.Printf("forktest_child %s: Starting goroutine/channel stress test...\n", childID)

	const numWorkers = 50
	const numItems = 200

	iteration := 0
	for active(deadline) {
		iteration++

		producer := make(chan int)
		collector := make(chan int, numItems)
		var wg sync.WaitGroup

		for i := 0; i < numWorkers; i++ {
			wg.Add(1)
			go func(workerID int) {
				defer wg.Done()
				for val := range producer {
					result := 0
					for j := 0; j < 1000; j++ {
						result += val * (j + 1)
					}
					collector <- result
				}
			}(i)
		}

		go func() {
			for i := 0; i < numItems; i++ {
				producer <- i
			}
			close(producer)
		}()

		wg.Wait()
		close(collector)

		count := 0
		for range collector {
			count++
		}

		fmt.Printf("forktest_child %s: [iter %d] Goroutine stress processed %d items across %d workers.\n",
			childID, iteration, count, numWorkers)

		if deadline.IsZero() {
			break // run once
		}
	}
	fmt.Printf("forktest_child %s: Goroutine stress test finished (%d iteration(s)).\n", childID, iteration)
}

func runCombinedStress(childID string, deadline time.Time) {
	fmt.Printf("forktest_child %s: Starting combined stress test (all modes concurrently)...\n", childID)

	var wg sync.WaitGroup

	wg.Add(3)
	go func() {
		defer wg.Done()
		runMmapStress(childID, deadline)
	}()
	go func() {
		defer wg.Done()
		runFileIOTest(childID, deadline)
	}()
	go func() {
		defer wg.Done()
		runGoroutineStress(childID, deadline)
	}()

	wg.Wait()
	fmt.Printf("forktest_child %s: Combined stress test finished.\n", childID)
}
