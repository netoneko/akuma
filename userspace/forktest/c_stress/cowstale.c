// cowstale — after a copy-on-write break, can a write land in the WRONG frame?
//
// Background: the cargo null-`Rc` defect
// (proposals/CARGO_HEAP_NULL_RC.md) is a live pointer qword in cargo's heap
// reading back as zero, with no fault at the moment of corruption. Every
// allocator-side theory for it has been ruled out by instrumentation: no
// premature free, no refcount desync, no bad protection record. What survives is
// a *stale translation* — a core still holding a mapping for a VA whose PTE has
// moved on.
//
// That has a benign face and a malignant one. Benign: the stale entry is more
// restrictive, so a write faults spuriously and the kernel absorbs it (observed,
// harmless). Malignant: the stale entry names the frame a CoW break just replaced,
// so the write lands in the OLD page while every reader sees the new one — which
// still holds the pre-copy contents. A pointer field written that way reads back
// as whatever the copy held, typically zero. No fault, no allocator involvement,
// nothing for a use-after-free detector to catch.
//
// This probe makes that mechanism observable directly instead of waiting on a
// ~1-in-5 ten-minute build:
//
//   1. Fill N pages with a parent pattern.
//   2. Keep T reader threads verifying those pages, so several cores hold live
//      translations for them when fork demotes the range to read-only.
//   3. Fork. The child writes a DIFFERENT pattern over every page, which forces a
//      CoW break on each one, then verifies it reads back its own writes.
//   4. The parent re-writes its own pattern (forcing the parent-side break too)
//      and verifies every page still holds it.
//
// The child's writes must never be visible to the parent, and vice versa. A page
// holding the other side's pattern means a write landed in a frame its writer no
// longer owned — the malignant case, caught deterministically. The reader threads
// check the same invariant continuously, so a leak that exists only briefly is
// still seen.
//
// Detection is exact; only *triggering* is timing-dependent, which is what the
// thread count and round count are for.
//
// Build: aarch64-linux-musl-gcc -static -O2 -o cowstale cowstale.c -pthread
// Usage: cowstale [rounds] [pages] [reader_threads]
// Exit code 0 = every round clean.

#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE_SIZE 4096
#define WORDS_PER_PAGE (PAGE_SIZE / 8)

static unsigned char *g_map;
static size_t g_pages = 64;
static size_t g_rounds = 200;
static int g_readers = 3;

static volatile int g_stop;
static volatile unsigned long g_reader_checks;
static volatile unsigned long g_reader_faults;

// Distinct, self-describing patterns so a mismatch names its own origin: the tag
// says which side wrote it and the page index says whether the write also landed
// at the wrong offset.
static uint64_t parent_word(size_t page) { return 0x5041524E00000000ULL | (uint64_t)page; }
static uint64_t child_word(size_t page)  { return 0x4348494C00000000ULL | (uint64_t)page; }

static void fill_pages(uint64_t (*word_of)(size_t))
{
    for (size_t p = 0; p < g_pages; p++) {
        uint64_t *q = (uint64_t *)(g_map + p * PAGE_SIZE);
        uint64_t w = word_of(p);
        for (size_t i = 0; i < WORDS_PER_PAGE; i++) q[i] = w;
    }
}

// Returns 0 if every page holds `word_of(page)`, else 1 and describes the first
// mismatch. `who` labels the checker so parent/child/reader reports are distinct.
static int verify_pages(uint64_t (*word_of)(size_t), const char *who, size_t round)
{
    for (size_t p = 0; p < g_pages; p++) {
        const uint64_t *q = (const uint64_t *)(g_map + p * PAGE_SIZE);
        uint64_t want = word_of(p);
        for (size_t i = 0; i < WORDS_PER_PAGE; i++) {
            if (q[i] != want) {
                printf("cowstale FAIL [%s] round=%zu page=%zu off=%zu want=%016llx got=%016llx\n",
                       who, round, p, i * 8,
                       (unsigned long long)want, (unsigned long long)q[i]);
                fflush(stdout);
                return 1;
            }
        }
    }
    return 0;
}

