//! Execution of planned copy actions; mirrors FreeBSD cp's utils.c
//! (copy_file / copy_link / copy_fifo / copy_special / setfile),
//! with progress reporting hooks.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::path::Path;

use crate::progress::Progress;
use crate::{
    atstat, cstr, err_str, fxstat, strerror, Action, Clobber, DirFixup, Kind, LinkMode, Meta,
    Options, To, O_SEARCH_FLAG, S_IFDIR, S_IFMT,
};

/// copy_file_range(2) chunk: big enough to be fast, small enough for
/// smooth progress updates.
const CFR_CHUNK: usize = 4 << 20;
/// Read/write fallback buffer size.
const BUF_SIZE: usize = 1 << 20;

// cp does all destination-side work relative to the kept target dir fd
// and, when recursing, with RESOLVE_BENEATH so a symlink in the copied
// tree cannot lead us out of the destination.
#[cfg(target_os = "freebsd")]
const AT_BENEATH: libc::c_int = libc::AT_RESOLVE_BENEATH;
#[cfg(not(target_os = "freebsd"))]
const AT_BENEATH: libc::c_int = 0;
#[cfg(target_os = "freebsd")]
const O_BENEATH: libc::c_int = libc::O_RESOLVE_BENEATH;
#[cfg(not(target_os = "freebsd"))]
const O_BENEATH: libc::c_int = 0;

/// Destination path relative to `to.dirfd`, as a C string ("." if empty).
fn relc(rel: &Path) -> std::ffi::CString {
    if rel.as_os_str().is_empty() {
        cstr(Path::new("."))
    } else {
        cstr(rel)
    }
}

fn at_flags(to: &To, extra: libc::c_int) -> libc::c_int {
    extra | if to.beneath { AT_BENEATH } else { 0 }
}

fn o_flags(to: &To, extra: libc::c_int) -> libc::c_int {
    extra | if to.beneath { O_BENEATH } else { 0 }
}

#[cfg(target_os = "freebsd")]
mod siginfo {
    use std::sync::atomic::{AtomicBool, Ordering};
    pub static INFO: AtomicBool = AtomicBool::new(false);
    extern "C" fn handler(_: libc::c_int) {
        INFO.store(true, Ordering::Relaxed);
    }
    pub fn install() {
        unsafe {
            let f: extern "C" fn(libc::c_int) = handler;
            libc::signal(libc::SIGINFO, f as usize);
        }
    }
    pub fn take() -> bool {
        INFO.swap(false, Ordering::Relaxed)
    }
}
#[cfg(not(target_os = "freebsd"))]
mod siginfo {
    pub fn install() {}
    pub fn take() -> bool {
        false
    }
}

pub fn install_siginfo() {
    siginfo::install();
}

fn check_siginfo(a: &Action, wtotal: u64, prog: &mut Progress) {
    if siginfo::take() {
        let pct = if a.meta.size == 0 {
            0
        } else {
            (100.0 * wtotal as f64 / a.meta.size as f64) as i64
        };
        prog.raw_err(&format!(
            "{} -> {} {:3}%\n",
            a.src.display(),
            a.dst.display(),
            pct
        ));
    }
}

pub fn exec_action(
    a: &Action,
    opts: &Options,
    to: &mut To,
    prog: &mut Progress,
    fixups: &mut [DirFixup],
    skips: &mut Vec<std::path::PathBuf>,
) -> bool {
    let bad = match a.kind {
        Kind::File => copy_file(a, opts, to, prog),
        Kind::Dir { fixup } => copy_dir(a, opts, to, prog, fixups, fixup, skips),
        Kind::Symlink => copy_link(a, opts, to, prog),
        Kind::Fifo => copy_fifo(a, opts, to, prog),
        Kind::Special => copy_special(a, opts, to, prog),
        Kind::Socket => {
            prog.warn(&format!("{} is a socket (not copied).", a.src.display()));
            false
        }
    };
    if opts.verbose && !bad {
        prog.out(&format!(
            "{} -> {}{}",
            a.src.display(),
            a.dst.display(),
            if a.slash { "/" } else { "" }
        ));
    }
    bad
}

