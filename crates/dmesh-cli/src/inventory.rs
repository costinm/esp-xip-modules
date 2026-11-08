//! Desired configuration, reconstructible device status and transient discovery.
//! No credentials are serialized by this module.
use dmesh_server::{announce, device_record::{DeviceRecord, DeviceStore}};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write, net::Ipv6Addr, path::{Path, PathBuf}, time::{SystemTime, UNIX_EPOCH}};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct Paths { pub intended: PathBuf, pub status: PathBuf, pub discovery: PathBuf, pub key: PathBuf }
impl Paths {
    pub fn from_env() -> Result<Self, String> {
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is required")?);
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/state"));
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/tmp/dmesh-{}", unsafe { libc::geteuid() })));
        Ok(Self { intended: config.join("dmesh/devices"), status: state.join("dmesh/devices"),
            discovery: runtime.join("dmesh/discovery"), key: home.join(".ssh/dmesh.key") })
    }
}
fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
pub(crate) fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.is_ascii() || s.len()%2 != 0 { return Err("invalid hex public key".into()); }
    (0..s.len()).step_by(2).map(|i|u8::from_str_radix(&s[i..i+2],16).map_err(|_|"invalid hex public key".into())).collect()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub public_key: String,
    pub vip6: String,
    #[serde(default)] pub macs: Vec<String>,
    #[serde(default)] pub name: Option<String>,
    #[serde(default)] pub device_type: Option<String>,
}
impl Identity {
    pub fn id(&self) -> Result<String, String> {
        let key = unhex(&self.public_key)?;
        let parsed = p256::PublicKey::from_sec1_bytes(&key).map_err(|_|"invalid P-256 public key")?;
        use p256::elliptic_curve::sec1::ToSec1Point;
        if parsed.to_sec1_point(true).as_bytes()!=key { return Err("expected canonical compressed P-256 public key".into()); }
        let vip = announce::virtual_ip6(&key).ok_or("invalid public key")?;
        if self.vip6.parse::<Ipv6Addr>().map_err(|_|"invalid VIP")? != Ipv6Addr::from(vip) { return Err("VIP does not match public key".into()); }
        for mac in &self.macs { parse_mac(mac)?; }
        Ok(hex(&Sha256::digest(&key)))
    }
    pub fn from_record(r: &DeviceRecord) -> Result<Self, String> {
        let identity = Self { public_key: hex(&r.public_key), vip6: r.vip6.clone().ok_or("missing VIP")?,
            macs: [r.mac, r.nan_mac].into_iter().flatten().map(|m|mac_text(&m)).collect(), name:r.name.clone(), device_type:r.device_type.clone() };
        identity.id()?; Ok(identity)
    }
    fn record(&self, endpoints: BTreeMap<String,String>) -> Result<DeviceRecord,String> {
        self.id()?;
        let ip: Ipv6Addr=self.vip6.parse().map_err(|_|"invalid VIP")?;
        Ok(DeviceRecord { device_id:hex(&ip.octets()[8..]), name:self.name.clone(), public_key:unhex(&self.public_key)?,
            vip6:Some(ip.to_string()), mac:self.macs.first().map(|m|parse_mac(m)).transpose()?,
            nan_mac:self.macs.get(1).map(|m|parse_mac(m)).transpose()?, device_type:self.device_type.clone(), endpoints, ..Default::default() })
    }
}
fn parse_mac(s:&str)->Result<[u8;6],String> {
    let b=unhex(&s.replace(':',""))?; b.try_into().map_err(|_|"invalid MAC".into())
}
fn mac_text(m:&[u8;6])->String { m.iter().map(|b|format!("{b:02x}")).collect::<Vec<_>>().join(":") }
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageSpec {
    #[serde(default)] pub version: Option<u32>,
    #[serde(default)] pub artifact: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intended {
    pub generation: u64,
    pub identity: Identity,
    #[serde(default)] pub hardware: Vec<String>,
    #[serde(default)] pub endpoints: BTreeMap<String,String>,
    #[serde(default)] pub modules: BTreeMap<String,ImageSpec>,
    #[serde(default)] pub runtime: Option<ImageSpec>,
    /// Only remove extra modules when explicitly requested in desired configuration.
    #[serde(default)] pub prune: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageStatus {
    pub kind: String, pub name: String, pub version: u32, pub size_bytes: u32,
    pub allocated_bytes: u32, pub offset: u32, pub running: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub identity: Identity,
    pub observed_at: u64,
    pub attempted_at: u64,
    pub endpoint: String,
    pub intended_digest: Option<String>,
    pub observed_generation: Option<u64>,
    pub images: Vec<ImageStatus>,
    pub flash: serde_json::Value,
    pub conditions: Vec<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub observed_at: u64, pub source: String, pub endpoint: String,
    pub verified: bool, pub identity: Option<Identity>, pub facts: serde_json::Value,
}
#[derive(Clone)]
pub struct Inventory { pub paths: Paths }
impl Inventory {
    pub fn from_env()->Result<Self,String>{Ok(Self{paths:Paths::from_env()?})}
    pub fn discovery_store(&self)->DeviceStore{DeviceStore::new(self.paths.discovery.join("records"))}
    pub fn observe_signed(&self, a:&announce::Announce, mac:Option<[u8;6]>, source:&str, endpoint:&str)->Result<(),String>{
        let Some(record)=DeviceRecord::from_announce(a,mac) else{return Ok(());};
        let identity=Identity::from_record(&record)?;
        let id=identity.id()?;
        self.discovery_store().observe(&record)?;
        let observation=Observation{observed_at:now(),source:source.into(),endpoint:endpoint.into(),verified:true,identity:Some(identity),facts:serde_json::json!({})};
        self.save_observation(&id,&observation)
    }
    pub fn observe_recovery(&self, facts:&serde_json::Value, endpoint:&str)->Result<(),String>{
        let mac=facts["mac"].as_str().ok_or("recovery announcement missing MAC")?;
        let id=format!("mac-{}",hex(&parse_mac(mac)?));
        self.save_observation(&id,&Observation{observed_at:now(),source:"mod_flash".into(),endpoint:endpoint.into(),verified:false,identity:None,facts:facts.clone()})
    }
    fn save_observation(&self,id:&str,o:&Observation)->Result<(),String>{
        atomic(&self.paths.discovery.join("observations").join(format!("{id}.json")),&serde_json::to_vec_pretty(o).map_err(|e|e.to_string())?)
    }
    pub fn intended(&self)->Result<Vec<(String,Intended)>,String>{
        read_files::<Intended>(&self.paths.intended,"toml")?.into_iter().map(|(stem,c)|{
            if c.identity.id()? != stem {return Err(format!("configuration filename {stem} does not match identity"));}
            if c.generation==0{return Err("configuration generation must be positive".into());}
            Ok((stem,c))
        }).collect()
    }
    pub fn save_intended(&self,c:&Intended)->Result<(),String>{
        let id=c.identity.id()?;
        atomic(&self.paths.intended.join(format!("{id}.toml")),toml::to_string_pretty(c).map_err(|e|e.to_string())?.as_bytes())
    }
    pub fn statuses(&self)->Result<Vec<(String,Status)>,String>{read_files(&self.paths.status,"json")}
    pub fn observations(&self)->Result<Vec<(String,Observation)>,String>{read_files(&self.paths.discovery.join("observations"),"json")}
    pub fn save_status(&self,s:&Status)->Result<(),String>{atomic(&self.paths.status.join(format!("{}.json",s.identity.id()?)),&serde_json::to_vec_pretty(s).map_err(|e|e.to_string())?)}
    pub fn records(&self)->Result<Vec<DeviceRecord>,String>{
        let mut records:BTreeMap<String,DeviceRecord>=BTreeMap::new();
        for (_,s) in self.statuses()? { records.insert(s.identity.id()?,s.identity.record(BTreeMap::from([("flash".into(),s.endpoint)]))?); }
        for r in self.discovery_store().list()? {records.insert(Identity::from_record(&r)?.id()?,r);}
        for (id,c) in self.intended()? {
            let mut merged=records.remove(&id).unwrap_or(c.identity.record(BTreeMap::new())?);
            let mut endpoints=merged.endpoints;endpoints.extend(c.endpoints);
            merged=c.identity.record(endpoints)?;records.insert(id,merged);
        }
        Ok(records.into_values().collect())
    }
    pub fn resolve(&self,target:&str)->Result<(String,Intended),String>{
        let found:Vec<_>=self.intended()?.into_iter().filter(|(id,c)|identity_matches(id,&c.identity,target)).collect();
        match found.len(){1=>Ok(found.into_iter().next().unwrap()),0=>Err(format!("no intended configuration for {target}; use devices adopt first")),_=>Err("ambiguous device selector".into())}
    }
    pub fn adopt(&self,target:&str)->Result<PathBuf,String>{
        let mut matches=Vec::new();
        for r in self.records()?.into_iter().chain(DeviceStore::from_env(None).list()?) {
            let Ok(identity)=Identity::from_record(&r) else{continue;};let id=identity.id()?;
            if identity_matches(&id,&identity,target) && !matches.iter().any(|(k,_)|k==&id){matches.push((id,identity));}
        }
        if matches.len()!=1{return Err("adopt requires one verified device identity (discover or devices sync first)".into());}
        let (id,identity)=matches.pop().unwrap();let path=self.paths.intended.join(format!("{id}.toml"));
        if path.exists(){return Err("intended configuration already exists; it was not overwritten".into());}
        self.save_intended(&Intended{generation:1,identity,hardware:vec![],endpoints:BTreeMap::new(),modules:BTreeMap::new(),runtime:None,prune:false})?;
        Ok(path)
    }
}
fn identity_matches(id:&str,i:&Identity,target:&str)->bool {
    id==target || i.vip6.parse::<Ipv6Addr>().ok().zip(target.parse::<Ipv6Addr>().ok()).is_some_and(|(a,b)|a==b)
        || i.name.as_deref()==Some(target) || i.public_key.eq_ignore_ascii_case(target)
        || i.macs.iter().any(|m|m.eq_ignore_ascii_case(target))
}
fn read_files<T:for<'de>Deserialize<'de>>(dir:&Path,extension:&str)->Result<Vec<(String,T)>,String>{
    let entries=match fs::read_dir(dir){Ok(e)=>e,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(vec![]),Err(e)=>return Err(e.to_string())};
    let mut out:Vec<(String,T)>=Vec::new();for entry in entries {let p=entry.map_err(|e|e.to_string())?.path();if p.extension().and_then(|s|s.to_str())!=Some(extension){continue;}
        let text=fs::read_to_string(&p).map_err(|e|format!("{}: {e}",p.display()))?;
        let v=if extension=="toml" {toml::from_str(&text).map_err(|e|e.to_string())}else{serde_json::from_str(&text).map_err(|e|e.to_string())}.map_err(|e|format!("{}: {e}",p.display()))?;
        out.push((p.file_stem().unwrap().to_str().ok_or("invalid filename")?.into(),v));}
    out.sort_by(|a,b|a.0.cmp(&b.0));Ok(out)
}
fn atomic(path:&Path,bytes:&[u8])->Result<(),String>{
    use std::os::unix::fs::{DirBuilderExt,OpenOptionsExt};
    let parent=path.parent().ok_or("missing parent")?;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(|e|e.to_string())?;
    static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);
    let tmp=parent.join(format!(".write-{}-{}",std::process::id(),NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)));
    let result=(||{let mut f=fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;f.sync_all()?;fs::rename(&tmp,path)?;fs::File::open(parent)?.sync_all()})().map_err(|e:std::io::Error|e.to_string());
    if result.is_err(){let _=fs::remove_file(tmp);}result
}

