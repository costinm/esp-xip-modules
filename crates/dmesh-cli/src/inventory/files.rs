//! Managed master files and the device-side configuration commit marker.
use super::*;
use std::{
    io::Read,
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};

pub const EPOCH_FILE: &str = "cfg_ep";
const MAX_FILE: u32 = 65504;
// xip-abi ERR_NOTFOUND, the existing mod_flash filesystem protocol.
const NOT_FOUND: i64 = -7;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileStatus {
    pub name: String,
    pub version: u32,
    pub size_bytes: u32,
    pub kind: u32,
    pub cache: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ManagedFile {
    pub name: String,
    pub source: PathBuf,
    pub version: u32,
    pub size_bytes: u32,
    pub sha256: String,
}

pub(super) fn request(endpoint: &str, method: u32, fields: &[u8]) -> Result<TcpStream, String> {
    let address = endpoint
        .strip_prefix("tcp://")
        .ok_or("expected tcp://HOST:PORT")?;
    let mut stream = None;
    for address in address.to_socket_addrs().map_err(|e| e.to_string())? {
        if let Ok(s) = TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
            stream = Some(s);
            break;
        }
    }
    let mut stream = stream.ok_or("cannot connect to mod_flash")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let mut e = minicbor::Encoder::new(Vec::new());
    e.map(4)
        .unwrap()
        .u32(1)
        .unwrap()
        .u32(44)
        .unwrap()
        .u32(2)
        .unwrap()
        .u32(method)
        .unwrap()
        .u32(3)
        .unwrap()
        .u32(1)
        .unwrap()
        .u32(5)
        .unwrap();
    let mut envelope = e.into_writer();
    envelope.extend_from_slice(fields);
    stream.write_all(&envelope).map_err(|e| e.to_string())?;
    Ok(stream)
}
fn metadata(stream: &mut TcpStream) -> Result<serde_json::Value, String> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        stream
            .read_exact(&mut byte)
            .map_err(|e| format!("file response metadata: {e}"))?;
        bytes.push(byte[0]);
        let mut d = minicbor::Decoder::new(&bytes);
        if d.skip().is_ok() && d.position() == bytes.len() {
            return crate::console::decode(&bytes);
        }
        if bytes.len() == 4096 {
            return Err("oversized mod_flash metadata".into());
        }
    }
}
fn check(v: &serde_json::Value) -> Result<(), String> {
    if let Some(e) = v["7"].as_i64().filter(|e| *e < 0) {
        Err(format!("flash error {e}"))
    } else {
        Ok(())
    }
}
fn read_versioned(endpoint: &str, file: &FileStatus) -> Result<Vec<u8>, String> {
    let mut e = minicbor::Encoder::new(Vec::new());
    e.map(2)
        .unwrap()
        .u32(1)
        .unwrap()
        .bytes(&file.wire_name()?)
        .unwrap()
        .u32(5)
        .unwrap()
        .u32(file.version)
        .unwrap();
    let mut stream = request(endpoint, 3, &e.into_writer())?;
    let v = metadata(&mut stream)?;
    if v["7"].as_i64() == Some(NOT_FOUND) {
        return Err(format!("file {} disappeared after listing", file.name));
    }
    check(&v)?;
    let total = u32_field(&v, "2")?;
    let size = u32_field(&v, "5")?;
    if u32_field(&v, "1")? != file.version
        || total != file.size_bytes
        || total > MAX_FILE
        || size != total
        || u32_field(&v, "6")? != 0
    {
        return Err("file changed after listing or invalid complete-file response".into());
    }
    let mut body = Vec::new();
    Read::by_ref(&mut stream)
        .take(size as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    if body.len() != size as usize {
        return Err("truncated or oversized file response".into());
    }
    Ok(body)
}
impl FileStatus {
    fn wire_name(&self) -> Result<Vec<u8>, String> {
        let mut name = match &self.name_hex {
            Some(v) => unhex(v)?,
            None => self.name.as_bytes().to_vec(),
        };
        if name.is_empty() || name.len() > 8 {
            return Err("invalid cached file name".into());
        }
        name.resize(8, 0);
        Ok(name)
    }
}
pub(super) fn list(endpoint: &str) -> Result<Vec<FileStatus>, String> {
    let mut after: Option<Vec<u8>> = None;
    let mut result = Vec::new();
    loop {
        let mut e = minicbor::Encoder::new(Vec::new());
        e.map(if after.is_some() { 2 } else { 1 })
            .unwrap()
            .u32(2)
            .unwrap()
            .u32(16)
            .unwrap();
        if let Some(after) = &after {
            e.u32(1).unwrap().bytes(after).unwrap();
        }
        let mut stream = request(endpoint, 6, &e.into_writer())?;
        let v = metadata(&mut stream)?;
        check(&v)?;
        let entries = v["1"].as_array().ok_or("invalid file list")?;
        if entries.len() > 16 {
            return Err("oversized file list page".into());
        }
        for entry in entries {
            let bytes = unhex(
                entry["1"]
                    .as_str()
                    .and_then(|s| s.strip_prefix("hex:"))
                    .ok_or("invalid file name")?,
            )?;
            if bytes.is_empty() || bytes.len() > 8 || after.as_ref().is_some_and(|a| a >= &bytes) {
                return Err("non-progressing file inventory".into());
            }
            let ordinary = std::str::from_utf8(&bytes)
                .ok()
                .map(|s| s.trim_end_matches('\0'))
                .filter(|s| !s.is_empty() && *s != "." && *s != ".." && !s.contains(['\0', '/']));
            let name_hex = ordinary.is_none().then(|| hex(&bytes));
            let name = ordinary
                .map(str::to_owned)
                .unwrap_or_else(|| format!("hex:{}", hex(&bytes)));
            result.push(FileStatus {
                name,
                version: u32_field(entry, "2")?,
                size_bytes: u32_field(entry, "3")?,
                kind: u32_field(entry, "4")?,
                cache: entry["5"].as_bool().ok_or("missing file cache flag")?,
                name_hex,
                sha256: None,
            });
            after = Some(bytes);
        }
        let more = v["2"].as_bool().ok_or("missing file list completion")?;
        if !more {
            return Ok(result);
        }
        if entries.is_empty() {
            return Err("file inventory did not advance".into());
        }
    }
}
pub(super) fn write(
    endpoint: &str,
    name: &str,
    path: &Path,
    version: Option<u32>,
) -> Result<(), String> {
    let mut args = vec![
        endpoint.into(),
        "flash.file.write".into(),
        format!("--name=hex:{}", hex(name.as_bytes())),
        format!("--input={}", path.display()),
    ];
    if let Some(version) = version {
        args.push(format!("--version={version}"));
    }
    crate::tcp::run_quiet(&args)
}
pub(super) fn write_generation(endpoint: &str, value: u64, staging: &Path) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new_in(staging).map_err(|e| e.to_string())?;
    file.write_all(&value.to_le_bytes())
        .map_err(|e| e.to_string())?;
    write(endpoint, EPOCH_FILE, file.path(), None)
}
impl Inventory {
    pub fn status_dir(&self, identity: &Identity) -> Result<PathBuf, String> {
        Ok(self.paths.status.join(identity.id()?))
    }
    pub fn status_files_dir(&self, identity: &Identity) -> Result<PathBuf, String> {
        Ok(self.status_dir(identity)?.join("files"))
    }
    fn cached_path(&self, identity: &Identity, file: &FileStatus) -> Result<PathBuf, String> {
        let root = self.status_dir(identity)?;
        Ok(match &file.name_hex {
            Some(v) => root.join("opaque-files").join(hex(&unhex(v)?)),
            None => {
                let name = &file.name;
                if name.is_empty() || name == "." || name == ".." || name.contains(['\0', '/']) {
                    return Err("invalid cached file path".into());
                }
                root.join("files").join(name)
            }
        })
    }
    pub(super) fn pull_files(&self, status: &mut Status) -> Result<(), String> {
        let id = status.identity.id()?;
        let previous = self
            .statuses()?
            .into_iter()
            .find(|(k, _)| *k == id)
            .map(|(_, s)| s);
        for file in &mut status.files {
            let path = self.cached_path(&status.identity, file)?;
            let old = previous.as_ref().and_then(|s| {
                s.files.iter().find(|f| {
                    f.name == file.name
                        && f.name_hex == file.name_hex
                        && f.version == file.version
                        && f.size_bytes == file.size_bytes
                })
            });
            let cached = old.and_then(|f| f.sha256.as_ref()).and_then(|hash| {
                let body = fs::read(&path).ok()?;
                (body.len() == file.size_bytes as usize && hex(&Sha256::digest(&body)) == *hash)
                    .then(|| hash.clone())
            });
            file.sha256 = Some(match cached {
                Some(hash) => hash,
                None => {
                    let body = read_versioned(&status.endpoint, file)?;
                    let hash = hex(&Sha256::digest(&body));
                    atomic(&path, &body)?;
                    hash
                }
            });
            if file.name == EPOCH_FILE && file.name_hex.is_none() {
                status.observed_generation = fs::read(&path)
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .map(u64::from_le_bytes)
                    .filter(|g| *g != 0);
            }
        }
        Ok(())
    }
    pub fn bump_files(&self, c: &mut Intended) -> Result<(), String> {
        let id = c.identity.id()?;
        let status = self
            .statuses()?
            .into_iter()
            .find(|(key, _)| *key == id)
            .map(|(_, s)| s);
        for file in self.managed_files(c)? {
            let observed = status.as_ref().and_then(|s| {
                s.files
                    .iter()
                    .find(|f| f.name == file.name && f.name_hex.is_none())
            });
            let old_hash = c
                .files
                .get(&file.name)
                .and_then(|s| s.sha256.as_ref())
                .or_else(|| observed.and_then(|f| f.sha256.as_ref()));
            let version = if old_hash == Some(&file.sha256) {
                if !c.files.contains_key(&file.name) {
                    observed.map_or(file.version, |f| f.version)
                } else {
                    file.version
                }
            } else {
                file.version
                    .max(observed.map_or(0, |f| f.version))
                    .checked_add(1)
                    .ok_or("file version exhausted")?
            };
            c.files.insert(
                file.name,
                FileSpec {
                    version,
                    sha256: Some(file.sha256),
                },
            );
        }
        Ok(())
    }
    pub fn device_dir(&self, c: &Intended) -> Result<PathBuf, String> {
        Ok(self.paths.intended.join(c.identity.id()?))
    }
    pub fn files_dir(&self, c: &Intended) -> Result<PathBuf, String> {
        Ok(self.device_dir(c)?.join("files"))
    }
    pub fn config_path(&self, c: &Intended) -> Result<PathBuf, String> {
        let canonical = self.device_dir(c)?.join("device.toml");
        let legacy = self
            .paths
            .intended
            .join(format!("{}.toml", c.identity.id()?));
        if canonical.exists() && legacy.exists() {
            return Err("duplicate flat and directory intended configuration".into());
        }
        Ok(if legacy.exists() { legacy } else { canonical })
    }
    pub fn managed_files(&self, c: &Intended) -> Result<Vec<ManagedFile>, String> {
        let entries = match fs::read_dir(self.files_dir(c)?) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.to_string()),
        };
        let mut result = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "managed file name must be UTF-8")?;
            if name.is_empty() || name.len() > 8 || name.contains('\0') || name == EPOCH_FILE {
                return Err(format!(
                    "managed file {name:?} must use 1..8 bytes and cannot be {EPOCH_FILE}"
                ));
            }
            if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                return Err(format!(
                    "managed files must be regular files: {}",
                    entry.path().display()
                ));
            }
            let size = entry.metadata().map_err(|e| e.to_string())?.len();
            if size > MAX_FILE as u64 {
                return Err(format!("managed file {name} exceeds {MAX_FILE} bytes"));
            }
            let bytes = fs::read(entry.path()).map_err(|e| e.to_string())?;
            if bytes.len() > MAX_FILE as usize {
                return Err(format!("managed file {name} exceeds {MAX_FILE} bytes"));
            }
            result.push(ManagedFile {
                name: name.clone(),
                source: entry.path(),
                version: match c.files.get(&name) {
                    Some(spec) if spec.version > 0 => spec.version,
                    Some(_) => return Err("file versions must be positive".into()),
                    None => c
                        .generation
                        .try_into()
                        .ok()
                        .filter(|v| *v > 0)
                        .ok_or("file version exceeds u32; specify a per-file version")?,
                },
                size_bytes: bytes.len() as u32,
                sha256: hex(&Sha256::digest(bytes)),
            });
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }
    pub(super) fn snapshot_digest(&self, c: &Intended) -> Result<String, String> {
        let mut h = Sha256::new();
        h.update(config_digest(c)?.as_bytes());
        for file in self.managed_files(c)? {
            h.update((file.name.len() as u64).to_le_bytes());
            h.update(file.name.as_bytes());
            h.update(file.version.to_le_bytes());
            h.update(file.sha256.as_bytes());
        }
        Ok(hex(&h.finalize()))
    }
}
