/* Port subscriptions share the existing per-frame fan-out and packet references. */
static int port_bind(struct xip_instance *in)
{
    if (!in->component) return XIP_ERR_ARG;
    int rc = XIP_OK;
    portENTER_CRITICAL(&registry_lock);
    int free_at = -1;
    bool already_bound = false;
    for (int i = 0; i < XIP_PORTS_MAX; i++) {
        if (ports[i].port == in->port && ports[i].component == in->component) already_bound = true;
        if (!ports[i].port && free_at < 0) free_at = i;
    }
    if (!rc && !already_bound && free_at < 0) rc = XIP_ERR_NOMEM;
    if (!rc && !already_bound) { ports[free_at].port = in->port; ports[free_at].component = in->component; }
    portEXIT_CRITICAL(&registry_lock);
    if (rc) return rc;
    /* the serial link was registered at xip_host_init: binding a port only routes records to it */
    return XIP_OK;
}

