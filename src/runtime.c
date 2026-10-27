#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

typedef struct { int64_t len; int64_t *data; } lm_arr;
typedef struct { int64_t len; uint8_t *data; } lm_bytes;
typedef struct { int64_t len; const uint8_t *data; } lm_str;
typedef struct { void *fn; void *env; } lm_fn;

static void lm_oom(void) { fputs("lamda: out of memory\n", stderr); exit(101); }

/* ---- Garbage collector -------------------------------------------------------
   Conservative mark-and-sweep. All GC memory lives in one reserved virtual
   region split into 64 KB pages, so finding the object behind any address
   (including interior pointers from slices) is arithmetic. Small objects share
   pages by size class and are found through per-page mark bits; large objects
   get whole pages of their own. Objects that hold no pointers (strings, arrays
   of ints) are never scanned. Roots are the C stack and the registers, which
   __builtin_unwind_init spills to the stack. Sweeping is lazy: a page is swept
   when its size class next needs memory. */

enum { LM_PAGE_SHIFT = 16, LM_PAGE = 1 << LM_PAGE_SHIFT, LM_MAX_SMALL = 8192, LM_NCLASS = 32, LM_LARGE = 255 };

/* Collect after allocating this much, or twice the live data if that is larger. Lower
   keeps the heap small; collections are cheap while little is live (alloc benchmark:
   8 MB = 9.5 MB peak, 512 KB = 1.9 MB peak, 0.8 ms slower). */
#define LM_MIN_TRIGGER ((size_t)512 << 10)

static const uint32_t lm_class_size[LM_NCLASS] = {
    16, 32, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512,
    640, 768, 896, 1024, 1280, 1536, 1792, 2048, 2560, 3072, 3584, 4096, 5120, 6144, 7168, 8192,
};

typedef struct lm_page {
    uint8_t *start;
    size_t bytes;          /* large objects: whole allocation */
    uint32_t size, count;  /* small pages: object size and objects per page */
    uint8_t cls, scan, large_marked;
    uint8_t mapped;        /* large object backed by a file mapping (read_file) */
    struct lm_page *next;
    uint64_t marks[LM_PAGE / 16 / 64];
} lm_page;

static uint8_t *lm_heap, *lm_heap_top, *lm_heap_end;
static lm_page **lm_pages;                    /* descriptor of each page in the region */
static uint8_t lm_class_of[LM_MAX_SMALL / 16 + 1];
static void *lm_free_list[2][LM_NCLASS];
static uint8_t *lm_bump[2][LM_NCLASS], *lm_bump_end[2][LM_NCLASS];
static lm_page *lm_class_pages[2][LM_NCLASS], *lm_sweep_next[2][LM_NCLASS];
static lm_page *lm_large;
static uint8_t **lm_spare_pages;              /* freed pages, reused by small classes */
static size_t lm_spare_count, lm_spare_cap;
static size_t lm_since_gc, lm_threshold = LM_MIN_TRIGGER, lm_live, lm_collections;
static size_t lm_heap_used, lm_peak_heap;   /* small pages plus live large objects */
static uint8_t *lm_stack_bottom;
static bool lm_stress;
static struct { void *p; size_t n; } *lm_mark_stack;
static size_t lm_mark_len, lm_mark_cap;

static void lm_gc_stats(void) {
    fprintf(stderr, "lamda gc: %zu collections, peak heap %zu KB, live at last collection %zu KB\n",
            lm_collections, lm_peak_heap >> 10, lm_live >> 10);
}

