/* SPDX-License-Identifier: Apache-2.0 */
/* The loader scan's choice among installed images (components/xip_loader/src/xip_select.c). */
#include <stdio.h>
#include <string.h>
#include "xip_select.h"

static int failures;
#define CHECK(cond) do { if (!(cond)) { printf("FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond); failures++; } } while (0)

static xip_candidate_t img(uint32_t page, const char *name, uint32_t version, uint32_t cell)
{
    xip_candidate_t c = {.page = page, .version = version, .static_addr = 0x1000 + 16 * cell};
    strncpy(c.name, name, sizeof(c.name) - 1);
    return c;
}

static void test_versions(void)
{
    xip_choice_t o[8];
    xip_candidate_t one[] = {img(0, "a", 0, 0)};
    xip_select(one, 1, o);
    CHECK(o[0].role == XIP_ROLE_ACTIVE && o[0].by == -1);

    /* higher version later in flash wins; the older one is shadowed by it */
    xip_candidate_t up[] = {img(0, "a", 1, 0), img(1, "b", 0, 1), img(2, "a", 2, 0)};
    xip_select(up, 3, o);
    CHECK(o[0].role == XIP_ROLE_SHADOWED && o[0].by == 2);
    CHECK(o[1].role == XIP_ROLE_ACTIVE);
    CHECK(o[2].role == XIP_ROLE_ACTIVE);

    /* higher version earlier in flash */
    xip_candidate_t down[] = {img(0, "a", 5, 0), img(3, "a", 4, 0), img(4, "a", 0, 0)};
    xip_select(down, 3, o);
    CHECK(o[0].role == XIP_ROLE_ACTIVE);
    CHECK(o[1].role == XIP_ROLE_SHADOWED && o[1].by == 0);
    CHECK(o[2].role == XIP_ROLE_SHADOWED && o[2].by == 0);

    /* the same name and version twice: the lowest page keeps it */
    xip_candidate_t dup[] = {img(1, "a", 3, 0), img(2, "a", 3, 0), img(5, "a", 2, 0)};
    xip_select(dup, 3, o);
    CHECK(o[0].role == XIP_ROLE_ACTIVE);
    CHECK(o[1].role == XIP_ROLE_DUPLICATE && o[1].by == 0);
    CHECK(o[2].role == XIP_ROLE_SHADOWED && o[2].by == 0);

    /* an absent version is 0: any version beats it, two of them are duplicates */
    xip_candidate_t zero[] = {img(0, "a", 0, 0), img(1, "a", 0, 0), img(2, "a", 1, 0)};
    xip_select(zero, 3, o);
    CHECK(o[0].role == XIP_ROLE_SHADOWED && o[0].by == 2);
    CHECK(o[1].role == XIP_ROLE_SHADOWED && o[1].by == 2);
    CHECK(o[2].role == XIP_ROLE_ACTIVE);
}

static void test_cells(void)
{
    xip_choice_t o[8];
    /* versions of one module share its cell: no conflict */
    xip_candidate_t same[] = {img(0, "a", 1, 3), img(1, "a", 2, 3)};
    xip_select(same, 2, o);
    CHECK(o[0].role == XIP_ROLE_SHADOWED && o[1].role == XIP_ROLE_ACTIVE);

    /* two active modules on one cell: the lower page keeps it */
    xip_candidate_t clash[] = {img(0, "a", 1, 3), img(1, "b", 9, 3), img(2, "c", 0, 4)};
    xip_select(clash, 3, o);
    CHECK(o[0].role == XIP_ROLE_ACTIVE);
    CHECK(o[1].role == XIP_ROLE_CELL_CONFLICT && o[1].by == 0);
    CHECK(o[2].role == XIP_ROLE_ACTIVE);

    /* a shadowed image on another module's cell is irrelevant: only active images compete */
    xip_candidate_t quiet[] = {img(0, "a", 1, 3), img(1, "b", 1, 4), img(2, "b", 2, 4), img(3, "a", 0, 4)};
    xip_select(quiet, 4, o);
    CHECK(o[0].role == XIP_ROLE_ACTIVE && o[1].role == XIP_ROLE_SHADOWED && o[2].role == XIP_ROLE_ACTIVE);
    CHECK(o[3].role == XIP_ROLE_SHADOWED);

    /* the higher version moves to a cell held by an earlier module: that one is the conflict loser only if later */
    xip_candidate_t move[] = {img(0, "a", 1, 3), img(1, "b", 1, 4), img(2, "a", 2, 4)};
    xip_select(move, 3, o);
    CHECK(o[0].role == XIP_ROLE_SHADOWED);
    CHECK(o[1].role == XIP_ROLE_ACTIVE);
    CHECK(o[2].role == XIP_ROLE_CELL_CONFLICT && o[2].by == 1);
}

static void test_spans(void)
{
    xip_choice_t out[4];
    xip_candidate_t c[]={img(0,"nan",1,7),img(2,"other",1,8),img(3,"adjacent",1,9)};
    c[0].window_slots=2;
    xip_select(c,3,out);
    CHECK(out[0].role==XIP_ROLE_ACTIVE);
    CHECK(out[1].role==XIP_ROLE_CELL_CONFLICT && out[1].by==0);
    CHECK(out[2].role==XIP_ROLE_ACTIVE);
    /* The same overlap helper guards activation before mappings are created. */
    CHECK(xip_slots_overlap(c[0].static_addr,2,c[1].static_addr,1));
    CHECK(xip_slots_overlap(c[1].static_addr,1,c[0].static_addr,2));
    CHECK(!xip_slots_overlap(c[0].static_addr,2,c[2].static_addr,1));
    /* Shadowed old spans do not reserve windows during scan selection. */
    xip_candidate_t old[]={img(0,"nan",1,7),img(1,"other",1,8),img(2,"nan",2,7)};
    old[0].window_slots=3;
    xip_select(old,3,out);
    CHECK(out[0].role==XIP_ROLE_SHADOWED && out[1].role==XIP_ROLE_ACTIVE && out[2].role==XIP_ROLE_ACTIVE);
    CHECK(!xip_slots_overlap(0xfffffff0u,2,0,1)); /* no address wrap */
}

int main(void)
{
    test_versions();
    test_cells();
    test_spans();
    puts(failures ? "FAILED" : "select ok");
    return failures != 0;
}
