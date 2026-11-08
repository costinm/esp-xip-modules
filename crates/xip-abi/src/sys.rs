//! Fixed-layout memory diagnostic data of the sys component (49); native mechanisms remain in the runtime.
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SysHeapInfo {
    pub free: u32,
    pub largest: u32,
    pub min_free: u32,
    pub total: u32,
    pub internal_free: u32,
    pub internal_largest: u32,
}
#[repr(C)]
pub struct SysHeapDump {
    pub kind: i32,
}
impl Default for SysHeapDump {
    fn default() -> Self { Self { kind: -1 } }
}
const _: () = assert!(core::mem::size_of::<SysHeapInfo>() == 24);
const _: () = assert!(core::mem::size_of::<SysHeapDump>() == 4);