fn copy_dir(
    a: &Action,
    opts: &Options,
    to: &mut To,
    prog: &mut Progress,
    fixups: &mut [DirFixup],
    fx: usize,
    skips: &mut Vec<std::path::PathBuf>,
) -> bool {
    // DIR_TO_DNE root: create the target directory itself and keep an fd
    // to it; everything below is done relative to (and beneath) it.
    if a.rel.as_os_str().is_empty() && to.dirfd < 0 {
        let mode = (a.meta.mode | 0o700) & 0o7777;
        let (r, e) = mkdir_juggled(|c| unsafe { libc::mkdir(cstr(&a.dst).as_ptr(), c) }, mode, opts);
        if r != 0 {
            prog.warn(&format!("{}: {}", a.dst.display(), err_str(&e)));
            skips.push(a.dst.clone());
            fixups[fx].failed = true;
            return true;
        }
        let c = cstr(&a.dst);
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_DIRECTORY | O_SEARCH_FLAG) };
        if fd < 0 {
            prog.warn(&format!(
                "{}: {}",
                a.dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            unsafe {
                libc::rmdir(c.as_ptr());
            }
            skips.push(a.dst.clone());
            fixups[fx].failed = true;
            return true;
        }
        to.dirfd = fd;
        fixups[fx].created = true;
        return false;
    }

    let lst = atstat(to, &a.rel, at_flags(to, libc::AT_SYMLINK_NOFOLLOW));
    if let Ok(dm) = &lst {
        if dm.dev == a.meta.dev && dm.ino == a.meta.ino {
            prog.warn(&format!(
                "{} and {} are identical (not copied).",
                a.dst.display(),
                a.src.display()
            ));
            skips.push(a.dst.clone());
            fixups[fx].failed = true;
            return true;
        }
    }
    match &lst {
        Err(_) => {
            // Create with the source mode plus owner RWX bits (fixed up
            // later); temporarily unblock owner bits in the umask if it
            // would prevent us from writing into the new directory.
            let mode = (a.meta.mode | 0o700) & 0o7777;
            let rc = relc(&a.rel);
            let (r, e) = mkdir_juggled(
                |c| unsafe { libc::mkdirat(to.dirfd, rc.as_ptr(), c) },
                mode,
                opts,
            );
            if r != 0 {
                prog.warn(&format!("{}: {}", a.dst.display(), err_str(&e)));
                skips.push(a.dst.clone());
                fixups[fx].failed = true;
                return true;
            }
            fixups[fx].created = true;
        }
        Ok(dm) if dm.mode & S_IFMT != S_IFDIR => {
            prog.warn(&format!(
                "{}: {}",
                a.dst.display(),
                strerror(libc::ENOTDIR)
            ));
            skips.push(a.dst.clone());
            fixups[fx].failed = true;
            return true;
        }
        _ => {}
    }
    false
}

/// Run a mkdir-style call with the owner RWX bits temporarily unblocked
/// in the umask, like cp does, so we can descend into what we create.
fn mkdir_juggled<F: FnOnce(libc::mode_t) -> libc::c_int>(
    f: F,
    mode: u32,
    opts: &Options,
) -> (libc::c_int, io::Error) {
    let block = opts.umask & 0o700;
    let r;
    let e;
    unsafe {
        if block != 0 {
            libc::umask((opts.umask & !0o700) as libc::mode_t);
        }
        r = f(mode as libc::mode_t);
        e = io::Error::last_os_error();
        if block != 0 {
            libc::umask(opts.umask as libc::mode_t);
        }
    }
    (r, e)
}