// Reader threads never write. They exist to keep translations for these VAs live
// on other cores across the fork, which is the condition a stale-translation bug
// needs, and they double as continuous observers of the same invariant.
static void *reader(void *arg)
{
    (void)arg;
    while (!g_stop) {
        for (size_t p = 0; p < g_pages && !g_stop; p++) {
            const uint64_t *q = (const uint64_t *)(g_map + p * PAGE_SIZE);
            uint64_t want = parent_word(p);
            // Sample a few offsets rather than the whole page: this loop runs
            // continuously and its job is to hold translations, not to be thorough.
            // The parent's own end-of-round pass is the exhaustive check.
            if (q[0] != want || q[WORDS_PER_PAGE / 2] != want || q[WORDS_PER_PAGE - 1] != want) {
                g_reader_faults++;
                printf("cowstale FAIL [reader] page=%zu want=%016llx got=%016llx,%016llx,%016llx\n",
                       p, (unsigned long long)want,
                       (unsigned long long)q[0],
                       (unsigned long long)q[WORDS_PER_PAGE / 2],
                       (unsigned long long)q[WORDS_PER_PAGE - 1]);
                fflush(stdout);
            }
            g_reader_checks++;
        }
    }
    return NULL;
}

int main(int argc, char **argv)
{
    if (argc > 1) g_rounds = strtoul(argv[1], NULL, 0);
    if (argc > 2) g_pages = strtoul(argv[2], NULL, 0);
    if (argc > 3) g_readers = atoi(argv[3]);
    if (g_pages == 0) g_pages = 1;

    size_t len = g_pages * PAGE_SIZE;
    g_map = mmap(NULL, len, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (g_map == MAP_FAILED) { perror("mmap"); return 2; }

    printf("cowstale: rounds=%zu pages=%zu readers=%d map=%p\n",
           g_rounds, g_pages, g_readers, (void *)g_map);
    fflush(stdout);

    fill_pages(parent_word);

    pthread_t th[16];
    if (g_readers > 16) g_readers = 16;
    for (int i = 0; i < g_readers; i++) {
        if (pthread_create(&th[i], NULL, reader, NULL) != 0) {
            printf("cowstale: pthread_create failed at %d (continuing with fewer)\n", i);
            g_readers = i;
            break;
        }
    }

    int failures = 0;
    for (size_t round = 0; round < g_rounds; round++) {
        // Re-establish the parent pattern. After the first round the pages are
        // read-only from the previous fork's demote, so this also drives the
        // PARENT-side CoW break — the half a stale translation would corrupt.
        fill_pages(parent_word);

        fflush(stdout);
        pid_t pid = fork();
        if (pid < 0) { perror("fork"); failures++; break; }
        if (pid == 0) {
            // Child: break CoW on every page by writing, then check it reads back
            // its own writes. A child that sees the parent's pattern here got a
            // frame it should not have.
            fill_pages(child_word);
            int bad = verify_pages(child_word, "child", round);
            _exit(bad ? 1 : 0);
        }

        int status = 0;
        if (waitpid(pid, &status, 0) < 0) { perror("waitpid"); failures++; break; }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
            printf("cowstale FAIL [child-exit] round=%zu status=0x%x\n", round, status);
            fflush(stdout);
            failures++;
        }

        // The child is gone; nothing it wrote may be visible here.
        if (verify_pages(parent_word, "parent", round)) failures++;

        if (failures > 8) {
            printf("cowstale: stopping early after %d failures\n", failures);
            break;
        }
    }

    g_stop = 1;
    for (int i = 0; i < g_readers; i++) pthread_join(th[i], NULL);

    failures += (int)g_reader_faults;
    printf("cowstale: reader_checks=%lu reader_faults=%lu failures=%d\n",
           g_reader_checks, g_reader_faults, failures);
    printf("cowstale %s\n", failures == 0 ? "PASS" : "FAIL");
    return failures == 0 ? 0 : 1;
}
