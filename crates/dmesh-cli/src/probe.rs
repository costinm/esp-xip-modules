//! The canonical validated probe client for native TCP/QUIC module services.
use std::time::Duration;
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let mut a = args.to_vec();
    if a.first().is_some_and(|s| s == "probe") {
        if a.get(1).is_none_or(|s| s == "--help") {
            println!("dmesh-cli probe SERIAL|udp://IP:3339|tcp://IP:5228 --bytes=N [--direction=upload|download|both] [--verify=true] [--via FIRST_HOP:DCID] [--baud N] [--stats]");
            return Ok(());
        }
        a.remove(0);
        a.insert(1, "probe.start".into());
    }
    let mut target = a.first().ok_or("missing probe target")?.clone();
    let (mut baud, mut via, mut timeout) = (None, None, Duration::from_secs(120));
    let mut stats = false;
    let mut words = vec!["probe.start".to_string()];
    let mut i = 2;
    while i < a.len() {
        if let Some(value) = a[i].strip_prefix("--via=") {
            via = Some(crate::via::Via::parse(value)?); i += 1; continue;
        }
        let key = &a[i];
        if key == "--stats" { stats = true; i += 1; continue; }
        if matches!(key.as_str(), "--via" | "--baud" | "--timeout-secs") {
            i += 1;
            let v = a.get(i).ok_or("missing host option value")?;
            match key.as_str() {
                "--baud" => baud = Some(v.parse::<u32>().map_err(|e| e.to_string())?),
                "--timeout-secs" => {
                    let s = v.parse::<u64>().map_err(|e| e.to_string())?;
                    if s == 0 || s > 300 {
                        return Err("timeout must be 1..300 seconds".into());
                    }
                    timeout = Duration::from_secs(s);
                }
                _ => {
                    via = Some(crate::via::Via::parse(v)?);
                }
            }
        } else {
            words.push(key.clone());
        }
        i += 1;
    }
    let bytes =
        crate::schema::encode_stream_argv_with_id(&words, crate::client::fresh_request_id())
            .map_err(|e| e.to_string())?;
    let (_, request) = dmesh_server::tagged::decode(&bytes)
        .and_then(dmesh_server::probe::decode_probe_start_record)
        .ok_or("invalid probe request")?;
    if via.is_none() && !target.contains("://") && !target.starts_with('/') {
        let profile = crate::device::load_device(&target)?;
        target = profile
            .serial_path()?
            .ok_or("device has no UART path")?
            .display()
            .to_string();
        baud = baud.or(profile.uart_baud);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = rt.block_on(crate::node_client::native_probe(
        &target, baud, via, request, timeout, stats,
    ))?;
    println!("dmesh_cli_probe target={target} direction={} bytes={} elapsed_us={} upload_mbps={:.3} download_mbps={:.3} upload_elapsed_us={} download_elapsed_us={} callback_errors={:?}",request.direction.as_str(),result.bytes,result.elapsed_us,result.upload_mbps(),result.download_mbps(),result.upload_elapsed_us,result.download_elapsed_us,result.callback_errors);
    Ok(())
}
