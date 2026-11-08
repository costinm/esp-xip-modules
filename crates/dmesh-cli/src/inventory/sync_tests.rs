mod sync {
    use super::*;
    use std::{
        io::Read,
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };
    #[derive(Default)]
    struct DeviceState {
        files: BTreeMap<String, (u32, Vec<u8>)>,
        images: Vec<serde_json::Value>,
        writes: Vec<String>,
        reads: BTreeMap<String, usize>,
        lose_reply: Option<String>,
        corrupt_read: Option<String>,
        mutate_master: Option<PathBuf>,
    }
    struct Device {
        endpoint: String,
        state: Arc<Mutex<DeviceState>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Device {
        fn new() -> Self {
            Self::start(Arc::new(Mutex::new(DeviceState::default())))
        }
        fn start(state: Arc<Mutex<DeviceState>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("tcp://{}", listener.local_addr().unwrap());
            let stop = Arc::new(AtomicBool::new(false));
            let done = stop.clone();
            let shared = state.clone();
            let thread = thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let (mut socket, _) = match listener.accept() {
                        Ok(s) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => panic!("accept: {e}"),
                    };
                    socket
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = vec![];
                    loop {
                        let mut b = [0];
                        socket.read_exact(&mut b).unwrap();
                        request.push(b[0]);
                        let mut d = minicbor::Decoder::new(&request);
                        if d.skip().is_ok() {
                            break;
                        }
                    }
                    let r = crate::console::decode(&request).unwrap();
                    let method = r["2"].as_u64().unwrap();
                    let fields = &r["5"];
                    let name = fields["1"]
                        .as_str()
                        .and_then(|s| s.strip_prefix("hex:"))
                        .map(|s| {
                            String::from_utf8(unhex(s).unwrap())
                                .unwrap()
                                .trim_end_matches('\0')
                                .to_owned()
                        });
                    let mut response_body = vec![];
                    let mut s = shared.lock().unwrap();
                    let response = match method {
                        23 => {
                            let cursor = fields["6"].as_u64().unwrap();
                            let mut images = vec![
                                serde_json::json!({"1":"runtime","2":"","3":22,"4":100000,"5":131072,"6":0,"7":true}),
                            ];
                            images.extend(s.images.clone());
                            let image = images
                                .into_iter()
                                .find(|i| i["6"].as_u64().unwrap() >= cursor);
                            let next = image
                                .as_ref()
                                .map_or(0, |i| i["6"].as_u64().unwrap() + i["5"].as_u64().unwrap());
                            serde_json::json!({"1":image.into_iter().collect::<Vec<_>>(),"2":next,"3":format!("hex:{}",identity(7).public_key),"4":65536,"5":4128768})
                        }
                        8 => serde_json::json!({"1":20,"2":0,"3":65504,"5":1310720}),
                        6 => {
                            let after = name.as_deref().unwrap_or("").trim_end_matches('\0');
                            let mut entries = vec![];
                            for (name, (version, body)) in &s.files {
                                if name.as_str() <= after {
                                    continue;
                                }
                                let mut bytes = name.as_bytes().to_vec();
                                bytes.resize(8, 0);
                                entries.push(serde_json::json!({"1":format!("hex:{}",hex(&bytes)),"2":version,"3":body.len(),"4":1,"5":false}));
                            }
                            let max = fields["2"].as_u64().unwrap() as usize;
                            let more = entries.len() > max;
                            entries.truncate(max);
                            serde_json::json!({"1":entries,"2":more})
                        }
                        3 => {
                            let key = name.as_ref().unwrap();
                            *s.reads.entry(key.clone()).or_default() += 1;
                            match s.files.get(key).cloned() {
                                Some((version, mut body)) => {
                                    if s.corrupt_read.as_ref() == name.as_ref() {
                                        if let Some(b) = body.first_mut() {
                                            *b ^= 0xff;
                                        }
                                    }
                                    if fields["5"].as_u64().is_some_and(|v| v != version as u64) {
                                        drop(s);
                                        let mut e = minicbor::Encoder::new(Vec::new());
                                        encode_json(&mut e, &serde_json::json!({"7":-21}));
                                        socket.write_all(&e.into_writer()).unwrap();
                                        continue;
                                    }
                                    let n = body.len();
                                    response_body = body;
                                    serde_json::json!({"1":version,"2":n,"3":1,"4":false,"5":n,"6":0})
                                }
                                None => serde_json::json!({"7":-7}),
                            }
                        }
                        4 | 13 => {
                            let n = fields["2"].as_u64().unwrap() as usize;
                            let mut body = vec![0; n];
                            socket.read_exact(&mut body).unwrap();
                            let name = name.unwrap();
                            s.writes.push(name.clone());
                            let reply = if method == 4 {
                                let current = s.files.get(&name).map_or(0, |v| v.0);
                                let version = fields["9"]
                                    .as_u64()
                                    .map(|v| v as u32)
                                    .unwrap_or(current + 1);
                                assert!(version > current, "server-provided version must increase");
                                s.files.insert(name.clone(), (version, body));
                                if let Some(path) = s.mutate_master.take() {
                                    fs::write(path, b"changed during upload").unwrap();
                                }
                                serde_json::json!({"1":version,"2":n,"3":1,"4":false})
                            } else {
                                let (actual, version) = artifact_identity(&body, "module").unwrap();
                                assert_eq!(actual, name);
                                assert_eq!(fields["9"], version);
                                assert!(fields["8"].as_u64().is_some());
                                s.images.push(serde_json::json!({"1":"module","2":name,"3":version,"4":n,"5":65536,"6":196608,"7":false}));
                                serde_json::json!({"1":version,"2":n,"6":196608})
                            };
                            if s.lose_reply.as_ref() == Some(&name) {
                                s.lose_reply = None;
                                continue;
                            }
                            reply
                        }
                        other => panic!("unexpected method {other}"),
                    };
                    drop(s);
                    let mut e = minicbor::Encoder::new(Vec::new());
                    encode_json(&mut e, &response);
                    socket.write_all(&e.into_writer()).unwrap();
                    socket.write_all(&response_body).unwrap();
                }
            });
            Self {
                endpoint,
                state,
                stop,
                thread: Some(thread),
            }
        }
        fn epoch(&self) -> Option<u64> {
            self.state
                .lock()
                .unwrap()
                .files
                .get(files::EPOCH_FILE)
                .map(|(_, b)| u64::from_le_bytes(b.as_slice().try_into().unwrap()))
                .filter(|v| *v != 0)
        }
    }
    impl Drop for Device {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }
    fn put_master(i: &Inventory, c: &Intended, name: &str, body: &[u8]) {
        fs::write(i.files_dir(c).unwrap().join(name), body).unwrap();
    }
    #[test]
    fn pushes_master_files_commits_epoch_last_and_same_epoch_is_a_noop() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let mut c = config();
        c.files.insert(
            "hello".into(),
            FileSpec {
                version: 100,
                sha256: None,
            },
        );
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "_m_nan", &[0xa1, 1, 6]);
        put_master(&i, &c, "hello", b"master wins");
        let d = Device::new();
        d.state
            .lock()
            .unwrap()
            .files
            .insert("hello".into(), (99, b"device content".to_vec()));
        let before = i.refresh(&c, &d.endpoint).unwrap();
        assert_eq!(before.observed_generation, None);
        assert!(
            before
                .files
                .iter()
                .any(|f| f.name == "hello" && f.version == 99)
        );
        let applied = i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(d.epoch(), Some(1));
        assert_eq!(applied["status"]["observed_generation"], 1);
        {
            let s = d.state.lock().unwrap();
            assert_eq!(s.files["hello"].1, b"master wins");
            assert_eq!(s.writes, vec!["_m_nan", "hello", "cfg_ep"]);
        }
        assert!(
            i.reconcile(&c, &d.endpoint, true).unwrap()["applied"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(d.state.lock().unwrap().writes.len(), 3);
        let mut c = c;
        put_master(&i, &c, "hello", b"changed on master");
        i.bump_files(&mut c).unwrap();
        c.generation = 2;
        i.save_intended(&c).unwrap();
        i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(d.epoch(), Some(2));
        assert_eq!(
            d.state.lock().unwrap().files["hello"].1,
            b"changed on master"
        );
    }
    #[test]
    fn lost_file_reply_and_device_restart_keep_old_epoch_and_retry_all_files() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let mut c = config();
        c.generation = 4;
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "a", b"one");
        put_master(&i, &c, "b", b"two");
        let d = Device::new();
        {
            let mut s = d.state.lock().unwrap();
            s.files
                .insert("cfg_ep".into(), (1, 3u64.to_le_bytes().to_vec()));
            s.lose_reply = Some("b".into());
        }
        assert!(i.reconcile(&c, &d.endpoint, true).is_err());
        assert_eq!(d.epoch(), Some(3));
        let state = d.state.clone();
        drop(d);
        let d = Device::start(state);
        i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(d.epoch(), Some(4));
        assert_eq!(d.state.lock().unwrap().writes, vec!["a", "b", "cfg_ep"]);
    }
    #[test]
    fn readback_mismatch_does_not_commit_epoch() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let c = config();
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "hello", b"correct");
        let d = Device::new();
        d.state.lock().unwrap().corrupt_read = Some("hello".into());
        assert!(
            i.reconcile(&c, &d.endpoint, true)
                .unwrap_err()
                .contains("differs after write")
        );
        assert_eq!(d.epoch(), None);
        assert!(i.statuses().unwrap()[0].1.error.is_some());
    }
    #[test]
    fn directory_layout_legacy_compatibility_and_relative_artifacts() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let c = config();
        i.save_intended(&c).unwrap();
        let canonical = i.config_path(&c).unwrap();
        assert_eq!(canonical, i.device_dir(&c).unwrap().join("device.toml"));
        assert!(i.files_dir(&c).unwrap().is_dir());
        assert_eq!(i.intended().unwrap().len(), 1);
        let spec = ImageSpec {
            version: None,
            artifact: Some(PathBuf::from("flash.dmod")),
        };
        assert_eq!(
            i.artifact(&c, &spec).unwrap(),
            canonical.parent().unwrap().join("flash.dmod")
        );
        let legacy = i
            .paths
            .intended
            .join(format!("{}.toml", c.identity.id().unwrap()));
        fs::rename(&canonical, &legacy).unwrap();
        assert_eq!(i.intended().unwrap().len(), 1);
        assert_eq!(
            i.artifact(&c, &spec).unwrap(),
            i.paths.intended.join("flash.dmod")
        );
        i.save_intended(&c).unwrap();
        assert!(legacy.exists());
        assert!(!canonical.exists());
        fs::copy(&legacy, &canonical).unwrap();
        assert!(i.intended().is_err());
    }
    #[test]
    fn managed_names_reserved_marker_sizes_and_non_regular_files_are_validated() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let c = config();
        i.save_intended(&c).unwrap();
        for (name, body) in [
            ("cfg_ep", vec![0; 8]),
            ("toolonggg", vec![]),
            ("big", vec![0; 65505]),
        ] {
            put_master(&i, &c, name, &body);
            assert!(i.managed_files(&c).is_err());
            fs::remove_file(i.files_dir(&c).unwrap().join(name)).unwrap();
        }
        let dir = i.files_dir(&c).unwrap().join("nested");
        fs::create_dir(&dir).unwrap();
        assert!(i.managed_files(&c).is_err());
        fs::remove_dir(dir).unwrap();
        std::os::unix::fs::symlink("/dev/null", i.files_dir(&c).unwrap().join("link")).unwrap();
        assert!(i.managed_files(&c).is_err());
    }
    #[test]
    fn module_reconciliation_streams_artifact_and_commits_generation_after_readback() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let mut c = config();
        let mut m = minicbor::Encoder::new(Vec::new());
        m.map(2)
            .unwrap()
            .u32(1)
            .unwrap()
            .str("sample")
            .unwrap()
            .u32(8)
            .unwrap()
            .u32(17)
            .unwrap();
        let manifest = m.into_writer();
        let mut bytes = vec![0; 64];
        bytes[0] = b'M';
        bytes[2..4].copy_from_slice(&64u16.to_le_bytes());
        bytes[44..48].copy_from_slice(&64u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
        bytes.extend(manifest);
        let n = bytes.len();
        bytes[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        let path = t.path().join("sample.dmod");
        fs::write(&path, bytes).unwrap();
        c.modules.insert(
            "sample".into(),
            ImageSpec {
                version: Some(17),
                artifact: Some(path),
            },
        );
        i.save_intended(&c).unwrap();
        let d = Device::new();
        let result = i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(result["health_verified"], false);
        assert_eq!(d.epoch(), Some(1));
        assert_eq!(d.state.lock().unwrap().writes, vec!["sample", "cfg_ep"]);
        assert_eq!(
            fs::read_dir(i.paths.status.join(".staging"))
                .unwrap()
                .count(),
            0
        );
    }
    #[test]
    fn lost_epoch_reply_is_recovered_from_device_without_rewriting_files() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let c = config();
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "hello", b"desired");
        let d = Device::new();
        d.state.lock().unwrap().lose_reply = Some("cfg_ep".into());
        assert!(i.reconcile(&c, &d.endpoint, true).is_err());
        assert_eq!(d.epoch(), Some(1));
        let before = d.state.lock().unwrap().writes.clone();
        assert!(
            i.reconcile(&c, &d.endpoint, true).unwrap()["applied"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(d.state.lock().unwrap().writes, before);
    }
    #[test]
    fn changed_master_snapshot_during_apply_is_not_marked_synced() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let mut c = config();
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "hello", b"initial");
        let d = Device::new();
        d.state.lock().unwrap().mutate_master = Some(i.files_dir(&c).unwrap().join("hello"));
        assert!(
            i.reconcile(&c, &d.endpoint, true)
                .unwrap_err()
                .contains("changed before epoch commit")
        );
        assert_eq!(d.epoch(), None);
        i.bump_files(&mut c).unwrap();
        c.generation += 1;
        i.save_intended(&c).unwrap();
        i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(d.epoch(), Some(2));
        assert_eq!(
            d.state.lock().unwrap().files["hello"].1,
            b"changed during upload"
        );
    }
    #[test]
    fn old_inferred_generation_cache_is_not_reported_as_device_epoch() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let s = status();
        i.save_status(&s).unwrap();
        let path = i
            .paths
            .status
            .join(format!("{}.json", s.identity.id().unwrap()));
        let mut old = serde_json::to_value(&s).unwrap();
        old.as_object_mut().unwrap().remove("files");
        fs::remove_file(i.status_dir(&s.identity).unwrap().join("status.json")).unwrap();
        fs::write(path, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(i.statuses().unwrap()[0].1.observed_generation, None);
    }
    #[test]
    fn file_inventory_follows_padded_name_cursors() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let c = config();
        i.save_intended(&c).unwrap();
        let d = Device::new();
        for n in 0..35 {
            d.state
                .lock()
                .unwrap()
                .files
                .insert(format!("f{n:02}"), (1, vec![]));
        }
        let status = i.refresh(&c, &d.endpoint).unwrap();
        assert_eq!(status.files.len(), 35);
        assert_eq!(status.files[0].name, "f00");
        assert_eq!(status.files[34].name, "f34");
        assert_eq!(status.observed_generation, None);
    }
    #[test]
    fn matching_epoch_is_invalidated_before_repair_and_failed_repair_is_retryable() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let mut c = config();
        let mut m = minicbor::Encoder::new(Vec::new());
        m.map(2)
            .unwrap()
            .u32(1)
            .unwrap()
            .str("sample")
            .unwrap()
            .u32(8)
            .unwrap()
            .u32(17)
            .unwrap();
        let manifest = m.into_writer();
        let mut bytes = vec![0; 64];
        bytes[0] = b'M';
        bytes[2..4].copy_from_slice(&64u16.to_le_bytes());
        bytes[44..48].copy_from_slice(&64u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
        bytes.extend(manifest);
        let n = bytes.len();
        bytes[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        let path = t.path().join("sample.dmod");
        fs::write(&path, bytes).unwrap();
        c.modules.insert(
            "sample".into(),
            ImageSpec {
                version: Some(17),
                artifact: Some(path),
            },
        );
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "hello", b"desired");
        let d = Device::new();
        {
            let mut s = d.state.lock().unwrap();
            s.files
                .insert("cfg_ep".into(), (1, 1u64.to_le_bytes().to_vec()));
            s.corrupt_read = Some("hello".into());
        }
        assert!(i.reconcile(&c, &d.endpoint, true).is_err());
        assert_eq!(d.epoch(), None);
        {
            let mut s = d.state.lock().unwrap();
            assert_eq!(s.writes, vec!["cfg_ep", "sample", "hello"]);
            s.corrupt_read = None;
        }
        i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(d.epoch(), Some(1));
    }
    #[test]
    fn pulls_all_files_and_only_pushes_newer_config_versions() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let mut c = config();
        c.files.insert(
            "a".into(),
            FileSpec {
                version: 4,
                sha256: None,
            },
        );
        c.files.insert(
            "b".into(),
            FileSpec {
                version: 6,
                sha256: None,
            },
        );
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "a", b"older host copy");
        put_master(&i, &c, "b", b"new host copy");
        let d = Device::new();
        {
            let mut s = d.state.lock().unwrap();
            s.files.insert("a".into(), (4, b"device a".to_vec()));
            s.files.insert("b".into(), (5, b"device b".to_vec()));
            s.files
                .insert("extra".into(), (9, b"only on device".to_vec()));
        }
        let status = i.refresh(&c, &d.endpoint).unwrap();
        let cache = i.status_files_dir(&c.identity).unwrap();
        assert_eq!(fs::read(cache.join("extra")).unwrap(), b"only on device");
        assert_eq!(fs::read(cache.join("a")).unwrap(), b"device a");
        let plan = i.plan(&c, &status).unwrap();
        assert_eq!(
            plan.iter()
                .filter_map(|a| if let Action::WriteFile { name, .. } = a {
                    Some(name.as_str())
                } else {
                    None
                })
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        i.reconcile(&c, &d.endpoint, true).unwrap();
        {
            let s = d.state.lock().unwrap();
            assert_eq!(s.writes, vec!["b", "cfg_ep"]);
            assert_eq!(s.files["b"].0, 6);
            assert_eq!(s.reads["a"], 1);
            assert_eq!(s.reads["extra"], 1);
            assert_eq!(s.reads["b"], 2);
        }
        let before = d.state.lock().unwrap().reads.clone();
        i.refresh(&c, &d.endpoint).unwrap();
        assert_eq!(d.state.lock().unwrap().reads, before);
        {
            let mut s = d.state.lock().unwrap();
            s.files
                .insert("a".into(), (5, b"device updated a".to_vec()));
        }
        i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(fs::read(cache.join("a")).unwrap(), b"device updated a");
        assert_eq!(d.state.lock().unwrap().writes, vec!["b", "cfg_ep"]);
        fs::remove_file(cache.join("a")).unwrap();
        i.refresh(&c, &d.endpoint).unwrap();
        assert_eq!(d.state.lock().unwrap().reads["a"], 3);
    }
    #[test]
    fn bump_advances_only_master_files_with_changed_content() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let mut c = config();
        i.save_intended(&c).unwrap();
        put_master(&i, &c, "a", b"first");
        put_master(&i, &c, "b", b"unchanged");
        i.bump_files(&mut c).unwrap();
        c.generation += 1;
        i.save_intended(&c).unwrap();
        let d = Device::new();
        i.reconcile(&c, &d.endpoint, true).unwrap();
        let b = c.files["b"].version;
        put_master(&i, &c, "a", b"second");
        i.bump_files(&mut c).unwrap();
        c.generation += 1;
        i.save_intended(&c).unwrap();
        assert_eq!(c.files["b"].version, b);
        let result = i.reconcile(&c, &d.endpoint, true).unwrap();
        assert_eq!(
            result["applied"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| a["action"] == "write_file")
                .count(),
            1
        );
        assert_eq!(d.state.lock().unwrap().files["b"].0, b);
    }
    #[test]
    fn unsafe_device_names_are_archived_without_path_traversal() {
        let t = tempfile::tempdir().unwrap();
        let i = inventory(t.path());
        let c = config();
        i.save_intended(&c).unwrap();
        let d = Device::new();
        d.state
            .lock()
            .unwrap()
            .files
            .insert("../x".into(), (1, b"opaque".to_vec()));
        let s = i.refresh(&c, &d.endpoint).unwrap();
        assert!(s.files[0].name_hex.is_some());
        assert!(!i.status_dir(&c.identity).unwrap().join("x").exists());
        let dir = i.status_dir(&c.identity).unwrap().join("opaque-files");
        assert_eq!(
            fs::read(fs::read_dir(dir).unwrap().next().unwrap().unwrap().path()).unwrap(),
            b"opaque"
        );
    }
}
