//! Read-only, one-image-per-page inventory; never allocates or retires flash.
use crate::store::{Layout, PAGE};
use xip_abi::{ERR_IO, ERR_SIZE, ERR_UNSUPPORTED, Hw, cbor};

pub fn write(hw: Hw, layout: Layout, cursor: u32, w: &mut cbor::Writer<'_>) -> Result<(), i32> {
    let running = crate::running_partition(hw).ok_or(ERR_UNSUPPORTED)?;
    let mut found = None;
    layout.areas(hw, |a| {
        if found.is_none() && a.start >= cursor && matches!(a.kind, b'M' | 0xe9) {
            found = Some(a);
        }
        Ok(())
    })?;
    let key = xip_abi::CryptoClient::from_host(hw).and_then(|c| c.identity_public_key().ok());
    w.map(6).uint(1).array(if found.is_some() { 1 } else { 0 });
    if let Some(a) = found {
        let mut name = [0; 32];
        let (mut len, mut version, mut live) = (0, 0u32, false);
        let size;
        if a.kind == 0xe9 {
            size = layout.image_size(hw, a.start)?.ok_or(ERR_IO)?;
            let mut v = [0; 32];
            layout.read(hw, a.start + 48, &mut v)?;
            for b in v {
                if !b.is_ascii_digit() {
                    break;
                }
                version = version
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((b - b'0') as u32))
                    .ok_or(ERR_SIZE)?;
            }
            live = running.0 == layout.base + a.start;
        } else {
            let mut h = [0; 64];
            layout.read(hw, a.start, &mut h)?;
            size = crate::store::u32le(&h[4..]);
            let at = crate::store::u32le(&h[44..]);
            let bytes = crate::store::u32le(&h[48..]);
            if at < 64 || at > size || bytes > size - at {
                return Err(ERR_IO);
            }
            let mut manifest = [0; 48];
            let n = (bytes as usize).min(manifest.len());
            layout.read(hw, a.start + at, &mut manifest[..n])?;
            if let Some((n, v)) = crate::manifest_ident(&manifest[..n]) {
                if n.len() > name.len() {
                    return Err(ERR_SIZE);
                }
                len = n.len();
                name[..len].copy_from_slice(n);
                version = v;
            } else {
                return Err(ERR_IO);
            }
            if !crate::for_modules(hw, |module_name, _, active, page| {
                live |= active
                    && module_name == &name[..len]
                    && page == Some(((layout.base + a.start) / PAGE) as u64);
            }) {
                return Err(ERR_IO);
            }
        }
        w.map(7)
            .uint(1)
            .text(if a.kind == 0xe9 { "runtime" } else { "module" })
            .uint(2)
            .text_raw(&name[..len])
            .uint(3)
            .uint(version as u64)
            .uint(4)
            .uint(size as u64)
            .uint(5)
            .uint((a.end - a.start) as u64)
            .uint(6)
            .uint(a.start as u64)
            .uint(7)
            .boolean(live);
    }
    w.uint(2).uint(found.map_or(0, |a| a.end) as u64).uint(3);
    if let Some(k) = key {
        w.bytes(&k);
    } else {
        w.null();
    }
    w.uint(4)
        .uint(layout.base as u64)
        .uint(5)
        .uint(layout.size as u64);
    let mut state = xip_abi::WifiState::default();
    state.size = core::mem::size_of::<xip_abi::WifiState>() as u32;
    let available = hw
        .wifi()
        .and_then(|w| w.state)
        .is_some_and(|get| unsafe { get(hw.ctx(), &mut state) } >= 0);
    let sta = available && state.sta_mac != [0; 6];
    let ap = available && state.ap_mac != [0; 6] && (!sta || state.ap_mac != state.sta_mac);
    w.uint(6).array(sta as usize + ap as usize);
    if sta {
        w.bytes(&state.sta_mac);
    }
    if ap {
        w.bytes(&state.ap_mac);
    }
    Ok(())
}
