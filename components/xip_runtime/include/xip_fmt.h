/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <stdarg.h>
#include <stddef.h>

/* C99 snprintf semantics for the conversions listed in xip_fmt.c; -1 for any other. */
int xip_fmt_vsnprintf(char *s, size_t n, const char *fmt, va_list ap);
int xip_fmt_snprintf(char *s, size_t n, const char *fmt, ...);
