/* The C twin of the ESP-NOW action frame codec against the vectors generated from DMesh's code (tests/data/espnow-frames.txt):
 * the bytes on the air are the same as DMesh's, and the Rust twin checks the same file. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../../components/xip_runtime/src/xip_espnow_frame.h"

static int fails;
#define CHECK(c, ...) do { if (!(c)) { fprintf(stderr, "FAIL line %d: ", __LINE__); fprintf(stderr, __VA_ARGS__); fputc('\n', stderr); fails++; } } while (0)

static size_t unhex(const char *s, uint8_t *out)
{
    size_t n = strlen(s) / 2;
    for (size_t i = 0; i < n; i++) { unsigned v; sscanf(s + 2 * i, "%2x", &v); out[i] = (uint8_t)v; }
    return n;
}
static void mac(const char *s, uint8_t out[6]) { unsigned v[6]; sscanf(s, "%x:%x:%x:%x:%x:%x", &v[0], &v[1], &v[2], &v[3], &v[4], &v[5]); for (int i = 0; i < 6; i++) out[i] = (uint8_t)v[i]; }

int main(int argc, char **argv)
{
    FILE *f = fopen(argc > 1 ? argv[1] : "tests/data/espnow-frames.txt", "r");
    if (!f) { perror("vectors"); return 2; }
    static char line[8192];
    static uint8_t want[4096], got[4096], payload[1500], out[2048];
    int encoded = 0, parsed = 0;
    while (fgets(line, sizeof line, f)) {
        line[strcspn(line, "\n")] = 0;
        if (!line[0] || line[0] == '#') continue;
        char *w[8] = {0};
        int n = 0;
        for (char *t = strtok(line, " "); t && n < 8; t = strtok(NULL, " ")) w[n++] = t;
        if (!strcmp(w[0], "encode")) {
            /* the older sequence of 250-byte elements is the same bytes for one element; it is only parsed now */
            if (!strcmp(w[1], "v2") && atoi(w[5]) > 250) continue;
            uint8_t d[6], s[6], b[6];
            mac(w[2], d); mac(w[3], s); mac(w[4], b);
            size_t len = (size_t)atoi(w[5]);
            for (size_t i = 0; i < len; i++) payload[i] = (uint8_t)(i * 7 + 3);
            size_t used = xip_now_encode(out, sizeof out, d, s, b, payload, len);
            size_t wl = unhex(w[6], want);
            CHECK(used == wl && !memcmp(out, want, wl), "encode %zu bytes", len);
            CHECK(used == xip_now_frame_len(len), "frame length of %zu", len);
            encoded++;
        } else if (!strcmp(w[0], "parse-body")) {
            size_t bl = unhex(w[1], want);
            long r = xip_now_parse_body(want, bl, got, 1400);
            if (!strcmp(w[2], "none")) CHECK(r == -1, "parse should fail: %s", w[1]);
            else {
                size_t ol = unhex(w[3], payload);
                CHECK(r == atol(w[2]) && (size_t)r == ol && !memcmp(got, payload, ol), "parse %s", w[2]);
            }
            parsed++;
        } else if (!strcmp(w[0], "parse-small")) {
            size_t bl = unhex(w[1], want);
            CHECK(xip_now_parse_body(want, bl, got, 4) == -1, "output too small");
            parsed++;
        }
    }
    fclose(f);
    CHECK(encoded >= 5 && parsed >= 15, "vectors read: %d %d", encoded, parsed);
    /* a whole frame carries its source */
    uint8_t d[6] = {1, 2, 3, 4, 5, 6}, s[6] = {6, 5, 4, 3, 2, 1}, b[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff}, p[4] = {0x40, 1, 2, 3}, src[6];
    size_t used = xip_now_encode(out, sizeof out, d, s, b, p, 4);
    CHECK(xip_now_parse_frame(out, used, src, got, sizeof got) == 4 && !memcmp(src, s, 6) && !memcmp(got, p, 4), "frame round trip");
    CHECK(xip_now_encode(out, 8, d, s, b, p, 4) == 0, "output too small");
    printf(fails ? "espnow frame: FAILED\n" : "espnow frame ok (%d encoded, %d parsed)\n", encoded, parsed);
    return fails ? 1 : 0;
}