fn copy_file(a: &Action, opts: &Options, to: &To, prog: &mut Progress) -> bool {
    prog.file_start(&a.src.display().to_string(), a.meta.size);
    let bad = copy_file_inner(a, opts, to, prog);
    prog.file_end();
    bad
}

fn copy_file_inner(a: &Action, opts: &Options, to: &To, prog: &mut Progress) -> bool {
    // Identical-file check against the (followed) destination.
    let dstat = atstat(to, &a.rel, at_flags(to, 0));
    if let Ok(dm) = &dstat {
        if dm.dev == a.meta.dev && dm.ino == a.meta.ino {
            prog.warn(&format!(
                "{} and {} are identical (not copied).",
                a.dst.display(),
                a.src.display()
            ));
            return true;
        }
    }
    let mut dne = dstat.is_err();
    let rc = relc(&a.rel);

    let mut from: Option<fs::File> = None;
    if opts.link == LinkMode::Copy {
        let f = match fs::File::open(&a.src) {
            Err(e) => {
                prog.warn(&format!("{}: {}", a.src.display(), err_str(&e)));
                return true;
            }
            Ok(f) => f,
        };
        // Guard against the file being replaced with one of a different
        // type while we were not looking (cp: "File changed").
        match fxstat(f.as_raw_fd()) {
            Err(e) => {
                prog.warn(&format!("{}: {}", a.src.display(), err_str(&e)));
                return true;
            }
            Ok(now) => {
                if now.mode & S_IFMT != a.meta.mode & S_IFMT {
                    prog.warn(&format!("{}: File changed", a.src.display()));
                    return true;
                }
            }
        }
        from = Some(f);
    }

    if !dne {
        match opts.clobber {
            Clobber::NoClobber => {
                if opts.verbose {
                    prog.out(&format!("{} not overwritten", a.dst.display()));
                }
                return true;
            }
            Clobber::Interactive => {
                let yes =
                    prog.prompt_yn(&format!("overwrite {}? (y/n [n]) ", a.dst.display()));
                if !yes {
                    prog.raw_err("not overwritten\n");
                    return true;
                }
            }
            Clobber::Force => {
                unsafe {
                    libc::unlinkat(to.dirfd, rc.as_ptr(), at_flags(to, 0));
                }
                dne = true;
            }
            Clobber::Default => {}
        }
    }

    match opts.link {
        LinkMode::Hard => {
            let cs = cstr(&a.src);
            let r = unsafe { libc::linkat(libc::AT_FDCWD, cs.as_ptr(), to.dirfd, rc.as_ptr(), 0) };
            if r != 0 {
                prog.warn(&format!(
                    "{}: {}",
                    a.dst.display(),
                    err_str(&io::Error::last_os_error())
                ));
                return true;
            }
            return false;
        }
        LinkMode::Sym => {
            let cs = cstr(&a.src);
            let r = unsafe { libc::symlinkat(cs.as_ptr(), to.dirfd, rc.as_ptr()) };
            if r != 0 {
                prog.warn(&format!(
                    "{}: {}",
                    a.dst.display(),
                    err_str(&io::Error::last_os_error())
                ));
                return true;
            }
            return false;
        }
        LinkMode::Copy => {}
    }

    let from = from.unwrap();
    let tfd = if !dne {
        unsafe { libc::openat(to.dirfd, rc.as_ptr(), o_flags(to, libc::O_WRONLY | libc::O_TRUNC)) }
    } else {
        unsafe {
            libc::openat(
                to.dirfd,
                rc.as_ptr(),
                o_flags(to, libc::O_WRONLY | libc::O_TRUNC | libc::O_CREAT),
                (a.meta.mode & 0o7777 & !0o6000) as libc::c_uint,
            )
        }
    };
    if tfd < 0 {
        prog.warn(&format!(
            "{}: {}",
            a.dst.display(),
            err_str(&io::Error::last_os_error())
        ));
        return true;
    }
    let to_file = unsafe { fs::File::from_raw_fd(tfd) };

    let mut rval = false;
    if let Err(e) = copy_bytes(&from, &to_file, a, prog) {
        // cp reports read and write errors alike against the source path
        prog.warn(&format!("{}: {}", a.src.display(), err_str(&e)));
        rval = true;
        // don't remove the (possibly partial) target, like cp
    }

    if opts.preserve {
        if setfile(
            &a.meta,
            Some(to_file.as_raw_fd()),
            to,
            &a.rel,
            &a.dst,
            false,
            opts,
            prog,
        ) {
            rval = true;
        }
        if preserve_fd_acls(from.as_raw_fd(), to_file.as_raw_fd(), &a.dst, prog) {
            rval = true;
        }
    }
    let fd = to_file.into_raw_fd();
    if unsafe { libc::close(fd) } != 0 {
        prog.warn(&format!(
            "{}: {}",
            a.dst.display(),
            err_str(&io::Error::last_os_error())
        ));
        rval = true;
    }
    rval
}

