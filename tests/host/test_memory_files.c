/* Production memory backend and RTC state with only SDK services replaced. */
#define main panic_test_main
#include "test_module_panic.c"
#undef main
#define XIP_ERR_NOMEM (-6)
#define XIP_ERR_BUSY (-5)
#define XIP_ERR_NOTFOUND (-7)
#define XIP_ERR_SIZE (-8)
#define XIP_ERR_UNSUPPORTED (-2)
#define MALLOC_CAP_SPIRAM 1
#define MALLOC_CAP_8BIT 2
#define MALLOC_CAP_INTERNAL 4
static void *heap_caps_malloc(size_t len,int caps) {return (caps & MALLOC_CAP_SPIRAM)?NULL:malloc(len);}
static size_t snapshot_size=1;
static int xip_device_info(uint8_t *out,size_t cap) {if(cap<snapshot_size)return -8;memset(out,0,snapshot_size);out[0]=0xa0;return snapshot_size;}
static bool deny_internal;
static void *memory_internal_alloc(size_t size){return deny_internal?NULL:malloc(size);}
typedef struct {uint8_t *data;uint32_t len,version;uintptr_t token;uint8_t name[6],name_len,flags;} xip_memory_file;
typedef struct {
 uint32_t version,size;
 int (*open)(void *,uint8_t,const uint8_t *,size_t,uint8_t,uint32_t,uint32_t,xip_memory_file *);
 int (*close)(void *,uintptr_t,bool);
 int (*next)(void *,uint8_t,const uint8_t *,size_t,xip_memory_file *);
 int (*remove)(void *,uint8_t,const uint8_t *,size_t);
} xip_memory_files;
#include "../../components/xip_runtime/src/memory_files.inc"
static xip_memory_file open_file(uint8_t ns,const char *name,uint8_t mode,uint32_t size,uint32_t version) {
 xip_memory_file f;assert(memory_files_service.open(NULL,ns,(const uint8_t *)name,strlen(name),mode,size,version,&f)==0);return f;
}
int main(void) {
 panic_test_main();reboot(true);module_panics_init();
 xip_memory_file snapshot;
 snapshot_size=6000;snapshot=open_file(4,"status",0,0,0);assert(snapshot.len==6000);h_memory_close(NULL,snapshot.token,false);
 snapshot_size=17000;assert(h_memory_open(NULL,4,(const uint8_t *)"status",6,0,0,0,&snapshot)==XIP_ERR_SIZE);snapshot_size=1;
 deny_internal=true;assert(h_memory_open(NULL,1,(const uint8_t *)"low",3,1,100,0,&snapshot)==XIP_ERR_NOMEM);deny_internal=false;
 xip_memory_file f=open_file(1,"hello",1,5,0),b;
 memcpy(f.data,"hello",5);
 assert(h_memory_open(NULL,1,(const uint8_t *)"hello",5,1,5,0,&b)==XIP_ERR_BUSY);
 assert(h_memory_close(NULL,f.token,true)==0);
 f=open_file(1,"hello",0,0,0);assert(f.len==5 && f.version==1 && !memcmp(f.data,"hello",5));
 assert(h_memory_remove(NULL,1,(const uint8_t *)"hello",5)==XIP_ERR_BUSY);
 assert(h_memory_close(NULL,f.token,false)==0);
 f=open_file(1,"hello",1,2,2);memcpy(f.data,"no",2);assert(h_memory_close(NULL,f.token,false)==0);
 f=open_file(1,"hello",0,0,0);assert(f.version==1 && f.len==5);h_memory_close(NULL,f.token,false);
 assert(h_memory_open(NULL,1,(const uint8_t *)"hello",5,1,1,1,&f)==-21);
 assert(h_memory_open(NULL,2,(const uint8_t *)"ps",2,1,1,0,&f)==XIP_ERR_NOMEM);
 assert(h_memory_next(NULL,1,NULL,0,&f)==1 && !f.token && !f.data);
 assert(h_memory_remove(NULL,1,(const uint8_t *)"hello",5)==0);
 f=open_file(3,"rtc",1,4,7);assert(f.data >= retained_files.bytes && f.data+4 <= retained_files.bytes+6144);memcpy(f.data,"keep",4);assert(h_memory_close(NULL,f.token,true)==0);
 /* Simulate a new runtime: heap indices disappear; RTC bytes remain. */
 while(memory_files){struct memory_node *n=memory_files;memory_unlink(n);memory_free(n);}
 memory_files_initialized=false;reboot(false);
 f=open_file(3,"rtc",0,0,0);assert(f.version==7 && f.len==4 && !memcmp(f.data,"keep",4));h_memory_close(NULL,f.token,false);
 f=open_file(3,"rtc",1,4,8);memcpy(f.data,"drop",4);assert(h_memory_close(NULL,f.token,false)==0);
 f=open_file(3,"rtc",0,0,0);assert(f.version==7 && !memcmp(f.data,"keep",4));h_memory_close(NULL,f.token,false);
 assert(h_memory_open(NULL,3,(const uint8_t *)"big",3,1,6144,0,&f)==XIP_ERR_SIZE);
 assert(h_memory_remove(NULL,3,(const uint8_t *)"rtc",3)==0);
 assert(h_memory_open(NULL,3,(const uint8_t *)"rtc",3,1,1,8,&f)==-21);
 f=open_file(3,"rtc",1,1,0);assert(f.version==9);assert(h_memory_close(NULL,f.token,true)==0);
 assert(h_memory_remove(NULL,3,(const uint8_t *)"rtc",3)==0);
 while(memory_files){struct memory_node *n=memory_files;memory_unlink(n);memory_free(n);}
 memory_files_initialized=false;assert(h_memory_open(NULL,3,(const uint8_t *)"rtc",3,0,0,0,&f)==XIP_ERR_NOTFOUND);
 struct xip_instance in={.name="nan"};uint8_t text[]="panic";panic_as(&in,text,5);
 f=open_file(4,"panic",0,0,0);assert(f.len>5);h_memory_close(NULL,f.token,false);
 uint8_t saved[1024];int saved_len=panic_record(saved,sizeof(saved));assert(saved_len>0);
 f=open_file(5,"panic",1,(uint32_t)saved_len,0);memcpy(f.data,saved,saved_len);f.data[saved_len-1]^=1;
 assert(h_memory_close(NULL,f.token,true)==-21);assert(module_panics.message_len==5 && xip_module_panicked("nan"));
 f=open_file(5,"panic",1,(uint32_t)saved_len,0);memcpy(f.data,saved,saved_len);
 assert(h_memory_close(NULL,f.token,true)==0);assert(!module_panics.message_len && xip_module_panicked("nan"));
 f=open_file(4,"panic",0,0,0);assert(f.len==0);h_memory_close(NULL,f.token,false);
 /* A new panic is independent of the collector's earlier receipt. */
 panic_as(&in,text,5);
 f=open_file(5,"quar",1,3,0);memcpy(f.data,"nan",3);assert(h_memory_close(NULL,f.token,true)==0);assert(!xip_module_panicked("nan"));assert(module_panics.message_len==5);
 f=open_file(4,"status",0,0,0);assert(f.len==1);h_memory_close(NULL,f.token,false);
 f=open_file(1,"large",1,100000,0);memset(f.data,7,f.len);assert(h_memory_close(NULL,f.token,true)==0);
 f=open_file(1,"large",0,0,0);assert(f.len==100000 && f.data[99999]==7);h_memory_close(NULL,f.token,false);
 uint8_t *large_address=f.data;
 f=open_file(1,"large",3,0,0);assert(f.data==large_address);f.data[50000]=42;h_memory_close(NULL,f.token,false);
 f=open_file(1,"large",0,0,0);assert(f.version==2 && f.data[50000]==42 && f.data==large_address);h_memory_close(NULL,f.token,false);
 h_memory_remove(NULL,1,(const uint8_t *)"large",5);
 /* Variable records, updates, tombstones, and repeated compaction, without an index. */
 for(int i=0;i<80;i++) {
  f=open_file(3,"blob",1,300,0);memset(f.data,i,300);assert(h_memory_close(NULL,f.token,true)==0);
  f=open_file(3,"blob",0,0,0);assert(f.len==300 && f.version==(uint32_t)i+1 && f.data[299]==i);h_memory_close(NULL,f.token,false);
 }
 f=open_file(3,"tiny",1,1,0);f.data[0]=42;h_memory_close(NULL,f.token,true);
 assert(h_memory_remove(NULL,3,(const uint8_t *)"blob",4)==0);
 rtc_compact();assert(h_memory_open(NULL,3,(const uint8_t *)"blob",4,0,0,0,&f)==XIP_ERR_NOTFOUND);
 f=open_file(3,"tiny",0,0,0);assert(f.data[0]==42);h_memory_close(NULL,f.token,false);
 assert(memory_files==NULL); /* RTC has no heap catalog. */
 assert(sizeof(retained_files)==6144);
 puts("memory files: pin/abort/version/list, RTC retention, proc and quarantine ok");
}

