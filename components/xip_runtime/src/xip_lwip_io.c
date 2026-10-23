/* One readiness task for native UDP/TCP adapters and module TCP sockets. */
#include <stdbool.h>
#include <stdlib.h>
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "esp_netif.h"
#include "esp_timer.h"
#include "lwip/sockets.h"
#include "xip_lwip_io.h"

#define IO_SLOTS 12
#define IO_STACK 6144
#define IO_INDEX_BITS 4
#define IO_INDEX_MASK 15u

typedef struct {
    xip_io_registration token;
    int fd, tick_ms, backlog;
    int16_t events;
    uint16_t port;
    bool suspended;
    uint64_t last_tick;
    xip_io_ready_fn ready;
    xip_io_accept_fn accept;
    void *ctx;
} io_entry;
typedef struct { io_entry entries[IO_SLOTS]; } io_state;
static io_state *state;
static SemaphoreHandle_t mutex;
static portMUX_TYPE init_lock = portMUX_INITIALIZER_UNLOCKED;
static uint32_t generation;

static bool lock_init(void)
{
    portENTER_CRITICAL(&init_lock);
    bool present = mutex != NULL;
    portEXIT_CRITICAL(&init_lock);
    if (present) return true;
    SemaphoreHandle_t candidate = xSemaphoreCreateMutex();
    if (!candidate) return false;
    portENTER_CRITICAL(&init_lock);
    if (!mutex) { mutex = candidate; candidate = NULL; }
    portEXIT_CRITICAL(&init_lock);
    if (candidate) vSemaphoreDelete(candidate);
    return true;
}
/* Caller holds mutex. The low nibble is index+1, the rest a generation. */
static io_entry *find(xip_io_registration token)
{
    unsigned index = token & IO_INDEX_MASK;
    if (!state || !index || index > IO_SLOTS) return NULL;
    io_entry *entry = &state->entries[index - 1];
    return entry->token == token ? entry : NULL;
}
static void poll_task(void *arg);
static xip_io_registration add(io_entry value)
{
    if (!lock_init()) return 0;
    xSemaphoreTake(mutex, portMAX_DELAY);
    bool fresh = state == NULL;
    if (fresh) state = calloc(1, sizeof(*state));
    xip_io_registration token = 0;
    if (state) for (unsigned i = 0; i < IO_SLOTS; i++) {
        if (state->entries[i].token) continue;
        generation = (generation + 1) & (UINT32_MAX >> IO_INDEX_BITS);
        if (!generation) generation = 1;
        token = (generation << IO_INDEX_BITS) | (i + 1);
        value.token = token;
        state->entries[i] = value;
        break;
    }
    if (fresh && state && xTaskCreate(poll_task, "xip_lwip", IO_STACK, NULL, 4, NULL) != pdPASS) {
        free(state); state = NULL; token = 0;
    }
    xSemaphoreGive(mutex);
    return token;
}
xip_io_registration xip_io_watch(int fd, int16_t events, xip_io_ready_fn ready, void *ctx)
{
    if (fd < 0 || !ready) return 0;
    return add((io_entry){.fd=fd, .events=events, .ready=ready, .ctx=ctx});
}
xip_io_registration xip_io_listen_tcp(uint16_t port, int backlog, xip_io_accept_fn accept, void *ctx)
{
    if (!port || backlog <= 0 || !accept) return 0;
    return add((io_entry){.fd=-1, .events=POLLIN, .port=port, .backlog=backlog, .accept=accept, .ctx=ctx});
}
void xip_io_update(xip_io_registration token, int fd, int16_t events, int tick_ms)
{
    if (!lock_init()) return;
    xSemaphoreTake(mutex, portMAX_DELAY);
    io_entry *e = find(token);
    if (e && !e->port) {
        e->fd=fd; e->events=events; e->tick_ms=tick_ms; e->suspended=false;
    }
    xSemaphoreGive(mutex);
}
void xip_io_suspend(xip_io_registration token)
{
    if (!lock_init()) return;
    xSemaphoreTake(mutex, portMAX_DELAY);
    io_entry *e = find(token);
    if (e) e->suspended=true;
    xSemaphoreGive(mutex);
}
void xip_io_clear(xip_io_registration token)
{
    if (!lock_init()) return;
    xSemaphoreTake(mutex, portMAX_DELAY);
    io_entry *e = find(token);
    if (e) {
        if (e->port && e->fd >= 0) lwip_close(e->fd);
        *e=(io_entry){0};
    }
    xSemaphoreGive(mutex);
}
/* Caller holds mutex. SDK operations are nonblocking; no callbacks here. */
static void bind_listener(io_entry *e)
{
    if (!e->port || e->fd >= 0) return;
    int fd=lwip_socket(AF_INET6, SOCK_STREAM, 0);
    if (fd < 0) return;
    struct sockaddr_in6 address={.sin6_len=sizeof(address), .sin6_family=AF_INET6, .sin6_port=htons(e->port)};
    if (lwip_fcntl(fd, F_SETFL, O_NONBLOCK) == 0 &&
        lwip_bind(fd, (struct sockaddr *)&address, sizeof(address)) == 0 &&
        lwip_listen(fd, e->backlog) == 0) e->fd=fd;
    else lwip_close(fd);
}
static void listener_ready(const io_entry *snapshot, int16_t events)
{
    if (!(events & POLLIN)) {
        if (events) {
            xSemaphoreTake(mutex, portMAX_DELAY);
            io_entry *e=find(snapshot->token);
            if (e && e->fd == snapshot->fd) { lwip_close(e->fd); e->fd=-1; }
            xSemaphoreGive(mutex);
        }
        return;
    }
    for (unsigned n=0; n<4; n++) {
        xSemaphoreTake(mutex, portMAX_DELAY);
        io_entry *e=find(snapshot->token);
        int fd = e && e->fd == snapshot->fd ? lwip_accept(e->fd, NULL, NULL) : -1;
        xSemaphoreGive(mutex);
        if (fd < 0) break;
        snapshot->accept(fd, snapshot->ctx);
    }
}
/* One bounded polling pass. Keeping it separate also permits native tests of
 * the actual registry, generation validation and retry behavior. */