fn copy_bytes(
    from: &fs::File,
    to: &fs::File,
    a: &Action,
    prog: &mut Progress,
) -> io::Result<()> {
    let from_fd = from.as_raw_fd();
    let to_fd = to.as_raw_fd();
    let mut use_cfr = true;
    let mut buf: Vec<u8> = Vec::new();
    let mut wtotal: u64 = 0;
    loop {
        check_siginfo(a, wtotal, prog);
        if use_cfr {
            let n = unsafe {
                libc::copy_file_range(
                    from_fd,
                    std::ptr::null_mut(),
                    to_fd,
                    std::ptr::null_mut(),
                    CFR_CHUNK,
                    0,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EINVAL)
                    | Some(libc::ENOSYS)
                    | Some(libc::EXDEV)
                    | Some(libc::EOPNOTSUPP) => {
                        // non-seekable/cross-fs/unsupported: fall back
                        use_cfr = false;
                        continue;
                    }
                    _ => return Err(e),
                }
            } else if n == 0 {
                return Ok(());
            } else {
                wtotal += n as u64;
                prog.add(n as u64);
            }
        } else {
            if buf.is_empty() {
                buf = vec![0u8; BUF_SIZE];
            }
            let mut fr = from;
            match fr.read(&mut buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
                Ok(0) => return Ok(()),
                Ok(n) => {
                    let mut tw = to;
                    tw.write_all(&buf[..n])?;
                    wtotal += n as u64;
                    prog.add(n as u64);
                }
            }
        }
    }
}

