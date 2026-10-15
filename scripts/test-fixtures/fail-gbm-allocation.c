// Force the mirrored-resize test onto wl_shm after its first GPU frames.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <gbm.h>

struct gbm_bo *gbm_bo_create_with_modifiers2(struct gbm_device *gbm,
    uint32_t width, uint32_t height, uint32_t format,
    const uint64_t *modifiers, const unsigned int count, uint32_t flags) {
    if (width == 640 && height == 480) {
        errno = ENOMEM;
        return NULL;
    }
    struct gbm_bo *(*real_create)(struct gbm_device *, uint32_t, uint32_t,
        uint32_t, const uint64_t *, unsigned int, uint32_t) =
        dlsym(RTLD_NEXT, "gbm_bo_create_with_modifiers2");
    return real_create(gbm, width, height, format, modifiers, count, flags);
}

struct gbm_bo *gbm_bo_create(struct gbm_device *gbm, uint32_t width,
    uint32_t height, uint32_t format, uint32_t flags) {
    if (width == 640 && height == 480) {
        errno = ENOMEM;
        return NULL;
    }
    struct gbm_bo *(*real_create)(struct gbm_device *, uint32_t, uint32_t,
        uint32_t, uint32_t) = dlsym(RTLD_NEXT, "gbm_bo_create");
    return real_create(gbm, width, height, format, flags);
}
