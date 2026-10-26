//! Fixed-layout memory diagnostic data; native mechanisms remain in the runtime.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SystemHeapInfo {
    pub free: u32,
    pub largest: u32,
    pub min_free: u32,
    pub total: u32,
    pub internal_free: u32,
    pub internal_largest: u32,
}
#[repr(C)]
pub struct SystemHeapDump {
    pub kind: i32,
}
impl Default for SystemHeapDump {
    fn default() -> Self { Self { kind: -1 } }
}
const _: () = assert!(core::mem::size_of::<SystemHeapInfo>() == 24);
const _: () = assert!(core::mem::size_of::<SystemHeapDump>() == 4);
