/*
 * shmvar: five shapes of a writable MAP_SHARED file mapping shared between
 * mappers, the patterns Chromium's shared memory and SQLite use. Linux passes
 * all five; the amd64 kernel does since 2026-10-08.
 *
 *   a  one process, two mappings of the file
 *   b  map + touch, then fork; the child writes
 *   c  map without touching, then fork; the child writes and exits
 *   d  fork first; the file reaches the child by SCM_RIGHTS; the child
 *      writes and exits before the parent touches its own mapping
 *   e  like d, but the parent touches its mapping first
 *
 * Files are created and unlinked at once, as Chromium does. Argument: the
 * directory to create them in (default /tmp).
 *
 * See docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md, Fixes 3 and 4.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
#define SZ 65536
static const char *DIR = "/tmp";
static int mkfile(const char *tag) {
    char p[96]; snprintf(p, sizeof p, "%s/.shmvar.%s.%d", DIR, tag, getpid());
    int fd = open(p, O_RDWR|O_CREAT|O_EXCL, 0600); ftruncate(fd, SZ); unlink(p); return fd;
}
static void report(const char *n, int ok, const char *got) { printf("%s %s (saw '%.12s')\n", ok ? "PASS" : "FAIL", n, got); fflush(stdout); }
int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1) DIR = argv[1];
    printf("INFO dir %s\n", DIR);
    /* a: one process, two mappings */
    { int fd = mkfile("a"); char *m1 = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0), *m2 = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);
      strcpy(m1+4096, "via-m1"); report("a_same_process_two_maps", !strcmp(m2+4096,"via-m1"), m2+4096); close(fd); }
    /* b: map + touch, then fork; child writes */
    { int fd = mkfile("b"); char *m = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); m[4096]=0; int sv[2]; socketpair(AF_UNIX,SOCK_STREAM,0,sv); char c;
      if (!fork()) { strcpy(m+4096,"child-b"); write(sv[1],"x",1); _exit(0);} read(sv[0],&c,1); wait(0);
      report("b_fork_after_touch", !strcmp(m+4096,"child-b"), m+4096); close(fd); }
    /* c: map without touching, then fork; child writes */
    { int fd = mkfile("c"); char *m = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); int sv[2]; socketpair(AF_UNIX,SOCK_STREAM,0,sv); char c;
      if (!fork()) { strcpy(m+4096,"child-c"); write(sv[1],"x",1); _exit(0);} read(sv[0],&c,1); wait(0);
      report("c_fork_untouched", !strcmp(m+4096,"child-c"), m+4096); close(fd); }
    /* d: fork first; parent creates+maps, child maps via inherited-later? no: child opens by /proc/self/fd? use SCM */
    { int sv[2]; socketpair(AF_UNIX,SOCK_SEQPACKET,0,sv); char c;
      pid_t pid = fork();
      if (!pid) { char cb[CMSG_SPACE(4)]; struct iovec iov={&c,1}; struct msghdr mh={0}; mh.msg_iov=&iov; mh.msg_iovlen=1; mh.msg_control=cb; mh.msg_controllen=sizeof cb;
        recvmsg(sv[1],&mh,0); int fd; memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&mh)), 4);
        char *m = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); strcpy(m+4096,"child-d"); write(sv[1],"x",1); _exit(0);}
      int fd = mkfile("d"); char *m = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0);
      char cb[CMSG_SPACE(4)]; struct iovec iov={"f",1}; struct msghdr mh={0}; mh.msg_iov=&iov; mh.msg_iovlen=1; mh.msg_control=cb; mh.msg_controllen=sizeof cb;
      struct cmsghdr *ch=CMSG_FIRSTHDR(&mh); ch->cmsg_level=SOL_SOCKET; ch->cmsg_type=SCM_RIGHTS; ch->cmsg_len=CMSG_LEN(4); memcpy(CMSG_DATA(ch),&fd,4);
      sendmsg(sv[0],&mh,0); read(sv[0],&c,1); waitpid(pid,0,0);
      char pr[16]={0}; pread(fd, pr, 8, 4096);
      report("d_scm_passed_fd", !strcmp(m+4096,"child-d"), m+4096); printf("INFO d file bytes at 4096 after child exit: '%.8s'\n", pr); close(fd); }
    /* e: like d, but parent touches its mapping before the child writes */
    { int sv[2]; socketpair(AF_UNIX,SOCK_SEQPACKET,0,sv); char c;
      pid_t pid = fork();
      if (!pid) { char cb[CMSG_SPACE(4)]; struct iovec iov={&c,1}; struct msghdr mh={0}; mh.msg_iov=&iov; mh.msg_iovlen=1; mh.msg_control=cb; mh.msg_controllen=sizeof cb;
        recvmsg(sv[1],&mh,0); int fd; memcpy(&fd, CMSG_DATA(CMSG_FIRSTHDR(&mh)), 4);
        char *m = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); strcpy(m+4096,"child-e"); write(sv[1],"x",1); read(sv[1],&c,1); _exit(0);}
      int fd = mkfile("e"); char *m = mmap(0,SZ,PROT_READ|PROT_WRITE,MAP_SHARED,fd,0); volatile char t = m[4096]; (void)t;
      char cb[CMSG_SPACE(4)]; struct iovec iov={"f",1}; struct msghdr mh={0}; mh.msg_iov=&iov; mh.msg_iovlen=1; mh.msg_control=cb; mh.msg_controllen=sizeof cb;
      struct cmsghdr *ch=CMSG_FIRSTHDR(&mh); ch->cmsg_level=SOL_SOCKET; ch->cmsg_type=SCM_RIGHTS; ch->cmsg_len=CMSG_LEN(4); memcpy(CMSG_DATA(ch),&fd,4);
      sendmsg(sv[0],&mh,0); read(sv[0],&c,1);
      report("e_scm_parent_touched_first", !strcmp(m+4096,"child-e"), m+4096); write(sv[0],"y",1); waitpid(pid,0,0); close(fd); }
    return 0;
}
