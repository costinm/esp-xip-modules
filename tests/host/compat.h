/* strlcpy exists on the device (newlib) but not in every glibc; the host tests that compile main/ files add it. */
#include <stddef.h>
size_t strlcpy(char *dst, const char *src, size_t size);
