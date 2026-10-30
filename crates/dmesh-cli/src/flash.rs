//! Module package uploads over native QUIC streams.
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
fn unsigned_module(image: &[u8], name: &str, cpu: u8, dry: bool) -> Result<Vec<u8>, String> {
    if image.len() < 64 || &image[..4] != b"DMOD" || name.is_empty() || name.len() > 32 {
        return Err("expected a .dmod image and a module name of 1..32 bytes".into());
    }
    let at = u32::from_le_bytes(image[44..48].try_into().unwrap()) as usize;
    let len = u32::from_le_bytes(image[48..52].try_into().unwrap()) as usize;
    let manifest = image
        .get(at..at.checked_add(len).ok_or("invalid manifest length")?)
        .ok_or("invalid manifest extent")?;
    let mut d = minicbor::Decoder::new(manifest);
    let n = d
        .map()
        .map_err(|e| e.to_string())?
        .ok_or("indefinite manifest")?;
    let (mut version, mut found) = (0, None);
    for _ in 0..n {
        match d.u32().map_err(|e| e.to_string())? {
            1 => found = Some(d.str().map_err(|e| e.to_string())?),
            8 => version = d.u32().map_err(|e| e.to_string())?,
            _ => {
                d.skip().map_err(|e| e.to_string())?;
            }
        }
    }
    if found != Some(name) {
        return Err("--module does not match the image manifest".into());
    }
    let count = image.len().div_ceil(4096);
    if count > u16::MAX as usize {
        return Err("module too large".into());
    }
    let mut table = Vec::new();
    for block in image.chunks(4096) {
        table.extend_from_slice(&Sha256::digest(block)[..8]);
    }
    let mut h = [0u8; 216];
    h[..4].copy_from_slice(b"WLH1");
    h[4] = 1;
    h[5] = cpu;
    h[6] = 4;
    h[8..12].copy_from_slice(&version.to_le_bytes());
    h[12..16].copy_from_slice(&(image.len() as u32).to_le_bytes());
    h[16..18].copy_from_slice(&4096u16.to_le_bytes());
    h[18..20].copy_from_slice(&(count as u16).to_le_bytes());
    h[20..52].copy_from_slice(&Sha256::digest(image));
    h[52..84].copy_from_slice(&Sha256::digest(&table));
    h[84] = name.len() as u8;
    h[88..88 + name.len()].copy_from_slice(name.as_bytes());
    let mut out = vec![0u8; 145];
    out[..4].copy_from_slice(b"WLU1");
    out[4] = 1;
    out[5] = if dry { 2 } else { 0 };
    out.extend_from_slice(&h);
    out.extend_from_slice(&table);
    out.extend_from_slice(image);
    Ok(out)
}
pub(crate) fn run(a: &[String]) -> Result<(), String> {
    let usage = "dmesh-cli flash SERIAL|udp://HOST:3339 PACKAGE [--via FIRST_HOP:DCID] [--baud N] [--timeout-secs N]\nUnsigned module: ... IMAGE.dmod --unsigned --module NAME --chip esp32|esp32c6|esp32s3 [--dry]";
    if a.get(1).is_some_and(|s| s == "--help") {
        println!("{usage}");
        return Ok(());
    }
    let target = a.get(1).ok_or(usage)?;
    let path = a.get(2).ok_or(usage)?;
    let (mut baud, mut via, mut seconds, mut unsigned, mut name, mut cpu, mut dry) =
        (None, None, 120u64, false, None, 13u8, false);
    let mut i = 3;
    while i < a.len() {
        if let Some(value) = a[i].strip_prefix("--via=") {
            via = Some(crate::via::Via::parse(value)?); i += 1; continue;
        }
        let key = &a[i];
        if key == "--unsigned" {
            unsigned = true;
        } else if key == "--dry" {
            dry = true;
        } else {
            i += 1;
            let v = a.get(i).ok_or("missing option value")?;
            match key.as_str() {
                "--baud" => baud = Some(v.parse::<u32>().map_err(|e| e.to_string())?),
                "--timeout-secs" => seconds = v.parse::<u64>().map_err(|e| e.to_string())?,
                "--module" => name = Some(v.as_str()),
                "--chip" => {
                    cpu = match v.as_str() {
                        "esp32" => 0,
                        "esp32s3" => 9,
                        "esp32c6" => 13,
                        _ => return Err("unsupported chip".into()),
                    }
                }
                "--via" => { via=Some(crate::via::Via::parse(v)?); }
                _ => return Err(format!("unknown flash option {key}")),
            }
        }
        i += 1;
    }
    if seconds == 0 || seconds > 300 {
        return Err("timeout must be 1..300 seconds".into());
    }
    let bytes = std::fs::read(Path::new(path)).map_err(|e| e.to_string())?;
    let package = if unsigned {
        unsigned_module(
            &bytes,
            name.ok_or("--unsigned requires --module NAME")?,
            cpu,
            dry,
        )?
    } else {
        if name.is_some() || dry {
            return Err(
                "--module and --dry require --unsigned; signed packages retain their signed fields"
                    .into(),
            );
        }
        bytes
    };
    if package.len() < 361 || &package[..4] != b"WLU1" || &package[145..149] != b"WLH1" {
        return Err(
            "expected a WLU1 package; use --unsigned --module NAME for a .dmod image".into(),
        );
    }
    if package[151] != 4 {
        return Err("QUIC flash currently accepts module packages only".into());
    }
    let expected = u32::from_le_bytes(package[157..161].try_into().unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let response = rt.block_on(crate::node_client::upload(
        target,
        baud,
        &package,
        via,
        Duration::from_secs(seconds),
    ))?;
    let reply = std::str::from_utf8(&response)
        .map_err(|e| e.to_string())?
        .trim();
    println!("{reply}");
    if reply.split_whitespace().take(2).collect::<Vec<_>>() != ["OK", &expected.to_string()] {
        return Err(format!(
            "flash did not confirm {expected} image bytes: {reply}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsigned_module_preserves_manifest_version_and_block_hashes() {
        let manifest = [0xa2, 1, 0x62, b'h', b'w', 8, 9];
        let mut image = vec![0x7e; 9000];
        image[..4].copy_from_slice(b"DMOD");
        image[44..48].copy_from_slice(&64u32.to_le_bytes());
        image[48..52].copy_from_slice(&(manifest.len() as u32).to_le_bytes());
        image[64..64 + manifest.len()].copy_from_slice(&manifest);
        let package = unsigned_module(&image, "hw", 13, true).unwrap();
        assert_eq!(&package[..6], b"WLU1\x01\x02");
        assert_eq!(&package[16..145], &[0u8; 129]);
        assert_eq!(package[150], 13);
        assert_eq!(package[151], 4);
        assert_eq!(package[229], 2);
        assert_eq!(&package[233..235], b"hw");
        assert_eq!(u32::from_le_bytes(package[153..157].try_into().unwrap()), 9);
        assert_eq!(
            u32::from_le_bytes(package[157..161].try_into().unwrap()),
            9000
        );
        assert_eq!(&package[165..197], Sha256::digest(&image).as_slice());
        let table = &package[361..385];
        assert_eq!(&package[197..229], Sha256::digest(table).as_slice());
        for (i, block) in image.chunks(4096).enumerate() {
            assert_eq!(&table[i * 8..i * 8 + 8], &Sha256::digest(block)[..8]);
        }
        assert_eq!(&package[385..], image);
        assert!(unsigned_module(&image, "different", 13, false).is_err());
    }
}
