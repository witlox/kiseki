/* kiseki-fuse-sys bindgen wrapper.
 *
 * Includes the libfuse 3.x headers we need bindings for. We pull
 * in `fuse.h` (high-level API + transitive `fuse_common.h`) and
 * `fuse_lowlevel.h` (low-level API; the surface kiseki-fuse drives).
 * Direct inclusion of `fuse_common.h` is rejected by libfuse 3.18+
 * with `"Never include <fuse_common.h> directly"` — it must be
 * pulled in via `fuse.h` or `fuse_lowlevel.h`.
 *
 * Defining FUSE_USE_VERSION = 31 selects the FUSE 3.10+ API
 * surface (FUSE_SYNCFS opcode 50 dispatch is in this version
 * band; see ADR-043 §D2 libfuse row).
 */
#define FUSE_USE_VERSION 31

#include <fuse3/fuse.h>
#include <fuse3/fuse_lowlevel.h>
