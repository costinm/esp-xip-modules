//! Live structured NAN reports and existing commands on one accepted stream.
use std::time::Duration;
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let usage="dmesh-cli nan-stream SERIAL|udp://IP:3339|tcp://IP:5229 [--seconds N] [--baud N] [--via-now MAC] [nan.METHOD --field=value ...]";
    if args.get(1).is_none_or(|s| s == "--help") {
        println!("{usage}");
        return Ok(());
    }
    let mut target = args[1].clone();
    let (mut seconds, mut baud, mut via) = (30u64, None, None);
    let mut command = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" | "--baud" | "--via-now" => {
                let key = &args[i];
                i += 1;
                let v = args.get(i).ok_or("missing option value")?;
                match key.as_str() {
                    "--seconds" => seconds = v.parse().map_err(|_| "invalid seconds")?,
                    "--baud" => baud = Some(v.parse::<u32>().map_err(|_| "invalid baud")?),
                    _ => {
                        let hex = v.replace(':', "");
                        if hex.len() != 12 || !hex.is_ascii() {
                            return Err("NOW MAC must contain six bytes".into());
                        }
                        let mut mac = [0; 6];
                        for j in 0..6 {
                            mac[j] = u8::from_str_radix(&hex[j * 2..j * 2 + 2], 16)
                                .map_err(|_| "invalid NOW MAC")?;
                        }
                        if mac == [0; 6] || mac[0] & 1 != 0 {
                            return Err("NOW MAC must be unicast".into());
                        }
                        via = Some(mac);
                    }
                }
            }
            word => command.push(word.to_string()),
        }
        i += 1;
    }
    if seconds == 0 || seconds > 300 {
        return Err("seconds must be 1..300".into());
    }
    if !target.contains("://") && !target.starts_with('/') {
        let profile = crate::device::load_device(&target)?;
        target = profile
            .serial_path()?
            .ok_or("device has no UART path")?
            .display()
            .to_string();
        baud = baud.or(profile.uart_baud);
    }
    let id = crate::client::fresh_request_id() & i64::MAX as u64;
    let mut request = crate::console::stream_request(&["nan.report".into()], id)?;
    if !command.is_empty() {
        request.extend(crate::console::stream_request(&command, id + 1)?);
    }
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