/// A single private P-256 root. It is never serialized in device records or returned by Debug.
pub struct ControlKey(p256::SecretKey);
impl ControlKey {
    pub fn load_or_create(path:&Path)->Result<Self,String>{
        use p256::pkcs8::{DecodePrivateKey,EncodePrivateKey,LineEnding};
        if !path.exists(){
            let mut seed=[0u8;32];let mut f=fs::File::open("/dev/urandom").map_err(|e|e.to_string())?;
            std::io::Read::read_exact(&mut f,&mut seed).map_err(|e|e.to_string())?;
            let key=p256::SecretKey::from_slice(&seed).map_err(|_|"invalid random key")?;
            use std::os::unix::fs::{DirBuilderExt,OpenOptionsExt};
            fs::DirBuilder::new().recursive(true).mode(0o700).create(path.parent().ok_or("key has no parent")?).map_err(|e|e.to_string())?;
            // Exclusive create avoids replacing a root generated by another CLI process.
            let parent=path.parent().ok_or("key has no parent")?;
            let temporary=parent.join(format!(".dmesh-key-{}",std::process::id()));
            let result=(||->Result<(),String>{
                let mut f=fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary).map_err(|e|e.to_string())?;
                f.write_all(key.to_pkcs8_pem(LineEnding::LF).map_err(|e|e.to_string())?.as_bytes()).map_err(|e|e.to_string())?;
                f.sync_all().map_err(|e|e.to_string())?;
                match fs::hard_link(&temporary,path){Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>{},Err(e)=>return Err(e.to_string())}
                fs::File::open(parent).and_then(|f|f.sync_all()).map_err(|e|e.to_string())
            })();
            let _=fs::remove_file(temporary);result?;
        }
        let pem=fs::read_to_string(path).map_err(|e|e.to_string())?;
        Ok(Self(p256::SecretKey::from_pkcs8_pem(&pem).map_err(|_|"invalid dmesh control key (expected PKCS#8 P-256 PEM)")?))
    }
    pub fn public_key(&self)->Vec<u8>{use p256::elliptic_curve::sec1::ToSec1Point;self.0.public_key().to_sec1_point(true).as_bytes().to_vec()}
    pub fn device_secret(&self,identity:&Identity)->Result<[u8;32],String>{
        identity.id()?;
        let hk=hkdf::Hkdf::<sha2_10::Sha256>::new(Some(b"dmesh/control/v1"),self.0.to_bytes().as_slice());
        let mut info=b"device-credential\0".to_vec();info.extend(unhex(&identity.public_key)?);
        let mut out=[0;32];hk.expand(&info,&mut out).map_err(|_|"credential derivation failed")?;Ok(out)
    }
}

