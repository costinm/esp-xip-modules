//! Archive diagnostics durably before acknowledging collected telemetry.
use super::*;
use std::io::Read;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Diagnostics {
    pub collected_at: u64,
    pub quarantined: Option<Vec<String>>,
    pub panics: Vec<Archive>,
    pub logs: Vec<Archive>,
    pub errors: Vec<String>,
    #[serde(default)]
    pub stats: Vec<Archive>,
    #[serde(default)]
    pub schema_gaps: Vec<String>,
    #[serde(default)]
    pub schemas: std::collections::BTreeMap<String,String>,
    #[serde(default)]
    pub logs_acknowledged_through: Option<u64>,
    #[serde(default)] pub sleep_collected: bool,
    #[serde(default)] pub rtc_panic_cleared: bool,
    #[serde(default)] pub flash_panic_removed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Archive {
    pub source: String,
    pub path: String,
    #[serde(default)]
    pub text_path: Option<String>,
    #[serde(default)]
    pub schema_gaps: Vec<String>,
    #[serde(default)] pub offset: Option<u64>,
    #[serde(default)] pub length: Option<u64>,
    #[serde(default)] pub text_offset: Option<u64>,
    #[serde(default)] pub text_length: Option<u64>,
    #[serde(default)] pub segment: Option<u64>,
    #[serde(skip)] pub rotated: Option<(u64,String)>,
    pub sha256: String,
    pub size_bytes: usize,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub first_seq: Option<u64>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub count: Option<u64>,
}
fn fields(ns:u8,name:Option<&[u8]>)->Vec<u8>{
    let mut e=minicbor::Encoder::new(Vec::new());
    e.map(if name.is_some(){2}else{1}).unwrap().u8(16).unwrap().u8(ns).unwrap();
    if let Some(name)=name{e.u8(1).unwrap().bytes(name).unwrap();}e.into_writer()
}
fn read_virtual(endpoint:&str,ns:u8,name:&[u8])->Result<Vec<u8>,String>{
    let mut s=files::request(endpoint,3,&fields(ns,Some(name)))?;
    let meta=files::metadata(&mut s)?;files::check(&meta)?;
    let len=u32_field(&meta,"5")?;
    if len>16384 || u32_field(&meta,"6")?!=0 || u32_field(&meta,"2")?!=len{return Err("invalid diagnostic length".into());}
    let mut body=Vec::new();Read::by_ref(&mut s).take(len as u64+1).read_to_end(&mut body).map_err(|e|e.to_string())?;
    if body.len()!=len as usize{return Err("truncated diagnostic".into());}Ok(body)
}
pub(super) fn panic_file_body(bytes:&[u8])->Result<Option<&[u8]>,String>{
    if bytes.iter().all(|b|*b==255){return Ok(None);}
    if bytes.len()<16 || &bytes[..4]!=b"PFL1" || bytes[12..16]!=0x50414e49u32.to_le_bytes(){return Err("partial or invalid panic file".into());}
    let len=u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let body=bytes.get(16..16usize.checked_add(len).ok_or("panic length overflow")?).ok_or("invalid panic file length")?;
    let mut h=2166136261u32;for b in body{h=(h^*b as u32).wrapping_mul(16777619);}
    if h!=u32::from_le_bytes(bytes[8..12].try_into().unwrap()){return Err("panic file checksum mismatch".into());}
    Ok(if body.is_empty(){None}else{Some(body)})
}
pub(super) fn is_panic_record(body:&[u8])->bool {
    let mut d=minicbor::Decoder::new(body);
    d.map().ok().flatten().is_some_and(|n|(2..=32).contains(&n)) &&
        d.u32().ok()==Some(0) && d.u32().ok()==Some(3) &&
        d.u32().ok()==Some(1) && d.str().ok()==Some("panic")
}
impl Inventory {
    fn archive_diagnostic(&self,status:&Status,source:&str,body:&[u8],seq:Option<u64>,count:Option<u64>)->Result<Archive,String>{
        self.append_telemetry(status,source,body,seq,count)
    }
    pub(super) fn sample_serial(&self,status:&mut Status,path:&str,force:bool)->Result<(),String>{
        if status.diagnostics.sleep_collected && !force{return Ok(());}
        if !path.starts_with("/dev/"){return Err("serial sampling needs a /dev/ path".into());}
        use std::os::unix::fs::OpenOptionsExt;
        let dir=self.paths.status.join(status.identity.id()?);private_dir(&dir)?;
        let collection_lock=fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).open(dir.join(".collector.lock")).map_err(|e|e.to_string())?;
        if unsafe{libc::flock(std::os::fd::AsRawFd::as_raw_fd(&collection_lock),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err("another telemetry collector is active for this device".into());}
        let reply=crate::console::query_serial_args(path,&["files.file.get".into(),"--namespace=4".into(),"--name=status".into()])?;
        let body=unhex(reply["result"]["13"].as_str().and_then(|s|s.strip_prefix("hex:")).ok_or("invalid serial snapshot")?)?;
        if reply["result"]["2"].as_u64()!=Some(body.len()as u64){return Err("serial snapshot exceeds local read limit; use TCP".into());}
        let info=crate::console::decode(&body)?;
        let mac=info["1"]["7"].as_str().and_then(|s|s.strip_prefix("hex:")).ok_or("serial status has no MAC")?;
        if !status.identity.macs.iter().any(|m|m.replace(':',"").eq_ignore_ascii_case(mac)){return Err("serial device differs from verified inventory".into());}
        if let Some(modules)=info["4"].as_array(){
            status.diagnostics.quarantined=Some(modules.iter().filter(|m|m["14"]==true).filter_map(|m|m["1"].as_str().map(str::to_owned)).collect());
            let base=status.flash["xip_base"].as_u64().unwrap_or(65536);
            for image in &mut status.images{
                if image.kind=="module"{if let Some(m)=modules.iter().find(|m|m["1"]==image.name){image.running=m["4"]==true&&m["5"]==image.version&&(m["12"].as_u64().is_none_or(|page|page*65536==base+image.offset as u64));image.quarantined=m["14"].as_bool();}}
            }
        }
        let mut sample=status.clone();sample.observed_at=now();status.diagnostics.collected_at=sample.observed_at;
        status.diagnostics.stats.clear();
        let native=info["7"].as_object().is_some_and(|p|!p.is_empty());
        let archive=self.append_telemetry(&sample,"stats",&body,None,None)?;self.add_serial_archive(status,archive);
        status.diagnostics.sleep_collected=native;
        if !native {
            if !status.images.iter().any(|i|i.name=="nan"&&i.quarantined!=Some(true)){return Err("legacy sleep snapshot needs the installed NAN provider".into());}
            let answer=crate::console::query_serial(path,"nan.powerinfo")?;
            let map=answer["result"].as_object().filter(|p|p.contains_key("12")&&p.contains_key("13")).ok_or("sleep response has no counters")?;
            let mut e=minicbor::Encoder::new(Vec::new());e.map(map.len()as u64).unwrap();
            for(k,v)in map{e.u32(k.parse().map_err(|_|"invalid sleep field")?).unwrap();if let Some(n)=v.as_u64(){e.u64(n).unwrap();}else if let Some(b)=v.as_bool(){e.bool(b).unwrap();}else{return Err("invalid sleep value".into());}}
            let archive=self.append_telemetry(&sample,"power",&e.into_writer(),None,None)?;self.add_serial_archive(status,archive);status.diagnostics.sleep_collected=true;
        }
        status.conditions.retain(|c|c!="SleepInfoUnavailable");
        if !status.conditions.iter().any(|c|c=="SerialStatusObserved"){status.conditions.push("SerialStatusObserved".into());}Ok(())
    }
    fn add_serial_archive(&self,status:&mut Status,archive:Archive){
        if let Some((segment,base))=&archive.rotated{for old in status.diagnostics.stats.iter_mut().chain(&mut status.diagnostics.panics).chain(&mut status.diagnostics.logs){if old.segment==Some(*segment)&&old.path=="diagnostics/telemetry.cbor"{old.path=format!("diagnostics/{base}.cbor");old.text_path=Some(format!("diagnostics/{base}.txt"));}}}
        status.diagnostics.stats.push(archive);
    }
    pub(super) fn collect_diagnostics(&self,status:&mut Status)->Result<(),String>{
        // One host collector owns read/archive/drain for this device as one operation.
        use std::os::unix::fs::OpenOptionsExt;
        let dir=self.paths.status.join(status.identity.id()?).join("diagnostics");private_dir(dir.parent().unwrap())?;private_dir(&dir)?;
        let collection_lock=fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).open(dir.parent().unwrap().join(".collector.lock")).map_err(|e|e.to_string())?;
        if unsafe{libc::flock(std::os::fd::AsRawFd::as_raw_fd(&collection_lock),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err("another telemetry collector is active for this device".into());}

        let mut virtual_supported=false;
        let mut rtc_panic=None;
        let mut flash_panic=None;
        let mut d=Diagnostics{collected_at:now(),..Default::default()};
        d.schemas.insert("xip-host".into(),"embedded api/generated/host.api.json".into());
        for image in status.images.iter().filter(|i|i.kind=="module") {
            let name=format!("xip-{}",image.name);
            match mesh::catalog::service_catalog_resolver().resolve(&name) {
                Some(Ok(c))=>{d.schemas.insert(name,c.path.display().to_string());},
                Some(Err(e))=>d.schema_gaps.push(format!("{name}: {e}")),
                None=>d.schema_gaps.push(format!("missing catalog {name} (installed image {})",image.version)),
            }
        }
        if status.images.iter().filter(|i|i.kind=="module").all(|i|i.quarantined.is_some()) {
            d.quarantined=Some(status.images.iter().filter(|i|i.quarantined==Some(true)).map(|i|i.name.clone()).collect());
        }
        // Old mod_flash ignores unknown namespace fields. Require the namespace echo.
        match files::request(&status.endpoint,8,&fields(4,None)).and_then(|mut s|files::metadata(&mut s)) {
            Ok(meta) if meta["16"]==4=>{
                virtual_supported=true;
                match read_virtual(&status.endpoint,5,b"quar") {
                    Ok(body)=>match crate::console::decode(&body) {
                        Ok(v)=>if let Some(names)=v.as_array(){
                            let names:Option<Vec<String>>=names.iter().map(|n|n.as_str().map(str::to_owned)).collect();
                            if names.is_some(){d.quarantined=names;}else{d.errors.push("invalid quarantine names".into());}
                        }else{d.errors.push("invalid quarantine snapshot".into());},
                        Err(e)=>d.errors.push(e),
                    },Err(e)=>d.errors.push(format!("quarantine: {e}")),
                }
                match read_virtual(&status.endpoint,4,b"panic") {
                    Ok(body) if !body.is_empty()=>{
                        d.panics.push(self.archive_diagnostic(status,"rtc-panic",&body,None,None)?);rtc_panic=Some(body);
                    },
                    Ok(_)=>{},Err(e)=>d.errors.push(format!("RTC panic: {e}")),
                }
            }
            Ok(_)=>d.errors.push("virtual diagnostics unavailable on this firmware".into()),
            Err(e)=>d.errors.push(format!("diagnostic capabilities: {e}")),
        }
        if let Some(file)=status.files.iter().find(|f|f.name=="panic" && f.name_hex.is_none()).cloned() {
            match files::read_versioned(&status.endpoint,&file) {
                Ok(body)=>{
                    match panic_file_body(&body) {
                        Ok(Some(data))=>{
                            let hash=hex(&Sha256::digest(data));
                            if !d.panics.iter().any(|a|a.sha256==hash){d.panics.push(self.archive_diagnostic(status,"flash-panic",data,None,None)?);}
                            flash_panic=Some(file);
                        },
                        Ok(None)=>{},Err(e)=>{d.errors.push(e);d.panics.push(self.archive_diagnostic(status,"partial-panic",&body,None,None)?);flash_panic=Some(file);}
                    }
                },Err(e)=>d.errors.push(format!("flash panic: {e}")),
            }
        }
        // Archive receipts above are durable before either remote diagnostic is drained.
        if let Some(file)=flash_panic {
            let mut e=minicbor::Encoder::new(Vec::new());e.map(2).unwrap().u8(1).unwrap().bytes(b"panic\0\0\0").unwrap().u8(5).unwrap().u32(file.version).unwrap();
            match files::request(&status.endpoint,5,&e.into_writer()).and_then(|mut s|files::metadata(&mut s)).and_then(|m|files::check(&m)){
                Ok(())=>{d.flash_panic_removed=true;status.files.retain(|f|f.name!="panic"||f.name_hex.is_some());},Err(e)=>d.errors.push(format!("panic file drain: {e}")),
            }
        }
        if let Some(body)=rtc_panic {
            let mut e=minicbor::Encoder::new(Vec::new());e.map(3).unwrap().u8(16).unwrap().u8(5).unwrap().u8(1).unwrap().bytes(b"panic").unwrap().u8(2).unwrap().u32(body.len()as u32).unwrap();
            let drained=(||->Result<(),String>{let mut s=files::request(&status.endpoint,4,&e.into_writer())?;s.write_all(&body).map_err(|e|e.to_string())?;s.shutdown(std::net::Shutdown::Write).map_err(|e|e.to_string())?;files::check(&files::metadata(&mut s)?)})();
            match drained{Ok(())=>d.rtc_panic_cleared=true,Err(e)=>d.errors.push(format!("RTC panic drain: {e}"))}
        }
        if virtual_supported {
                match read_virtual(&status.endpoint,4,b"status") {
                    Ok(body)=>match self.archive_diagnostic(status,"stats",&body,None,None){
                        Ok(a)=>{
                            d.sleep_collected=crate::console::decode(&body).ok().and_then(|v|v["7"].as_object().map(|p|!p.is_empty())).unwrap_or(false);
                            d.stats.push(a);
                        },Err(e)=>d.errors.push(format!("stats: {e}")),
                    },Err(e)=>d.errors.push(format!("stats: {e}")),
                }
        }
        let mut acknowledged=None;
        let logs=(||->Result<(),String>{
            let mut e=minicbor::Encoder::new(Vec::new());e.map(1).unwrap().u8(10).unwrap().u64(0).unwrap();
            let mut s=files::request(&status.endpoint,11,&e.into_writer())?;
            let(mut last,mut total)=(None,0usize);
            for _ in 0..8192 {
                let m=files::metadata(&mut s)?;files::check(&m)?;
                if m.as_object().is_some_and(|v|v.is_empty()){acknowledged=last;return Ok(());}
                let first=m["1"].as_u64().ok_or("missing log sequence")?;
                let count=m["2"].as_u64().filter(|n|*n>0).ok_or("invalid log count")?;
                let end=first.checked_add(count-1).ok_or("log sequence overflow")?;
                let len=u32_field(&m,"3")? as usize;
                total=total.checked_add(len).ok_or("log length overflow")?;
                if len>65506 || total>16*1024*1024 || last.is_some_and(|v|first<=v){return Err("invalid log stream".into());}
                let mut body=vec![0;len];s.read_exact(&mut body).map_err(|e|e.to_string())?;
                let existing=if count==1 && is_panic_record(&body){let hash=hex(&Sha256::digest(&body));d.panics.iter().find(|a|a.sha256==hash).cloned()}else{None};
                let archive=match existing{Some(mut a)=>{a.first_seq=Some(first);a.count=Some(count);a},None=>self.archive_diagnostic(status,"log",&body,Some(first),Some(count))?};
                if count==1 && is_panic_record(&body) && !d.panics.iter().any(|a|a.sha256==archive.sha256){d.panics.push(archive.clone());}
                d.logs.push(archive);last=Some(end);
            }
            Err("too many log batches".into())
        })();
        if let Err(e)=logs{d.errors.push(format!("logs: {e}"));}
        // Never acknowledge an incomplete stream or bytes that failed durable archival.
        if let Some(end)=acknowledged {
            let mut e=minicbor::Encoder::new(Vec::new());e.map(1).unwrap().u8(10).unwrap().u64(end).unwrap();
            match files::request(&status.endpoint,12,&e.into_writer()).and_then(|mut s|files::metadata(&mut s)).and_then(|m|files::check(&m)) {
                Ok(())=>d.logs_acknowledged_through=Some(end),Err(e)=>d.errors.push(format!("log acknowledgement: {e}")),
            }
        }
        let rotations=d.stats.iter().chain(&d.panics).chain(&d.logs).filter_map(|a|a.rotated.clone()).collect::<Vec<_>>();
        for(segment,base)in rotations{for a in d.stats.iter_mut().chain(&mut d.panics).chain(&mut d.logs){
            if a.segment==Some(segment) && a.path=="diagnostics/telemetry.cbor" {a.path=format!("diagnostics/{base}.cbor");a.text_path=Some(format!("diagnostics/{base}.txt"));}
        }}
        for archive in d.stats.iter().chain(&d.panics).chain(&d.logs) {
            d.schema_gaps.extend(archive.schema_gaps.iter().cloned());
        }
        d.schema_gaps.sort();d.schema_gaps.dedup();
        if !d.schema_gaps.is_empty(){status.conditions.push("SchemaIncomplete".into());}
        if !d.sleep_collected{status.conditions.push("SleepInfoUnavailable".into());}
        if let Some(names)=d.quarantined.as_mut(){names.sort();names.dedup();}
        if d.quarantined.as_ref().is_some_and(|q|!q.is_empty()){status.conditions.push("ModulesQuarantined".into());}
        if d.quarantined.is_none(){status.conditions.push("QuarantineUnknown".into());}
        if !d.errors.is_empty(){status.conditions.push("DiagnosticsIncomplete".into());}
        status.diagnostics=d;Ok(())
    }
}
#[cfg(test)]
mod tests{
 use super::*;
 #[test]fn legacy_panic_in_log_is_recognized(){assert!(is_panic_record(b"\xa4\x00\x03\x01\x65panic\x02\x63nan\x03\x40"));assert!(!is_panic_record(b"\xa0"));}
 #[test]fn panic_file_validation(){
  assert!(panic_file_body(&[255;1024]).unwrap().is_none());
  assert!(panic_file_body(b"partial").is_err());
  let body=b"\xa0";let mut bytes=vec![255;1024];bytes[..4].copy_from_slice(b"PFL1");
  bytes[4..8].copy_from_slice(&1u32.to_le_bytes());let hash=(2166136261u32^0xa0).wrapping_mul(16777619);
  bytes[8..12].copy_from_slice(&hash.to_le_bytes());bytes[12..16].copy_from_slice(&0x50414e49u32.to_le_bytes());bytes[16]=body[0];
  assert_eq!(panic_file_body(&bytes).unwrap(),Some(body.as_slice()));bytes[16]=0;
  assert!(panic_file_body(&bytes).is_err());
 }
 #[test]fn refresh_archives_panics_and_logs_even_when_file_version_is_unchanged(){
  use crate::inventory::tests::{inventory,config,server};
  let root=std::env::temp_dir().join(format!("dmesh-diagnostic-test-{}-{}",std::process::id(),now()));
  let inv=inventory(&root);let config=config();
  for event in [1u8,2,3] {
   let mut body=b"\xa4\x00\x03\x01\x65panic\x02\x63nan\x03\x41".to_vec();body.push(event);
   let mut record=vec![255;1024];record[..4].copy_from_slice(b"PFL1");record[4..8].copy_from_slice(&(body.len() as u32).to_le_bytes());
   let mut hash=2166136261u32;for b in &body{hash=(hash^*b as u32).wrapping_mul(16777619);}
   record[8..12].copy_from_slice(&hash.to_le_bytes());record[12..16].copy_from_slice(&0x50414e49u32.to_le_bytes());record[16..16+body.len()].copy_from_slice(&body);
   let log_body=if event==3 {vec![0xff]}else{body.clone()};
   let mut log=log_body.clone();log.push(0xa0);
   let key=format!("hex:{}",config.identity.public_key);
   let mut replies=vec![
    serde_json::json!({"1":[{"1":"module","2":"nan","3":42,"4":2000,"5":65536,"6":0,"7":false,"8":true}],"2":65536,"3":key,"4":65536,"5":4128768}),
    serde_json::json!({"1":[],"2":0,"3":key,"4":65536,"5":4128768}),
    serde_json::json!({"1":20,"2":0,"3":65504,"5":1310720}),
    serde_json::json!({"1":[{"1":"hex:70616e6963000000","2":1,"3":1024,"4":1,"5":false}],"2":false}),
    serde_json::json!({"4":1024,"16":4}),
    serde_json::json!({"1":0,"2":5,"3":1,"4":true,"5":5,"6":0,"_body":"81636e616e"}),
    serde_json::json!({"1":0,"2":body.len(),"3":1,"4":true,"5":body.len(),"6":0,"_body":hex(&body)}),
    serde_json::json!({"1":1,"2":1024,"3":1,"4":false,"5":1024,"6":0,"_body":hex(&record)}),
    serde_json::json!({"_expect_method":5,"_expect_fields":{"1":"hex:70616e6963000000","5":1}}),
    serde_json::json!({"_expect_method":4,"_expect_body":hex(&body)}),
    serde_json::json!({"1":0,"2":1,"3":1,"4":true,"5":1,"6":0,"_body":"a0"}),
    serde_json::json!({"1":7,"2":1,"3":log_body.len(),"_body":hex(&log)}),
   ];
   if event!=3 {replies.push(serde_json::json!({"_expect_method":12,"_expect_fields":{"10":7}}));}

   let(endpoint,worker)=server(replies);let status=inv.refresh(&config,&endpoint).unwrap();worker.join().unwrap();
   assert_eq!(status.diagnostics.quarantined,Some(vec!["nan".into()]));
   if event==3 {
       assert!(!status.diagnostics.errors.is_empty());assert_eq!(status.diagnostics.logs_acknowledged_through,None);
   }else{
       assert!(status.diagnostics.errors.is_empty(),"{:?}",status.diagnostics.errors);
       assert_eq!(status.diagnostics.logs_acknowledged_through,Some(7));
   }
   assert_eq!(status.diagnostics.panics.len(),1);
   assert!(status.diagnostics.rtc_panic_cleared && status.diagnostics.flash_panic_removed);assert_eq!(status.diagnostics.logs.len(),if event==3{0}else{1});
   for archive in status.diagnostics.panics.iter().chain(&status.diagnostics.logs){
    let path=inv.paths.status.join(status.identity.id().unwrap()).join(&archive.path);
    let all=fs::read(path).unwrap();let offset=archive.offset.unwrap()as usize;let len=archive.length.unwrap()as usize;
    let event=crate::console::decode(&all[offset..offset+len]).unwrap();
    assert_eq!(event["source"],"panic");assert_eq!(event["priority"],"high");assert_eq!(event["body"]["1"],"panic");
   }
   assert!(status.conditions.iter().any(|c|c=="ModulesQuarantined"));
  }
  let dir=inv.paths.status.join(config.identity.id().unwrap()).join("diagnostics");
  assert_eq!(fs::read_dir(&dir).unwrap().filter(|p|p.as_ref().unwrap().path().extension().is_some_and(|e|e=="json")).count(),0);
  let files=fs::read_dir(&dir).unwrap().filter(|p|p.as_ref().unwrap().file_name().to_string_lossy().starts_with("telemetry.")).count();assert_eq!(files,2);
  fs::remove_dir_all(root).unwrap();
 }

}
