//! Mount and unmount through Disk Arbitration, the same service Finder uses.
//!
//! Volumes that Disk Arbitration mounts belong to root, so plain `umount`
//! fails for the console user and StorageKit (`diskutil unmount`) refuses
//! partitions whose type it does not know. Disk Arbitration itself authorizes
//! the console user for external disks, so no `sudo` is needed.

use std::ffi::{c_char, c_void, CString};
use std::ptr;

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFRunLoopRef = *const c_void;
type DASessionRef = *const c_void;
type DADiskRef = *const c_void;
type DADissenterRef = *const c_void;
type DAReturn = i32;
type DiskCallback = extern "C" fn(disk: DADiskRef, dissenter: DADissenterRef, context: *mut c_void);

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const DA_DISK_MOUNT_OPTION_DEFAULT: u32 = 0;
const DA_DISK_UNMOUNT_OPTION_DEFAULT: u32 = 0;
const DA_DISK_UNMOUNT_OPTION_FORCE: u32 = 0x0008_0000;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: CFRunLoopRef);
    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithCString(alloc: CFTypeRef, s: *const c_char, encoding: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, len: isize, encoding: u32) -> bool;
}

#[link(name = "DiskArbitration", kind = "framework")]
extern "C" {
    fn DASessionCreate(alloc: CFTypeRef) -> DASessionRef;
    fn DASessionScheduleWithRunLoop(session: DASessionRef, rl: CFRunLoopRef, mode: CFStringRef);
    fn DASessionUnscheduleFromRunLoop(session: DASessionRef, rl: CFRunLoopRef, mode: CFStringRef);
    fn DADiskCreateFromBSDName(
        alloc: CFTypeRef,
        session: DASessionRef,
        name: *const c_char,
    ) -> DADiskRef;
    fn DADiskMountWithArguments(
        disk: DADiskRef,
        path: CFTypeRef,
        options: u32,
        callback: DiskCallback,
        context: *mut c_void,
        arguments: *const CFStringRef,
    );
    fn DADiskUnmount(disk: DADiskRef, options: u32, callback: DiskCallback, context: *mut c_void);
    fn DADissenterGetStatus(dissenter: DADissenterRef) -> DAReturn;
    fn DADissenterGetStatusString(dissenter: DADissenterRef) -> CFStringRef;
}

struct Outcome {
    run_loop: CFRunLoopRef,
    result: Option<Result<(), String>>,
}

extern "C" fn done(_disk: DADiskRef, dissenter: DADissenterRef, context: *mut c_void) {
    // SAFETY: `context` is the `Outcome` owned by `run`, alive until the run loop stops.
    let outcome = unsafe { &mut *(context as *mut Outcome) };
    outcome.result = Some(if dissenter.is_null() {
        Ok(())
    } else {
        // SAFETY: dissenter is a valid DADissenterRef for the callback's duration.
        let status = unsafe { DADissenterGetStatus(dissenter) };
        let text = unsafe { cf_string(DADissenterGetStatusString(dissenter)) };
        Err(match text {
            Some(t) if !t.is_empty() => format!("{t} (status {status:#x})"),
            _ => format!("{} (status {status:#x})", describe(status)),
        })
    });
    // SAFETY: run_loop is the current thread's run loop captured in `run`.
    unsafe { CFRunLoopStop(outcome.run_loop) };
}

fn describe(status: DAReturn) -> &'static str {
    match status as u32 {
        0xF8DA_0002 => "the disk is busy",
        0xF8DA_0003 => "bad argument",
        0xF8DA_0004 => "the disk is exclusively claimed by another process",
        0xF8DA_0005 => "no resources",
        0xF8DA_0006 => "not found",
        0xF8DA_0007 => "not mounted",
        0xF8DA_0008 => "not permitted",
        0xF8DA_0009 => "not privileged",
        0xF8DA_000A => "not ready",
        0xF8DA_000B => "not writable",
        0xF8DA_000C => "unsupported",
        _ => "Disk Arbitration refused the request",
    }
}

unsafe fn cf_string(s: CFStringRef) -> Option<String> {
    if s.is_null() {
        return None;
    }
    let mut buf = vec![0 as c_char; 1024];
    if CFStringGetCString(
        s,
        buf.as_mut_ptr(),
        buf.len() as isize,
        K_CF_STRING_ENCODING_UTF8,
    ) {
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        None
    }
}

/// Run one Disk Arbitration request for `bsd_name` and wait for its result.
fn run(bsd_name: &str, start: impl FnOnce(DADiskRef, *mut c_void)) -> Result<(), String> {
    let name = CString::new(bsd_name).map_err(|_| "invalid disk name".to_string())?;
    // SAFETY: standard CoreFoundation/DiskArbitration usage; every created
    // object is released before returning.
    unsafe {
        let session = DASessionCreate(ptr::null());
        if session.is_null() {
            return Err("could not connect to Disk Arbitration".into());
        }
        let rl = CFRunLoopGetCurrent();
        DASessionScheduleWithRunLoop(session, rl, kCFRunLoopDefaultMode);
        let disk = DADiskCreateFromBSDName(ptr::null(), session, name.as_ptr());
        if disk.is_null() {
            DASessionUnscheduleFromRunLoop(session, rl, kCFRunLoopDefaultMode);
            CFRelease(session);
            return Err(format!("no disk named {bsd_name}"));
        }
        let mut outcome = Outcome {
            run_loop: rl,
            result: None,
        };
        start(disk, &mut outcome as *mut Outcome as *mut c_void);
        while outcome.result.is_none() {
            CFRunLoopRun();
        }
        DASessionUnscheduleFromRunLoop(session, rl, kCFRunLoopDefaultMode);
        CFRelease(disk);
        CFRelease(session);
        outcome.result.unwrap_or(Ok(()))
    }
}

/// Ask Disk Arbitration to check and mount `bsd_name` at its default place
/// (`/Volumes/<label>`), optionally read-only.
pub fn mount(bsd_name: &str, read_only: bool) -> Result<(), String> {
    run(bsd_name, |disk, ctx| unsafe {
        let mut args: Vec<CFStringRef> = Vec::new();
        if read_only {
            args.push(CFStringCreateWithCString(
                ptr::null(),
                c"rdonly".as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            ));
        }
        args.push(ptr::null());
        DADiskMountWithArguments(
            disk,
            ptr::null(),
            DA_DISK_MOUNT_OPTION_DEFAULT,
            done,
            ctx,
            args.as_ptr(),
        );
        for a in args.into_iter().filter(|a| !a.is_null()) {
            CFRelease(a);
        }
    })
}

/// Ask Disk Arbitration to unmount `bsd_name`.
pub fn unmount(bsd_name: &str, force: bool) -> Result<(), String> {
    let options = if force {
        DA_DISK_UNMOUNT_OPTION_FORCE
    } else {
        DA_DISK_UNMOUNT_OPTION_DEFAULT
    };
    run(bsd_name, |disk, ctx| unsafe {
        DADiskUnmount(disk, options, done, ctx)
    })
}
