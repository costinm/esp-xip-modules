/* SPDX-License-Identifier: Apache-2.0 */
/* The runtime's formatter: `snprintf` for modules (xip_native.c hands it out as `snprintf` of the `native` table) and for the host
 * build of C modules (crates/berry-c compiles this very file, so the host tests run the formatter a device carries).
 *
 * It covers the conversions the Berry interpreter produces - %c %d %i %u %o %x %X %s %p %f %e %g F/E/G %% with flags, width and
 * precision, and the l/ll length modifiers - and answers -1 for anything else. Precision is capped at 30 digits. The float
 * conversions render by decimal scaling in double arithmetic: right to the leading digits like the reference formatter, but the
 * last digit of a 17-digit round-trip can differ; display text, not numbers. It needs no C library float support (newlib-nano
 * has none) and allocates nothing.
 */
#include "xip_fmt.h"

#include <float.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>

/* ---- snprintf ---- the core below compiles on the host as well (xip_fmt_vsnprintf, no libc name
 * interposed), so host tests run the very formatter a device image carries; the device branch at
 * the end of the file hands it out under the libc names. */

#define XIP_FMT_PREC_MAX 30                     /* a longer fraction than this is noise */
#define XIP_FMT_BUF_MAX  384                    /* %f of 1e308 is 309 digits, of 1e-320 is 319 zeros, plus 30 of precision */
#define XIP_FMT_DIG_MAX  46                     /* digits generated per number, buffer bound */

typedef struct {
    char *s;
    size_t n, used;
} xip_sink;

static void xip_put(xip_sink *o, char c)
{
    if (o->used < o->n)
        o->s[o->used] = c;
    o->used++;
}

static void xip_pad(xip_sink *o, const char *b, size_t len, int width, char pad, bool left)
{
    int filler = width > 0 && (size_t)width > len ? width - (int)len : 0;
    if (!left)
        while (filler-- > 0)
            xip_put(o, pad);
    for (size_t i = 0; i < len; i++)
        xip_put(o, b[i]);
    if (left)
        while (filler-- > 0)
            xip_put(o, ' ');
}

/* unsigned to text into buf (no NUL), returns the length */
static size_t xip_utoa(unsigned long long v, int base, char *buf, bool upper, bool show_base)
{
    const char *dig = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    char tmp[24];
    int i = 0;
    if (!v)
        tmp[i++] = '0';
    while (v) {
        tmp[i++] = dig[v % (unsigned)base];
        v /= (unsigned)base;
    }
    size_t n = 0;
    if (show_base && base == 16)
        buf[n++] = '0', buf[n++] = 'x';
    else if (show_base && base == 8 && !(i == 1 && tmp[0] == '0'))
        buf[n++] = '0';
    while (i)
        buf[n++] = tmp[--i];
    return n;
}

/* Decimal digits of v (0 < v < DBL_MAX): `want` generated and rounded up on the first dropped
 * digit; `e` receives the exponent of the leading digit, out the keep first digit characters. */
static void xip_digits(double v, int want, int keep, int *e, char *out)
{
    int exp = 0;
    while (v >= 1e64) { v *= 1e-64; exp += 64; }
    while (v >= 1e16) { v *= 1e-16; exp += 16; }
    while (v >= 10.0) { v /= 10.0; exp++; }
    while (v != 0.0 && v < 1e-16) { v *= 1e64; exp -= 64; }
    while (v != 0.0 && v < 1.0) { v *= 10.0; exp--; }

    char d[XIP_FMT_DIG_MAX + 1];
    for (int i = 0; i < want; i++) {
        int dv = (int)v;
        d[i] = (char)dv;
        v = (v - (double)dv) * 10.0;
    }
    if ((int)v >= 5) {                          /* round half up, carrying left */
        int j = want - 1;
        while (j >= 0) {
            if (d[j] < 9) { d[j]++; break; }
            d[j] = 0; j--;
        }
        if (j < 0) {
            memmove(d + 1, d, (size_t)want);
            d[0] = 1;
            exp++;
        }
    }
    *e = exp;
    for (int i = 0; i < keep; i++)
        out[i] = (char)('0' + d[i]);
}

