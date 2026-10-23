#pragma once
#include <stdint.h>

/* Native readiness mechanism shared by module sockets and firmware adapters.
 * Callbacks run on one lazy lwIP task, outside the registry lock. They may
 * drain bounded RX/accept batches or wake an owner, never dispatch modules.
 * A registration is a generation-checked token; zero means admission failed.
 * watch/update never take ownership of a caller's descriptor. Clear before
 * closing it. A callback already in flight may finish after clear; tokens
 * prevent it from suspending/updating a replacement registration. */
typedef uint32_t xip_io_registration;
typedef void (*xip_io_ready_fn)(xip_io_registration, int, int16_t, void *);
typedef void (*xip_io_accept_fn)(int, void *);
xip_io_registration xip_io_watch(int fd, int16_t events, xip_io_ready_fn ready, void *ctx);
/* Binding waits for a netif and retries on failure. The service owns the
 * listener; accept transfers each client fd to the callback, including rejects. */
xip_io_registration xip_io_listen_tcp(uint16_t port, int backlog, xip_io_accept_fn accept, void *ctx);
void xip_io_update(xip_io_registration reg, int fd, int16_t events, int tick_ms);
void xip_io_suspend(xip_io_registration reg);
void xip_io_clear(xip_io_registration reg);
