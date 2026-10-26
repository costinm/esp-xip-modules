//! Memory and task diagnostics work without console or NAN, including low-memory conditions.
mod common;
use common::*;
use xip_abi::{*,cbor::{self,Writer}};
use xip_runtime_host::{Config,Runtime};
fn envelope(method:u64,fields:&[u8])->Vec<u8>{
    let mut out=vec![0;100];let mut w=Writer::new(&mut out);
    w.map(4).uint(1).uint(49).uint(2).uint(method).uint(3).uint(123).uint(5).raw(fields);
    let n=w.finish().unwrap().len();out.truncate(n);out
}
#[test]
fn system_serves_heap_and_tasks_without_console_or_nan(){
    let q=Requests::new();let m=q.rt.load_module(module("mod_system")).unwrap();m.start(b"");q.rt.run(0);
    let replies=q.send(&envelope(1,&[0xa0]));assert_eq!(replies.len(),1);
    let r=&replies[0];assert_eq!(cbor::get_int(r,1).unwrap(),Some(49));assert_eq!(cbor::get_int(r,3).unwrap(),Some(123));
    let result=cbor::get(r,6).unwrap().unwrap();assert_eq!(cbor::get_int(result.rest(),1).unwrap(),Some(41000));
    for (method,data) in [(2,&[0xa1,1,0][..]),(3,&[0xa0][..])]{
        let r=q.send(&envelope(method,data));assert_eq!(r.len(),1);assert!(cbor::get(&r[0],6).unwrap().is_some());
    }
    for data in [&[0xa0][..],&[0xa1,1,2][..],&[0xa1,1,7][..]]{
        let r=q.send(&envelope(2,data));assert_eq!(cbor::get_int(&r[0],7).unwrap(),Some(ERR_ARG as i64));
    }
    m.stop();q.rt.run(0);assert_eq!(m.mem_leaked(),0);
}
#[test]
fn system_heap_and_diagnostics_work_without_managed_allocation(){
    let rt=Runtime::new(Config {mem_quota:1,..Default::default()});
    let m=rt.load_module(module("mod_system")).unwrap();m.start(b"");rt.run(0);
    let heap=m.request(49,1,&[0xa0]).unwrap();assert_eq!(cbor::get_int(&heap,1).unwrap(),Some(41000));
    assert_eq!(cbor::get_int(&heap,4).unwrap(),Some(230000));
    for kind in [0,1,3,4,5]{assert_eq!(m.request(49,2,&[0xa1,1,kind]).unwrap(),[0xa0]);}
    assert_eq!(m.request(49,3,&[0xa0]).unwrap(),[0xa0]);
    m.stop();rt.run(0);assert_eq!(m.mem_leaked(),0);
}