/// Read a metadata-only mod_flash stream without half-closing the request.
fn query(endpoint:&str, method:u32, offset:Option<u32>)->Result<serde_json::Value,String>{
    use std::{io::Read,net::{TcpStream,ToSocketAddrs},time::Duration};
    let address=endpoint.strip_prefix("tcp://").ok_or("expected tcp://HOST:PORT")?;
    let mut stream=None;
    for address in address.to_socket_addrs().map_err(|e|e.to_string())? {
        if let Ok(s)=TcpStream::connect_timeout(&address,Duration::from_secs(5)){stream=Some(s);break;}
    }
    let mut stream=stream.ok_or("cannot connect to mod_flash")?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).map_err(|e|e.to_string())?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e|e.to_string())?;
    let mut e=minicbor::Encoder::new(Vec::new());
    e.map(4).unwrap().u32(1).unwrap().u32(44).unwrap().u32(2).unwrap().u32(method).unwrap()
        .u32(3).unwrap().u32(1).unwrap().u32(5).unwrap().map(if offset.is_some(){1}else{0}).unwrap();
    if let Some(offset)=offset{e.u32(6).unwrap().u32(offset).unwrap();}
    stream.write_all(&e.into_writer()).map_err(|e|e.to_string())?;
    let mut response=vec![];
    std::io::Read::by_ref(&mut stream).take(4097).read_to_end(&mut response).map_err(|e|e.to_string())?;
    if response.len()>4096{return Err("oversized mod_flash metadata".into());}
    let mut d=minicbor::Decoder::new(&response);d.skip().map_err(|_|"incomplete mod_flash response")?;
    if d.position()!=response.len(){return Err("unexpected mod_flash response body".into());}
    let result=crate::console::decode(&response)?;
    if let Some(e)=result["7"].as_i64().filter(|e|*e<0){return Err(format!("flash error {e}"));}
    Ok(result)
}
fn u32_field(v:&serde_json::Value,key:&str)->Result<u32,String>{
    v[key].as_u64().and_then(|v|v.try_into().ok()).ok_or_else(||format!("missing/invalid inventory field {key}"))
}
fn wire_key(v:&serde_json::Value)->Result<Vec<u8>,String>{
    // console::decode renders CBOR bytes as hex:HEX.
    let text=v.as_str().ok_or("device did not return a public key")?;
    let bytes=unhex(text.strip_prefix("hex:").unwrap_or(text))?;
    use p256::elliptic_curve::sec1::ToSec1Point;
    Ok(p256::PublicKey::from_sec1_bytes(&bytes).map_err(|_|"invalid device public key")?.to_sec1_point(true).as_bytes().to_vec())
}
impl Inventory {
    pub fn refresh(&self,config:&Intended,endpoint:&str)->Result<Status,String>{
        let identity=config.identity.clone();let id=identity.id()?;
        let observed=self.read_status(config,endpoint);
        match observed {
            Ok(mut s)=>{
                let actions=match self.plan(config,&s) { Ok(a)=>a, Err(error)=>{
                    s.conditions.push("InvalidIntendedConfiguration".into());s.error=Some(error.clone());self.save_status(&s)?;return Err(error);
                }};
                if actions.is_empty(){s.observed_generation=Some(config.generation);s.conditions.push("ConfigurationMatched".into());}
                else{s.conditions.push("ConfigurationDrift".into());}
                self.save_status(&s)?;Ok(s)
            }
            Err(error)=>{
                // Preserve the last successful inventory and its timestamp on failure.
                let previous=self.statuses()?.into_iter().find(|(k,_)|k==&id).map(|(_,s)|s);
                let mut s=previous.unwrap_or(Status{identity,observed_at:0,attempted_at:0,endpoint:endpoint.into(),intended_digest:None,observed_generation:None,images:vec![],flash:serde_json::Value::Null,conditions:vec![],error:None});
                s.attempted_at=now();s.conditions=vec!["UnreachableOrInvalidInventory".into()];s.error=Some(error.clone());
                self.save_status(&s)?;Err(error)
            }
        }
    }
    fn read_status(&self,config:&Intended,endpoint:&str)->Result<Status,String>{
        let expected=unhex(&config.identity.public_key)?;let mut cursor=0;let mut images=vec![];
        let mut geometry=None;
        for _ in 0..257 {
            let v=query(endpoint,23,Some(cursor))?;
            if wire_key(&v["3"])?!=expected{return Err("mod_flash identity differs from intended device".into());}
            let shape=(u32_field(&v,"4")?,u32_field(&v,"5")?);
            if geometry.is_some_and(|g|g!=shape){return Err("flash geometry changed while reading inventory".into());}geometry=Some(shape);
            let entries=v["1"].as_array().ok_or("invalid image list")?;
            if entries.len()>1{return Err("invalid inventory page".into());}
            for e in entries {
                let kind=e["1"].as_str().filter(|k|matches!(*k,"runtime"|"module")).ok_or("invalid image kind")?.to_owned();
                let size=u32_field(e,"4")?;let allocated=u32_field(e,"5")?;let offset=u32_field(e,"6")?;
                if size==0 || allocated!=size.div_ceil(65536)*65536 || offset<cursor || offset%65536!=0 || offset.checked_add(allocated).is_none_or(|end|end>shape.1){return Err("invalid image extent".into());}
                images.push(ImageStatus{kind,name:e["2"].as_str().ok_or("missing image name")?.into(),version:u32_field(e,"3")?,size_bytes:size,allocated_bytes:allocated,offset,running:e["7"].as_bool().ok_or("missing running state")?});
            }
            let next=u32_field(&v,"2")?;
            if next==0 {
                if !entries.is_empty(){return Err("inventory ended without terminal page".into());}
                let mut flash=query(endpoint,8,None)?;
                flash["xip_base"]=shape.0.into();flash["xip_size"]=shape.1.into();
                return Ok(Status{identity:config.identity.clone(),observed_at:now(),attempted_at:now(),endpoint:endpoint.into(),intended_digest:Some(config_digest(config)?),observed_generation:None,images,flash,conditions:vec!["InventoryObserved".into(),"TransportUnauthenticated".into()],error:None});
            }
            if entries.len()!=1 || next<=cursor || next%65536!=0 || next>shape.1 {return Err("non-progressing image inventory".into());}cursor=next;
        }
        Err("too many image inventory pages".into())
    }
    pub fn plan(&self,c:&Intended,s:&Status)->Result<Vec<Action>,String>{
        if c.identity.id()?!=s.identity.id()?{return Err("status identity mismatch".into());}
        if s.error.is_some() || s.observed_at==0{return Err("fresh successful status is required".into());}
        let mut actions=vec![];
        for (name,spec) in &c.modules {
            image_name(name)?;
            let expected=self.spec_version(spec,"module")?;
            let installed=s.images.iter().filter(|i|i.kind=="module" && i.name==*name).max_by_key(|i|(i.version,std::cmp::Reverse(i.offset)));
            if installed.is_some_and(|i|expected.is_none_or(|v|i.version==v)){continue;}
            if installed.is_some_and(|i|expected.is_some_and(|v|i.version>v)){return Err(format!("{name}: downgrade would be shadowed by installed newer image"));}
            actions.push(self.install_action(name,"module",spec)?);
        }
        if let Some(spec)=&c.runtime {
            let version=self.spec_version(spec,"runtime")?;
            if !s.images.iter().any(|i|i.kind=="runtime" && version.is_none_or(|v|v==i.version)) {actions.push(self.install_action("","runtime",spec)?);}
        }
        if c.prune {
            if !c.modules.contains_key("flash"){return Err("pruning requires flash in intended modules".into());}
            for i in &s.images {
                if i.kind=="module" && !c.modules.contains_key(&i.name){actions.push(Action::Remove{name:i.name.clone(),offset:i.offset,version:i.version});}
            }
        }
        Ok(actions)
    }
    fn artifact(&self,spec:&ImageSpec)->Result<PathBuf,String>{
        let path=spec.artifact.as_ref().ok_or("missing artifact for requested image change")?;
        Ok(if path.is_absolute(){path.clone()}else{self.paths.intended.join(path)})
    }
    fn spec_version(&self,spec:&ImageSpec,kind:&str)->Result<Option<u32>,String>{
        if spec.artifact.is_none(){return Ok(spec.version);}
        let bytes=fs::read(self.artifact(spec)?).map_err(|e|e.to_string())?;
        let (_,v)=artifact_identity(&bytes,kind)?;
        if spec.version.is_some_and(|want|want!=v){return Err("artifact version differs from intended version".into());}Ok(Some(v))
    }
    fn install_action(&self,name:&str,kind:&str,spec:&ImageSpec)->Result<Action,String>{
        let artifact=self.artifact(spec)?;let bytes=fs::read(&artifact).map_err(|e|e.to_string())?;
        let (actual,version)=artifact_identity(&bytes,kind)?;
        if actual!=name || spec.version.is_some_and(|v|v!=version){return Err("artifact identity/version differs from intended configuration".into());}
        Ok(Action::Install{name:name.into(),kind:kind.into(),version,artifact,sha256:hex(&Sha256::digest(bytes))})
    }
    pub fn reconcile(&self,c:&Intended,endpoint:&str,apply:bool)->Result<serde_json::Value,String>{
        let s=self.refresh(c,endpoint)?;let actions=self.plan(c,&s)?;
        if apply {
            // Validate every artifact and operation before the first mutation.
            for a in &actions {if let Action::Remove{offset,..}=a {if s.images.iter().any(|i|i.offset==*offset && i.running){return Err("a module to remove is running; stop it before reconciliation".into());}}}
            for a in &actions {
                match a {
                    Action::Install{kind,artifact,version,sha256,..}=>{
                        let bytes=fs::read(artifact).map_err(|e|e.to_string())?;
                        if hex(&Sha256::digest(bytes))!=*sha256{return Err("artifact changed after planning".into());}
                        crate::tcp::run(&[endpoint.into(),format!("flash.{kind}.write"),format!("--input={}",artifact.display()),format!("--version={version}")])?;
                    }
                    Action::Remove{offset,..}=>crate::tcp::run(&[endpoint.into(),"flash.image.remove".into(),format!("--offset={offset}")])?,
                }
            }
            let verified=self.refresh(c,endpoint)?;
            if !self.plan(c,&verified)?.is_empty(){return Err("device inventory still differs after reconciliation".into());}
            return Ok(serde_json::json!({"applied":actions,"status":verified,"health_verified":false}));
        }
        Ok(serde_json::json!({"plan":actions,"status":s}))
    }
}
fn config_digest(c:&Intended)->Result<String,String>{Ok(hex(&Sha256::digest(toml::to_string(c).map_err(|e|e.to_string())?.as_bytes())))}
fn image_name(name:&str)->Result<(),String>{if name.is_empty()||name.len()>32||!name.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'_'){Err("invalid module name".into())}else{Ok(())}}
#[derive(Clone,Debug,Serialize)]
#[serde(tag="action",rename_all="snake_case")]
pub enum Action { Install {name:String,kind:String,version:u32,artifact:PathBuf,sha256:String}, Remove{name:String,offset:u32,version:u32} }
fn artifact_identity(bytes:&[u8],kind:&str)->Result<(String,u32),String>{
    if kind=="runtime" {
        if bytes.len()<80 || bytes[0]!=0xe9{return Err("expected raw runtime image".into());}
        let n=bytes[48..80].split(|b|!b.is_ascii_digit()).next().unwrap_or(&[]);
        let v=std::str::from_utf8(n).map_err(|_|"invalid runtime version")?.parse().map_err(|_|"invalid runtime version")?;return Ok((String::new(),v));
    }
    if bytes.len()<64 || bytes[0]!=b'M'{return Err("expected module image".into());}
    let at=u32::from_le_bytes(bytes[44..48].try_into().unwrap()) as usize;let len=u32::from_le_bytes(bytes[48..52].try_into().unwrap()) as usize;
    let data=bytes.get(at..at.checked_add(len).ok_or("invalid manifest")?).ok_or("invalid manifest")?;
    let mut d=minicbor::Decoder::new(data);let count=d.map().map_err(|e|e.to_string())?.ok_or("indefinite manifest")?;
    let (mut name,mut version)=(None,None);for _ in 0..count {match d.u32().map_err(|e|e.to_string())?{1=>name=Some(d.str().map_err(|e|e.to_string())?.to_owned()),8=>version=Some(d.u32().map_err(|e|e.to_string())?),_=>{d.skip().map_err(|e|e.to_string())?;}}}
    Ok((name.ok_or("module manifest missing name")?,version.ok_or("module manifest missing version")?))
}
fn option<'a>(args:&'a [String],name:&str)->Option<&'a str>{args.iter().find_map(|a|a.strip_prefix(&format!("--{name}=")))}
pub(crate) fn run(args:&[String])->Result<(),String>{
    let inventory=Inventory::from_env()?;
    let command=args.first().map(String::as_str).unwrap_or("list");
    let output=match command {
        "paths"=>serde_json::to_value(&inventory.paths).map_err(|e|e.to_string())?,
        "key-init"=>{let key=ControlKey::load_or_create(&inventory.paths.key)?;serde_json::json!({"path":inventory.paths.key,"public_key":hex(&key.public_key())})},
        "list"=>serde_json::json!({"intended":inventory.intended()?,"status":inventory.statuses()?,"discovery":inventory.observations()?}),
        "adopt"=>{let target=args.get(1).ok_or("devices adopt ID|VIP|NAME|MAC")?;serde_json::json!({"config":inventory.adopt(target)?})},
        "show"|"refresh"|"reconcile"|"plan"=>{
            let target=args.get(1).ok_or("devices show|refresh|plan|reconcile DEVICE [--endpoint=tcp://HOST:5227] [--apply]")?;
            let (id,c)=inventory.resolve(target)?;
            if command=="show" {serde_json::json!({"id":id,"intended":c,"status":inventory.statuses()?.into_iter().find(|(k,_)|*k==id).map(|(_,s)|s)})}
            else {
                let endpoint=option(args,"endpoint").or_else(||c.endpoints.get("flash").map(String::as_str)).ok_or("supply --endpoint=tcp://HOST:5227 or intended endpoints.flash")?;
                if command=="refresh"{serde_json::to_value(inventory.refresh(&c,endpoint)?).map_err(|e|e.to_string())?}
                else {inventory.reconcile(&c,endpoint,command=="reconcile"&&args.iter().any(|a|a=="--apply"))?}
            }
        },
        "provision"=>{
            let (_,c)=inventory.resolve(args.get(1).ok_or("devices provision DEVICE --serial=/dev/serial/by-id/...")?)?;
            let serial=option(args,"serial").filter(|s|s.starts_with("/dev/")).ok_or("USB provisioning requires --serial=/dev/...")?;
            provision_usb(&inventory,&c,serial)?;serde_json::json!({"provisioned":c.identity.vip6})
        },
        _=>return Err("devices paths|list|adopt|show|refresh|plan|reconcile|key-init|provision (also check|sync)".into()),
    };
    println!("{}",serde_json::to_string_pretty(&output).map_err(|e|e.to_string())?);Ok(())
}
fn provision_usb(inventory:&Inventory,c:&Intended,serial:&str)->Result<(),String>{
    use dmesh_server::{provision,tagged};use std::time::Duration;
    let rt=tokio::runtime::Runtime::new().map_err(|e|e.to_string())?;
    let mut request=[0;256];let id=now()|1;
    let n=announce::encode_discovery_request(id,&mut request).ok_or("encode signed identity request")?;
    let response=rt.block_on(crate::node_client::request_uart(serial,None,&request[..n],Duration::from_secs(10)))?.bytes;
    let record=tagged::decode(&response).ok_or("invalid USB discovery response")?;
    if record.id!=Some(id){return Err("USB discovery response ID mismatch".into());}
    let a=announce::decode_record(record).ok_or("invalid USB discovery response")?;
    let observed=DeviceRecord::from_announce(&a,None).ok_or("USB identity is not signed")?;
    if Identity::from_record(&observed)?.id()?!=c.identity.id()?{return Err("USB device identity differs from intended device".into());}
    let key=ControlKey::load_or_create(&inventory.paths.key)?;let secret=key.device_secret(&c.identity)?;let root=key.public_key();
    let name=c.identity.name.as_deref().ok_or("intended device needs a name for provisioning")?;
    let n=provision::encode_install(id,provision::Install{name,root_public_key:&root,secret:&secret},&mut request).ok_or("invalid provisioning fields")?;
    let response=rt.block_on(crate::node_client::request_uart(serial,None,&request[..n],Duration::from_secs(10)))?.bytes;
    let record=tagged::decode(&response).ok_or("invalid provisioning response")?;
    if record.component!=Some(tagged::Name::Tag(provision::COMPONENT)) || record.method!=Some(tagged::Name::Tag(provision::INSTALL)) || record.id!=Some(id) || record.error.is_some() || record.result!=Some(&[0xf5]){return Err("device did not confirm durable provisioning".into());}Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn identity(seed:u8)->Identity {
        use p256::elliptic_curve::sec1::ToSec1Point;
        let key=p256::SecretKey::from_slice(&[seed;32]).unwrap().public_key().to_sec1_point(true);
        Identity{public_key:hex(key.as_bytes()),vip6:Ipv6Addr::from(announce::virtual_ip6(key.as_bytes()).unwrap()).to_string(),macs:vec!["02:00:00:00:00:07".into()],name:Some(format!("board{seed}")),device_type:Some("esp32c6".into())}
    }
    fn inventory(root:&Path)->Inventory{Inventory{paths:Paths{intended:root.join("config/devices"),status:root.join("state/devices"),discovery:root.join("run/discovery"),key:root.join("ssh/dmesh.key")}}}
    fn config()->Intended{Intended{generation:1,identity:identity(7),hardware:vec![],endpoints:BTreeMap::new(),modules:BTreeMap::new(),runtime:None,prune:false}}
    fn status()->Status{Status{identity:identity(7),observed_at:12,attempted_at:12,endpoint:"tcp://127.0.0.1:5227".into(),intended_digest:None,observed_generation:None,images:vec![ImageStatus{kind:"module".into(),name:"flash".into(),version:100,size_bytes:70000,allocated_bytes:131072,offset:0x200000,running:true}],flash:serde_json::json!({"1":20}),conditions:vec![],error:None}}
    #[test] fn identity_and_public_configuration_are_strict(){
        let c=config();let text=toml::to_string(&c).unwrap();assert!(!text.contains("secret"));
        assert!(toml::from_str::<Intended>(&format!("secret = 'bad'\n{text}")).is_err());
        let mut i=c.identity;i.vip6="fc00::1".into();assert!(i.id().is_err());
        let mut i=identity(7);i.public_key="ff".repeat(33);assert!(i.id().is_err());
    }
    #[test] fn desired_and_observed_survive_discovery_loss(){
        let t=tempfile::tempdir().unwrap();let i=inventory(t.path());let c=config();i.save_intended(&c).unwrap();i.save_status(&status()).unwrap();
        i.observe_recovery(&serde_json::json!({"mac":"02:00:00:00:00:07","port":5227}),"192.0.2.1:5227").unwrap();
        assert_eq!(i.observations().unwrap().len(),1);let original=fs::read(i.paths.intended.join(format!("{}.toml",c.identity.id().unwrap()))).unwrap();
        fs::remove_dir_all(&i.paths.discovery).unwrap();let reopened=inventory(t.path());
        assert!(reopened.observations().unwrap().is_empty());assert_eq!(reopened.statuses().unwrap()[0].1.images[0].size_bytes,70000);
        assert_eq!(reopened.resolve("board7").unwrap().1.generation,1);assert_eq!(reopened.records().unwrap().len(),1);
        assert_eq!(original,fs::read(i.paths.intended.join(format!("{}.toml",c.identity.id().unwrap()))).unwrap());
    }
    #[test] fn root_key_persists_and_credentials_are_identity_specific(){
        let t=tempfile::tempdir().unwrap();let i=inventory(t.path());let key=ControlKey::load_or_create(&i.paths.key).unwrap();let secret=key.device_secret(&identity(7)).unwrap();
        assert_ne!(secret,key.device_secret(&identity(8)).unwrap());
        let loaded=ControlKey::load_or_create(&i.paths.key).unwrap();assert_eq!(loaded.device_secret(&identity(7)).unwrap(),secret);assert_eq!(loaded.public_key(),key.public_key());
        assert_eq!(fs::metadata(&i.paths.key).unwrap().permissions().mode()&0o777,0o600);
        fs::write(&i.paths.key,b"broken root").unwrap();assert!(ControlKey::load_or_create(&i.paths.key).is_err());assert_eq!(fs::read(&i.paths.key).unwrap(),b"broken root");
    }
    #[test] fn plan_preserves_extras_and_blocks_downgrade_and_flash_pruning(){
        let t=tempfile::tempdir().unwrap();let i=inventory(t.path());let mut c=config();let s=status();
        assert!(i.plan(&c,&s).unwrap().is_empty());c.modules.insert("flash".into(),ImageSpec{version:Some(99),artifact:None});assert!(i.plan(&c,&s).unwrap_err().contains("downgrade"));
        c.modules.clear();c.prune=true;assert!(i.plan(&c,&s).unwrap_err().contains("requires flash"));
        c.modules.insert("flash".into(),ImageSpec::default());assert!(i.plan(&c,&s).unwrap().is_empty());
        c.modules.insert("berry".into(),ImageSpec::default());assert!(i.plan(&c,&s).is_err());
    }
    fn encode_json(e:&mut minicbor::Encoder<Vec<u8>>,v:&serde_json::Value){
        use serde_json::Value::*;
        match v{Null=>{e.null().unwrap();},Bool(v)=>{e.bool(*v).unwrap();},Number(v)=>{e.i64(v.as_i64().unwrap()).unwrap();},String(v)=>{if let Some(s)=v.strip_prefix("hex:"){e.bytes(&unhex(s).unwrap()).unwrap();}else{e.str(v).unwrap();}},Array(v)=>{e.array(v.len()as u64).unwrap();for v in v{encode_json(e,v);}},Object(v)=>{e.map(v.len()as u64).unwrap();for(k,v)in v{e.u32(k.parse().unwrap()).unwrap();encode_json(e,v);}}}
    }
    fn server(replies:Vec<serde_json::Value>)->(String,std::thread::JoinHandle<()>){
        use std::net::TcpListener;use std::io::Read;
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();let addr=listener.local_addr().unwrap();
        (format!("tcp://{addr}"),std::thread::spawn(move||for reply in replies{
            let(mut socket,_)=listener.accept().unwrap();socket.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
            let mut request=vec![];loop{let mut b=[0];socket.read_exact(&mut b).unwrap();request.push(b[0]);let mut d=minicbor::Decoder::new(&request);if d.skip().is_ok(){break;}}
            // Deliberately reply before FIN: metadata-only requests are length delimited.
            let mut e=minicbor::Encoder::new(Vec::new());encode_json(&mut e,&reply);socket.write_all(&e.into_writer()).unwrap();
        }))
    }
    fn replies(id:&Identity)->Vec<serde_json::Value>{
        let key=format!("hex:{}",id.public_key);
        vec![serde_json::json!({"1":[{"1":"runtime","2":"","3":22,"4":100000,"5":131072,"6":0,"7":true}],"2":131072,"3":key,"4":65536,"5":4128768}),
            serde_json::json!({"1":[],"2":0,"3":key,"4":65536,"5":4128768}),serde_json::json!({"1":20,"2":0,"3":65504,"5":1310720})]
    }
    #[test] fn refresh_reads_real_sizes_and_persists_failure_without_losing_last_status(){
        let t=tempfile::tempdir().unwrap();let i=inventory(t.path());let c=config();i.save_intended(&c).unwrap();
        let(endpoint,thread)=server(replies(&c.identity));let s=i.refresh(&c,&endpoint).unwrap();thread.join().unwrap();
        assert_eq!(s.images[0].version,22);assert_eq!(s.images[0].size_bytes,100000);assert_eq!(s.images[0].allocated_bytes,131072);assert_eq!(s.observed_generation,Some(1));
        let(endpoint,thread)=server(vec![serde_json::json!({"7":-7})]);assert!(i.refresh(&c,&endpoint).is_err());thread.join().unwrap();
        let saved=i.statuses().unwrap().pop().unwrap().1;assert_eq!(saved.images[0].version,22);assert_eq!(saved.observed_at,s.observed_at);assert!(saved.error.is_some());
    }
    #[test] fn refresh_rejects_other_device_and_bad_cursor(){
        let t=tempfile::tempdir().unwrap();let i=inventory(t.path());let c=config();
        let(endpoint,thread)=server(vec![replies(&identity(8)).remove(0)]);assert!(i.refresh(&c,&endpoint).unwrap_err().contains("identity differs"));thread.join().unwrap();
        let mut bad=replies(&c.identity).remove(0);bad["2"]=0.into();let(endpoint,thread)=server(vec![bad]);assert!(i.refresh(&c,&endpoint).is_err());thread.join().unwrap();
        assert!(i.statuses().unwrap()[0].1.images.is_empty());
    }
    #[test] fn atomic_public_files_have_private_modes_and_no_temporary_debris(){
        let t=tempfile::tempdir().unwrap();let i=inventory(t.path());i.save_intended(&config()).unwrap();i.save_status(&status()).unwrap();
        let paths:Vec<_>=fs::read_dir(&i.paths.status).unwrap().map(|e|e.unwrap().path()).collect();assert_eq!(paths.len(),1);assert_eq!(fs::metadata(&paths[0]).unwrap().permissions().mode()&0o777,0o600);
    }
}