/* render a finite nonneg v into buf, returns the length; conv is f e g (lower case forms) */
static size_t xip_ftoa(double v, char conv, int prec, bool has_prec, bool alt, char *buf)
{
    size_t n = 0;
    if (v == 0.0) {
        buf[n++] = '0';
        if (conv == 'e') {
            for (int i = 0; i < prec; i++) { buf[n++] = i == 0 && prec ? '.' : '0'; }
            buf[n++] = 'e'; buf[n++] = '+'; buf[n++] = '0'; buf[n++] = '0';
        } else if (conv == 'f' && prec > 0) {
            buf[n++] = '.';
            for (int i = 0; i < prec; i++) buf[n++] = '0';
        }
        return n;
    }

    int p = has_prec ? prec : 6;
    if (conv == 'g' && p == 0)
        p = 1;                                  /* %.g: one significant digit */

    char dig[XIP_FMT_DIG_MAX];
    char eb[12];
    int exp = 0;

    if (conv == 'f') {
        xip_digits(v, 2, 2, &exp, dig);         /* where the point falls decides the digit budget */
        int ip = exp + 1 > 0 ? exp + 1 : 1;
        int want = ip + p + 2;
        if (want > XIP_FMT_DIG_MAX)
            want = XIP_FMT_DIG_MAX;
        xip_digits(v, want, want, &exp, dig);
        ip = exp + 1;
        if (ip > 0) {
            int shown = ip < XIP_FMT_DIG_MAX ? ip : XIP_FMT_DIG_MAX;
            for (int i = 0; i < shown; i++)
                buf[n++] = dig[i];
            for (int i = shown; i < ip; i++)    /* digits past the budget: the value is huge, these are its low places */
                buf[n++] = '0';
            if (p > 0) {
                buf[n++] = '.';
                for (int i = 0; i < p; i++)
                    buf[n++] = ip + i < want ? dig[ip + i] : '0';
            } else if (alt) {
                buf[n++] = '.';
            }
        } else {
            buf[n++] = '0';
            if (p > 0) {
                buf[n++] = '.';
                for (int i = 0; i < -exp - 1; i++)
                    buf[n++] = '0';
                for (int i = 0; i < p; i++)
                    buf[n++] = 1 - exp - 1 + i < want ? dig[1 - exp - 1 + i] : '0';
            } else if (alt) {
                buf[n++] = '.';
            }
        }
        return n;
    }

    int want = p + 2 > 2 ? p + 2 : 2;
    if (want > XIP_FMT_DIG_MAX)
        want = XIP_FMT_DIG_MAX;
    xip_digits(v, want, want, &exp, dig);

    if (conv == 'e') {
        buf[n++] = dig[0];
        if (p > 0) {
            buf[n++] = '.';
            for (int i = 1; i <= p; i++)
                buf[n++] = i < want ? dig[i] : '0';
        } else if (alt) {
            buf[n++] = '.';
        }
        buf[n++] = 'e';
        size_t el = xip_utoa((unsigned long long)(exp < 0 ? -exp : exp), 10, eb, false, false);
        buf[n++] = exp < 0 ? '-' : '+';
        if (el < 2)
            buf[n++] = '0';
        for (size_t i = 0; i < el; i++)
            buf[n++] = eb[i];
        return n;
    }

    /* %g: exponent form when the point would sit far from the digits */
    if (exp < -4 || exp >= p) {
        buf[n++] = dig[0];
        if (p > 1) {
            buf[n++] = '.';
            for (int i = 1; i < p; i++)
                buf[n++] = i < want ? dig[i] : '0';
            if (!alt)
                while (n > 1 && buf[n - 1] == '0') n--;
            if (!alt && n > 1 && buf[n - 1] == '.') n--;
        }
        buf[n++] = 'e';
        size_t el2 = xip_utoa((unsigned long long)(exp < 0 ? -exp : exp), 10, eb, false, false);
        buf[n++] = exp < 0 ? '-' : '+';
        if (el2 < 2) buf[n++] = '0';
        for (size_t i = 0; i < el2; i++)
            buf[n++] = eb[i];
        return n;
    }
    if (exp >= 0) {
        int ip = exp + 1;
        for (int i = 0; i < ip; i++)
            buf[n++] = i < want ? dig[i] : '0';
        int frac = p - ip;
        if (frac > 0 && (alt || ip < p)) {
            buf[n++] = '.';
            for (int i = 0; i < frac; i++)
                buf[n++] = ip + i < want ? dig[ip + i] : '0';
        }
    } else {
        buf[n++] = '0';
        buf[n++] = '.';
        for (int i = 0; i < -exp - 1; i++)
            buf[n++] = '0';
        int shown = p < want ? p : want;
        for (int i = 0; i < shown; i++)
            buf[n++] = dig[i];
    }
    if (!alt) {
        while (n && buf[n - 1] == '0') n--;
        if (n && buf[n - 1] == '.') n--;
    }
    return n;
}

