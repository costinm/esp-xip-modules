//! Two instances with private memory and one shared pool: the pool is seen by both at the same address, the private parts are not.
use wasmi::{Engine, Instance, Linker, Module, Store, TypedFunc};
use xip_wasm::arena::SharedPool;

const WAT: &str = r#"(module
  (import "env" "memory" (memory 1))
  (func (export "put") (param i32 i32) local.get 0 local.get 1 i32.store)
  (func (export "get") (param i32) (result i32) local.get 0 i32.load))"#;

struct Inst {
    store: Store<()>,
    put: TypedFunc<(u32, u32), ()>,
    get: TypedFunc<u32, u32>,
}

fn instance(engine: &Engine, pool: &SharedPool, private: usize) -> Inst {
    let mut store = Store::new(engine, ());
    let arena = pool.memory(&mut store, private).unwrap();
    let mut linker = <Linker<()>>::new(engine);
    linker.define("env", "memory", arena.memory).unwrap();
    let module = Module::new(engine, wat::parse_str(WAT).unwrap()).unwrap();
    let i: Instance = linker.instantiate_and_start(&mut store, &module).unwrap();
    let put = i.get_typed_func(&store, "put").unwrap();
    let get = i.get_typed_func(&store, "get").unwrap();
    Inst { store, put, get }
}

#[test]
fn pool_is_shared_and_private_memory_is_not() {
    let engine = Engine::default();
    let pool = SharedPool::new(128 * 1024).unwrap();
    let private = 128 * 1024;
    let mut a = instance(&engine, &pool, private);
    a.put.call(&mut a.store, (private as u32 + 16, 0xfeed)).unwrap();
    a.put.call(&mut a.store, (4096, 7)).unwrap();
    // `b` is created after `a` wrote: the pool is not wiped
    let mut b = instance(&engine, &pool, private);
    assert_eq!(b.get.call(&mut b.store, private as u32 + 16).unwrap(), 0xfeed);
    assert_eq!(b.get.call(&mut b.store, 4096).unwrap(), 0, "private memory is not shared");
    b.put.call(&mut b.store, (private as u32 + 20, 9)).unwrap();
    assert_eq!(a.get.call(&mut a.store, private as u32 + 20).unwrap(), 9);
    // outside the private part + pool: a trap, not a wild access
    assert!(a.get.call(&mut a.store, (private + pool.len()) as u32).is_err());
}