static void lm_gc_init(void *stack_bottom) {
    lm_stack_bottom = stack_bottom;
    /* Reserve address space only; the OS commits pages when they are touched. */
    size_t region = (size_t)64 << 30;
    for (;;) {
        void *p = mmap(NULL, region, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
        if (p != MAP_FAILED) { lm_heap = lm_heap_top = p; lm_heap_end = lm_heap + region; break; }
        if ((region >>= 1) < ((size_t)256 << 20)) lm_oom();
    }
    lm_pages = calloc(region >> LM_PAGE_SHIFT, sizeof *lm_pages);
    if (!lm_pages) lm_oom();
    for (unsigned i = 0, c = 0; i <= LM_MAX_SMALL / 16; i++) {
        while (lm_class_size[c] < i * 16) c++;
        lm_class_of[i] = (uint8_t)c;
    }
    const char *stress = getenv("LAMDA_GC_STRESS");
    lm_stress = stress && *stress && *stress != '0';
    const char *stats = getenv("LAMDA_GC_STATS");
    if (stats && *stats && *stats != '0') atexit(lm_gc_stats);
}

static void lm_heap_grew(size_t bytes) {
    lm_heap_used += bytes;
    if (lm_heap_used > lm_peak_heap) lm_peak_heap = lm_heap_used;
}

static uint8_t *lm_take_page(void) {
    lm_heap_grew(LM_PAGE);
    if (lm_spare_count) return lm_spare_pages[--lm_spare_count];
    if ((size_t)(lm_heap_end - lm_heap_top) < LM_PAGE) lm_oom();
    uint8_t *p = lm_heap_top;
    lm_heap_top += LM_PAGE;
    return p;
}

static inline size_t lm_page_index(const void *p) { return (size_t)((const uint8_t *)p - lm_heap) >> LM_PAGE_SHIFT; }

static void lm_mark_push(void *p, size_t n) {
    if (lm_mark_len == lm_mark_cap) {
        lm_mark_cap = lm_mark_cap ? lm_mark_cap * 2 : 1024;
        lm_mark_stack = realloc(lm_mark_stack, lm_mark_cap * sizeof *lm_mark_stack);
        if (!lm_mark_stack) lm_oom();
    }
    lm_mark_stack[lm_mark_len].p = p;
    lm_mark_stack[lm_mark_len++].n = n;
}

static inline void lm_mark_word(uintptr_t w) {
    if (w < (uintptr_t)lm_heap || w >= (uintptr_t)lm_heap_top) return;
    lm_page *pg = lm_pages[lm_page_index((void *)w)];
    if (!pg) return;
    if (pg->cls == LM_LARGE) {
        if (pg->large_marked) return;
        pg->large_marked = 1;
        lm_live += pg->bytes;
        if (pg->scan) lm_mark_push(pg->start, pg->bytes);
        return;
    }
    size_t i = (w - (uintptr_t)pg->start) / pg->size;
    if (i >= pg->count) return;
    uint64_t bit = (uint64_t)1 << (i & 63);
    if (pg->marks[i >> 6] & bit) return;
    pg->marks[i >> 6] |= bit;
    lm_live += pg->size;
    if (pg->scan) lm_mark_push(pg->start + i * pg->size, pg->size);
}

/* Reads every word in the range, including stack padding that AddressSanitizer
   poisons, so it is excluded from ASan. */
__attribute__((no_sanitize("address")))
static void lm_mark_range(const void *from, const void *to) {
    const uintptr_t *p = (const uintptr_t *)(((uintptr_t)from + 7) & ~(uintptr_t)7);
    for (; (const void *)(p + 1) <= to; p++) lm_mark_word(*p);
}

/* Separate and not inlined, so its frame lies below everything the collector's
   caller (and __builtin_unwind_init) put on the stack. */
static __attribute__((noinline)) void lm_mark_stack_roots(void) {
    lm_mark_range(__builtin_frame_address(0), lm_stack_bottom);
}

static __attribute__((noinline)) void lm_gc_collect(void) {
    __builtin_unwind_init();
    for (int s = 0; s < 2; s++)
        for (int c = 0; c < LM_NCLASS; c++)
            for (lm_page *pg = lm_class_pages[s][c]; pg; pg = pg->next)
                memset(pg->marks, 0, ((pg->count + 63) / 64) * sizeof(uint64_t));
    for (lm_page *pg = lm_large; pg; pg = pg->next) pg->large_marked = 0;

    lm_live = 0;
    lm_mark_stack_roots();
    while (lm_mark_len) {
        lm_mark_len--;
        lm_mark_range(lm_mark_stack[lm_mark_len].p, (uint8_t *)lm_mark_stack[lm_mark_len].p + lm_mark_stack[lm_mark_len].n);
    }

    for (int s = 0; s < 2; s++)
        for (int c = 0; c < LM_NCLASS; c++) {
            lm_free_list[s][c] = NULL;
            lm_bump[s][c] = lm_bump_end[s][c] = NULL;
            lm_sweep_next[s][c] = lm_class_pages[s][c];
        }
    for (lm_page **link = &lm_large; *link;) {
        lm_page *pg = *link;
        if (pg->large_marked) { link = &pg->next; continue; }
        *link = pg->next;
        lm_heap_used -= pg->bytes;
        size_t first = lm_page_index(pg->start), n = pg->bytes >> LM_PAGE_SHIFT;
        if (pg->mapped) {
            /* Replace the file mapping with fresh anonymous memory for reuse. */
            if (mmap(pg->start, pg->bytes, PROT_READ | PROT_WRITE, MAP_FIXED | MAP_PRIVATE | MAP_ANON, -1, 0) == MAP_FAILED)
                lm_oom();
        } else {
            madvise(pg->start, pg->bytes, MADV_FREE);
        }
        if (lm_spare_count + n > lm_spare_cap) {
            lm_spare_cap = (lm_spare_count + n) * 2;
            lm_spare_pages = realloc(lm_spare_pages, lm_spare_cap * sizeof *lm_spare_pages);
            if (!lm_spare_pages) lm_oom();
        }
        for (size_t i = 0; i < n; i++) {
            lm_pages[first + i] = NULL;
            lm_spare_pages[lm_spare_count++] = pg->start + i * LM_PAGE;
        }
        free(pg);
    }
    lm_since_gc = 0;
    lm_threshold = lm_live * 2 > LM_MIN_TRIGGER ? lm_live * 2 : LM_MIN_TRIGGER;
    lm_collections++;
}

/* Gives size class (s, c) memory to allocate from: a swept page with free
   slots, or a fresh page. */
static void lm_refill(int s, unsigned c) {
    while (lm_sweep_next[s][c]) {
        lm_page *pg = lm_sweep_next[s][c];
        lm_sweep_next[s][c] = pg->next;
        uint32_t marked = 0, words = (pg->count + 63) / 64;
        for (uint32_t w = 0; w < words; w++) marked += (uint32_t)__builtin_popcountll(pg->marks[w]);
        if (marked == pg->count) continue;
        lm_since_gc += (size_t)(pg->count - marked) * pg->size;
        if (marked == 0) {
            lm_bump[s][c] = pg->start;
            lm_bump_end[s][c] = pg->start + (size_t)pg->count * pg->size;
            return;
        }
        void *head = NULL;
        for (uint32_t i = pg->count; i-- > 0;) {
            if (pg->marks[i >> 6] >> (i & 63) & 1) continue;
            void *slot = pg->start + (size_t)i * pg->size;
            *(void **)slot = head;
            head = slot;
        }
        lm_free_list[s][c] = head;
        return;
    }
    uint8_t *mem = lm_take_page();
    lm_page *pg = calloc(1, sizeof *pg);
    if (!pg) lm_oom();
    pg->start = mem;
    pg->size = lm_class_size[c];
    pg->count = LM_PAGE / pg->size;
    pg->cls = (uint8_t)c;
    pg->scan = (uint8_t)s;
    pg->next = lm_class_pages[s][c];
    lm_class_pages[s][c] = pg;
    lm_pages[lm_page_index(mem)] = pg;
    lm_bump[s][c] = mem;
    lm_bump_end[s][c] = mem + (size_t)pg->count * pg->size;
    lm_since_gc += (size_t)pg->count * pg->size;
}

/* Large objects always come from never-used address space, so they start zeroed. */
static void lm_gc_collect(void);

static void *lm_alloc_large(size_t size, int scan) {
    /* Small allocations check this in the slow path; large ones never take the fast path. */
    if (lm_since_gc >= lm_threshold || lm_stress) lm_gc_collect();
    size_t n = (size + LM_PAGE - 1) >> LM_PAGE_SHIFT;
    if ((size_t)(lm_heap_end - lm_heap_top) < n * LM_PAGE) lm_oom();
    uint8_t *mem = lm_heap_top;
    lm_heap_top += n * LM_PAGE;
    lm_heap_grew(n * LM_PAGE);
    lm_page *pg = calloc(1, sizeof *pg);
    if (!pg) lm_oom();
    pg->start = mem;
    pg->bytes = n * LM_PAGE;
    pg->cls = LM_LARGE;
    pg->scan = (uint8_t)scan;
    pg->next = lm_large;
    lm_large = pg;
    for (size_t i = 0, first = lm_page_index(mem); i < n; i++) lm_pages[first + i] = pg;
    lm_since_gc += pg->bytes;
    return mem;
}

static __attribute__((noinline)) void *lm_gc_alloc_slow(size_t size, int s) {
    if (lm_since_gc >= lm_threshold || lm_stress) lm_gc_collect();
    if (size > LM_MAX_SMALL) return lm_alloc_large(size, s);
    unsigned c = lm_class_of[(size + 15) >> 4];
    while (!lm_free_list[s][c] && lm_bump[s][c] == lm_bump_end[s][c]) lm_refill(s, c);
    void *p;
    if (lm_free_list[s][c]) {
        p = lm_free_list[s][c];
        lm_free_list[s][c] = *(void **)p;
    } else {
        p = lm_bump[s][c];
        lm_bump[s][c] += lm_class_size[c];
    }
    if (lm_stress) { /* force every allocation through a collection */
        lm_free_list[s][c] = NULL;
        lm_bump[s][c] = lm_bump_end[s][c];
    }
    return p;
}

/* `scan`: whether the object may hold pointers to other GC objects. The
   memory is not zeroed. */
static inline void *lm_gc_alloc(size_t size, int scan) {
    if (size <= LM_MAX_SMALL) {
        unsigned c = lm_class_of[(size + 15) >> 4];
        void *p = lm_free_list[scan][c];
        if (p) {
            lm_free_list[scan][c] = *(void **)p;
            return p;
        }
        uint8_t *b = lm_bump[scan][c];
        if (b != lm_bump_end[scan][c]) {
            lm_bump[scan][c] = b + lm_class_size[c];
            return b;
        }
    }
    return lm_gc_alloc_slow(size, scan);
}

/* ---- Values ------------------------------------------------------------------ */

static void lm_print_int(int64_t x) { printf("%lld\n", (long long)x); }
static void lm_print_bool(bool x) { puts(x ? "true" : "false"); }

/* Zeroed, pointer-free memory for `n` elements of `size` bytes. */
static void *lm_alloc(int64_t n, size_t size) {
    if (n < 0) { fprintf(stderr, "lamda: array length %lld is negative\n", (long long)n); exit(101); }
    if ((uint64_t)n > SIZE_MAX / size) lm_oom();
    size_t bytes = (size_t)n * size;
    void *p = lm_gc_alloc(bytes, 0);
    if (bytes <= LM_MAX_SMALL) memset(p, 0, bytes);
    return p;
}

static lm_arr lm_array(int64_t n) { return (lm_arr){ n, lm_alloc(n, sizeof(int64_t)) }; }
static lm_bytes lm_bytes_new(int64_t n) { return (lm_bytes){ n, lm_alloc(n, 1) }; }

static void lm_print_str(lm_str s) { fwrite(s.data, 1, (size_t)s.len, stdout); putchar('\n'); }

static bool lm_str_eq(lm_str a, lm_str b) {
    return a.len == b.len && memcmp(a.data, b.data, (size_t)a.len) == 0;
}

static lm_str lm_concat(lm_str a, lm_str b) {
    uint8_t *p = lm_gc_alloc((size_t)(a.len + b.len), 0);
    memcpy(p, a.data, (size_t)a.len);
    memcpy(p + a.len, b.data, (size_t)b.len);
    return (lm_str){ a.len + b.len, p };
}

/* Index of the first occurrence of `n` in `h`, or -1.
   Vectorized first/last-byte filter: 32 candidate positions at a time, keep
   those where both the needle's first byte and its last byte line up, and
   only then compare the middle. A frequent first byte alone (the `s` in
   "status=500") would otherwise stop the scan at nearly every word. */
typedef uint8_t lm_v16 __attribute__((vector_size(16)));

static inline lm_v16 lm_load16(const uint8_t *p) { lm_v16 v; memcpy(&v, p, 16); return v; }

/* Which lanes of a compare mask are set, as bits: lane k is bit k << LM_LANE_SHIFT. */
#if defined(__aarch64__)
#include <arm_neon.h>
#define LM_LANE_SHIFT 2
static inline uint64_t lm_lanes(lm_v16 m) {
    uint8x8_t narrow = vshrn_n_u16(vreinterpretq_u16_u8((uint8x16_t)m), 4);
    return vget_lane_u64(vreinterpret_u64_u8(narrow), 0) & 0x8888888888888888ull;
}
#elif defined(__SSE2__)
#include <emmintrin.h>
#define LM_LANE_SHIFT 0
static inline uint64_t lm_lanes(lm_v16 m) { return (uint64_t)_mm_movemask_epi8((__m128i)m); }
#else
#define LM_LANE_SHIFT 0
static inline uint64_t lm_lanes(lm_v16 m) {
    uint64_t bits = 0;
    for (int k = 0; k < 16; k++) bits |= (uint64_t)(m[k] != 0) << k;
    return bits;
}
#endif

static inline int lm_first_lane(uint64_t bits) { return __builtin_ctzll(bits) >> LM_LANE_SHIFT; }

static inline bool lm_match_at(const uint8_t *h, lm_str n) {
    return h[0] == n.data[0] && h[n.len - 1] == n.data[n.len - 1]
        && memcmp(h + 1, n.data + 1, (size_t)n.len - 2) == 0;
}

static int64_t lm_find(lm_str h, lm_str n) {
    if (n.len == 0) return 0;
    if (n.len > h.len) return -1;
    if (n.len == 1) {
        const uint8_t *p = memchr(h.data, n.data[0], (size_t)h.len);
        return p ? p - h.data : -1;
    }
    const int64_t last = n.len - 1, end = h.len - n.len; /* candidate starts: 0..=end */
    const lm_v16 first_b = (lm_v16){0} + n.data[0], last_b = (lm_v16){0} + n.data[last];
    int64_t i = 0;
    for (; i + 32 <= end + 1; i += 32) {
        const uint8_t *p = h.data + i;
        lm_v16 a = (lm_v16)(lm_load16(p) == first_b) & (lm_v16)(lm_load16(p + last) == last_b);
        lm_v16 b = (lm_v16)(lm_load16(p + 16) == first_b) & (lm_v16)(lm_load16(p + 16 + last) == last_b);
        if (!__builtin_reduce_max(a | b)) continue;
        /* Check only the candidate positions, not all 32. */
        for (int half = 0; half < 2; half++)
            for (uint64_t bits = lm_lanes(half ? b : a); bits; bits &= bits - 1) {
                int64_t j = i + half * 16 + lm_first_lane(bits);
                if (memcmp(h.data + j + 1, n.data + 1, (size_t)n.len - 2) == 0) return j;
            }
    }
    for (; i <= end; i++)
        if (lm_match_at(h.data + i, n)) return i;
    return -1;
}

/* Integer formatting two digits at a time; snprintf is several times slower. */
static lm_str lm_str_of(int64_t x) {
    static const char pairs[] =
        "00010203040506070809101112131415161718192021222324252627282930313233343536373839"
        "40414243444546474849505152535455565758596061626364656667686970717273747576777879"
        "8081828384858687888990919293949596979899";
    char buf[20], *end = buf + sizeof buf, *p = end;
    uint64_t u = x < 0 ? 0 - (uint64_t)x : (uint64_t)x;
    while (u >= 100) {
        unsigned d = (unsigned)(u % 100) * 2;
        u /= 100;
        *--p = pairs[d + 1];
        *--p = pairs[d];
    }
    if (u >= 10) {
        *--p = pairs[u * 2 + 1];
        *--p = pairs[u * 2];
    } else {
        *--p = (char)('0' + u);
    }
    if (x < 0) *--p = '-';
    int64_t len = end - p;
    uint8_t *out = lm_gc_alloc((size_t)len, 0);
    memcpy(out, p, (size_t)len);
    return (lm_str){ len, out };
}

/* The integer at the start of `s` (optional `-`, then digits), or 0. */
static int64_t lm_parse(lm_str s) {
    int64_t i = 0, sign = 1, v = 0;
    if (s.len > 0 && s.data[0] == '-') { sign = -1; i = 1; }
    for (; i < s.len && s.data[i] >= '0' && s.data[i] <= '9'; i++) v = v * 10 + (s.data[i] - '0');
    return sign * v;
}

static void lm_sigbus(int sig) {
    (void)sig;
    static const char msg[] = "lamda: bus error (a file passed to read_file was probably truncated while in use)\n";
    write(2, msg, sizeof msg - 1);
    _exit(101);
}

/* Files of at least 1 MB are memory-mapped instead of copied, which skips
   copying the whole file (recount: 20 ms -> 12 ms). The mapping lives in the GC
   heap as a large object and is replaced by anonymous memory when collected.
   As with any mmap, another program shrinking the file while it is in use
   raises SIGBUS, and modifying it in place changes the string's contents. */
enum { LM_MMAP_MIN = 1 << 20 };

static lm_str lm_read_file(lm_str path) {
    char *cpath = malloc((size_t)path.len + 1);
    if (!cpath) lm_oom();
    memcpy(cpath, path.data, (size_t)path.len);
    cpath[path.len] = 0;
    int fd = open(cpath, O_RDONLY);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0) { fprintf(stderr, "lamda: cannot read %s\n", cpath); exit(101); }
    int64_t n = st.st_size;
    uint8_t *p;
    if (n >= LM_MMAP_MIN && S_ISREG(st.st_mode)) {
        static bool handler;
        if (!handler) {
            signal(SIGBUS, lm_sigbus);
            handler = true;
        }
        p = lm_alloc_large((size_t)n, 0);
        if (mmap(p, (size_t)n, PROT_READ, MAP_FIXED | MAP_PRIVATE, fd, 0) == MAP_FAILED) {
            fprintf(stderr, "lamda: cannot read %s\n", cpath);
            exit(101);
        }
        lm_pages[lm_page_index(p)]->mapped = 1;
    } else {
        p = lm_gc_alloc((size_t)n, 0);
        for (int64_t done = 0; done < n;) {
            ssize_t got = read(fd, p + done, (size_t)(n - done));
            if (got <= 0) { fprintf(stderr, "lamda: cannot read %s\n", cpath); exit(101); }
            done += got;
        }
    }
    close(fd);
    free(cpath);
    return (lm_str){ n, p };
}

