#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <math.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

typedef struct { int64_t len; int64_t *data; } lm_arr;
typedef struct { int64_t len; uint8_t *data; } lm_bytes;
typedef struct { int64_t len; const uint8_t *data; } lm_str;
typedef struct { int64_t len; lm_str *data; } lm_As;   /* [str] */
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
/* Address space freed by large objects, as runs of whole pages sorted by start
   and merged with their neighbours. It is zero-filled, so new large objects and
   small-object pages are carved from it before the region grows. */
typedef struct { size_t first, n; } lm_run;
static lm_run *lm_runs;
static size_t lm_nruns, lm_runs_cap;
static size_t lm_since_gc, lm_threshold = LM_MIN_TRIGGER, lm_live, lm_collections;
static size_t lm_heap_used, lm_peak_heap;   /* small pages plus live large objects */
static uint8_t *lm_stack_bottom;
static bool lm_stress;
static struct { void *p; size_t n; } *lm_mark_stack;
static size_t lm_mark_len, lm_mark_cap;

static void lm_gc_stats(void) {
    fprintf(stderr, "lamda gc: %zu collections, peak heap %zu KB, live at last collection %zu KB, address space %zu KB\n",
            lm_collections, lm_peak_heap >> 10, lm_live >> 10, (size_t)(lm_heap_top - lm_heap) >> 10);
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

/* `n` contiguous zero-filled pages: the first free run that fits, or new space. */
static uint8_t *lm_take_pages(size_t n) {
    lm_heap_grew(n * LM_PAGE);
    for (size_t r = 0; r < lm_nruns; r++) {
        if (lm_runs[r].n < n) continue;
        uint8_t *p = lm_heap + (lm_runs[r].first << LM_PAGE_SHIFT);
        lm_runs[r].first += n;
        lm_runs[r].n -= n;
        if (lm_runs[r].n == 0) memmove(&lm_runs[r], &lm_runs[r + 1], (--lm_nruns - r) * sizeof *lm_runs);
        return p;
    }
    if ((size_t)(lm_heap_end - lm_heap_top) < n * LM_PAGE) lm_oom();
    uint8_t *p = lm_heap_top;
    lm_heap_top += n * LM_PAGE;
    return p;
}

static void lm_free_run(size_t first, size_t n) {
    size_t r = 0;
    while (r < lm_nruns && lm_runs[r].first < first) r++;
    bool joins_prev = r > 0 && lm_runs[r - 1].first + lm_runs[r - 1].n == first;
    bool joins_next = r < lm_nruns && first + n == lm_runs[r].first;
    if (joins_prev && joins_next) {
        lm_runs[r - 1].n += n + lm_runs[r].n;
        memmove(&lm_runs[r], &lm_runs[r + 1], (--lm_nruns - r) * sizeof *lm_runs);
    } else if (joins_prev) {
        lm_runs[r - 1].n += n;
    } else if (joins_next) {
        lm_runs[r].first = first;
        lm_runs[r].n += n;
    } else {
        if (lm_nruns == lm_runs_cap) {
            lm_runs_cap = lm_runs_cap ? lm_runs_cap * 2 : 64;
            lm_runs = realloc(lm_runs, lm_runs_cap * sizeof *lm_runs);
            if (!lm_runs) lm_oom();
        }
        memmove(&lm_runs[r + 1], &lm_runs[r], (lm_nruns++ - r) * sizeof *lm_runs);
        lm_runs[r] = (lm_run){ first, n };
    }
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
        /* Return the memory to the OS and leave zero-fill-on-demand pages in its place. */
        if (mmap(pg->start, pg->bytes, PROT_READ | PROT_WRITE, MAP_FIXED | MAP_PRIVATE | MAP_ANON, -1, 0) == MAP_FAILED)
            lm_oom();
        for (size_t i = 0; i < n; i++) lm_pages[first + i] = NULL;
        lm_free_run(first, n);
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
    uint8_t *mem = lm_take_pages(1);
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

/* Large objects start zeroed: they come from new address space or from a freed run. */
static void lm_gc_collect(void);

static void *lm_alloc_large(size_t size, int scan) {
    /* Small allocations check this in the slow path; large ones never take the fast path. */
    if (lm_since_gc >= lm_threshold || lm_stress) lm_gc_collect();
    size_t n = (size + LM_PAGE - 1) >> LM_PAGE_SHIFT;
    uint8_t *mem = lm_take_pages(n);
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

static __attribute__((noreturn, cold, noinline)) void lm_panic(int line, const char *msg) {
    fflush(stdout);
    fprintf(stderr, "lamda: line %d: %s\n", line, msg);
    exit(101);
}

static __attribute__((noreturn, cold, noinline)) void lm_panic_index(int64_t i, int64_t len, int line) {
    char msg[96];
    snprintf(msg, sizeof msg, "index %lld out of range for length %lld", (long long)i, (long long)len);
    lm_panic(line, msg);
}

static __attribute__((noreturn, cold, noinline)) void lm_panic_slice(int64_t lo, int64_t hi, int64_t len, int line) {
    char msg[128];
    snprintf(msg, sizeof msg, "slice %lld..%lld out of range for length %lld", (long long)lo, (long long)hi, (long long)len);
    lm_panic(line, msg);
}

static __attribute__((noreturn, cold, noinline)) void lm_panic_div(int line) { lm_panic(line, "division by zero"); }

static void lm_print_int(int64_t x) { printf("%lld\n", (long long)x); }
static void lm_print_bool(bool x) { puts(x ? "true" : "false"); }

static int64_t lm_abs(int64_t x) { return x < 0 ? -x : x; }
static int64_t lm_min(int64_t a, int64_t b) { return a < b ? a : b; }
static int64_t lm_max(int64_t a, int64_t b) { return a > b ? a : b; }

/* Truncates toward zero, saturating at the int range, NaN -> 0 (Rust's `as`). */
static int64_t lm_float_to_int(double x) {
    if (x != x) return 0;
    if (x >= 9223372036854775807.0) return INT64_MAX;
    if (x <= -9223372036854775808.0) return INT64_MIN;
    return (int64_t)x;
}

/* Formats like Rust's `{:?}` for f64: the shortest digits that round-trip, in
   plain notation with at least one decimal for 1e-4 <= |x| < 1e16, and as
   d.ddde[-]x otherwise. Returns the length written to `out` (>= 32 bytes). */
static int lm_format_float(double x, char *out) {
    if (x != x) return sprintf(out, "NaN");
    if (isinf(x)) return sprintf(out, x < 0 ? "-inf" : "inf");
    if (x == 0) return sprintf(out, signbit(x) ? "-0.0" : "0.0");
    char sci[40];
    for (int prec = 0; prec < 17; prec++) {
        snprintf(sci, sizeof sci, "%.*e", prec, x);
        if (strtod(sci, NULL) == x) break;
    }
    /* sci is [-]d[.ddd]e[+-]xx */
    char digits[24];
    int nd = 0, exp10, neg = sci[0] == '-';
    const char *p = sci + neg;
    for (; *p != 'e'; p++)
        if (*p != '.') digits[nd++] = *p;
    exp10 = atoi(p + 1);
    while (nd > 1 && digits[nd - 1] == '0') nd--;
    char *o = out;
    if (neg) *o++ = '-';
    double ax = fabs(x);
    if (ax >= 1e-4 && ax < 1e16) {
        if (exp10 < 0) {
            *o++ = '0';
            *o++ = '.';
            for (int k = -1; k > exp10; k--) *o++ = '0';
            for (int k = 0; k < nd; k++) *o++ = digits[k];
        } else {
            for (int k = 0; k <= exp10; k++) *o++ = k < nd ? digits[k] : '0';
            *o++ = '.';
            if (nd > exp10 + 1)
                for (int k = exp10 + 1; k < nd; k++) *o++ = digits[k];
            else
                *o++ = '0';
        }
    } else {
        *o++ = digits[0];
        if (nd > 1) {
            *o++ = '.';
            for (int k = 1; k < nd; k++) *o++ = digits[k];
        }
        o += sprintf(o, "e%d", exp10);
    }
    *o = 0;
    return (int)(o - out);
}

static void lm_print_float(double x) {
    char buf[48];
    lm_format_float(x, buf);
    puts(buf);
}

/* A growable byte buffer for print/str of structs and arrays (malloc'd, not GC). */
typedef struct { char *data; size_t len, cap; } lm_buf;

static void lm_buf_put(lm_buf *b, const void *p, size_t n) {
    if (b->len + n > b->cap) {
        b->cap = (b->len + n) * 2 + 64;
        b->data = realloc(b->data, b->cap);
        if (!b->data) lm_oom();
    }
    memcpy(b->data + b->len, p, n);
    b->len += n;
}

static void lm_buf_puts(lm_buf *b, const char *s) { lm_buf_put(b, s, strlen(s)); }

static void lm_buf_print(lm_buf *b) {
    lm_buf_put(b, "\n", 1);
    fwrite(b->data, 1, b->len, stdout);
    free(b->data);
}

static void lm_w_int(lm_buf *b, int64_t x) {
    char t[24];
    lm_buf_put(b, t, (size_t)snprintf(t, sizeof t, "%lld", (long long)x));
}

static void lm_w_float(lm_buf *b, double x) {
    char t[48];
    lm_buf_put(b, t, (size_t)lm_format_float(x, t));
}

static void lm_w_bool(lm_buf *b, bool x) { lm_buf_puts(b, x ? "true" : "false"); }
static void lm_w_fn(lm_buf *b, lm_fn f) { (void)f; lm_buf_puts(b, "<fn>"); }

/* A string inside a struct or array: quoted and escaped, like Rust's `{:?}`. */
static void lm_w_strq(lm_buf *b, lm_str s) {
    lm_buf_put(b, "\"", 1);
    for (int64_t i = 0; i < s.len; i++) {
        uint8_t c = s.data[i];
        switch (c) {
        case '"': lm_buf_puts(b, "\\\""); break;
        case '\\': lm_buf_puts(b, "\\\\"); break;
        case '\n': lm_buf_puts(b, "\\n"); break;
        case '\t': lm_buf_puts(b, "\\t"); break;
        case '\r': lm_buf_puts(b, "\\r"); break;
        case 0: lm_buf_puts(b, "\\0"); break;
        default:
            if (c < 0x20 || c == 0x7f) {
                char t[12];
                lm_buf_put(b, t, (size_t)snprintf(t, sizeof t, "\\u{%x}", c));
            } else {
                lm_buf_put(b, &c, 1);
            }
        }
    }
    lm_buf_put(b, "\"", 1);
}

/* Zeroed memory for `n` elements of `size` bytes; `scan` if they can hold pointers. */
static void *lm_alloc(int64_t n, size_t size, int scan) {
    if (n < 0) { fprintf(stderr, "lamda: array length %lld is negative\n", (long long)n); exit(101); }
    if ((uint64_t)n > SIZE_MAX / size) lm_oom();
    size_t bytes = (size_t)n * size;
    void *p = lm_gc_alloc(bytes, scan);
    if (bytes <= LM_MAX_SMALL) memset(p, 0, bytes);
    return p;
}

static lm_arr lm_array(int64_t n) { return (lm_arr){ n, lm_alloc(n, sizeof(int64_t), 0) }; }
static lm_bytes lm_bytes_new(int64_t n) { return (lm_bytes){ n, lm_alloc(n, 1, 0) }; }

static void lm_print_str(lm_str s) { fwrite(s.data, 1, (size_t)s.len, stdout); putchar('\n'); }

static bool lm_str_eq(lm_str a, lm_str b) {
    return a.len == b.len && memcmp(a.data, b.data, (size_t)a.len) == 0;
}

/* Byte-wise (for UTF-8, code point) order: negative, zero or positive. */
static int lm_str_cmp(lm_str a, lm_str b) {
    int c = memcmp(a.data, b.data, (size_t)(a.len < b.len ? a.len : b.len));
    return c ? c : (a.len > b.len) - (a.len < b.len);
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

static lm_str lm_str_of_float(double x) {
    char buf[48];
    int len = lm_format_float(x, buf);
    uint8_t *out = lm_gc_alloc((size_t)len, 0);
    memcpy(out, buf, (size_t)len);
    return (lm_str){ len, out };
}

/* The float at the start of `s` (after optional spaces), or 0.0. */
static double lm_parse_float(lm_str s) {
    char buf[64];
    size_t n = s.len < (int64_t)sizeof buf - 1 ? (size_t)s.len : sizeof buf - 1;
    memcpy(buf, s.data, n);
    buf[n] = 0;
    return strtod(buf, NULL);
}

static lm_str lm_buf_to_str(lm_buf *b) {
    uint8_t *out = lm_gc_alloc(b->len, 0);
    memcpy(out, b->data, b->len);
    lm_str s = { (int64_t)b->len, out };
    free(b->data);
    return s;
}

/* Splits `s` around each `sep`, like Rust's `str::split`: n separators give n + 1
   pieces (empty ones included), and the pieces share memory with `s`. An empty
   separator splits between UTF-8 characters, with an empty piece at each end. */
static lm_As lm_split(lm_str s, lm_str sep) {
    int64_t n = 1;
    if (sep.len == 0) {
        for (int64_t i = 0; i < s.len; i++) n += (s.data[i] & 0xC0) != 0x80;
        n += 1;
    } else {
        for (int64_t at = 0, k; (k = lm_find((lm_str){ s.len - at, s.data + at }, sep)) >= 0; at += k + sep.len) n++;
    }
    lm_str *parts = lm_alloc(n, sizeof(lm_str), 1);
    int64_t piece = 0, start = 0;
    if (sep.len == 0) {
        parts[piece++] = (lm_str){ 0, s.data };
        for (int64_t i = 0; i < s.len; i = start) {
            start = i + 1;
            while (start < s.len && (s.data[start] & 0xC0) == 0x80) start++;
            parts[piece++] = (lm_str){ start - i, s.data + i };
        }
        parts[piece++] = (lm_str){ 0, s.data + s.len };
    } else {
        for (int64_t k; (k = lm_find((lm_str){ s.len - start, s.data + start }, sep)) >= 0; start += k + sep.len)
            parts[piece++] = (lm_str){ k, s.data + start };
        parts[piece++] = (lm_str){ s.len - start, s.data + start };
    }
    return (lm_As){ n, parts };
}

static lm_str lm_join(lm_As xs, lm_str sep) {
    int64_t total = 0;
    for (int64_t i = 0; i < xs.len; i++) total += xs.data[i].len + (i ? sep.len : 0);
    uint8_t *out = lm_gc_alloc((size_t)total, 0), *o = out;
    for (int64_t i = 0; i < xs.len; i++) {
        if (i) { memcpy(o, sep.data, (size_t)sep.len); o += sep.len; }
        memcpy(o, xs.data[i].data, (size_t)xs.data[i].len);
        o += xs.data[i].len;
    }
    return (lm_str){ total, out };
}

/* The integer at the start of `s` (optional `-`, then digits), or 0. */
static int64_t lm_parse(lm_str s) {
    int64_t i = 0, sign = 1, v = 0;
    if (s.len > 0 && s.data[0] == '-') { sign = -1; i = 1; }
    for (; i < s.len && s.data[i] >= '0' && s.data[i] <= '9'; i++) v = v * 10 + (s.data[i] - '0');
    return sign * v;
}

static lm_str lm_read_file(lm_str path) {
    char *cpath = malloc((size_t)path.len + 1);
    if (!cpath) lm_oom();
    memcpy(cpath, path.data, (size_t)path.len);
    cpath[path.len] = 0;
    int fd = open(cpath, O_RDONLY);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0) { fprintf(stderr, "lamda: cannot read %s\n", cpath); exit(101); }
    int64_t n = st.st_size;
    uint8_t *p = lm_gc_alloc((size_t)n, 0);
    for (int64_t done = 0; done < n;) {
        ssize_t got = read(fd, p + done, (size_t)(n - done));
        if (got <= 0) { fprintf(stderr, "lamda: cannot read %s\n", cpath); exit(101); }
        done += got;
    }
    close(fd);
    free(cpath);
    return (lm_str){ n, p };
}

/* ---- Regex ---------------------------------------------------------------------
   Regex literals are compiled by the lamda compiler (src/regex.rs), with the
   semantics of Rust's regex::bytes. A search runs a forward DFA to find where the
   leftmost-first match ends, skipping ahead with a prefilter whenever no match is
   in progress, then a reverse DFA from that end to find where it starts. Capture
   groups come from a Pike VM run over just the match. Patterns without DFAs, and
   Unicode word boundaries on non-ASCII text (where the DFA gives up), are searched
   with the Pike VM, which is slower but still linear. */

typedef struct { uint8_t op, lo, hi, look; int32_t x, y; } lm_rinst;
enum { LM_RANGE, LM_SPLIT, LM_SAVE, LM_LOOK, LM_MATCH, LM_FAIL };
enum {
    LM_LOOK_START, LM_LOOK_END, LM_LOOK_START_LF, LM_LOOK_END_LF, LM_LOOK_START_CRLF, LM_LOOK_END_CRLF,
    LM_LOOK_WORD_A, LM_LOOK_NOT_WORD_A, LM_LOOK_WORD_U, LM_LOOK_NOT_WORD_U, LM_LOOK_WSTART_A, LM_LOOK_WEND_A,
    LM_LOOK_WSTART_U, LM_LOOK_WEND_U, LM_LOOK_WSTART_HALF_A, LM_LOOK_WEND_HALF_A, LM_LOOK_WSTART_HALF_U,
    LM_LOOK_WEND_HALF_U,
};

typedef struct {
    const uint8_t *classes;        /* byte -> class; class stride-1 is end of text */
    int32_t stride;
    /* Dense DFAs (NULL: Pike VM). A state is its offset into the table, and
       table[state + class] is the next state. State 0 is dead, `stride` gives up
       on non-ASCII text, [match[0], match[1]) are reached when a match ended just
       before the byte that led there, and [match[1], match[2]) have no match in
       progress, so the search may skip ahead. Below match[2] is special. */
    const uint32_t *fwd, *rev;
    uint32_t fwd_start[4], rev_start[4];
    uint32_t fwd_match[3], rev_match[3];
    bool quit_non_ascii;           /* the DFAs give up on bytes >= 0x80 */
    const lm_rinst *prog;
    int32_t prog_len, start, ngroups;
    const char *const *names;      /* group names, NULL for unnamed groups */
    const uint32_t *uword;         /* Unicode \w ranges as pairs, for the Pike VM */
    int32_t nuword;
    lm_str prefix;                 /* every match starts with this */
    uint8_t nfirst, firsts[4];     /* when 1..4 bytes can start a match, scanned with SIMD */
    bool first[256];
    bool can_be_empty, anchored_start;
    lm_str pattern;
} lm_re;

enum { LM_DEAD = 0, LM_QUIT = 1 };

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

static inline bool lm_is_word_byte(uint8_t b) {
    return (b >= '0' && b <= '9') || (b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') || b == '_';
}

/* DFA start context of position i, from the byte before it (or the text start). */
static inline int lm_re_ctx(lm_str s, int64_t i) {
    if (i == 0) return 0;
    uint8_t b = s.data[i - 1];
    return b == '\n' ? 1 : lm_is_word_byte(b) ? 2 : 3;
}

/* The same for the reverse DFA, reading position i backwards: the byte after it. */
static inline int lm_re_rctx(lm_str s, int64_t i) {
    if (i == s.len) return 0;
    uint8_t b = s.data[i];
    return b == '\n' ? 1 : lm_is_word_byte(b) ? 2 : 3;
}

/* Next position >= i where a match could start, or -1. */
static inline int64_t lm_re_candidate(const lm_re *re, lm_str s, int64_t i) {
    if (re->prefix.len) {
        int64_t k = lm_find((lm_str){ s.len - i, s.data + i }, re->prefix);
        return k < 0 ? -1 : i + k;
    }
    if (re->nfirst) {
        int64_t p = i + lm_scan_set(s.data + i, s.len - i, re->firsts);
        return p == s.len ? -1 : p;
    }
    while (i < s.len && !re->first[s.data[i]]) i++;
    return i == s.len ? -1 : i;
}

/* End of the leftmost-first match starting at or after `from`; -1 if there is
   none, -2 if the DFA gave up on non-ASCII text. */
static int64_t lm_re_fwd(const lm_re *re, lm_str s, int64_t from) {
    const uint32_t *t = re->fwd;
    const uint8_t *cls = re->classes;
    const uint32_t quit = (uint32_t)re->stride, mlo = re->fwd_match[0], mhi = re->fwd_match[1], special = re->fwd_match[2];
    if (re->anchored_start && from > 0) return -1;
    if (re->quit_non_ascii && from > 0 && s.data[from - 1] >= 0x80) return -2;
    int64_t i = from, end = -1;
    uint32_t st = re->fwd_start[lm_re_ctx(s, i)];
    if (st >= mhi && st < special) goto skip;
    while (i < s.len) {
        st = t[st + cls[s.data[i]]];
        i++;
        if (__builtin_expect(st < special, 0)) {
            if (st < mlo) return st == 0 ? end : -2;
            if (st < mhi) {
                end = i - 1;
                continue;
            }
        skip:;
            /* No match is in progress: jump to the next place one could start. */
            int64_t p = lm_re_candidate(re, s, i);
            if (p < 0) return end;
            if (p > i) {
                i = p;
                if (re->quit_non_ascii && s.data[i - 1] >= 0x80) return -2;
                st = re->fwd_start[lm_re_ctx(s, i)];
            }
        }
    }
    st = t[st + quit - 1];
    if (st >= mlo && st < mhi) end = s.len;
    return end;
}

/* Start of the leftmost match that ends at `end`, no earlier than `from`. */
static int64_t lm_re_rev(const lm_re *re, lm_str s, int64_t from, int64_t end) {
    const uint32_t *t = re->rev;
    const uint8_t *cls = re->classes;
    const uint32_t quit = (uint32_t)re->stride, mlo = re->rev_match[0], mhi = re->rev_match[1];
    if (re->quit_non_ascii && end < s.len && s.data[end] >= 0x80) return -2;
    uint32_t st = re->rev_start[lm_re_rctx(s, end)];
    int64_t start = -1;
    for (int64_t i = end - 1; i >= from; i--) {
        st = t[st + cls[s.data[i]]];
        if (__builtin_expect(st < mhi, 0)) {
            if (st < mlo) return st == 0 ? start : -2;
            start = i + 1;
        }
    }
    st = t[st + (from > 0 ? cls[s.data[from - 1]] : quit - 1)];
    if (st == quit) return -2;
    if (st >= mlo && st < mhi) start = from;
    return start;
}

/* ---- Pike VM ---- */

/* The code point ending just before `at` (backwards) or starting at `at`, or -1
   at the edge of the text or on invalid UTF-8. */
static int32_t lm_utf8_at(lm_str s, int64_t at, bool backwards) {
    int64_t i = at;
    if (backwards) {
        if (i == 0) return -1;
        int64_t lo = i - 4 < 0 ? 0 : i - 4;
        i--;
        while (i > lo && (s.data[i] & 0xC0) == 0x80) i--;
    } else if (i >= s.len) {
        return -1;
    }
    uint8_t b = s.data[i];
    int n = b < 0x80 ? 1 : (b & 0xE0) == 0xC0 ? 2 : (b & 0xF0) == 0xE0 ? 3 : (b & 0xF8) == 0xF0 ? 4 : 0;
    if (!n || i + n > s.len || (backwards && i + n != at)) return -1;
    int32_t cp = n == 1 ? b : b & (0x7F >> n);
    for (int k = 1; k < n; k++) {
        if ((s.data[i + k] & 0xC0) != 0x80) return -1;
        cp = cp << 6 | (s.data[i + k] & 0x3F);
    }
    return cp;
}

static bool lm_is_word_cp(const lm_re *re, int32_t cp) {
    if (cp < 0) return false;
    if (cp < 0x80) return lm_is_word_byte((uint8_t)cp);
    int32_t lo = 0, hi = re->nuword - 1;
    while (lo <= hi) {
        int32_t mid = (lo + hi) / 2;
        if ((uint32_t)cp < re->uword[2 * mid]) hi = mid - 1;
        else if ((uint32_t)cp > re->uword[2 * mid + 1]) lo = mid + 1;
        else return true;
    }
    return false;
}

static bool lm_look(const lm_re *re, int look, lm_str s, int64_t at) {
    bool pa = at > 0 && lm_is_word_byte(s.data[at - 1]), na = at < s.len && lm_is_word_byte(s.data[at]);
    bool pu = false, nu = false, split = false;
    if (look >= LM_LOOK_WORD_U) {
        int32_t before = lm_utf8_at(s, at, true), after = lm_utf8_at(s, at, false);
        pu = lm_is_word_cp(re, before);
        nu = lm_is_word_cp(re, after);
        /* Like regex-automata, the negated and half Unicode assertions never match
           where a side cannot be decoded, so they never split a character. */
        split = (at > 0 && before < 0) || (at < s.len && after < 0);
    }
    switch (look) {
    case LM_LOOK_START: return at == 0;
    case LM_LOOK_END: return at == s.len;
    case LM_LOOK_START_LF: return at == 0 || s.data[at - 1] == '\n';
    case LM_LOOK_END_LF: return at == s.len || s.data[at] == '\n';
    case LM_LOOK_START_CRLF:
        return at == 0 || s.data[at - 1] == '\n' || (s.data[at - 1] == '\r' && !(at < s.len && s.data[at] == '\n'));
    case LM_LOOK_END_CRLF:
        return at == s.len || s.data[at] == '\r' || (s.data[at] == '\n' && !(at > 0 && s.data[at - 1] == '\r'));
    case LM_LOOK_WORD_A: return pa != na;
    case LM_LOOK_NOT_WORD_A: return pa == na;
    case LM_LOOK_WORD_U: return pu != nu;
    case LM_LOOK_NOT_WORD_U: return !split && pu == nu;
    case LM_LOOK_WSTART_A: return !pa && na;
    case LM_LOOK_WEND_A: return pa && !na;
    case LM_LOOK_WSTART_U: return !pu && nu;
    case LM_LOOK_WEND_U: return pu && !nu;
    case LM_LOOK_WSTART_HALF_A: return !pa;
    case LM_LOOK_WEND_HALF_A: return !na;
    case LM_LOOK_WSTART_HALF_U: return !(at > 0 && lm_utf8_at(s, at, true) < 0) && !pu;
    case LM_LOOK_WEND_HALF_U: return !(at < s.len && lm_utf8_at(s, at, false) < 0) && !nu;
    }
    return false;
}

typedef struct { int32_t *dense, *sparse, n; int64_t *slots; } lm_threads;
typedef struct { int32_t pc, slot; int64_t val; } lm_frame; /* pc < 0: restore slots[slot] = val */

/* Adds the threads reachable from `pc0` at `at`, in priority order, each with a
   copy of `slots` as updated by the Save instructions on its way. */
static void lm_pike_add(const lm_re *re, lm_threads *l, int32_t pc0, int64_t *slots, int nslots, lm_str s,
                        int64_t at, lm_frame *stk) {
    int64_t top = 0;
    stk[top++] = (lm_frame){ pc0, 0, 0 };
    while (top) {
        lm_frame f = stk[--top];
        if (f.pc < 0) {
            slots[f.slot] = f.val;
            continue;
        }
        /* Sparse-set membership: `sparse` is uninitialized, so check the slot it names. */
        uint32_t k = (uint32_t)l->sparse[f.pc];
        if (k < (uint32_t)l->n && l->dense[k] == f.pc) continue;
        l->sparse[f.pc] = l->n;
        l->dense[l->n++] = f.pc;
        const lm_rinst *in = &re->prog[f.pc];
        switch (in->op) {
        case LM_RANGE:
        case LM_MATCH:
            memcpy(&l->slots[(int64_t)f.pc * nslots], slots, (size_t)nslots * sizeof *slots);
            break;
        case LM_SPLIT:
            stk[top++] = (lm_frame){ in->y, 0, 0 };
            stk[top++] = (lm_frame){ in->x, 0, 0 };
            break;
        case LM_SAVE:
            if (in->y < nslots) {
                stk[top++] = (lm_frame){ -1, in->y, slots[in->y] };
                slots[in->y] = at;
            }
            stk[top++] = (lm_frame){ in->x, 0, 0 };
            break;
        case LM_LOOK:
            if (lm_look(re, in->look, s, at)) stk[top++] = (lm_frame){ in->x, 0, 0 };
            break;
        }
    }
}

/* Leftmost-first match starting at or after `from` (exactly at `from` when
   anchored). Fills `out` with the first `nslots` capture slots. */
static bool lm_pike(const lm_re *re, lm_str s, int64_t from, bool anchored, int64_t *out, int nslots) {
    const int32_t n = re->prog_len;
    size_t per = (size_t)n * (2 * sizeof(int32_t) + (size_t)nslots * sizeof(int64_t));
    char *mem = malloc(2 * per + (2 * (size_t)n + 2) * sizeof(lm_frame) + (size_t)nslots * sizeof(int64_t));
    if (!mem) lm_oom();
    lm_threads lists[2];
    for (int k = 0; k < 2; k++) {
        char *base = mem + k * per;
        lists[k] = (lm_threads){ (int32_t *)base, (int32_t *)base + n, 0, (int64_t *)(base + 2 * (size_t)n * sizeof(int32_t)) };
    }
    lm_frame *stk = (lm_frame *)(mem + 2 * per);
    int64_t *tmp = (int64_t *)(stk + 2 * (size_t)n + 2);
    lm_threads *cur = &lists[0], *next = &lists[1];
    bool matched = false;
    for (int64_t at = from;; at++) {
        if (!matched && (at == from || !anchored) && !(re->anchored_start && at > 0)) {
            for (int k = 0; k < nslots; k++) tmp[k] = -1;
            lm_pike_add(re, cur, re->start, tmp, nslots, s, at, stk);
        }
        next->n = 0;
        for (int32_t k = 0; k < cur->n; k++) {
            int32_t pc = cur->dense[k];
            const lm_rinst *in = &re->prog[pc];
            if (in->op == LM_MATCH) {
                matched = true;
                memcpy(out, &cur->slots[(int64_t)pc * nslots], (size_t)nslots * sizeof *out);
                break; /* lower-priority threads lose (leftmost-first) */
            }
            if (in->op == LM_RANGE && at < s.len && s.data[at] >= in->lo && s.data[at] <= in->hi) {
                memcpy(tmp, &cur->slots[(int64_t)pc * nslots], (size_t)nslots * sizeof *tmp);
                lm_pike_add(re, next, in->x, tmp, nslots, s, at + 1, stk);
            }
        }
        if (at >= s.len || (next->n == 0 && (matched || anchored || re->anchored_start))) break;
        lm_threads *t = cur;
        cur = next;
        next = t;
    }
    free(mem);
    return matched;
}

/* ---- Searching ---- */

/* Leftmost-first match starting at or after `from`: returns its end and stores
   its start, or returns -1. */
static int64_t lm_re_search(const lm_re *re, lm_str s, int64_t from, int64_t *start) {
    if (re->fwd) {
        int64_t end = lm_re_fwd(re, s, from);
        if (end == -1) return -1;
        if (end >= 0) {
            int64_t st = lm_re_rev(re, s, from, end);
            if (st >= 0) {
                *start = st;
                return end;
            }
        }
    }
    int64_t slots[2];
    if (!lm_pike(re, s, from, false, slots, 2)) return -1;
    *start = slots[0];
    return slots[1];
}

/* Like lm_re_search, when only the end is needed (a match cannot be empty). */
static int64_t lm_re_search_end(const lm_re *re, lm_str s, int64_t from) {
    if (re->fwd) {
        int64_t end = lm_re_fwd(re, s, from);
        if (end != -2) return end;
    }
    int64_t start;
    return lm_re_search(re, s, from, &start);
}

/* Capture slots (two per group, -1 if the group did not take part) of the match
   that starts at `start` and ends at `end`. */
static void lm_re_groups(const lm_re *re, lm_str s, int64_t start, int64_t end, int64_t *slots) {
    if (re->ngroups == 1) {
        slots[0] = start;
        slots[1] = end;
        return;
    }
    lm_pike(re, s, start, true, slots, 2 * re->ngroups);
}

/* Iterates matches like Rust's find_iter: the next search starts at the previous
   match's end, and an empty match there is skipped. */
static inline bool lm_re_next(lm_str s, const lm_re *re, int64_t *pos, int64_t *last_end, int64_t *start, int64_t *end) {
    for (int64_t from = *pos; from <= s.len; from++) {
        int64_t e = lm_re_search(re, s, from, start);
        if (e < 0) return false;
        if (e == *start && e == *last_end) continue;
        *end = *pos = *last_end = e;
        return true;
    }
    return false;
}

static bool lm_re_matches(lm_str s, const lm_re *re) {
    if (!re->can_be_empty) return lm_re_search_end(re, s, 0) >= 0;
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

static int64_t lm_re_count(lm_str s, const lm_re *re) {
    int64_t n = 0;
    if (!re->can_be_empty) {
        /* Matches are never empty, so each search starts where the last ended. */
        for (int64_t pos = 0, end; pos <= s.len && (end = lm_re_search_end(re, s, pos)) >= 0; pos = end) n++;
        return n;
    }
    int64_t pos = 0, last = -1, start, end;
    while (lm_re_next(s, re, &pos, &last, &start, &end)) n++;
    return n;
}

static lm_str lm_slot_str(lm_str s, const int64_t *slots, int g) {
    return slots[2 * g] < 0 ? (lm_str){ 0, s.data } : (lm_str){ slots[2 * g + 1] - slots[2 * g], s.data + slots[2 * g] };
}

/* The groups of the match [start, end) as a GC array of strings. */
static lm_As lm_re_group_strs(const lm_re *re, lm_str s, int64_t start, int64_t end) {
    int64_t slots[2 * re->ngroups];
    lm_re_groups(re, s, start, end, slots);
    lm_str *parts = lm_alloc(re->ngroups, sizeof(lm_str), 1);
    for (int g = 0; g < re->ngroups; g++) parts[g] = lm_slot_str(s, slots, g);
    return (lm_As){ re->ngroups, parts };
}

/* The first match's groups (group 0 is the whole match; "" for a group that did
   not take part), or an empty array without a match. */
static lm_As lm_re_captures(lm_str s, const lm_re *re) {
    int64_t start, end = lm_re_search(re, s, 0, &start);
    if (end < 0) return (lm_As){ 0, NULL };
    return lm_re_group_strs(re, s, start, end);
}

typedef struct { int64_t len; lm_As *data; } lm_AAs; /* [[str]] */

static lm_As lm_re_all_matches(lm_str s, const lm_re *re) {
    int64_t cap = 16, n = 0, pos = 0, last = -1, start, end;
    lm_str *out = lm_alloc(cap, sizeof(lm_str), 1);
    while (lm_re_next(s, re, &pos, &last, &start, &end)) {
        if (n == cap) {
            lm_str *bigger = lm_alloc(cap *= 2, sizeof(lm_str), 1);
            memcpy(bigger, out, (size_t)n * sizeof *out);
            out = bigger;
        }
        out[n++] = (lm_str){ end - start, s.data + start };
    }
    return (lm_As){ n, out };
}

static lm_AAs lm_re_all_captures(lm_str s, const lm_re *re) {
    int64_t cap = 16, n = 0, pos = 0, last = -1, start, end;
    lm_As *out = lm_alloc(cap, sizeof(lm_As), 1);
    while (lm_re_next(s, re, &pos, &last, &start, &end)) {
        if (n == cap) {
            lm_As *bigger = lm_alloc(cap *= 2, sizeof(lm_As), 1);
            memcpy(bigger, out, (size_t)n * sizeof *out);
            out = bigger;
        }
        out[n] = lm_re_group_strs(re, s, start, end);
        n++;
    }
    return (lm_AAs){ n, out };
}

/* The group a replacement refers to by `name` (digits or a group name), or -1. */
static int lm_re_group_named(const lm_re *re, const uint8_t *name, int64_t len) {
    bool digits = len > 0;
    int64_t index = 0;
    for (int64_t i = 0; i < len && digits; i++) {
        digits = name[i] >= '0' && name[i] <= '9';
        if (index < 1000000) index = index * 10 + (name[i] - '0');
    }
    if (digits) return index < re->ngroups ? (int)index : -1;
    for (int g = 0; g < re->ngroups; g++)
        if (re->names[g] && (int64_t)strlen(re->names[g]) == len && memcmp(re->names[g], name, (size_t)len) == 0) return g;
    return -1;
}

/* Replaces every match, expanding `$1`, `$name`, `${name}` and `$$` in `rep`
   the way Rust's Regex::replace_all does. */
static lm_str lm_re_replace(lm_str s, const lm_re *re, lm_str rep) {
    lm_buf b = { 0 };
    int64_t pos = 0, last = -1, copied = 0, start, end;
    int64_t slots[2 * re->ngroups];
    while (lm_re_next(s, re, &pos, &last, &start, &end)) {
        lm_buf_put(&b, s.data + copied, (size_t)(start - copied));
        copied = end;
        bool groups = memchr(rep.data, '$', (size_t)rep.len) != NULL;
        if (groups) lm_re_groups(re, s, start, end, slots);
        for (int64_t i = 0; i < rep.len;) {
            if (rep.data[i] != '$' || i + 1 == rep.len) {
                lm_buf_put(&b, rep.data + i++, 1);
                continue;
            }
            if (rep.data[i + 1] == '$') {
                lm_buf_put(&b, "$", 1);
                i += 2;
                continue;
            }
            int64_t name = i + 1, len = 0, after;
            if (rep.data[name] == '{') {
                const uint8_t *close = memchr(rep.data + name, '}', (size_t)(rep.len - name));
                if (!close) {
                    lm_buf_put(&b, rep.data + i++, 1);
                    continue;
                }
                name++;
                len = close - (rep.data + name);
                after = name + len + 1;
            } else {
                while (name + len < rep.len && (lm_is_word_byte(rep.data[name + len]))) len++;
                after = name + len;
                if (len == 0) {
                    lm_buf_put(&b, rep.data + i++, 1);
                    continue;
                }
            }
            int g = lm_re_group_named(re, rep.data + name, len);
            if (g >= 0) {
                lm_str part = lm_slot_str(s, slots, g);
                lm_buf_put(&b, part.data, (size_t)part.len);
            }
            i = after;
        }
    }
    lm_buf_put(&b, s.data + copied, (size_t)(s.len - copied));
    return lm_buf_to_str(&b);
}

static void lm_w_re(lm_buf *b, const lm_re *re) {
    lm_buf_put(b, "`", 1);
    lm_buf_put(b, re->pattern.data, (size_t)re->pattern.len);
    lm_buf_put(b, "`", 1);
}
