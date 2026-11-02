/* SPDX-License-Identifier: Apache-2.0 */
/* Trampolines from the libc names to the runtime's `native` table (xip_native_libc.h). Only what a module's code references is linked
 * (--gc-sections). The ctype tests, tolower/toupper and abs are here as code: newlib's reach for a global table. */
#ifdef XIP_CELL_ADDR
#include <math.h>
#include <stddef.h>
#include "xip_native_libc.h"

#define N (XIP_NATIVE())

void *memcpy(void *d, const void *s, size_t n) { return N->memcpy(d, s, n); }
void *memmove(void *d, const void *s, size_t n) { return N->memmove(d, s, n); }
void *memset(void *d, int c, size_t n) { return N->memset(d, c, n); }
int memcmp(const void *a, const void *b, size_t n) { return N->memcmp(a, b, n); }
size_t strlen(const char *s) { return N->strlen(s); }
int strcmp(const char *a, const char *b) { return N->strcmp(a, b); }
int strncmp(const char *a, const char *b, size_t n) { return N->strncmp(a, b, n); }
char *strcpy(char *d, const char *s) { return N->strcpy(d, s); }
char *strncpy(char *d, const char *s, size_t n) { return N->strncpy(d, s, n); }
char *strcat(char *d, const char *s) { return N->strcat(d, s); }
char *strchr(const char *s, int c) { return N->strchr(s, c); }
char *strstr(const char *h, const char *n) { return N->strstr(h, n); }
double fmod(double x, double y) { return N->fmod(x, y); }
double floor(double x) { return N->floor(x); }
double ceil(double x) { return N->ceil(x); }
double trunc(double x) { return N->trunc(x); }
double rint(double x) { return N->rint(x); }
double sqrt(double x) { return N->sqrt(x); }
double fabs(double x) { return N->fabs(x); }
double copysign(double x, double y) { return N->copysign(x, y); }
float floorf(float x) { return N->floorf(x); }
float ceilf(float x) { return N->ceilf(x); }
float truncf(float x) { return N->truncf(x); }
float rintf(float x) { return N->rintf(x); }
float sqrtf(float x) { return N->sqrtf(x); }
float fabsf(float x) { return N->fabsf(x); }
float copysignf(float x, float y) { return N->copysignf(x, y); }

int isdigit(int c) { return c >= '0' && c <= '9'; }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int isalnum(int c) { return isdigit(c) || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
int tolower(int c) { return c >= 'A' && c <= 'Z' ? c + ('a' - 'A') : c; }
int toupper(int c) { return c >= 'a' && c <= 'z' ? c - ('a' - 'A') : c; }
int abs(int v) { return v < 0 ? -v : v; }
#endif
