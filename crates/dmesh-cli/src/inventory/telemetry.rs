//! Append-only per-device collector. The CBOR checkpoint commits both log offsets.
use super::*;
use mesh::message::TextRecord;
use std::{
    io::{Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    time::{SystemTime, UNIX_EPOCH},
};
const MAX_BYTES: u64 = 16 * 1024 * 1024;
#[derive(Default)]
struct State {
    base: String,
    cbor: u64,
    text: u64,
    started: u64,
    generation: u64,
}
fn checkpoint(state: &State) -> Vec<u8> {
    let mut e = minicbor::Encoder::new(Vec::new());
    e.map(5)
        .unwrap()
        .u8(1)
        .unwrap()
        .str(&state.base)
        .unwrap()
        .u8(2)
        .unwrap()
        .u64(state.cbor)
        .unwrap()
        .u8(3)
        .unwrap()
        .u64(state.text)
        .unwrap()
        .u8(4)
        .unwrap()
        .u64(state.started)
        .unwrap()
        .u8(5)
        .unwrap()
        .u64(state.generation)
        .unwrap();
    e.into_writer()
}
fn state(path: &Path) -> Result<State, String> {
    match fs::read(path) {
        Ok(b) => {
            let v = crate::console::decode(&b)?;
            let base = v["1"].as_str().ok_or("invalid telemetry checkpoint")?;
            if (base != "telemetry" && !base.starts_with("telemetry-"))
                || base.contains('/')
                || base.contains('\\')
            {
                return Err("invalid telemetry log path".into());
            }
            Ok(State {
                base: base.into(),
                cbor: v["2"].as_u64().ok_or("invalid CBOR offset")?,
                text: v["3"].as_u64().ok_or("invalid text offset")?,
                started: v["4"].as_u64().ok_or("invalid start time")?,
                generation: v["5"].as_u64().unwrap_or(0),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.to_string()),
    }
}
fn recover(path: &Path, len: u64) -> Result<fs::File, String> {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    if f.metadata().map_err(|e| e.to_string())?.len() < len {
        return Err("telemetry file shorter than durable checkpoint".into());
    }
    f.set_len(len).map_err(|e| e.to_string())?;
    f.seek(SeekFrom::Start(len)).map_err(|e| e.to_string())?;
    Ok(f)
}
pub(super) fn dated(at: u64) -> Result<String, String> {
    let seconds: libc::time_t = at.try_into().map_err(|_| "invalid telemetry date")?;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    if unsafe { libc::gmtime_r(&seconds, tm.as_mut_ptr()) }.is_null() {
        return Err("invalid telemetry date".into());
    }
    let tm = unsafe { tm.assume_init() };
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    ))
}
fn week(at: u64) -> u64 {
    (at / 86400 + 3) / 7
}
fn envelope(
    e: &mut minicbor::Encoder<Vec<u8>>,
    status: &Status,
    source: &str,
    priority: &str,
    record_id: &str,
    time: u64,
    raw: &[u8],
    partial: bool,
    capture: &str,
    sequence: Option<u64>,
    fingerprint: &str,
    schema: &str,
    gaps: &[String],
) {
    e.map(12)
        .unwrap()
        .str("observed_unix_nano")
        .unwrap()
        .u64(time)
        .unwrap()
        .str("device_id")
        .unwrap()
        .str(&status.identity.id().unwrap())
        .unwrap()
        .str("source")
        .unwrap()
        .str(source)
        .unwrap()
        .str("priority")
        .unwrap()
        .str(priority)
        .unwrap()
        .str("record_id")
        .unwrap()
        .str(record_id)
        .unwrap()
        .str("images")
        .unwrap();
    let images = serde_json::to_value(&status.images).unwrap();
    encode_json(e, &images);
    e.str("capture").unwrap().str(capture).unwrap();
    e.str("producer_sequence").unwrap();
    if let Some(n) = sequence {
        e.u64(n).unwrap();
    } else {
        e.null().unwrap();
    }
    e.str("fingerprint").unwrap().str(fingerprint).unwrap();
    e.str("body").unwrap();
    if partial {
        e.bytes(raw).unwrap();
    } else {
        e.writer_mut().extend_from_slice(raw);
    }
    e.str("schema").unwrap().str(schema).unwrap();
    e.str("schema_gaps")
        .unwrap()
        .array(gaps.len() as u64)
        .unwrap();
    for gap in gaps {
        e.str(gap).unwrap();
    }
}
// JSON is used in memory only to encode the common metadata; no JSON log/sidecar is written.
fn encode_json(e: &mut minicbor::Encoder<Vec<u8>>, v: &serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Null => {
            e.null().unwrap();
        }
        Value::Bool(b) => {
            e.bool(*b).unwrap();
        }
        Value::Number(n) => {
            if let Some(n) = n.as_u64() {
                e.u64(n).unwrap();
            } else if let Some(n) = n.as_i64() {
                e.i64(n).unwrap();
            } else {
                e.f64(n.as_f64().unwrap()).unwrap();
            }
        }
        Value::String(s) => {
            e.str(s).unwrap();
        }
        Value::Array(a) => {
            e.array(a.len() as u64).unwrap();
            for v in a {
                encode_json(e, v);
            }
        }
        Value::Object(o) => {
            e.map(o.len() as u64).unwrap();
            for (k, v) in o {
                e.str(k).unwrap();
                encode_json(e, v);
            }
        }
    }
}
impl Inventory {
    pub(super) fn append_telemetry(
        &self,
        status: &Status,
        source: &str,
        body: &[u8],
        seq: Option<u64>,
        count: Option<u64>,
    ) -> Result<super::diagnostics::Archive, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?;
        let limit = std::env::var("DMESH_TELEMETRY_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(MAX_BYTES);
        self.append_at(status, source, body, seq, count, now, limit)
    }
    pub(super) fn append_at(
        &self,
        status: &Status,
        source: &str,
        body: &[u8],
        seq: Option<u64>,
        count: Option<u64>,
        now: std::time::Duration,
        limit: u64,
    ) -> Result<super::diagnostics::Archive, String> {
        let time = u64::try_from(now.as_nanos()).map_err(|_| "telemetry timestamp overflow")?;
        let hash = hex(&Sha256::digest(body));
        let partial = source == "partial-panic";
        let (rendered, gaps) =
            super::diagnostic_text::render(source, body, status.observed_at, &status.images)?;
        let mut chunks = Vec::new();
        if partial {
            chunks.push(body);
        } else {
            let mut d = minicbor::Decoder::new(body);
            while d.position() < body.len() {
                let start = d.position();
                d.skip().map_err(|e| e.to_string())?;
                chunks.push(&body[start..d.position()]);
            }
        }
        if chunks.is_empty() {
            return Err("empty telemetry record".into());
        }
        if count.is_some_and(|n| n != chunks.len() as u64) {
            return Err("telemetry batch count mismatch".into());
        }
        let lines = rendered.lines().collect::<Vec<_>>();
        if lines.len() != chunks.len() {
            return Err("telemetry text/CBOR record mismatch".into());
        }
        let mut cbor = minicbor::Encoder::new(Vec::new());
        let mut text = String::new();
        for (index, (raw, line)) in chunks.iter().zip(lines).enumerate() {
            let priority = if source.contains("panic") || super::diagnostics::is_panic_record(raw) {
                "high"
            } else {
                "normal"
            };
            let kind = if priority == "high" { "panic" } else { source };
            let record_id = format!("{time}-{index}-{}", &hash[..16]);
            let record = TextRecord::parse(line).map_err(|e| e.to_string())?;
            envelope(
                &mut cbor,
                status,
                kind,
                priority,
                &record_id,
                time,
                raw,
                partial,
                source,
                seq.map(|n| n + index as u64),
                &hash,
                &record.kind,
                &gaps,
            );
            text.push_str(&super::telemetry_text::render(&record,time,&status.identity.id()?,priority,&status.images,&gaps)?);
        }
        let cbor = cbor.into_writer();
        let dir = self
            .paths
            .status
            .join(status.identity.id()?)
            .join("diagnostics");
        private_dir(&self.paths.status.join(status.identity.id()?))?;
        private_dir(&dir)?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(dir.join(".telemetry.lock"))
            .map_err(|e| e.to_string())?;
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
            return Err("cannot lock telemetry log".into());
        }
        let checkpoint_path = dir.join(".telemetry.cbor");
        let mut st = state(&checkpoint_path)?;
        let mut rotated = None;
        // A checkpoint naming an archive is an interrupted pair rotation. Finish both renames.
        if !st.base.is_empty() && st.base != "telemetry" {
            for ext in ["cbor", "txt"] {
                let target = dir.join(format!("{}.{}", st.base, ext));
                let active = dir.join(format!("telemetry.{ext}"));
                if !target.exists() {
                    fs::rename(&active, &target).map_err(|e| e.to_string())?;
                }
                recover(&target, if ext == "cbor" { st.cbor } else { st.text })?
                    .sync_all()
                    .map_err(|e| e.to_string())?;
            }
            rotated = Some((st.generation, st.base.clone()));
            st = State {
                base: "telemetry".into(),
                started: now.as_secs(),
                generation: st.generation + 1,
                ..State::default()
            };
            atomic(&checkpoint_path, &checkpoint(&st))?;
        }
        if st.base.is_empty() {
            st.base = "telemetry".into();
            st.started = now.as_secs();
            atomic(&checkpoint_path, &checkpoint(&st))?;
        }
        recover(&dir.join("telemetry.cbor"), st.cbor)?
            .sync_all()
            .map_err(|e| e.to_string())?;
        recover(&dir.join("telemetry.txt"), st.text)?
            .sync_all()
            .map_err(|e| e.to_string())?;
        if st.cbor > 0
            && (week(st.started) != week(now.as_secs())
                || st.cbor + cbor.len() as u64 > limit
                || st.text + text.len() as u64 > limit)
        {
            let date = dated(st.started)?;
            let date = &date[..10];
            let mut suffix = 0;
            let base = loop {
                let base = if suffix == 0 {
                    format!("telemetry-{date}")
                } else {
                    format!("telemetry-{date}-{suffix}")
                };
                if !dir.join(format!("{base}.cbor")).exists()
                    && !dir.join(format!("{base}.txt")).exists()
                {
                    break base;
                }
                suffix += 1;
            };
            let old_generation = st.generation;
            st.base = base.clone();
            atomic(&checkpoint_path, &checkpoint(&st))?;
            for ext in ["cbor", "txt"] {
                fs::rename(
                    dir.join(format!("telemetry.{ext}")),
                    dir.join(format!("{base}.{ext}")),
                )
                .map_err(|e| e.to_string())?;
            }
            fs::File::open(&dir)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            rotated = Some((old_generation, base));
            st = State {
                base: "telemetry".into(),
                started: now.as_secs(),
                generation: old_generation + 1,
                ..State::default()
            };
            atomic(&checkpoint_path, &checkpoint(&st))?;
        }
        let mut cb = recover(&dir.join(format!("{}.cbor", st.base)), st.cbor)?;
        let mut tx = recover(&dir.join(format!("{}.txt", st.base)), st.text)?;
        let offset = st.cbor;
        let text_offset = st.text;
        cb.write_all(&cbor)
            .and_then(|_| cb.sync_all())
            .map_err(|e| e.to_string())?;
        tx.write_all(text.as_bytes())
            .and_then(|_| tx.sync_all())
            .map_err(|e| e.to_string())?;
        st.cbor += cbor.len() as u64;
        st.text += text.len() as u64;
        atomic(&checkpoint_path, &checkpoint(&st))?;
        Ok(super::diagnostics::Archive {
            segment: Some(st.generation),
            rotated,
            source: source.into(),
            path: format!("diagnostics/{}.cbor", st.base),
            text_path: Some(format!("diagnostics/{}.txt", st.base)),
            schema_gaps: gaps,
            sha256: hash,
            size_bytes: body.len(),
            first_seq: seq,
            count,
            offset: Some(offset),
            length: Some(cbor.len() as u64),
            text_offset: Some(text_offset),
            text_length: Some(text.len() as u64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn sample() -> Status {
        Status {
            identity: super::super::tests::config().identity,
            observed_at: 1000000,
            attempted_at: 1000000,
            endpoint: "tcp://unused:1".into(),
            intended_digest: None,
            observed_generation: None,
            images: vec![],
            files: vec![],
            flash: serde_json::json!({}),
            conditions: vec![],
            error: None,
            diagnostics: Default::default(),
        }
    }
    #[test]
    fn pair_recovery_rotation_and_sleep_fields() {
        let root = tempfile::tempdir().unwrap();
        let inv = super::super::tests::inventory(root.path());
        let s = sample();
        let body = b"\xa1\x07\xa3\x0c\x01\x0d\x01\x18\x19\x01";
        let first = inv
            .append_at(
                &s,
                "stats",
                body,
                None,
                None,
                Duration::from_secs(1000000),
                MAX_BYTES,
            )
            .unwrap();
        let dir = inv.paths.status.join(s.identity.id().unwrap());
        let cb = dir.join(&first.path);
        let tx = dir.join(first.text_path.as_ref().unwrap());
        fs::OpenOptions::new()
            .append(true)
            .open(&cb)
            .unwrap()
            .write_all(b"broken-tail")
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&tx)
            .unwrap()
            .write_all(b"broken-tail")
            .unwrap();
        let second = inv
            .append_at(
                &s,
                "stats",
                body,
                None,
                None,
                Duration::from_secs(1000001),
                MAX_BYTES,
            )
            .unwrap();
        assert_eq!(first.path, second.path);
        assert_eq!(second.offset, first.length);
        let all = fs::read(&cb).unwrap();
        let mut d = minicbor::Decoder::new(&all);
        let mut n = 0;
        while d.position() < all.len() {
            d.skip().unwrap();
            n += 1;
        }
        assert_eq!(n, 2);
        let text = fs::read_to_string(&tx).unwrap();
        assert_eq!(text.lines().filter(|l|l.contains(" sleep dev=")).count(),2);
        let line=text.lines().find(|l|l.contains(" sleep dev=")).unwrap();
        let power=TextRecord::parse(line.split_once(' ').unwrap().1).unwrap();
        assert_eq!(power.fields["sleep_attempts"],"1");assert_eq!(power.fields["soft_sleep"],"1");
        let size_rotated = inv
            .append_at(
                &s,
                "stats",
                body,
                None,
                None,
                Duration::from_secs(1000002),
                1,
            )
            .unwrap();
        assert_eq!(size_rotated.path, "diagnostics/telemetry.cbor");
        assert_ne!(size_rotated.segment, second.segment);
        assert!(
            dir.join(format!(
                "diagnostics/{}.cbor",
                size_rotated.rotated.as_ref().unwrap().1
            ))
            .exists()
        );
        let week_rotated = inv
            .append_at(
                &s,
                "stats",
                body,
                None,
                None,
                Duration::from_secs(1000002 + 604800),
                MAX_BYTES,
            )
            .unwrap();
        assert_ne!(week_rotated.segment, size_rotated.segment);
        assert!(
            fs::read_to_string(dir.join(week_rotated.text_path.unwrap()))
                .unwrap()
                .contains("sleep_entries")
        );
    }
    #[test]
    fn interrupted_pair_rotation_finishes_without_losing_the_old_segment() {
        let root = tempfile::tempdir().unwrap();
        let inv = super::super::tests::inventory(root.path());
        let sample = sample();
        let first = inv
            .append_at(
                &sample,
                "stats",
                b"\xa0",
                None,
                None,
                Duration::from_secs(1000000),
                MAX_BYTES,
            )
            .unwrap();
        let dir = inv
            .paths
            .status
            .join(sample.identity.id().unwrap())
            .join("diagnostics");
        let mut pending = state(&dir.join(".telemetry.cbor")).unwrap();
        pending.base = "telemetry-1970-01-12".into();
        atomic(&dir.join(".telemetry.cbor"), &checkpoint(&pending)).unwrap();
        fs::rename(
            dir.join("telemetry.cbor"),
            dir.join("telemetry-1970-01-12.cbor"),
        )
        .unwrap();
        let next = inv
            .append_at(
                &sample,
                "stats",
                b"\xa0",
                None,
                None,
                Duration::from_secs(1000001),
                MAX_BYTES,
            )
            .unwrap();
        assert_eq!(
            next.rotated,
            Some((first.segment.unwrap(), "telemetry-1970-01-12".into()))
        );
        assert_eq!(
            fs::metadata(dir.join("telemetry-1970-01-12.cbor"))
                .unwrap()
                .len(),
            first.length.unwrap()
        );
        assert_eq!(
            fs::metadata(dir.join("telemetry-1970-01-12.txt"))
                .unwrap()
                .len(),
            first.text_length.unwrap()
        );
        assert_eq!(next.path, "diagnostics/telemetry.cbor");
        assert_eq!(next.offset, Some(0));
        assert_eq!(
            state(&dir.join(".telemetry.cbor")).unwrap().base,
            "telemetry"
        );
    }
    #[test]
    fn failed_checkpoint_does_not_return_receipt() {
        let root = tempfile::tempdir().unwrap();
        let inv = super::super::tests::inventory(root.path());
        let s = sample();
        let path = inv
            .paths
            .status
            .join(s.identity.id().unwrap())
            .join("diagnostics");
        fs::create_dir_all(&path).unwrap();
        fs::create_dir(path.join(".telemetry.cbor")).unwrap();
        assert!(
            inv.append_at(
                &s,
                "stats",
                b"\xa0",
                None,
                None,
                Duration::from_secs(1000000),
                MAX_BYTES
            )
            .is_err()
        );
    }
}
