#ifndef PROBE_PORT
#define PROBE_PORT 18080
#endif
typedef unsigned long u64;
static long call(long n,long a,long b,long c,long d,long e,long f){register long r10 __asm__("r10")=d,r8 __asm__("r8")=e,r9 __asm__("r9")=f;long r;__asm__ volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");return r;}
static void say(char *s){long n=0;while(s[n])n++;call(1,1,(long)s,n,0,0,0);}
void _start(void){
    /* SLIRP's host alias, 10.0.2.2. */
    unsigned char addr[16]={2,0,PROBE_PORT >> 8,PROBE_PORT & 255,10,0,2,2};
    char req[]="GET / HTTP/1.0\r\nHost: probe\r\n\r\n", buf[256];
    long fd=call(41,2,1,0,0,0,0);
    if(fd<0){say("NET_SOCKET_FAIL\n");goto fail;}
    if(call(42,fd,(long)addr,16,0,0,0)<0){say("NET_CONNECT_FAIL\n");goto fail;}
    if(call(1,fd,(long)req,sizeof(req)-1,0,0,0)!=(long)sizeof(req)-1){say("NET_SEND_FAIL\n");goto fail;}
    long n=0;
    while(n<12){long got=call(0,fd,(long)(buf+n),sizeof(buf)-n,0,0,0);if(got<=0)break;n+=got;}
    if(n<12 || buf[9]!='2' || buf[10]!='0' || buf[11]!='0'){say("NET_RESPONSE_FAIL\n");goto fail;}
    say("BOX_NET_PASS\n");call(60,0,0,0,0,0,0);
fail: call(60,1,0,0,0,0,0);
}
