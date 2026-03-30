package main

import (
	"bytes"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"syscall"
	"time"

	"golang.org/x/sys/unix"
)

const maxEpollEvents = 10
const childReadBufSize = 1024

var (
	numChildrenFlag = flag.Int("num_children", 3, "Number of child processes to spawn")
	mmapTestParent  = flag.Bool("mmap_test", false, "Enable mmap/munmap stress testing for children")
	fileIOParent    = flag.Bool("file_io", false, "Enable O_APPEND file I/O testing for children")
	sendSignal      = flag.Bool("send_signal", false, "Send SIGINT to one child to test signal handling")
	goroutineStress = flag.Bool("goroutine_stress", false, "Enable goroutine/channel stress testing for children")
	combinedStress  = flag.Bool("combined_stress", false, "Enable all stress modes concurrently for children")
	durationFlag    = flag.Duration("duration", 0, "Total test duration (e.g. 30s, 1m). 0 = run until all children finish")
)

type ChildInfo struct {
	ID        int
	Cmd       *exec.Cmd
	ReadPipe  *os.File
	WritePipe *os.File
	Output    *bytes.Buffer
	Done      bool
}

func buildChildArgs() []string {
	var args []string
	if *durationFlag > 0 {
		args = append(args, fmt.Sprintf("-duration=%s", durationFlag.String()))
	}
	if *combinedStress {
		args = append(args, "-combined_stress=true")
	} else {
		if *mmapTestParent {
			args = append(args, "-mmap_test=true")
		}
		if *fileIOParent {
			args = append(args, "-file_io=true")
		}
		if *goroutineStress {
			args = append(args, "-goroutine_stress=true")
		}
	}
	return args
}