/* ---- Regex ---------------------------------------------------------------------
   Regex literals are compiled to DFAs by the lamda compiler. The search jumps
   to candidate starts with the literal every match begins with (via lm_find),
   or else skips bytes that cannot begin a match, then runs the DFA for the
   longest match from there. Matching is leftmost-longest. */

typedef struct {
    const uint16_t *next;   /* next[state * 256 + byte]; 0 is dead, 1 is start */
    const bool *accept;
    lm_str prefix;          /* every match starts with this */
    bool first[256];        /* bytes that can begin a match */
    bool anchor_start, anchor_end;
    uint8_t nfirst;         /* when 1..4 bytes can begin a match: how many, */
    uint8_t firsts[4];      /* and which, so the scan for them can be vectorized */
} lm_re;

/* Index of the first byte of `p[0..n)` that is one of `set[0..4)`, or n. */
static inline int64_t lm_scan_set(const uint8_t *p, int64_t n, const uint8_t *set) {
    const lm_v16 s0 = (lm_v16){0} + set[0], s1 = (lm_v16){0} + set[1];
    const lm_v16 s2 = (lm_v16){0} + set[2], s3 = (lm_v16){0} + set[3];
    int64_t i = 0;
    for (; i + 16 <= n; i += 16) {
        lm_v16 v = lm_load16(p + i);
        lm_v16 m = (lm_v16)(v == s0) | (lm_v16)(v == s1) | (lm_v16)(v == s2) | (lm_v16)(v == s3);
        if (__builtin_reduce_max(m)) return i + lm_first_lane(lm_lanes(m));
    }
    for (; i < n; i++)
        if (p[i] == set[0] || p[i] == set[1] || p[i] == set[2] || p[i] == set[3]) return i;
    return n;
}

