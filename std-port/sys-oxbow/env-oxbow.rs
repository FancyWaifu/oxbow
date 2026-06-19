//! oxbow environment — an in-process table (oxbow has no spawn-passed env yet, so
//! it starts with a few sensible defaults; `set_var`/`var`/`vars`/`remove_var` all
//! work via it). Uses std's own `Mutex`, which now runs on oxbow's futex.
pub use super::common::Env;
use crate::collections::BTreeMap;
use crate::ffi::{OsStr, OsString};
use crate::io;
use crate::sync::{Mutex, OnceLock};

fn table() -> &'static Mutex<BTreeMap<OsString, OsString>> {
    static T: OnceLock<Mutex<BTreeMap<OsString, OsString>>> = OnceLock::new();
    T.get_or_init(|| {
        let mut m = BTreeMap::new();
        m.insert(OsString::from("PATH"), OsString::from("/bin"));
        m.insert(OsString::from("HOME"), OsString::from("/home"));
        m.insert(OsString::from("TERM"), OsString::from("oxterm"));
        Mutex::new(m)
    })
}

pub fn env() -> Env {
    let t = table().lock().unwrap();
    Env::new(t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

pub fn getenv(k: &OsStr) -> Option<OsString> {
    table().lock().unwrap().get(k).cloned()
}

pub unsafe fn setenv(k: &OsStr, v: &OsStr) -> io::Result<()> {
    table().lock().unwrap().insert(k.to_os_string(), v.to_os_string());
    Ok(())
}

pub unsafe fn unsetenv(k: &OsStr) -> io::Result<()> {
    table().lock().unwrap().remove(k);
    Ok(())
}