static bool poll_once(bool *thread_initialized)
{
    io_entry snapshots[IO_SLOTS];
    struct pollfd fds[IO_SLOTS];
    unsigned count=0;
    bool netif=esp_netif_next_unsafe(NULL) != NULL;
    if (netif && !*thread_initialized) {
        lwip_socket_thread_init(); *thread_initialized=true;
    }
    xSemaphoreTake(mutex, portMAX_DELAY);
    bool any=false;
    for (unsigned i=0; i<IO_SLOTS; i++) {
        io_entry *e=&state->entries[i];
        if (!e->token) continue;
        any=true;
        if (!netif) continue;
        bind_listener(e);
        if (e->fd < 0 || e->suspended) continue;
        snapshots[count]=*e;
        fds[count++]=(struct pollfd){.fd=e->fd, .events=e->events};
    }
    if (!any) { free(state); state=NULL; }
    xSemaphoreGive(mutex);
    if (!any) return false;
    if (!count) { vTaskDelay(pdMS_TO_TICKS(10)); return true; }
    // A short upper bound also admits newly registered/rearmed descriptors
    // promptly while another socket is idle in poll.
    int timeout=50;
    for (unsigned i=0; i<count; i++)
        if (snapshots[i].tick_ms > 0 && snapshots[i].tick_ms < timeout) timeout=snapshots[i].tick_ms;
    int result=lwip_poll(fds, count, timeout);
    if (result < 0) { vTaskDelay(pdMS_TO_TICKS(10)); return true; }
    uint64_t now=(uint64_t)esp_timer_get_time()/1000;
    for (unsigned i=0; i<count; i++) {
        io_entry snapshot=snapshots[i];
        xSemaphoreTake(mutex, portMAX_DELAY);
        io_entry *e=find(snapshot.token);
        bool valid=e && !e->suspended && e->fd==snapshot.fd;
        bool expired=valid && e->tick_ms>0 && now-e->last_tick >= (uint64_t)e->tick_ms;
        if (expired) e->last_tick=now;
        xSemaphoreGive(mutex);
        if (!valid || (!fds[i].revents && !expired)) continue;
        if (snapshot.port) listener_ready(&snapshot, fds[i].revents);
        else snapshot.ready(snapshot.token, snapshot.fd, fds[i].revents, snapshot.ctx);
    }
    return true;
}
static void poll_task(void *arg)
{
    (void)arg;
    bool initialized=false;
    while (poll_once(&initialized)) {}
    if (initialized) lwip_socket_thread_cleanup();
    vTaskDelete(NULL);
}