/* End of the longest match starting at `i`, or -1. */
static inline int64_t lm_re_match_at(const lm_re *re, lm_str s, int64_t i) {
    uint32_t st = 1;
    if (re->anchor_end) {
        for (int64_t j = i; j < s.len; j++)
            if (!(st = re->next[st * 256 + s.data[j]])) return -1;
        return re->accept[st] ? s.len : -1;
    }
    int64_t end = re->accept[1] ? i : -1;
    for (int64_t j = i; j < s.len; j++) {
        if (!(st = re->next[st * 256 + s.data[j]])) break;
        if (re->accept[st]) end = j + 1;
    }
    return end;
}

/* Leftmost-longest match starting at or after `from`: returns its end and
   stores its start, or returns -1. */
static inline int64_t lm_re_search(const lm_re *re, lm_str s, int64_t from, int64_t *start) {
    if (re->anchor_start) {
        if (from > 0) return -1;
        *start = 0;
        return lm_re_match_at(re, s, 0);
    }
    for (int64_t i = from; i <= s.len; i++) {
        if (re->prefix.len > 0) {
            int64_t k = lm_find((lm_str){ s.len - i, s.data + i }, re->prefix);
            if (k < 0) return -1;
            i += k;
        } else if (re->nfirst) {
            i += lm_scan_set(s.data + i, s.len - i, re->firsts);
            if (i == s.len) return -1;
        } else if (!re->accept[1]) {
            while (i < s.len && !re->first[s.data[i]]) i++;
            if (i == s.len) return -1;
        }
        int64_t end = lm_re_match_at(re, s, i);
        if (end >= 0) {
            *start = i;
            return end;
        }
    }
    return -1;
}

