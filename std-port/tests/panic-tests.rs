// panic suite for oxbow. (A) the verbatim trait test from library/std/tests/panic.rs;
// (B) oxbow-native tests exercising the panic=unwind runtime directly: catch_unwind
// ok/err, payload downcast, resume_unwind rethrow, nested catch, panic hooks (set/take/
// update), panic::Location, AssertUnwindSafe.
#![no_main]
#![feature(custom_test_frameworks)]
#![reexport_test_harness_main = "harness_main"]
#![feature(panic_update_hook)]
#![allow(internal_features, dead_code)]
extern crate oxbow_rt;

use std::any::Any;
use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe, UnwindSafe};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

// ---------- (A) verbatim from std tests/panic.rs ----------
struct Foo {
    a: i32,
}

fn assert<T: UnwindSafe + ?Sized>() {}

#[test]
fn panic_safety_traits() {
    assert::<i32>();
    assert::<&i32>();
    assert::<*mut i32>();
    assert::<*const i32>();
    assert::<usize>();
    assert::<str>();
    assert::<&str>();
    assert::<Foo>();
    assert::<&Foo>();
    assert::<Vec<i32>>();
    assert::<String>();
    assert::<RefCell<i32>>(); // not UnwindSafe by default, but via AssertUnwindSafe below
    assert::<Box<i32>>();
    assert::<Mutex<i32>>();
    assert::<RwLock<i32>>();
    assert::<&Mutex<i32>>();
    assert::<&RwLock<i32>>();
    assert::<Rc<i32>>();
    assert::<Arc<i32>>();
    assert::<Box<[u8]>>();

    {
        trait Trait: UnwindSafe {}
        assert::<Box<dyn Trait>>();
    }

    fn bar<T>() {
        assert::<Mutex<T>>();
        assert::<RwLock<T>>();
    }
    fn baz<T: UnwindSafe>() {
        assert::<Box<T>>();
        assert::<Vec<T>>();
        assert::<RefCell<T>>();
        assert::<AssertUnwindSafe<T>>();
        assert::<&AssertUnwindSafe<T>>();
        assert::<Rc<AssertUnwindSafe<T>>>();
        assert::<Arc<AssertUnwindSafe<T>>>();
    }
}

// ---------- (B) oxbow-native unwind-runtime tests ----------

#[test]
fn catch_unwind_ok() {
    let r = panic::catch_unwind(|| 42);
    assert_eq!(r.unwrap(), 42);
}

#[test]
fn catch_unwind_catches() {
    let r = panic::catch_unwind(|| panic!("boom"));
    assert!(r.is_err());
}

#[test]
fn catch_unwind_payload_str() {
    match panic::catch_unwind(|| panic!("static message")) {
        Err(e) => {
            let s = e.downcast::<&'static str>().unwrap();
            assert_eq!(*s, "static message");
        }
        Ok(()) => panic!("should have unwound"),
    }
}

#[test]
fn catch_unwind_payload_string() {
    match panic::catch_unwind(|| panic::panic_any("owned".to_string())) {
        Err(e) => assert_eq!(*e.downcast::<String>().unwrap(), "owned"),
        Ok(()) => panic!("should have unwound"),
    }
}

#[test]
fn catch_unwind_payload_custom() {
    #[derive(Debug, PartialEq)]
    struct Custom(u32);
    match panic::catch_unwind(|| panic::panic_any(Custom(7))) {
        Err(e) => {
            let any: Box<dyn Any + Send> = e;
            assert_eq!(*any.downcast::<Custom>().unwrap(), Custom(7));
        }
        Ok(()) => panic!("should have unwound"),
    }
}

#[test]
fn resume_unwind_rethrows() {
    let outer = panic::catch_unwind(|| {
        let payload = panic::catch_unwind(|| panic!("inner")).unwrap_err();
        // rethrow the captured payload
        panic::resume_unwind(payload);
    });
    let e = outer.unwrap_err();
    assert_eq!(*e.downcast::<&'static str>().unwrap(), "inner");
}

#[test]
fn nested_catch_unwind_isolates() {
    let r = panic::catch_unwind(|| {
        let inner = panic::catch_unwind(AssertUnwindSafe(|| panic!("inner only")));
        assert!(inner.is_err());
        "outer survived"
    });
    assert_eq!(r.unwrap(), "outer survived");
}

#[test]
fn panic_hook_fires() {
    static HITS: AtomicUsize = AtomicUsize::new(0);
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|_info| {
        HITS.fetch_add(1, Ordering::SeqCst);
    }));
    let _ = panic::catch_unwind(|| panic!("hooked"));
    let _ = panic::catch_unwind(|| panic!("hooked again"));
    panic::set_hook(prev); // restore
    assert_eq!(HITS.load(Ordering::SeqCst), 2);
}

#[test]
fn panic_hook_sees_location_and_payload() {
    static OK: AtomicUsize = AtomicUsize::new(0);
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|info| {
        let msg_ok = info
            .payload()
            .downcast_ref::<&str>()
            .map_or(false, |s| *s == "with-loc");
        let loc_ok = info.location().map_or(false, |l| l.file().contains("main.rs"));
        if msg_ok && loc_ok {
            OK.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let _ = panic::catch_unwind(|| panic!("with-loc"));
    panic::set_hook(prev);
    assert_eq!(OK.load(Ordering::SeqCst), 1);
}

#[test]
fn update_hook_wraps_existing() {
    static OUTER: AtomicUsize = AtomicUsize::new(0);
    static INNER: AtomicUsize = AtomicUsize::new(0);
    let base = panic::take_hook();
    panic::set_hook(Box::new(|_| {
        INNER.fetch_add(1, Ordering::SeqCst);
    }));
    panic::update_hook(move |prev, info| {
        OUTER.fetch_add(1, Ordering::SeqCst);
        prev(info); // chain to the inner hook
    });
    let _ = panic::catch_unwind(|| panic!("chained"));
    panic::set_hook(base);
    assert_eq!(OUTER.load(Ordering::SeqCst), 1);
    assert_eq!(INNER.load(Ordering::SeqCst), 1);
}

#[test]
fn assert_unwind_safe_allows_refcell() {
    let cell = RefCell::new(0);
    let r = panic::catch_unwind(AssertUnwindSafe(|| {
        *cell.borrow_mut() += 1;
        cell.borrow().clone()
    }));
    assert_eq!(r.unwrap(), 1);
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    harness_main();
    std::process::exit(0);
}
