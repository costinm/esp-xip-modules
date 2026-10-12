/* SPDX-License-Identifier: Apache-2.0 */
/* Drives components/espmod_install/src/install.c and the loader's slots.c (unchanged), the module sink of esp-wifi-loader, the way the
 * loader does after it verified a package: open, head, begin, write.., commit, done. Verifying the package itself
 * (key, signature, block hashes) is esp-wifi-loader's and tested there. The flash is memory that refuses to set a
 * bit without an erase. tests/test_install.py writes the script this reads; packages are made by sign.py.
 *
 *   reset | running SLOT | stopfail CODE
 *   send BODYFILE EXPECT...        install it; EXPECT is "OK" or "ERR code" (what the loader would reply)
 *   dryrun BODYFILE EXPECT...      the same without begin/write/commit
 *   interrupt BODYFILE BLOCKS      begin, write BLOCKS blocks, then the connection dies: no commit
 *   expect version SLOT N | starts N | stops N | violations 0 | image SLOT BODYFILE | untouched | empty SLOT
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "fake_loader.h"
#include "internal.h"
#include "esp_wifi_loader.h"
#include "espmod_install.h"

static int failures;
#define CHECK(cond, ...) do { if (!(cond)) { printf("FAIL line %d: ", line_no); printf(__VA_ARGS__); printf("\n"); failures++; } } while (0)
static int line_no;
static uint8_t snapshot[FAKE_FLASH_SIZE];

/* the signed header of esp-wifi-loader's package (docs/protocol.md) */
typedef struct __attribute__((packed)) {
    uint32_t magic;
    uint8_t format, cpu, target, flags;
    uint32_t version, image_size;
    uint16_t block_size, block_count;
    uint8_t image_sha256[32], table_sha256[32];
    uint8_t name_len, pad[3];
    char name[128];
} pkg_header_t;
#define BLOCK 4096u
#define PKG_HEADER_AT (16 + 65 + 64)

static uint8_t *slurp(const char *path, size_t *n)
{
    FILE *f = fopen(path, "rb");
    if (!f) { perror(path); exit(2); }
    fseek(f, 0, SEEK_END); *n = ftell(f); fseek(f, 0, SEEK_SET);
    uint8_t *b = malloc(*n + 1);
    if (fread(b, 1, *n, f) != *n) exit(2);
    fclose(f);
    return b;
}

/* What esp-wifi-loader does with a verified package; `blocks` < 0: all of it. Returns the reply. */
static void install(const char *path, bool dry, int blocks, bool commit, char *reply, size_t cap)
{
    size_t n;
    uint8_t *pkg = slurp(path, &n);
    pkg_header_t h;
    memcpy(&h, pkg + PKG_HEADER_AT, sizeof(h));
    const uint8_t *image = pkg + PKG_HEADER_AT + sizeof(h) + (size_t)h.block_count * 8;
    const esp_wifi_loader_module_sink_t *s = espmod_install_sink();
    char name[129] = {0};
    memcpy(name, h.name, h.name_len);
    size_t head_len = h.image_size < BLOCK ? h.image_size : BLOCK;
    int rc = s->open(s->arg, name, h.version, h.image_size, dry);
    bool opened = rc == 0, begun = false, done = false;
    if (!rc) rc = s->head(s->arg, image, head_len);
    if (!rc && !dry) { rc = s->begin(s->arg); begun = rc == 0; }
    for (uint32_t off = head_len, i = 0; !rc && !dry && off < h.image_size; off += BLOCK, i++) {
        if (blocks >= 0 && (int)i >= blocks) break;
        size_t len = h.image_size - off < BLOCK ? h.image_size - off : BLOCK;
        rc = s->write(s->arg, off, image + off, len);
    }
    if (!rc && !dry && commit && blocks < 0) { rc = s->commit(s->arg, image, head_len); done = rc == 0; }
    if (opened) s->done(s->arg, done, begun);
    if (rc) snprintf(reply, cap, "ERR %d", rc); else snprintf(reply, cap, "OK");
    free(pkg);
}

int main(int argc, char **argv)
{
    if (argc != 2) { fprintf(stderr, "usage: test_install SCRIPT\n"); return 2; }
    FILE *script = fopen(argv[1], "r");
    char line[512], reply[256];
    while (fgets(line, sizeof(line), script)) {
        line_no++;
        line[strcspn(line, "\n")] = 0;
        if (!*line || *line == '#') continue;
        char a[200]; int x, y;
        if (!strcmp(line, "reset")) {
            fake_reset();
        } else if (sscanf(line, "running %d", &x) == 1) {
            fake_mark_running(x);
        } else if (sscanf(line, "stopfail %d", &x) == 1) {
            fake_stop_result = x;
        } else if (sscanf(line, "interrupt %199s %d", a, &x) == 2) {
            memcpy(snapshot, fake_flash, sizeof(snapshot));
            install(a, false, x, false, reply, sizeof(reply));
        } else if (sscanf(line, "send %199s", a) == 1 || sscanf(line, "dryrun %199s", a) == 1) {
            memcpy(snapshot, fake_flash, sizeof(snapshot));
            install(a, line[0] == 'd', -1, true, reply, sizeof(reply));
            const char *want = strchr(line, ' ');
            want = want ? strchr(want + 1, ' ') : NULL;
            want = want ? want + 1 : "";
            CHECK(!strncmp(reply, want, strlen(want)), "reply \"%s\" does not start with \"%s\"", reply, want);
        } else if (sscanf(line, "expect version %d %d", &x, &y) == 2) {
            CHECK((int)fake_version(x) == y, "version of slot %d is %u, want %d", x, fake_version(x), y);
        } else if (sscanf(line, "expect starts %d", &x) == 1) {
            CHECK(fake_starts == x, "starts %d want %d", fake_starts, x);
        } else if (sscanf(line, "expect stops %d", &x) == 1) {
            CHECK(fake_stops == x, "stops %d want %d", fake_stops, x);
        } else if (!strcmp(line, "expect violations 0")) {
            CHECK(fake_flash_violations == 0, "%d flash violations", fake_flash_violations);
        } else if (!strcmp(line, "expect untouched")) {
            CHECK(!memcmp(snapshot, fake_flash, sizeof(snapshot)), "flash changed");
        } else if (sscanf(line, "expect image %d %199s", &x, a) == 2) {
            size_t bn;
            uint8_t *body = slurp(a, &bn);
            const pkg_header_t *h = (const pkg_header_t *)(body + PKG_HEADER_AT);
            const uint8_t *image = body + PKG_HEADER_AT + sizeof(*h) + (size_t)h->block_count * 8;
            CHECK(!memcmp(fake_flash + em.cfg.slots[x].offset, image, h->image_size), "slot %d does not hold the image", x);
            free(body);
        } else if (sscanf(line, "expect empty %d", &x) == 1) {
            int ok = 1;
            for (uint32_t i = 0; i < 4096; i++) if (fake_flash[em.cfg.slots[x].offset + i] != 0xff) ok = 0;
            CHECK(ok, "slot %d head is not erased", x);
        } else {
            printf("bad script line %d: %s\n", line_no, line);
            failures++;
        }
    }
    puts(failures ? "FAILED" : "install ok");
    return failures != 0;
}
