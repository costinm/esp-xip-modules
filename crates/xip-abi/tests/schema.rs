use std::mem::size_of;
use xip_abi::schema::*;
use xip_abi::*;

#[test]
fn every_table_method_has_a_schema_entry_in_order() {
    let sizes = [("wifi", size_of::<Wifi>()), ("wifi-tuning", size_of::<WifiTuner>()), ("link", size_of::<Link>()), ("power", size_of::<Power>()), ("nimble", size_of::<Nimble>()), ("stream", size_of::<Stream>()), ("quic", size_of::<Quic>()), ("hw", size_of::<Hardware>())];
    for (name, size) in sizes {
        let blob = component(name).unwrap();
        assert_eq!(methods(blob).count(), (size - 8) / size_of::<usize>(), "{name}");
        assert!(methods(blob).all(|m| m.nargs() <= 6 || !m.note.is_empty()), "{name}: a method that does not fit must say so");
    }
    assert_eq!(find("link", "route").map(|(_, i)| i), Some(9));
}

#[test]
fn arguments_decode_with_the_sizes_of_the_structs() {
    let (blob, i) = find("wifi", "set").unwrap();
    let m = method(blob, i).unwrap();
    assert_eq!((m.nargs(), m.arg(0), m.arg(1), m.arg(2)), (2, Some(Arg::In(size_of::<WifiIntent>())), Some(Arg::Out(4)), None));
    let (blob, i) = find("link", "route").unwrap();
    assert_eq!(method(blob, i).unwrap().arg(1), Some(Arg::Bytes(2)));
    let (blob, i) = find("wifi", "scan").unwrap();
    assert_eq!(method(blob, i).unwrap().arg(0), Some(Arg::OutArr(size_of::<WifiAp>(), 1)));
}

#[test]
fn a_schema_is_found_inside_a_larger_block() {
    let blob = component("wifi").unwrap();
    let mut block = blob.to_vec();
    block.extend([7, 7, 7]);
    assert_eq!(trim(&block), blob);
    assert_eq!(trim(b"x\0\0"), b"");
    assert!(SCHEMA_BLE.len() > 1);
}
