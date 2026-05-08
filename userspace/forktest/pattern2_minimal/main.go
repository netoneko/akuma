// Minimal Go repro for forktest Pattern 2 (docs/GO_FORKTEST_DEBUG.md):
// epoll + EPOLLONESHOT, pipe read drain, epoll_ctl MOD re-arm, spawn /bin/mmap_stress.
// No combined_stress, file_io, or extra goroutines — isolates runtime surface vs forktest_parent.
//
// Build for Akuma (from userspace/forktest): output name must differ from ./pattern2_minimal dir.
//   GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o pattern2_minimal.bin ./pattern2_minimal
package main

import (
	"flag"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"time"

	"golang.org/x/sys/unix"
)

const maxEpollEvents = 16
const readSz = 1024

func main() {
	num := flag.Int("num_children", 3, "children")
	mb := flag.Int("mmap_alloc_mb", 70, "mmap_stress -mmap_alloc_mb")
	dur := flag.Duration("duration", 10*time.Second, "run duration (e.g. 10s)")
	flag.Parse()

	if *num < 1 {
		*num = 1
	}

	epfd, err := unix.EpollCreate1(unix.EPOLL_CLOEXEC)
	if err != nil {
		fmt.Fprintf(os.Stderr, "pattern2_minimal: epoll_create1: %v\n", err)
		os.Exit(1)
	}
	defer unix.Close(epfd)

	type slot struct {
		readFd int
		done   bool
		cmd    *exec.Cmd
	}
	slots := make([]slot, *num)
	fdToIdx := map[int]int{}

	stress := "/bin/mmap_stress"
	if _, err := os.Stat(stress); err != nil {
		stress = resolvePath("mmap_stress")
	}

	deadline := time.Now().Add(*dur)

	for i := 0; i < *num; i++ {
		pipefd := make([]int, 2)
		if err := unix.Pipe(pipefd); err != nil {
			fmt.Fprintf(os.Stderr, "pattern2_minimal: pipe: %v\n", err)
			os.Exit(1)
		}
		w := os.NewFile(uintptr(pipefd[1]), "pipew")
		cmd := exec.Command(stress,
			fmt.Sprintf("-duration=%s", dur.String()),
			fmt.Sprintf("-mmap_alloc_mb=%d", *mb),
		)
		cmd.Env = append(os.Environ(), fmt.Sprintf("FORKTEST_CHILD_ID=%d", i))
		cmd.Stdout = w
		cmd.Stderr = os.Stderr
		if err := cmd.Start(); err != nil {
			fmt.Fprintf(os.Stderr, "pattern2_minimal: start child %d: %v\n", i, err)
			os.Exit(1)
		}
		_ = w.Close()
		rfd := pipefd[0]
		slots[i].readFd = rfd
		slots[i].cmd = cmd
		fdToIdx[rfd] = i

		ev := unix.EpollEvent{
			Events: unix.EPOLLIN | unix.EPOLLRDHUP | unix.EPOLLONESHOT,
			Fd:     int32(rfd),
		}
		if err := unix.EpollCtl(epfd, unix.EPOLL_CTL_ADD, rfd, &ev); err != nil {
			fmt.Fprintf(os.Stderr, "pattern2_minimal: epoll_ctl ADD: %v\n", err)
			os.Exit(1)
		}
	}

	active := *num
	buf := make([]byte, readSz)
	fmt.Fprintf(os.Stderr, "pattern2_minimal: %d children mmap_stress mb=%d duration=%s\n", *num, *mb, dur.String())

	for active > 0 {
		if time.Now().After(deadline) {
			for i := 0; i < *num; i++ {
				if slots[i].cmd != nil && slots[i].cmd.Process != nil {
					_ = slots[i].cmd.Process.Signal(syscall.SIGTERM)
				}
			}
			break
		}
		ms := int(time.Until(deadline).Milliseconds())
		if ms > 100 {
			ms = 100
		}
		if ms < 0 {
			ms = 0
		}
		evs := make([]unix.EpollEvent, maxEpollEvents)
		n, err := unix.EpollWait(epfd, evs, ms)
		if err != nil {
			if err == unix.EINTR {
				continue
			}
			fmt.Fprintf(os.Stderr, "pattern2_minimal: EpollWait: %v\n", err)
			os.Exit(1)
		}
		for i := 0; i < n; i++ {
			cfd := int(evs[i].Fd)
			idx, ok := fdToIdx[cfd]
			if !ok {
				continue
			}
			ch := &slots[idx]
			if evs[i].Events&unix.EPOLLIN != 0 {
				for {
					_, rerr := unix.Read(cfd, buf)
					if rerr != nil {
						break
					}
				}
			}
			if evs[i].Events&unix.EPOLLRDHUP != 0 {
				for {
					nr, rerr := unix.Read(cfd, buf)
					if nr <= 0 || rerr != nil {
						break
					}
				}
				if !ch.done {
					ch.done = true
					active--
				}
			}
			if !ch.done {
				re := unix.EpollEvent{
					Events: unix.EPOLLIN | unix.EPOLLRDHUP | unix.EPOLLONESHOT,
					Fd:     int32(cfd),
				}
				if err := unix.EpollCtl(epfd, unix.EPOLL_CTL_MOD, cfd, &re); err != nil {
					fmt.Fprintf(os.Stderr, "pattern2_minimal: epoll_ctl MOD: %v\n", err)
				}
			}
		}
	}

	for i := 0; i < *num; i++ {
		if slots[i].cmd != nil {
			_ = slots[i].cmd.Wait()
		}
		if slots[i].readFd >= 0 {
			_ = unix.Close(slots[i].readFd)
		}
	}

	fmt.Fprintf(os.Stderr, "pattern2_minimal: exiting\n")
	os.Exit(0)
}

func resolvePath(name string) string {
	exe, err := os.Executable()
	if err != nil {
		return "./" + name
	}
	if resolved, err := filepath.EvalSymlinks(exe); err == nil {
		exe = resolved
	}
	candidate := filepath.Join(filepath.Dir(exe), name)
	if st, err := os.Stat(candidate); err == nil && !st.IsDir() {
		return candidate
	}
	if st, err := os.Stat("/bin/" + name); err == nil && !st.IsDir() {
		return "/bin/" + name
	}
	return "./" + name
}