fn copy_link(a: &Action, opts: &Options, to: &To, prog: &mut Progress) -> bool {
    let dstat = atstat(to, &a.rel, at_flags(to, libc::AT_SYMLINK_NOFOLLOW));
    if let Ok(dm) = &dstat {
        if dm.dev == a.meta.dev && dm.ino == a.meta.ino {
            prog.warn(&format!(
                "{} and {} are identical (not copied).",
                a.dst.display(),
                a.src.display()
            ));
            return true;
        }
    }
    let dne = dstat.is_err();
    if !dne && opts.clobber == Clobber::NoClobber {
        if opts.verbose {
            prog.out(&format!("{} not overwritten", a.dst.display()));
        }
        return true;
    }
    let target = match fs::read_link(&a.src) {
        Err(e) => {
            prog.warn(&format!("readlink: {}: {}", a.src.display(), err_str(&e)));
            return true;
        }
        Ok(t) => t,
    };
    let rc = relc(&a.rel);
    if !dne {
        let r = unsafe { libc::unlinkat(to.dirfd, rc.as_ptr(), at_flags(to, 0)) };
        if r != 0 {
            prog.warn(&format!(
                "unlink: {}: {}",
                a.dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            return true;
        }
    }
    let ct = cstr(&target);
    if unsafe { libc::symlinkat(ct.as_ptr(), to.dirfd, rc.as_ptr()) } != 0 {
        prog.warn(&format!(
            "symlink: {}: {}",
            target.display(),
            err_str(&io::Error::last_os_error())
        ));
        return true;
    }
    if opts.preserve {
        return setfile(&a.meta, None, to, &a.rel, &a.dst, true, opts, prog);
    }
    false
}

fn clobber_precheck(a: &Action, opts: &Options, to: &To, prog: &mut Progress) -> Option<bool> {
    // shared -n / unlink handling of copy_fifo()/copy_special()
    let dstat = atstat(to, &a.rel, at_flags(to, 0));
    if let Ok(dm) = &dstat {
        if dm.dev == a.meta.dev && dm.ino == a.meta.ino {
            prog.warn(&format!(
                "{} and {} are identical (not copied).",
                a.dst.display(),
                a.src.display()
            ));
            return Some(true);
        }
    }
    let dne = dstat.is_err();
    if !dne {
        if opts.clobber == Clobber::NoClobber {
            if opts.verbose {
                prog.out(&format!("{} not overwritten", a.dst.display()));
            }
            return Some(true);
        }
        let rc = relc(&a.rel);
        let r = unsafe { libc::unlinkat(to.dirfd, rc.as_ptr(), at_flags(to, 0)) };
        if r != 0 {
            prog.warn(&format!(
                "unlink: {}: {}",
                a.dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            return Some(true);
        }
    }
    None
}

fn copy_fifo(a: &Action, opts: &Options, to: &To, prog: &mut Progress) -> bool {
    if let Some(bad) = clobber_precheck(a, opts, to, prog) {
        return bad;
    }
    let rc = relc(&a.rel);
    if unsafe { libc::mkfifoat(to.dirfd, rc.as_ptr(), (a.meta.mode & 0o7777) as libc::mode_t) }
        != 0
    {
        prog.warn(&format!(
            "mkfifo: {}: {}",
            a.dst.display(),
            err_str(&io::Error::last_os_error())
        ));
        return true;
    }
    if opts.preserve {
        return setfile(&a.meta, None, to, &a.rel, &a.dst, false, opts, prog);
    }
    false
}

fn copy_special(a: &Action, opts: &Options, to: &To, prog: &mut Progress) -> bool {
    if let Some(bad) = clobber_precheck(a, opts, to, prog) {
        return bad;
    }
    let rc = relc(&a.rel);
    if unsafe {
        libc::mknodat(
            to.dirfd,
            rc.as_ptr(),
            (a.meta.mode & (S_IFMT | 0o7777)) as libc::mode_t,
            a.meta.rdev as libc::dev_t,
        )
    } != 0
    {
        prog.warn(&format!(
            "mknod: {}: {}",
            a.dst.display(),
            err_str(&io::Error::last_os_error())
        ));
        return true;
    }
    if opts.preserve {
        return setfile(&a.meta, None, to, &a.rel, &a.dst, false, opts, prog);
    }
    false
}

fn ts(t: (i64, i64)) -> libc::timespec {
    libc::timespec {
        tv_sec: t.0 as libc::time_t,
        tv_nsec: t.1 as _,
    }
}

/// Mirror of cp's setfile(): preserve timestamps, ownership, mode and
/// (on FreeBSD) file flags.  `fd` is used when the target is open;
/// otherwise operations go through `to.dirfd` + `rel` (beneath-resolved
/// when recursing), `dst` being only the display path.
#[allow(clippy::too_many_arguments)]
pub fn setfile(
    meta: &Meta,
    fd: Option<i32>,
    to: &To,
    rel: &Path,
    dst: &Path,
    islink: bool,
    opts: &Options,
    prog: &mut Progress,
) -> bool {
    let mut rval = false;
    let mut mode = meta.mode & 0o7777;
    let atflag = at_flags(to, if islink { libc::AT_SYMLINK_NOFOLLOW } else { 0 });
    let c = relc(rel);

    let times = [ts(meta.atime), ts(meta.mtime)];
    let r = unsafe {
        match fd {
            Some(f) => libc::futimens(f, times.as_ptr()),
            None => libc::utimensat(to.dirfd, c.as_ptr(), times.as_ptr(), atflag),
        }
    };
    if r != 0 {
        prog.warn(&format!(
            "utimensat: {}: {}",
            dst.display(),
            err_str(&io::Error::last_os_error())
        ));
        rval = true;
    }

    let now = match fd {
        Some(f) => fxstat(f).ok(),
        None => atstat(to, rel, atflag).ok(),
    };

    let chown_needed = match &now {
        Some(n) => meta.uid != n.uid || meta.gid != n.gid,
        None => true,
    };
    if chown_needed {
        let r = unsafe {
            match fd {
                Some(f) => libc::fchown(f, meta.uid, meta.gid),
                None => libc::fchownat(to.dirfd, c.as_ptr(), meta.uid, meta.gid, atflag),
            }
        };
        if r != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EPERM) {
                prog.warn(&format!("chown: {}: {}", dst.display(), err_str(&e)));
                rval = true;
            }
            // current BSD behavior: lose setuid/setgid if chown failed
            mode &= !0o6000;
        }
    }

    let chmod_needed = match &now {
        Some(n) => mode != n.mode & 0o7777,
        None => true,
    };
    if chmod_needed && !(islink && cfg!(not(target_os = "freebsd"))) {
        let r = unsafe {
            match fd {
                Some(f) => libc::fchmod(f, mode as libc::mode_t),
                None => libc::fchmodat(to.dirfd, c.as_ptr(), mode as libc::mode_t, atflag),
            }
        };
        if r != 0 {
            prog.warn(&format!(
                "chmod: {}: {}",
                dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            rval = true;
        }
    }

    #[cfg(target_os = "freebsd")]
    {
        let flags_needed = match &now {
            Some(n) => meta.flags != n.flags,
            None => true,
        };
        if !opts.no_flags && flags_needed {
            let r = unsafe {
                match fd {
                    Some(f) => libc::fchflags(f, meta.flags as libc::c_ulong),
                    None => libc::chflagsat(
                        to.dirfd,
                        c.as_ptr(),
                        meta.flags as libc::c_ulong,
                        atflag,
                    ),
                }
            };
            if r != 0 {
                let e = io::Error::last_os_error();
                // NFS does not support flags; only complain when we would
                // actually lose bits (like cp).
                if e.raw_os_error() != Some(libc::EOPNOTSUPP) || meta.flags != 0 {
                    prog.warn(&format!("chflags: {}: {}", dst.display(), err_str(&e)));
                    rval = true;
                }
            }
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let _ = &opts.no_flags;
    }

    rval
}

// ---------------------------------------------------------------------
// ACL preservation (FreeBSD only, mirrors cp's preserve_fd_acls()).

#[cfg(target_os = "freebsd")]
mod facl {
    pub type AclT = *mut libc::c_void;
    pub const ACL_TYPE_ACCESS: libc::c_int = 0x2;
    pub const ACL_TYPE_NFS4: libc::c_int = 0x4;
    pub const PC_ACL_EXTENDED: libc::c_int = 59;
    pub const PC_ACL_NFS4: libc::c_int = 64;
    extern "C" {
        pub fn acl_get_fd_np(fd: libc::c_int, ty: libc::c_int) -> AclT;
        pub fn acl_set_fd_np(fd: libc::c_int, acl: AclT, ty: libc::c_int) -> libc::c_int;
        pub fn acl_is_trivial_np(acl: AclT, trivialp: *mut libc::c_int) -> libc::c_int;
        pub fn acl_free(obj: *mut libc::c_void) -> libc::c_int;
    }
}

#[cfg(target_os = "freebsd")]
pub fn preserve_fd_acls(from_fd: i32, to_fd: i32, dst: &Path, prog: &mut Progress) -> bool {
    unsafe {
        let mut acl_type = facl::ACL_TYPE_ACCESS;
        let mut supported = false;
        *libc::__error() = 0;
        let r = libc::fpathconf(from_fd, facl::PC_ACL_NFS4);
        if r > 0 {
            supported = true;
            acl_type = facl::ACL_TYPE_NFS4;
        } else if r < 0 && *libc::__error() != 0 && *libc::__error() != libc::EINVAL {
            prog.warn(&format!(
                "fpathconf(..., _PC_ACL_NFS4) failed for {}: {}",
                dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            return true;
        }
        if !supported {
            *libc::__error() = 0;
            let r = libc::fpathconf(from_fd, facl::PC_ACL_EXTENDED);
            if r > 0 {
                supported = true;
                acl_type = facl::ACL_TYPE_ACCESS;
            } else if r < 0 && *libc::__error() != 0 && *libc::__error() != libc::EINVAL {
                prog.warn(&format!(
                    "fpathconf(..., _PC_ACL_EXTENDED) failed for {}: {}",
                    dst.display(),
                    err_str(&io::Error::last_os_error())
                ));
                return true;
            }
        }
        if !supported {
            return false;
        }
        let acl = facl::acl_get_fd_np(from_fd, acl_type);
        if acl.is_null() {
            prog.warn(&format!(
                "failed to get acl entries while setting {}: {}",
                dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            return true;
        }
        let mut trivial: libc::c_int = 0;
        if facl::acl_is_trivial_np(acl, &mut trivial) != 0 {
            prog.warn(&format!(
                "acl_is_trivial() failed for {}: {}",
                dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            facl::acl_free(acl);
            return true;
        }
        if trivial != 0 {
            facl::acl_free(acl);
            return false;
        }
        if facl::acl_set_fd_np(to_fd, acl, acl_type) < 0 {
            prog.warn(&format!(
                "failed to set acl entries for {}: {}",
                dst.display(),
                err_str(&io::Error::last_os_error())
            ));
            facl::acl_free(acl);
            return true;
        }
        facl::acl_free(acl);
        false
    }
}

#[cfg(not(target_os = "freebsd"))]
pub fn preserve_fd_acls(_from_fd: i32, _to_fd: i32, _dst: &Path, _prog: &mut Progress) -> bool {
    false
}

#[cfg(target_os = "freebsd")]
pub fn preserve_dir_acls(
    src: &Path,
    to: &To,
    rel: &Path,
    dst: &Path,
    prog: &mut Progress,
) -> bool {
    unsafe {
        let cs = cstr(src);
        let sfd = libc::open(cs.as_ptr(), libc::O_DIRECTORY | libc::O_RDONLY);
        if sfd < 0 {
            prog.warn(&format!(
                "{}: failed to copy ACLs: {}",
                src.display(),
                err_str(&io::Error::last_os_error())
            ));
            return true;
        }
        // For the copy root, cp uses the kept target dir fd directly.
        let (dfd, owned) = if rel.as_os_str().is_empty() {
            (to.dirfd, false)
        } else {
            let rc = relc(rel);
            let fd = libc::openat(to.dirfd, rc.as_ptr(), o_flags(to, libc::O_DIRECTORY));
            if fd < 0 {
                prog.warn(&format!(
                    "{}: failed to copy ACLs to {}: {}",
                    src.display(),
                    dst.display(),
                    err_str(&io::Error::last_os_error())
                ));
                libc::close(sfd);
                return true;
            }
            (fd, true)
        };
        let r = preserve_fd_acls(sfd, dfd, dst, prog);
        if owned {
            libc::close(dfd);
        }
        libc::close(sfd);
        r
    }
}

#[cfg(not(target_os = "freebsd"))]
pub fn preserve_dir_acls(
    _src: &Path,
    _to: &To,
    _rel: &Path,
    _dst: &Path,
    _prog: &mut Progress,
) -> bool {
    false
}
