/* Included with runtime instance access; no module pointer is retained by lwIP. */
static int c_ip_open(void *ctx,const xip_ip_config *config,uint32_t *link)
{ return xip_ip_open(I(ctx)->id,config,link); }
static int c_ip_set_up(void *ctx,uint32_t link,uint32_t up)
{ return xip_ip_set_up(I(ctx)->id,link,up); }
static int c_ip_close(void *ctx,uint32_t link)
{ return xip_ip_close(I(ctx)->id,link); }
static const xip_ip ip_component={.version=1,.size=sizeof(xip_ip),.open=c_ip_open,.set_up=c_ip_set_up,.close=c_ip_close};
