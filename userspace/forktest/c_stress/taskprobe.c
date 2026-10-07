/*
 * taskprobe: /proc/self/task as Chromium's sandbox helper reads it
 * (sandbox/linux/services/thread_helpers.cc): fstatat(proc_fd, "self/task/")
 * and st_nlink == 2 + threads, so a single-threaded process sees 3. Then the
 * directory listing and a per-thread status file. Akuma before 2026-10-08 had
 * no task directory; the ENOENT was a CHECK failure in every zygote.
 * Linux: nlink 3, then 4 with a second thread; both tids listed; status reads.
 */
#include <dirent.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int fails;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int stop;

static void *idle(void *a) {
    (void)a;
    pthread_mutex_lock(&mu);
    while (!stop) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    return 0;
}

static long nlink(int procfd) {
    struct stat st;
    if (fstatat(procfd, "self/task/", &st, 0) != 0) { perror("taskprobe: fstatat self/task/"); return -1; }
    return (long)st.st_nlink;
}

static int count_entries(void) {
    DIR *d = opendir("/proc/self/task");
    if (!d) { perror("taskprobe: opendir"); return -1; }
    int n = 0;
    struct dirent *e;
    while ((e = readdir(d))) if (e->d_name[0] != '.') n++;
    closedir(d);
    return n;
}

int main(void) {
    int procfd = open("/proc", O_RDONLY | O_DIRECTORY);
    long one = nlink(procfd);
    printf("taskprobe: single-threaded nlink=%ld (want 3) %s\n", one, one == 3 ? "ok" : "FAIL");
    if (one != 3) fails++;

    pthread_t t;
    pthread_create(&t, 0, idle, 0);
    usleep(100000);
    long two = nlink(procfd);
    int listed = count_entries();
    printf("taskprobe: two threads nlink=%ld listed=%d (want 4, 2) %s\n", two, listed,
           two == 4 && listed == 2 ? "ok" : "FAIL");
    if (two != 4 || listed != 2) fails++;

    char path[64], buf[256] = {0};
    snprintf(path, sizeof path, "/proc/self/task/%d/status", getpid());
    int fd = open(path, O_RDONLY);
    ssize_t n = fd >= 0 ? read(fd, buf, sizeof buf - 1) : -1;
    int named = n > 0 && strstr(buf, "Name:") != NULL;
    printf("taskprobe: %s read=%zd %s\n", path, n, named ? "ok" : "FAIL");
    if (!named) fails++;

    pthread_mutex_lock(&mu);
    stop = 1;
    pthread_cond_signal(&cv);
    pthread_mutex_unlock(&mu);
    pthread_join(t, 0);
    printf("taskprobe: %s\n", fails ? "FAIL" : "PASS");
    return fails != 0;
}