static bool lm_re_matches(lm_str s, const lm_re *re) {
    int64_t start;
    return lm_re_search(re, s, 0, &start) >= 0;
}

static int64_t lm_re_find(lm_str s, const lm_re *re) {
    int64_t start;
    return lm_re_search(re, s, 0, &start) >= 0 ? start : -1;
}

static lm_str lm_re_extract(lm_str s, const lm_re *re) {
    int64_t start, end = lm_re_search(re, s, 0, &start);
    return end >= 0 ? (lm_str){ end - start, s.data + start } : (lm_str){ 0, s.data };
}

/* Calls f(acc, match) for each non-overlapping match, left to right. An empty
   match moves the search forward one byte so it cannot repeat forever. */
static int64_t lm_re_fold(lm_str s, const lm_re *re, int64_t acc, lm_fn f) {
    int64_t (*fn)(void *, int64_t, lm_str) = (int64_t (*)(void *, int64_t, lm_str))f.fn;
    int64_t pos = 0, start, end;
    while (pos <= s.len && (end = lm_re_search(re, s, pos, &start)) >= 0) {
        acc = fn(f.env, acc, (lm_str){ end - start, s.data + start });
        pos = end > start ? end : end + 1;
    }
    return acc;
}

static int64_t lm_re_count(lm_str s, const lm_re *re) {
    int64_t n = 0, pos = 0, start, end;
    while (pos <= s.len && (end = lm_re_search(re, s, pos, &start)) >= 0) {
        n++;
        pos = end > start ? end : end + 1;
    }
    return n;
}