static void xip_fmt_float(xip_sink *o, double v, char conv, int prec, bool has_prec, bool alt,
                          int width, bool left, char pad, bool plus, bool space)
{
    char buf[XIP_FMT_BUF_MAX];
    size_t n = 0;
    char sign = 0;
    bool upper = conv == 'E' || conv == 'F' || conv == 'G';
    char lower = (char)(upper ? conv + ('e' - 'E') : conv);   /* xip_ftoa renders the lower case forms */
    if (v < 0) { sign = '-'; v = -v; }
    if (v != v) {
        buf[n++] = 'n'; buf[n++] = 'a'; buf[n++] = 'n';
    } else if (v > DBL_MAX) {
        buf[n++] = 'i'; buf[n++] = 'n'; buf[n++] = 'f';
    } else {
        n = xip_ftoa(v, lower, prec, has_prec, alt, buf);
    }
    if (upper)                                /* %E %F %G: exponent letter, and INF / NAN */
        for (size_t i = 0; i < n; i++)
            if (buf[i] >= 'a' && buf[i] <= 'z')
                buf[i] = (char)(buf[i] - ('a' - 'A'));
    size_t total = n + (sign || plus || space ? 1 : 0);
    int filler = width > 0 && (size_t)width > total ? width - (int)total : 0;
    if (filler && pad == '0' && !left) {
        if (sign) xip_put(o, sign);
        else if (plus) xip_put(o, '+');
        else if (space) xip_put(o, ' ');
        while (filler-- > 0) xip_put(o, '0');
    } else {
        while (filler-- > 0 && !left) xip_put(o, pad);
        if (sign) xip_put(o, sign);
        else if (plus) xip_put(o, '+');
        else if (space) xip_put(o, ' ');
    }
    for (size_t i = 0; i < n; i++)
        xip_put(o, buf[i]);
    if (left) {
        filler = width > 0 && (size_t)width > total ? width - (int)total : 0;
        while (filler-- > 0) xip_put(o, ' ');
    }
}

