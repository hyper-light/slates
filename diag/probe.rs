// Diagnostics only (never main): the launcher's in-process sequence, to learn what an AppArmor profile changes.
use std::ffi::CString;
use std::os::unix::process::CommandExt;

const CLONE_NEWUSER: i32 = 0x10000000;
const CLONE_NEWNS: i32 = 0x00020000;
const MS_REC: u64 = 16384;
const MS_PRIVATE: u64 = 1 << 18;
const MS_BIND: u64 = 4096;

extern "C" {
    fn unshare(flags: i32) -> i32;
    fn mount(src: *const i8, target: *const i8, fstype: *const i8, flags: u64, data: *const u8) -> i32;
    fn getuid() -> u32;
    fn getgid() -> u32;
}

fn label() -> String {
    match std::fs::read_to_string("/proc/self/attr/apparmor/current") {
        Ok(s) => s.trim_end_matches(['\n', '\0']).to_string(),
        Err(e) => format!("<{e}>"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("bind");
    if mode == "label" {
        println!("label: {}", label());
        return;
    }
    let src = &args[2];
    let at = &args[3];
    println!("[{mode}] label before: {}", label());
    if mode == "stepout" || mode == "stepout-exec" {
        let r = std::fs::write("/proc/self/attr/apparmor/current", "changeprofile unconfined");
        println!("[{mode}] changeprofile unconfined: {:?}; label now: {}", r, label());
    }
    let uid = unsafe { getuid() };
    let gid = unsafe { getgid() };
    let r = unsafe { unshare(CLONE_NEWUSER | CLONE_NEWNS) };
    println!("[{mode}] unshare: {} {:?}", r, std::io::Error::last_os_error());
    println!("[{mode}] label after unshare: {}", label());
    for (p, c) in [
        ("/proc/self/setgroups", "deny".to_string()),
        ("/proc/self/gid_map", format!("{gid} {gid} 1")),
        ("/proc/self/uid_map", format!("{uid} {uid} 1")),
    ] {
        println!("[{mode}] write {p}: {:?}", std::fs::write(p, c));
    }
    let root = CString::new("/").unwrap();
    let none = CString::new("none").unwrap();
    let r = unsafe { mount(none.as_ptr(), root.as_ptr(), std::ptr::null(), MS_REC | MS_PRIVATE, std::ptr::null()) };
    println!("[{mode}] rprivate: {} {:?}", r, std::io::Error::last_os_error());
    let s = CString::new(src.as_str()).unwrap();
    let t = CString::new(at.as_str()).unwrap();
    let r = unsafe { mount(s.as_ptr(), t.as_ptr(), std::ptr::null(), MS_BIND, std::ptr::null()) };
    println!("[{mode}] bind: {} {:?}", r, std::io::Error::last_os_error());
    if mode == "exec-drop" {
        let r = std::fs::write("/proc/self/attr/apparmor/exec", "exec unconfined");
        println!("[{mode}] onexec unconfined: {:?}", r);
    }
    let script = format!(
        "echo \"[{mode}] child label: $(cat /proc/self/attr/apparmor/current)\"; echo \"[{mode}] child id: $(id -u):$(id -g)\"; \
         echo \"[{mode}] marker: $(cat {at}/marker 2>&1)\"; \
         if unshare -Ur true 2>/dev/null; then echo \"[{mode}] nested unshare -Ur: ok\"; else echo \"[{mode}] nested unshare -Ur: refused\"; fi; \
         if unshare -Urm sh -c 'mount -t tmpfs none /mnt' 2>/dev/null; then echo \"[{mode}] nested mount: ok\"; else echo \"[{mode}] nested mount: refused\"; fi"
    );
    let err = std::process::Command::new("sh").arg("-c").arg(script).exec();
    println!("[{mode}] exec: {err}");
}
