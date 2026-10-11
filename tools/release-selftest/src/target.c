/*
 * Compiled by build.rs with the target's C compiler; see main.rs for why.
 * The headers come from the target's C library, so this also checks that the
 * cross toolchain finds them.
 */
#include <limits.h>
#include <stdint.h>

/* Pointer width in bits as the C compiler saw the target. */
uint32_t release_selftest_c_pointer_bits(void) {
    return (uint32_t)(sizeof(void *) * CHAR_BIT);
}