int xip_fmt_vsnprintf(char *s, size_t n, const char *fmt, va_list ap)
{
    xip_sink o = { .s = s, .n = n, .used = 0 };
    while (*fmt) {
        if (*fmt != '%') {
            xip_put(&o, *fmt++);
            continue;
        }
        fmt++;
        bool left = false, plus = false, space = false, zero = false, alt = false;
        for (;; fmt++) {
            if (*fmt == '-') left = true;
            else if (*fmt == '+') plus = true;
            else if (*fmt == ' ') space = true;
            else if (*fmt == '0') zero = true;
            else if (*fmt == '#') alt = true;
            else break;
        }
        int width = 0;
        while (*fmt >= '0' && *fmt <= '9')
            width = width * 10 + (*fmt++ - '0');
        bool has_prec = false;
        int prec = 6;
        if (*fmt == '.') {
            fmt++;
            has_prec = true;
            prec = 0;
            while (*fmt >= '0' && *fmt <= '9')
                prec = prec * 10 + (*fmt++ - '0');
            if (prec > XIP_FMT_PREC_MAX)
                prec = XIP_FMT_PREC_MAX;
        }
        int lng = 0;                            /* 0 int, 1 long, 2 long long */
        for (;; fmt++) {
            if (*fmt == 'h' || *fmt == 'z' || *fmt == 'j' || *fmt == 't' || *fmt == 'L')
                continue;
            if (*fmt == 'l') { if (lng < 2) lng++; continue; }
            break;
        }
        char conv = *fmt++;
        char pad = zero && !left ? '0' : ' ';
        char num[32];
        size_t nl = 0;

        switch (conv) {
        case 'c': {
            char c = (char)va_arg(ap, int);
            xip_pad(&o, &c, 1, width, ' ', left);
            break;
        }
        case 's': {
            const char *b = va_arg(ap, const char *);
            if (!b)
                b = "(null)";
            size_t len = strlen(b);
            if (has_prec && prec < (int)len)
                len = (size_t)prec;
            xip_pad(&o, b, len, width, ' ', left);
            break;
        }
        case 'p': {
            void *p = va_arg(ap, void *);
            char ib[16];
            size_t l = xip_utoa((unsigned long long)(uintptr_t)p, 16, ib, false, true);
            xip_pad(&o, ib, l, width, ' ', left);
            break;
        }
        case 'd': case 'i': case 'u': case 'o': case 'x': case 'X': {
            unsigned long long v;
            bool neg = false, sgn = conv == 'd' || conv == 'i';
            if (sgn) {
                long long sv = lng == 2 ? va_arg(ap, long long) :
                               lng == 1 ? (long long)va_arg(ap, long) : (long long)va_arg(ap, int);
                neg = sv < 0;
                v = neg ? (unsigned long long)(-(sv + 1)) + 1ULL : (unsigned long long)sv;
            } else {
                v = lng == 2 ? va_arg(ap, unsigned long long) :
                    lng == 1 ? (unsigned long long)va_arg(ap, unsigned long) : va_arg(ap, unsigned int);
            }
            char ib[24];
            size_t l = xip_utoa(v, conv == 'x' || conv == 'X' ? 16 : conv == 'o' ? 8 : 10, ib, conv == 'X', alt && !sgn);
            if (neg) num[nl++] = '-';
            else if (plus) num[nl++] = '+';
            else if (space) num[nl++] = ' ';
            memcpy(num + nl, ib, l);
            nl += l;
            if (pad == '0' && !left && width > 0 && (size_t)width > nl) {
                size_t skip = nl && (num[0] == '-' || num[0] == '+') ? 1 : 0;
                if (skip) xip_put(&o, num[0]);
                for (int i = width - (int)nl; i > 0; i--)
                    xip_put(&o, '0');
                xip_pad(&o, num + skip, nl - skip, 0, ' ', false);
            } else {
                xip_pad(&o, num, nl, width, pad, left);
            }
            break;
        }
        case 'f': case 'e': case 'g': case 'F': case 'E': case 'G': {
            double v = va_arg(ap, double);
            xip_fmt_float(&o, v, conv, prec, has_prec, alt, width, left, pad, plus, space);
            break;
        }
        case '%':
            xip_pad(&o, "%", 1, width, ' ', left);
            break;
        default:
            return -1;                          /* a conversion this port does not render */
        }
    }
    if (s) {
        if (o.used < n)
            s[o.used] = '\0';
        else if (n > 0)
            s[n - 1] = '\0';   /* truncated: at most n - 1 characters, the NUL owns the last byte */
    }
    return (int)o.used;
}

int xip_fmt_snprintf(char *s, size_t n, const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    int r = xip_fmt_vsnprintf(s, n, fmt, ap);
    va_end(ap);
    return r;
}
