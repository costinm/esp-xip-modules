//! Production inventory reconciler over TCP to standalone mod_flash Wasm on 4 MiB flash.
use dmesh_cli::inventory::{Identity, Intended, Inventory, Paths};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::Path,
    time::Duration,
};
use xip_runtime_host::{flash::FlashFile, Instance, Runtime};

fn host(path: &Path) -> (Runtime, Instance) {
    let rt = Runtime::default();
    rt.attach_flash(FlashFile::open(path, 4 * 1024 * 1024).unwrap())
        .unwrap();
    rt.state_mut().running_flash = Some((0x10000, 0x180000));
    let modules = std::env::var_os("XIP_HOST_MODULES")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/host-modules"));
    let m = rt.load_module(modules.join("mod_flash.wasm")).unwrap();
    m.start(&[]);
    rt.run(30000);
    assert!(!m.ended(), "{:?}", m.logs());
    (rt, m)
}
fn master(root: &Path) -> (Inventory, Intended) {
    let key = xip_psa::identity_public_key();
    let mut public = vec![2 | (key[64] & 1)];
    public.extend_from_slice(&key[1..33]);
    let hash = xip_psa::hash(xip_psa::ALG_SHA_256, &public).unwrap();
    let mut vip = [0; 16];
    vip[0] = 0xfc;
    vip[8..].copy_from_slice(&hash[..8]);
    let identity = Identity {
        public_key: public.iter().map(|b| format!("{b:02x}")).collect(),
        vip6: Ipv6Addr::from(vip).to_string(),
        macs: vec![],
        name: Some("host-flash".into()),
        device_type: None,
    };
    let inventory = Inventory {
        paths: Paths {
            intended: root.join("config"),
            status: root.join("status"),
            discovery: root.join("discovery"),
            key: root.join("unused.key"),
        },
    };
    let intended = Intended {
        generation: 1,
        identity,
        hardware: vec![],
        endpoints: BTreeMap::new(),
        modules: BTreeMap::new(),
        files: BTreeMap::new(),
        runtime: None,
        prune: false,
    };
    inventory.save_intended(&intended).unwrap();
    (inventory, intended)
}
async fn reconcile(
    rt: &Runtime,
    m: &Instance,
    inventory: Inventory,
    intended: Intended,
    apply: bool,
) -> serde_json::Value {
    let ports = m
        .listen_network(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
        .await
        .unwrap();
    let endpoint = format!("tcp://{}", ports.stream.unwrap());
    let client =
        tokio::task::spawn_blocking(move || inventory.reconcile(&intended, &endpoint, apply));
    tokio::select! {
        result=client=>result.unwrap().unwrap(),
        _=rt.run_realtime(Duration::from_secs(120))=>panic!("standalone inventory timed out; ended={} logs={:?}",m.ended(),m.logs()),
    }
}
#[tokio::test(flavor = "current_thread")]
async fn managed_files_and_epoch_survive_real_flash_restart_and_matching_epoch_does_not_write() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("flash.bin");
    let mut flash = FlashFile::open(&path, 4 * 1024 * 1024).unwrap();
    let mut table = [255; 64];
    table[..2].copy_from_slice(&[0xaa, 0x50]);
    table[2..4].fill(0);
    table[4..8].copy_from_slice(&0x10000u32.to_le_bytes());
    table[8..12].copy_from_slice(&0x3f0000u32.to_le_bytes());
    table[12..16].copy_from_slice(b"xip\0");
    flash.program(0x8000, &table).unwrap();
    drop(flash);
    let (i, mut c) = master(root.path());
    let files = i.files_dir(&c).unwrap();
    std::fs::write(files.join("hello"), b"master first").unwrap();
    {
        let (rt, m) = host(&path);
        let result = reconcile(&rt, &m, i.clone(), c.clone(), true).await;
        assert_eq!(result["status"]["observed_generation"], 1);
        assert_eq!(result["applied"].as_array().unwrap().len(), 2);
        assert!(result["status"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "hello" && f["version"] == 1));
        m.stop();
        rt.run(0);
    }
    // The local status is disposable; the reconstructed generation must come from flash.
    std::fs::remove_file(i.status_dir(&c.identity).unwrap().join("status.json")).unwrap();
    {
        let (rt, m) = host(&path);
        let before = std::fs::read(&path).unwrap();
        let result = reconcile(&rt, &m, i.clone(), c.clone(), true).await;
        assert!(result["applied"].as_array().unwrap().is_empty());
        assert_eq!(result["status"]["observed_generation"], 1);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "same epoch must not program or erase flash"
        );
        c.generation = 2;
        i.save_intended(&c).unwrap();
        std::fs::write(files.join("hello"), b"master second").unwrap();
        // Reuse the same listener rather than attaching a competing network owner.
        let endpoint = i.statuses().unwrap()[0].1.endpoint.clone();
        let master = i.clone();
        let config = c.clone();
        let client =
            tokio::task::spawn_blocking(move || master.reconcile(&config, &endpoint, true));
        let result = tokio::select! {r=client=>r.unwrap().unwrap(),_=rt.run_realtime(Duration::from_secs(120))=>panic!("second epoch timed out")};
        assert_eq!(result["status"]["observed_generation"], 2);
        assert!(result["status"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "hello" && f["version"] == 2));
        let device_file = root.path().join("device-write");
        std::fs::write(&device_file, b"newer device content").unwrap();
        let target = i.statuses().unwrap()[0].1.endpoint.clone();
        let writer = tokio::task::spawn_blocking(move || {
            dmesh_cli::run_dmesh_cli_args(vec![
                target,
                "flash.file.write".into(),
                "--name=hello".into(),
                format!("--input={}", device_file.display()),
            ])
        });
        tokio::select! {r=writer=>r.unwrap().unwrap(),_=rt.run_realtime(Duration::from_secs(120))=>panic!("device write timed out")};
        let before = std::fs::read(&path).unwrap();
        let master = i.clone();
        let config = c.clone();
        let target = i.statuses().unwrap()[0].1.endpoint.clone();
        let client = tokio::task::spawn_blocking(move || master.reconcile(&config, &target, true));
        let result = tokio::select! {r=client=>r.unwrap().unwrap(),_=rt.run_realtime(Duration::from_secs(120))=>panic!("device pull timed out")};
        assert!(result["applied"].as_array().unwrap().is_empty());
        assert_eq!(
            std::fs::read(i.status_files_dir(&c.identity).unwrap().join("hello")).unwrap(),
            b"newer device content"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "older config cannot overwrite newer device version"
        );
        m.stop();
        rt.run(0);
    }
    {
        let (rt, m) = host(&path);
        let result = reconcile(&rt, &m, i, c, true).await;
        assert!(result["applied"].as_array().unwrap().is_empty());
        assert_eq!(result["status"]["observed_generation"], 2);
        m.stop();
        rt.run(0);
    }
}
