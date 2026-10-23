//! Decode installed module names and method IDs once for firmware registration.
//! Manifest policy checks belong to generation/deployment and tests.
//! Reading it never maps or executes module code.
use alloc::vec::Vec;
use xip_abi::cbor::{self, Item, Reader};

pub struct InstalledApi<'a> {
    pub name: &'a str,
    pub component: u64,
    pub methods: Vec<(u64, &'a str)>,
}

fn item(data: &[u8], key: u64) -> Option<&[u8]> {
    let mut r = cbor::get(data, key).ok()??;
    let rest = r.rest();
    r.skip().then_some(&rest[..rest.len() - r.rest().len()])
}

impl<'a> InstalledApi<'a> {
    pub fn decode(data: &'a [u8]) -> Option<Self> {
        let name = core::str::from_utf8(cbor::get_text(data, 1).ok()??).ok()?;
        let component = u64::try_from(cbor::get_int(data, 2).ok()??).ok()?;
        // The router uses unsigned IDs; the C call ABI takes i32.
        i32::try_from(component).ok()?;
        let mut methods = Vec::new();
        let mut reader = Reader::new(item(data, 3)?);
        let Item::Array(count) = reader.next()? else { return None };
        for _ in 0..count {
            let rest = reader.rest();
            if !reader.skip() { return None; }
            let method = &rest[..rest.len() - reader.rest().len()];
            let id = u64::try_from(cbor::get_int(method, 1).ok()??).ok()?;
            let method_name = core::str::from_utf8(cbor::get_text(method, 2).ok()??).ok()?;
            i32::try_from(id).ok()?;
            methods.push((id, method_name));
        }
        Some(Self { name, component, methods })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_manifest_advertises_binary_and_compatible_text_methods() {
        let data = include_bytes!("../../../modules/mod_con/API.manifest");
        let api = InstalledApi::decode(data).expect("installed console API");
        assert_eq!((api.name, api.component), ("console", 20));
        assert_eq!(api.methods, [(1, "parse"), (2, "render"), (4, "dispatch"), (5, "status"), (6, "start"), (7, "stop"), (8, "reset"), (9, "heap"), (10, "nv")]);
        assert!(InstalledApi::decode(&data[..2]).is_none(), "required name bytes are missing");
    }

}