func main() {
	flag.Parse()

	numChildren := *numChildrenFlag
	if numChildren < 1 {
		numChildren = 1
	}

	var deadline time.Time
	if *durationFlag > 0 {
		deadline = time.Now().Add(*durationFlag)
		fmt.Printf("forktest_parent: Starting with %d children, duration=%s (deadline %s).\n",
			numChildren, durationFlag.String(), deadline.Format(time.RFC3339))
	} else {
		fmt.Printf("forktest_parent: Starting parent process with epoll monitoring (%d children).\n", numChildren)
	}

	epollFD, err := unix.EpollCreate1(unix.EPOLL_CLOEXEC)
	if err != nil {
		fmt.Fprintf(os.Stderr, "forktest_parent: Failed to create epoll instance: %v\n", err)
		os.Exit(1)
	}
	defer unix.Close(epollFD)

	children := make([]*ChildInfo, numChildren)
	childMap := make(map[int]*ChildInfo)
	childArgs := buildChildArgs()

	for i := 0; i < numChildren; i++ {
		readPipe, writePipe, err := os.Pipe()
		if err != nil {
			fmt.Fprintf(os.Stderr, "forktest_parent: Failed to create pipe for child %d: %v\n", i, err)
			os.Exit(1)
		}

		cmd := exec.Command("./forktest_child", childArgs...)
		cmd.Env = append(os.Environ(), fmt.Sprintf("FORKTEST_CHILD_ID=%d", i))
		cmd.Stdout = writePipe
		cmd.Stderr = os.Stderr

		childInfo := &ChildInfo{
			ID:        i,
			Cmd:       cmd,
			ReadPipe:  readPipe,
			WritePipe: writePipe,
			Output:    new(bytes.Buffer),
			Done:      false,
		}
		children[i] = childInfo
		childMap[int(readPipe.Fd())] = childInfo

		event := unix.EpollEvent{
			Events: unix.EPOLLIN | unix.EPOLLRDHUP | unix.EPOLLONESHOT,
			Fd:     int32(readPipe.Fd()),
		}
		if err := unix.EpollCtl(epollFD, unix.EPOLL_CTL_ADD, int(readPipe.Fd()), &event); err != nil {
			fmt.Fprintf(os.Stderr, "forktest_parent: Failed to add pipe %d to epoll: %v\n", i, err)
			os.Exit(1)
		}

		writePipe.Close()
	}

	for _, child := range children {
		fmt.Printf("forktest_parent: Launching child %d...\n", child.ID)
		if err := child.Cmd.Start(); err != nil {
			fmt.Fprintf(os.Stderr, "forktest_parent: Failed to start child %d: %v\n", child.ID, err)
			os.Exit(1)
		}
	}

	if *sendSignal && numChildren > 0 {
		go func() {
			time.Sleep(500 * time.Millisecond)
			target := children[0]
			fmt.Printf("forktest_parent: Sending SIGINT to child %d (pid %d)...\n", target.ID, target.Cmd.Process.Pid)
			if err := target.Cmd.Process.Signal(syscall.SIGINT); err != nil {
				fmt.Fprintf(os.Stderr, "forktest_parent: Failed to send SIGINT to child %d: %v\n", target.ID, err)
			}
		}()
	}

	events := make([]unix.EpollEvent, maxEpollEvents)
	activeChildrenCount := numChildren

	for activeChildrenCount > 0 {
		// Check deadline before blocking on epoll.
		if !deadline.IsZero() && time.Now().After(deadline) {
			fmt.Printf("forktest_parent: Duration elapsed, killing %d remaining children.\n", activeChildrenCount)
			for _, child := range children {
				if !child.Done && child.Cmd.Process != nil {
					_ = child.Cmd.Process.Signal(syscall.SIGTERM)
				}
			}
			break
		}

		n, err := unix.EpollWait(epollFD, events, 100)
		if err != nil {
			if err == syscall.EINTR {
				continue
			}
			fmt.Fprintf(os.Stderr, "forktest_parent: EpollWait failed: %v\n", err)
			os.Exit(1)
		}

		for i := 0; i < n; i++ {
			event := events[i]
			childFd := int(event.Fd)
			childInfo, ok := childMap[childFd]
			if !ok {
				fmt.Fprintf(os.Stderr, "forktest_parent: Received event for unknown FD: %d\n", childFd)
				continue
			}

			if (event.Events & unix.EPOLLIN) != 0 {
				buf := make([]byte, childReadBufSize)
				nRead, readErr := unix.Read(childFd, buf)
				if readErr != nil {
					if readErr != io.EOF {
						fmt.Fprintf(os.Stderr, "forktest_parent: Error reading from child %d pipe: %v\n", childInfo.ID, readErr)
					}
				} else if nRead > 0 {
					childInfo.Output.Write(buf[:nRead])
				}
			}

			if (event.Events & unix.EPOLLRDHUP) != 0 {
				buf := make([]byte, childReadBufSize)
				for {
					nRead, readErr := unix.Read(childFd, buf)
					if nRead > 0 {
						childInfo.Output.Write(buf[:nRead])
					}
					if readErr != nil || nRead == 0 {
						break
					}
				}

				if !childInfo.Done {
					childInfo.Done = true
					activeChildrenCount--
					fmt.Printf("forktest_parent: Child %d pipe closed (EPOLLRDHUP). Active children: %d\n", childInfo.ID, activeChildrenCount)
				}
			}

			if !childInfo.Done && (event.Events&unix.EPOLLONESHOT) != 0 {
				event.Events = unix.EPOLLIN | unix.EPOLLRDHUP | unix.EPOLLONESHOT
				if err := unix.EpollCtl(epollFD, unix.EPOLL_CTL_MOD, childFd, &event); err != nil {
					fmt.Fprintf(os.Stderr, "forktest_parent: Failed to re-arm epoll for child %d: %v\n", childInfo.ID, err)
				}
			}
		}
	}

	for _, child := range children {
		finalWaitErr := child.Cmd.Wait()
		if finalWaitErr != nil {
			fmt.Fprintf(os.Stderr, "forktest_parent: Child %d finished with error: %v\n", child.ID, finalWaitErr)
		} else {
			fmt.Printf("forktest_parent: Child %d finished successfully.\n", child.ID)
		}
		fmt.Printf("forktest_parent: Child %d final output: %s\n", child.ID, child.Output.String())
		child.ReadPipe.Close()
	}

	fmt.Println("forktest_parent: All children processed via epoll. Parent exiting.")
	os.Exit(0)
}
