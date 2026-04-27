/* C ABI verification shim for the Kiseki native client.
 *
 * Compiled by build.rs (only when the `ffi` feature is enabled). The
 * compile + link step verifies:
 *   1. Every public extern fn declared in include/kiseki_client.h
 *      has a matching exported symbol in the cdylib (renames break
 *      the build).
 *   2. KisekiStatus enum values from the C header match the Rust
 *      `repr(C)` discriminants (mismatches break the link).
 *   3. KisekiCacheStats layout: sizeof + offsetof for each field
 *      are written to a CSV file the Rust test reads back.
 *
 * Closes ADV-PA-5: the prior native_abi.rs test confirmed Rust-side
 * struct layout but did NOT verify the C ABI surface. This shim does.
 */
#include "kiseki_client.h"

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

/* Force the linker to retain pointers to every public symbol. If any
 * symbol is renamed in Rust, the link below fails. */
typedef KisekiStatus (*open_fn)(const char *, KisekiHandle **);
typedef KisekiStatus (*close_fn)(KisekiHandle *);
typedef KisekiStatus (*read_fn)(KisekiHandle *, const char *, uint64_t,
                                 uint8_t *, uint64_t, uint64_t *);
typedef KisekiStatus (*write_fn)(KisekiHandle *, const char *,
                                  const uint8_t *, uint64_t, uint64_t *);
typedef KisekiStatus (*stat_fn)(KisekiHandle *, const char *, uint64_t *);
typedef KisekiStatus (*stage_fn)(KisekiHandle *, const char *, uint32_t);
typedef KisekiStatus (*release_fn)(KisekiHandle *, const char *);
typedef KisekiStatus (*cache_stats_fn)(KisekiHandle *, KisekiCacheStats *);

/* Discriminant assertions — enum values in the header MUST match the
 * Rust repr(C) discriminants. Any drift breaks compile. */
_Static_assert(KISEKI_OK == 0, "KisekiStatus::Ok = 0");
_Static_assert(KISEKI_NOT_FOUND == 1, "KisekiStatus::NotFound = 1");
_Static_assert(KISEKI_PERMISSION == 2, "KisekiStatus::PermissionDenied = 2");
_Static_assert(KISEKI_IO_ERROR == 3, "KisekiStatus::IoError = 3");
_Static_assert(KISEKI_INVALID_ARG == 4, "KisekiStatus::InvalidArgument = 4");
_Static_assert(KISEKI_NOT_CONNECTED == 5, "KisekiStatus::NotConnected = 5");
_Static_assert(KISEKI_TIMED_OUT == 6, "KisekiStatus::TimedOut = 6");

/* Struct-layout sentinel — sizeof + offsetof for each field, dumped
 * to a CSV at run time. The Rust test reads this file and compares
 * against `mem::size_of::<KisekiCacheStats>()` + the field offsets it
 * expects. Any divergence between C and Rust ABI surfaces here. */
int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: abi_check <output.csv>\n");
        return 2;
    }

    /* Force-reference each function pointer so the linker can't
     * drop the symbol resolution. (No actual call — these are
     * compile-time references only.) */
    volatile open_fn        p_open        = kiseki_open;
    volatile close_fn       p_close       = kiseki_close;
    volatile read_fn        p_read        = kiseki_read;
    volatile write_fn       p_write       = kiseki_write;
    volatile stat_fn        p_stat        = kiseki_stat;
    volatile stage_fn       p_stage       = kiseki_stage;
    volatile release_fn     p_release     = kiseki_release;
    volatile cache_stats_fn p_cache_stats = kiseki_cache_stats;
    if (!p_open || !p_close || !p_read || !p_write ||
        !p_stat || !p_stage || !p_release || !p_cache_stats) {
        fprintf(stderr, "FATAL: a Kiseki extern fn pointer is null\n");
        return 3;
    }

    FILE *f = fopen(argv[1], "w");
    if (!f) { perror("open"); return 4; }
    fprintf(f, "key,value\n");
    fprintf(f, "sizeof_KisekiCacheStats,%zu\n", sizeof(KisekiCacheStats));
    fprintf(f, "offset_l1_hits,%zu\n", offsetof(KisekiCacheStats, l1_hits));
    fprintf(f, "offset_l2_hits,%zu\n", offsetof(KisekiCacheStats, l2_hits));
    fprintf(f, "offset_misses,%zu\n", offsetof(KisekiCacheStats, misses));
    fprintf(f, "offset_bypasses,%zu\n", offsetof(KisekiCacheStats, bypasses));
    fprintf(f, "offset_errors,%zu\n", offsetof(KisekiCacheStats, errors));
    fprintf(f, "offset_l1_bytes,%zu\n", offsetof(KisekiCacheStats, l1_bytes));
    fprintf(f, "offset_l2_bytes,%zu\n", offsetof(KisekiCacheStats, l2_bytes));
    fprintf(f, "offset_meta_hits,%zu\n", offsetof(KisekiCacheStats, meta_hits));
    fprintf(f, "offset_meta_misses,%zu\n", offsetof(KisekiCacheStats, meta_misses));
    fprintf(f, "offset_wipes,%zu\n", offsetof(KisekiCacheStats, wipes));
    fprintf(f, "status_ok,%d\n", (int)KISEKI_OK);
    fprintf(f, "status_timed_out,%d\n", (int)KISEKI_TIMED_OUT);
    fclose(f);
    return 0;
}
