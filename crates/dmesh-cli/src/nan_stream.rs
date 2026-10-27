//! Read live NAN report bodies after one runtime-dispatched opening request.
use std::time::Duration;
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let usage="dmesh-cli nan-stream SERIAL|udp://IP:3339 [--seconds N] [--baud N] [--via FIRST_HOP:DCID]";
    if args.get(1).is_none_or(|s| s == "--help") {
        println!("{usage}");
        return Ok(());
    }
    let mut target = args[1].clone();
    let (mut seconds, mut baud, mut via) = (30u64, None, None);
    let mut i = 2;
    while i < args.len() {
        if let Some(value) = args[i].strip_prefix("--via=") {
            via = Some(crate::via::Via::parse(value)?); i += 1; continue;
        }
        match args[i].as_str() {
            "--seconds" | "--baud" | "--via" => {
                let key = &args[i];
                i += 1;
                let v = args.get(i).ok_or("missing option value")?;
                match key.as_str() {
                    "--seconds" => seconds = v.parse().map_err(|_| "invalid seconds")?,
                    "--baud" => baud = Some(v.parse::<u32>().map_err(|_| "invalid baud")?),
                    _ => {
                        via = Some(crate::via::Via::parse(v)?);
                    }
                }
            }
            word => return Err(format!("unexpected argument {word}; nan-stream only watches reports")),
        }
        i += 1;
    }
    if seconds == 0 || seconds > 300 {
        return Err("seconds must be 1..300".into());
    }
    if via.is_none() && !target.contains("://") && !target.starts_with('/') {
        let profile = crate::device::load_device(&target)?;
        target = profile
            .serial_path()?
            .ok_or("device has no UART path")?
            .display()
            .to_string();
        baud = baud.or(profile.uart_baud);
    }
    let id = crate::client::fresh_request_id() & i64::MAX as u64;
    let request = crate::console::stream_request(&["nan.report".into()], id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(crate::node_client::nan_stream(
        &target,
        baud,
        via,
        &request,
        Duration::from_secs(seconds),
    ))
}
