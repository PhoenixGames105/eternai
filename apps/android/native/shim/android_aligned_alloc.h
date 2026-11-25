/*
 * Bionic only declares aligned_alloc from API 28, but the app targets API 26.
 * llama.cpp's checkpoint allocator calls it unconditionally on non-MSVC
 * platforms, which breaks the cross-compile.
 *
 * Force-included via -include when configuring the llama.cpp build (see
 * build-android.ps1). Because it is included before the system headers, every
 * call resolves to this posix_memalign wrapper instead, and the released
 * pointer still goes to free(), which is the correct pairing for
 * posix_memalign. No upstream source is modified.
 */
#ifndef ETERN_ANDROID_ALIGNED_ALLOC_SHIM_H
#define ETERN_ANDROID_ALIGNED_ALLOC_SHIM_H

#include <stdlib.h>

#if defined(__ANDROID__) && defined(__ANDROID_API__) && __ANDROID_API__ < 28
#    ifndef aligned_alloc
static inline void * etern_aligned_alloc(size_t alignment, size_t size) {
    /* posix_memalign requires a power-of-two multiple of sizeof(void *). */
    size_t align = alignment < sizeof(void *) ? sizeof(void *) : alignment;
    void * p     = NULL;
    if (posix_memalign(&p, align, size) != 0) {
        return NULL;
    }
    return p;
}
#        define aligned_alloc etern_aligned_alloc
#    endif
#endif

#endif /* ETERN_ANDROID_ALIGNED_ALLOC_SHIM_H */
