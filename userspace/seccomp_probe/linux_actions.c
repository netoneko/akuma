#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <unistd.h>
#include <errno.h>
#include <sys/wait.h>
struct ins { uint16_t code; uint8_t jt, jf; uint32_t k; };
struct prog { uint16_t len; const struct ins *p; };
int install(const struct ins *p, unsigned n) {
 struct prog f={n,p};
 return syscall(317,1,0,&f);
}
void trap(int sig, siginfo_t *info, void *data) {
 ucontext_t *uc=data;
 dprintf(1,"trap sig=%d code=%d errno=%d arch=%x nr=%d addr=%p ip=%llx ax=%lld\n",sig,info->si_code,info->si_errno,info->si_arch,info->si_syscall,info->si_call_addr,uc->uc_mcontext.gregs[REG_RIP],uc->uc_mcontext.gregs[REG_RAX]);
}
void test(uint32_t op, uint32_t arg, uint32_t action) {
 if (!fork()) {
  struct sigaction sa={.sa_sigaction=trap,.sa_flags=SA_SIGINFO};
  sigaction(SIGSYS,&sa,0);
  syscall(157,38,1,0,0,0);
  struct ins p[]={{0x20,0,0,0},{0x15,0,5,39},{0,0,0,1},{1,0,0,arg},{op,0,0,0},{0x44,0,0,action},{0x16,0,0,0},{6,0,0,0x7fff0000}};
  if (install(p,8)) _exit(99);
  errno=0;
  long rc=syscall(39);
  dprintf(1,"op=%x arg=%u action=%x rc=%ld errno=%d\n",op,arg,action,rc,errno);
  _exit(0);
 }
 int status;
 wait(&status);
 printf("status=%d\n",status);
 fflush(stdout);
}
int main() {
 test(0x6c,32,0x50000);
 test(0x6c,33,0x50000);
 test(0x3c,0,0x50000);
 test(0x04,0,0x30040);
 test(0x04,0,0x7ff00000);
 test(0x04,0,0x50000|5000);
}
