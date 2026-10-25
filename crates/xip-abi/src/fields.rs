//! Reading the request fields of a method (a CBOR map keyed by the field tags of its `API.md`). Used by the code `tools/gen_vtables.py`
//! generates for a module that hosts a component; every failure is `ERR_ARG`, an absent field is `Ok(None)`.
use crate::cbor::{self, Item};
use crate::ERR_ARG;

/// The unsigned integer under `tag`.
pub fn get_uint(fields: &[u8], tag: u64) -> Result<Option<u64>, i32> {
    match cbor::get(fields, tag).map_err(|_| ERR_ARG)? {
        None => Ok(None),
        Some(mut r) => match r.next() {
            Some(Item::Uint(v)) => Ok(Some(v)),
            _ => Err(ERR_ARG),
        },
    }
}

/// The signed integer under `tag`.
pub fn get_int(fields: &[u8], tag: u64) -> Result<Option<i64>, i32> {
    cbor::get_int(fields, tag).map_err(|_| ERR_ARG)
}

/// The boolean under `tag`.
pub fn get_bool(fields: &[u8], tag: u64) -> Result<Option<bool>, i32> {
    match cbor::get(fields, tag).map_err(|_| ERR_ARG)? {
        None => Ok(None),
        Some(mut r) => match r.next() {
            Some(Item::Bool(b)) => Ok(Some(b)),
            _ => Err(ERR_ARG),
        },
    }
}

fn hex_decode(digits: &[u8], out: &mut [u8]) -> Option<usize> {
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    if digits.len() % 2 != 0 || digits.len() / 2 > out.len() {
        return None;
    }
    for (slot, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *slot = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(digits.len() / 2)
}

/// The bytes under `tag`, copied to the front of `out`: CBOR bytes, or the text `hex:` and digits as DMesh clients send it. Returns the length;
/// `ERR_ARG` when it does not fit.
pub fn get_bytes(fields: &[u8], tag: u64, out: &mut [u8]) -> Result<Option<usize>, i32> {
    match cbor::get(fields, tag).map_err(|_| ERR_ARG)? {
        None => Ok(None),
        Some(mut r) => match r.next() {
            Some(Item::Bytes(b)) if b.len() <= out.len() => {
                out[..b.len()].copy_from_slice(b);
                Ok(Some(b.len()))
            }
            Some(Item::Text(t)) => match t.strip_prefix(b"hex:") {
                Some(digits) => hex_decode(digits, out).map(Some).ok_or(ERR_ARG),
                None => Err(ERR_ARG),
            },
            _ => Err(ERR_ARG),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_typed_and_bounded() {
        let m = [0xa5, 1, 0x05, 2, 0xf5, 3, 0x43, 1, 2, 3, 4, 0x68, b'h', b'e', b'x', b':', b'0', b'a', b'F', b'f', 5, 0x20];
        assert_eq!(get_uint(&m, 1), Ok(Some(5)));
        assert_eq!(get_uint(&m, 9), Ok(None));
        assert_eq!(get_uint(&m, 2), Err(ERR_ARG));
        assert_eq!(get_bool(&m, 2), Ok(Some(true)));
        assert_eq!(get_int(&m, 5), Ok(Some(-1)));
        let mut out = [0u8; 3];
        assert_eq!(get_bytes(&m, 3, &mut out), Ok(Some(3)));
        assert_eq!(out, [1, 2, 3]);
        assert_eq!(get_bytes(&m, 4, &mut out), Ok(Some(2)));
        assert_eq!(&out[..2], &[0x0a, 0xff]);
        assert_eq!(get_bytes(&m, 3, &mut [0u8; 2]), Err(ERR_ARG));
        assert_eq!(get_bytes(&[0xa1, 3, 0x66, b'h', b'e', b'x', b':', b'z', b'z'], 3, &mut out), Err(ERR_ARG));
    }
}
