//! One-time, host-only compaction of the earlier inventory/archive layout.
use super::*;
use std::{collections::HashSet, os::unix::fs::OpenOptionsExt, time::Duration};
#[derive(Clone)]
struct Event {
    at: u64,
    source: String,
    body: Vec<u8>,
    images: Vec<ImageStatus>,
    seq: Option<u64>,
    count: Option<u64>,
}
fn field<'a>(bytes: &'a [u8], name: &str) -> Result<&'a [u8], String> {
    let mut d = minicbor::Decoder::new(bytes);
    let n = d
        .map()
        .map_err(|e| e.to_string())?
        .ok_or("indefinite telemetry map")?;
    for _ in 0..n {
        let key = d.str().map_err(|e| e.to_string())?;
        let start = d.position();
        d.skip().map_err(|e| e.to_string())?;
        if key == name {
            return Ok(&bytes[start..d.position()]);
        }
    }
    Err(format!("missing telemetry {name}"))
}
fn rename(source: &Path, target: &Path) -> Result<bool, String> {
    if source == target {
        return Ok(false);
    }
    if target.exists() {
        return Err(format!("refusing to replace {}", target.display()));
    }
    fs::rename(source, target).map_err(|e| e.to_string())?;
    fs::File::open(target.parent().unwrap())
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(true)
}
impl Inventory {
    pub(super) fn compact(&self) -> Result<serde_json::Value, String> {
        let mut renamed = 0;
        let mut held = BTreeMap::new();
        // Read identities directly: normal inventory validation deliberately requires the new path.
        for (root, config) in [(&self.paths.intended, true), (&self.paths.status, false)] {
            if !root.exists() {
                continue;
            }
            let entries = fs::read_dir(root)
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            for entry in entries {
                let path = entry.path();
                let is_dir = entry.file_type().map_err(|e| e.to_string())?.is_dir();
                let metadata = if is_dir {
                    path.join(if config { "device.toml" } else { "status.json" })
                } else {
                    path.clone()
                };
                if !metadata.is_file()
                    || (!is_dir
                        && metadata.extension().and_then(|s| s.to_str())
                            != Some(if config { "toml" } else { "json" }))
                {
                    continue;
                }
                let identity: Identity = if config {
                    toml::from_str::<Intended>(
                        &fs::read_to_string(&metadata).map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| e.to_string())?
                    .identity
                } else {
                    serde_json::from_slice::<Status>(
                        &fs::read(&metadata).map_err(|e| e.to_string())?,
                    )
                    .map_err(|e| e.to_string())?
                    .identity
                };
                let old = if is_dir {
                    entry.file_name().to_string_lossy().into_owned()
                } else {
                    path.file_stem().unwrap().to_string_lossy().into_owned()
                };
                let id = identity.id()?;
                if old != id && old != identity.legacy_id()? {
                    return Err(format!("invalid inventory path {old}"));
                }
                if !config && is_dir {
                    let file = fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .read(true)
                        .write(true)
                        .mode(0o600)
                        .open(path.join(".collector.lock"))
                        .map_err(|e| e.to_string())?;
                    if unsafe {
                        libc::flock(
                            std::os::fd::AsRawFd::as_raw_fd(&file),
                            libc::LOCK_EX | libc::LOCK_NB,
                        )
                    } != 0
                    {
                        return Err(
                            "another collector is active; compaction left data intact".into()
                        );
                    }
                    held.insert(id.clone(), file);
                }
                let target = if is_dir {
                    root.join(&id)
                } else {
                    root.join(format!("{id}.{}", if config { "toml" } else { "json" }))
                };
                if rename(&path, &target)? {
                    renamed += 1;
                    if config && is_dir {
                        let cfg = target.join("device.toml");
                        let mut c: Intended =
                            toml::from_str(&fs::read_to_string(&cfg).map_err(|e| e.to_string())?)
                                .map_err(|e| e.to_string())?;
                        let mut changed = false;
                        for spec in c.runtime.iter_mut().chain(c.modules.values_mut()) {
                            if let Some(artifact) = &mut spec.artifact {
                                if let Ok(relative) = artifact.strip_prefix(&path) {
                                    *artifact = target.join(relative);
                                    changed = true;
                                }
                            }
                        }
                        if changed {
                            atomic(
                                &cfg,
                                toml::to_string_pretty(&c)
                                    .map_err(|e| e.to_string())?
                                    .as_bytes(),
                            )?;
                        }
                    }
                }
            }
        }
        let mut devices = Vec::new();
        for (_, mut status) in self.statuses()? {
            let locked = held.contains_key(&status.identity.id()?);
            devices.push(self.compact_logs(&mut status, locked)?);
        }
        // Discovery is transient; normalize known identities and keep only the newest observation per VIP.
        let known = self
            .intended()?
            .into_iter()
            .map(|(_, c)| c.identity)
            .chain(self.statuses()?.into_iter().map(|(_, s)| s.identity))
            .collect::<Vec<_>>();
        let observations = self.observations()?;
        let mut merged: BTreeMap<String, Observation> = BTreeMap::new();
        for (old, original) in &observations {
            let mut o = original.clone();
            if o.identity.is_none() {
                if let Some(mac) = o.facts["mac"].as_str() {
                    o.identity = known
                        .iter()
                        .find(|i| i.macs.iter().any(|m| m.eq_ignore_ascii_case(mac)))
                        .cloned();
                }
            }
            let id = match &o.identity {
                Some(i) => i.id()?,
                None => old.clone(),
            };
            if merged
                .get(&id)
                .is_none_or(|p| p.observed_at < o.observed_at)
            {
                merged.insert(id, o.clone());
            }
        }
        for (id, o) in &merged {
            self.save_observation(id, o)?;
        }
        for (old, _) in observations {
            if !merged.contains_key(&old) {
                fs::remove_file(
                    self.paths
                        .discovery
                        .join("observations")
                        .join(format!("{old}.json")),
                )
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(serde_json::json!({"renamed":renamed,"devices":devices}))
    }
    fn compact_logs(&self, status: &mut Status, locked: bool) -> Result<serde_json::Value, String> {
        let root = self.paths.status.join(status.identity.id()?);
        let dir = root.join("diagnostics");
        if !dir.exists() {
            return Ok(serde_json::json!({"device":status.identity.id()?,"events":0}));
        }
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(root.join(".collector.lock"))
            .map_err(|e| e.to_string())?;
        if !locked
            && unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&lock),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            } != 0
        {
            return Err("another collector is active".into());
        }
        let mut events = Vec::new();
        let mut legacy = Vec::new();
        let mut seen = HashSet::new();
        for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let name = path.file_name().unwrap().to_string_lossy();
            let known = name == ".telemetry.cbor"
                || name == ".collector.lock"
                || name == ".telemetry.lock"
                || [
                    "telemetry.",
                    "telemetry-",
                    "rtc-panic-",
                    "flash-panic-",
                    "partial-panic-",
                    "log-",
                    "stats-",
                ]
                .iter()
                .any(|p| name.starts_with(p));
            if !known || !path.is_file() {
                return Err(format!(
                    "unrecognized diagnostic path {}; left untouched",
                    path.display()
                ));
            }
            if name.starts_with('.') || path.extension().and_then(|s| s.to_str()) != Some("cbor") {
                continue;
            }
            if name == "telemetry.cbor" || name.starts_with("telemetry-") {
                let bytes = fs::read(&path).map_err(|e| e.to_string())?;
                let mut d = minicbor::Decoder::new(&bytes);
                while d.position() < bytes.len() {
                    let start = d.position();
                    d.skip().map_err(|e| e.to_string())?;
                    let raw = &bytes[start..d.position()];
                    let meta = crate::console::decode(raw)?;
                    let at = meta["observed_unix_nano"]
                        .as_u64()
                        .ok_or("missing telemetry timestamp")?;
                    let source = meta["capture"]
                        .as_str()
                        .or_else(|| meta["source"].as_str())
                        .ok_or("missing telemetry source")?
                        .to_owned();
                    let body = field(raw, "body")?;
                    let body = if source == "partial-panic" {
                        minicbor::Decoder::new(body)
                            .bytes()
                            .map_err(|e| e.to_string())?
                            .to_vec()
                    } else {
                        body.to_vec()
                    };
                    seen.insert(hex(&Sha256::digest(&body)));
                    events.push(Event {
                        at,
                        source,
                        body,
                        images: serde_json::from_value(meta["images"].clone())
                            .map_err(|e| e.to_string())?,
                        seq: meta["producer_sequence"].as_u64(),
                        count: Some(1),
                    });
                }
            } else {
                legacy.push(path);
            }
        }
        legacy.sort();
        for path in legacy {
            let body = fs::read(&path).map_err(|e| e.to_string())?;
            let hash = hex(&Sha256::digest(&body));
            if !seen.insert(hash.clone()) {
                // Enrich a surviving panic copy with its producer sequence when an old log sidecar has it.
                if let Ok(metadata) = fs::read(path.with_extension("json")) {
                    if let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&metadata) {
                        if let Some(seq) = meta["first_seq"].as_u64() {
                            if let Some(event) = events
                                .iter_mut()
                                .rev()
                                .find(|e| e.seq.is_none() && hex(&Sha256::digest(&e.body)) == hash)
                            {
                                event.seq = Some(seq);
                            }
                        }
                    }
                }
                continue;
            }
            let meta: serde_json::Value = serde_json::from_slice(
                &fs::read(path.with_extension("json"))
                    .map_err(|e| format!("{}: {e}", path.display()))?,
            )
            .map_err(|e| e.to_string())?;
            let source = meta["source"]
                .as_str()
                .ok_or("missing legacy source")?
                .to_owned();
            let at = meta["observed_at"]
                .as_u64()
                .ok_or("missing legacy time")?
                .checked_mul(1_000_000_000)
                .ok_or("invalid legacy time")?;
            events.push(Event {
                at,
                source,
                body,
                images: serde_json::from_value(meta["images"].clone())
                    .map_err(|e| e.to_string())?,
                seq: meta["first_seq"].as_u64(),
                count: meta["count"].as_u64(),
            });
        }
        let cached = root.join("files/panic");
        if cached.is_file() {
            let raw = fs::read(&cached).map_err(|e| e.to_string())?;
            let (source, body) = match super::diagnostics::panic_file_body(&raw) {
                Ok(None) => ("flash-panic", Vec::new()),
                Ok(Some(body)) => ("flash-panic", body.to_vec()),
                Err(_) => ("partial-panic", raw),
            };
            if !body.is_empty() && seen.insert(hex(&Sha256::digest(&body))) {
                let at = fs::metadata(&cached)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(status.observed_at * 1_000_000_000);
                events.push(Event {
                    at,
                    source: source.into(),
                    body,
                    images: status.images.clone(),
                    seq: None,
                    count: None,
                });
            }
        }
        events.sort_by(|a, b| {
            a.at.cmp(&b.at)
                .then(a.source.cmp(&b.source))
                .then(a.body.cmp(&b.body))
        });
        if events.is_empty() {
            if cached.is_file() {
                fs::remove_file(&cached).map_err(|e| e.to_string())?;
            }
            return Ok(serde_json::json!({"device":status.identity.id()?,"events":0}));
        }
        let temporary = tempfile::Builder::new()
            .prefix(".compact-")
            .tempdir_in(&self.paths.status)
            .map_err(|e| e.to_string())?;
        let mut staged = self.clone();
        staged.paths.status = temporary.path().join("status");
        let mut receipts = Vec::new();
        for event in &events {
            let mut sample = status.clone();
            sample.observed_at = event.at / 1_000_000_000;
            sample.images = event.images.clone();
            let a = staged.append_at(
                &sample,
                &event.source,
                &event.body,
                event.seq,
                event.count,
                Duration::from_nanos(event.at),
                16 * 1024 * 1024,
            )?;
            if let Some((segment, base)) = &a.rotated {
                for old in &mut receipts {
                    let old: &mut super::diagnostics::Archive = old;
                    if old.segment == Some(*segment) && old.path == "diagnostics/telemetry.cbor" {
                        old.path = format!("diagnostics/{base}.cbor");
                        old.text_path = Some(format!("diagnostics/{base}.txt"));
                    }
                }
            }
            receipts.push(a);
        }
        let backup = root.join("diagnostics.previous");
        if backup.exists() {
            return Err("a previous incomplete compaction needs recovery; original data retained in diagnostics.previous".into());
        }
        let staged_dir = staged
            .paths
            .status
            .join(status.identity.id()?)
            .join("diagnostics");
        fs::rename(&dir, &backup).map_err(|e| e.to_string())?;
        if let Err(e) = fs::rename(&staged_dir, &dir) {
            let _ = fs::rename(&backup, &dir);
            return Err(e.to_string());
        }
        fs::File::open(&root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        for group in [
            &mut status.diagnostics.stats,
            &mut status.diagnostics.panics,
            &mut status.diagnostics.logs,
        ] {
            *group = group
                .iter()
                .filter_map(|old| {
                    receipts
                        .iter()
                        .rev()
                        .find(|a| a.sha256 == old.sha256)
                        .cloned()
                })
                .collect();
        }
        self.save_status(status)?;
        if cached.is_file() {
            fs::remove_file(&cached).map_err(|e| e.to_string())?;
        }
        fs::remove_dir_all(&backup).map_err(|e| e.to_string())?;
        Ok(
            serde_json::json!({"device":status.identity.id()?,"events":events.len(),"files":["telemetry.cbor","telemetry.txt"]}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_identity_and_captures_compact_without_changing_files_or_credentials() {
        let t = tempfile::tempdir().unwrap();
        let inv = super::super::tests::inventory(t.path());
        let cfg = super::super::tests::config();
        let mut status = super::super::tests::status();
        let short = cfg.identity.id().unwrap();
        let full = cfg.identity.legacy_id().unwrap();
        assert_eq!(short.len(), 16);
        assert!(full.starts_with(&short));
        inv.save_intended(&cfg).unwrap();
        inv.save_status(&status).unwrap();
        let file = inv.files_dir(&cfg).unwrap().join("example");
        fs::write(&file, b"master").unwrap();
        let a = inv
            .append_at(
                &status,
                "stats",
                b"\xa0",
                None,
                None,
                Duration::from_secs(1000000),
                16 * 1024 * 1024,
            )
            .unwrap();
        status.diagnostics.stats.push(a);
        inv.save_status(&status).unwrap();
        let old = inv.paths.status.join(&short).join("diagnostics");
        let body = b"\xa4\x00\x03\x01\x65panic\x02\x63nan\x03\x41x";
        let base = old.join("rtc-panic-legacy");
        fs::write(base.with_extension("cbor"), body).unwrap();
        fs::write(base.with_extension("txt"), "old view").unwrap();
        fs::write(
            base.with_extension("json"),
            serde_json::to_vec(
                &serde_json::json!({"source":"rtc-panic","images":status.images,"observed_at":1000000}),
            )
            .unwrap(),
        )
        .unwrap();
        fs::rename(inv.paths.status.join(&short), inv.paths.status.join(&full)).unwrap();
        fs::rename(
            inv.paths.intended.join(&short),
            inv.paths.intended.join(&full),
        )
        .unwrap();
        let key = ControlKey::load_or_create(&inv.paths.key).unwrap();
        let secret = key.device_secret(&cfg.identity).unwrap();
        inv.compact().unwrap();
        assert!(!inv.paths.status.join(&full).exists());
        assert!(!inv.paths.intended.join(&full).exists());
        assert_eq!(
            fs::read(inv.files_dir(&cfg).unwrap().join("example")).unwrap(),
            b"master"
        );
        assert_eq!(key.device_secret(&cfg.identity).unwrap(), secret);
        assert_eq!(inv.resolve(&full).unwrap().0, short);
        assert_eq!(inv.statuses().unwrap().len(), 1);
        let dir = inv.paths.status.join(&short).join("diagnostics");
        let bytes = fs::read(dir.join("telemetry.cbor")).unwrap();
        let mut d = minicbor::Decoder::new(&bytes);
        let mut records = vec![];
        while d.position() < bytes.len() {
            let start = d.position();
            d.skip().unwrap();
            records.push(crate::console::decode(&bytes[start..d.position()]).unwrap());
        }
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r["device_id"] == short));
        assert!(
            records
                .iter()
                .any(|r| r["priority"] == "high" && r["body"]["3"] == "hex:78")
        );
        assert!(!dir.join("rtc-panic-legacy.json").exists());
        assert!(
            !inv.paths
                .status
                .join(&short)
                .join("diagnostics.previous")
                .exists()
        );
        let before = fs::read(dir.join("telemetry.cbor")).unwrap();
        inv.compact().unwrap();
        assert_eq!(fs::read(dir.join("telemetry.cbor")).unwrap(), before);
    }
}
