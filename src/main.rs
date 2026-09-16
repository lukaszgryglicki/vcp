//! vcp — cp(1) clone with progress bars.
//!
//! Option-compatible with FreeBSD 15 cp(1) (see /usr/src/bin/cp), the
//! "v" means visible progress: MC-like ASCII progress bars on stderr.

mod copy;
mod progress;

use std::env;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::exit;

use progress::Progress;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFSOCK: u32 = 0o140000;

const EX_USAGE: i32 = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Clobber {
    Default,
    Force,       // -f
    Interactive, // -i
    NoClobber,   // -n
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LinkMode {
    Copy,
    Hard, // -l
    Sym,  // -s
}

pub struct Options {
    pub recursive: bool,   // -R (or -r)
    pub follow_root: bool, // follow symlinks given on the command line
    pub follow_all: bool,  // follow symlinks in the hierarchy
    pub clobber: Clobber,
    pub link: LinkMode,
    pub preserve: bool, // -p
    pub verbose: bool,  // -v
    pub one_fs: bool,   // -x
    pub no_flags: bool, // -N
    pub sort: bool,     // --sort
    pub mask: u32,      // ~umask & 07777
    pub umask: u32,     // umask & 07777
}

#[derive(Clone)]
pub struct Meta {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub dev: u64,
    pub ino: u64,
    pub rdev: u64,
    pub atime: (i64, i64),
    pub mtime: (i64, i64),
    pub flags: u32,
}

pub enum Kind {
    File,                 // copy contents (also -l/-s targets)
    Dir { fixup: usize }, // create directory, attributes fixed post-order
    Symlink,              // re-create symlink
    Fifo,                 // mkfifo
    Special,              // mknod
    Socket,               // warn only (like cp)
}

pub struct Action {
    pub src: PathBuf,
    pub dst: PathBuf,
    /// dst relative to To.dirfd ("" = the target dir itself); equals dst
    /// when To.dirfd is AT_FDCWD (FILE_TO_FILE case)
    pub rel: PathBuf,
    pub kind: Kind,
    pub meta: Meta,
    /// print dst with a trailing '/' in -v output (DIR_TO_DNE root, like cp)
    pub slash: bool,
}

/// Destination context, mirroring cp's PATH_T: all destination-side
/// operations are performed relative to `dirfd`, and with -R into a
/// directory FreeBSD cp resolves paths with RESOLVE_BENEATH so that
/// symlinks in the destination cannot escape the target directory.
pub struct To {
    pub dirfd: libc::c_int, // AT_FDCWD, or the target directory fd
    pub beneath: bool,      // -R and target is (or becomes) a directory
}

pub struct DirFixup {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub rel: PathBuf,
    pub meta: Meta,
    pub created: bool,
    pub failed: bool,
}

#[derive(Default)]
struct Plan {
    actions: Vec<Action>,
    fixups: Vec<DirFixup>,
    rval: i32,
}

// BSD file flags (chflags) exist on FreeBSD and macOS.
#[cfg(any(target_os = "freebsd", target_os = "macos"))]
fn stflags(st: &libc::stat) -> u32 {
    st.st_flags as u32
}
#[cfg(not(any(target_os = "freebsd", target_os = "macos")))]
fn stflags(_st: &libc::stat) -> u32 {
    0
}

fn meta_from(st: &libc::stat) -> Meta {
    Meta {
        mode: st.st_mode as u32,
        uid: st.st_uid as u32,
        gid: st.st_gid as u32,
        size: st.st_size as u64,
        dev: st.st_dev as u64,
        ino: st.st_ino as u64,
        rdev: st.st_rdev as u64,
        atime: (st.st_atime as i64, st.st_atime_nsec as i64),
        mtime: (st.st_mtime as i64, st.st_mtime_nsec as i64),
        flags: stflags(st),
    }
}

pub fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap_or_default()
}

pub fn xstat(path: &Path, follow: bool) -> io::Result<Meta> {
    let c = cstr(path);
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        let r = if follow {
            libc::stat(c.as_ptr(), &mut st)
        } else {
            libc::lstat(c.as_ptr(), &mut st)
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(meta_from(&st))
    }
}

pub fn fxstat(fd: i32) -> io::Result<Meta> {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(meta_from(&st))
    }
}

/// fstatat relative to a To context; empty rel means the dir itself (".").
pub fn atstat(to: &To, rel: &Path, flags: libc::c_int) -> io::Result<Meta> {
    let c = if rel.as_os_str().is_empty() {
        cstr(Path::new("."))
    } else {
        cstr(rel)
    };
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstatat(to.dirfd, c.as_ptr(), &mut st, flags) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(meta_from(&st))
    }
}

pub fn strerror(code: i32) -> String {
    unsafe {
        let p = libc::strerror(code);
        if p.is_null() {
            format!("Unknown error: {}", code)
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

pub fn err_str(e: &io::Error) -> String {
    match e.raw_os_error() {
        // Like cp: the kernel returns ENOTCAPABLE when a path lookup
        // violates RESOLVE_BENEATH; translate it to the more widely
        // recognized EACCES ("Permission denied").
        #[cfg(target_os = "freebsd")]
        Some(c) if c == libc::ENOTCAPABLE => strerror(libc::EACCES),
        Some(c) => strerror(c),
        None => e.to_string(),
    }
}

fn usage() -> ! {
    eprint!(
        "{}\n{}\n",
        "usage: vcp [-R [-H | -L | -P]] [-f | -i | -n] [-alpsvx] source_file target_file",
        "       vcp [-R [-H | -L | -P]] [-f | -i | -n] [-alpsvx] source_file ... target_directory"
    );
    exit(EX_USAGE);
}

fn errx1(msg: &str) -> ! {
    eprintln!("vcp: {}", msg);
    exit(1);
}

#[derive(Default)]
struct RawFlags {
    h: bool,
    big_l: bool,
    big_p: bool,
    big_r: bool,
    small_r: bool,
    a: bool,
    f: bool,
    i: bool,
    n: bool,
    l: bool,
    big_n: bool,
    p: bool,
    s: bool,
    v: bool,
    x: bool,
    sort: bool,
}

// Long options of FreeBSD 15 cp(1); "sort" maps to the internal 'S'.
const LONG_OPTS: &[(&str, char)] = &[
    ("archive", 'a'),
    ("force", 'f'),
    ("interactive", 'i'),
    ("dereference", 'L'),
    ("link", 'l'),
    ("no-clobber", 'n'),
    ("no-dereference", 'P'),
    ("recursive", 'R'),
    ("symbolic-link", 's'),
    ("verbose", 'v'),
    ("one-file-system", 'x'),
    ("sort", 'S'),
];

fn apply_flag(c: char, r: &mut RawFlags) {
    match c {
        'H' => {
            r.h = true;
            r.big_l = false;
            r.big_p = false;
        }
        'L' => {
            r.big_l = true;
            r.h = false;
            r.big_p = false;
        }
        'P' => {
            r.big_p = true;
            r.h = false;
            r.big_l = false;
        }
        'R' => r.big_r = true,
        'a' => {
            r.a = true;
            r.p = true;
            r.big_r = true;
            r.big_p = true;
            r.h = false;
            r.big_l = false;
        }
        'f' => {
            r.f = true;
            r.i = false;
            r.n = false;
        }
        'i' => {
            r.i = true;
            r.f = false;
            r.n = false;
        }
        'l' => r.l = true,
        'N' => r.big_n = true,
        'n' => {
            r.n = true;
            r.f = false;
            r.i = false;
        }
        'p' => r.p = true,
        'r' => {
            r.small_r = true;
            r.big_l = true;
            r.h = false;
            r.big_p = false;
        }
        's' => r.s = true,
        'v' => r.v = true,
        'x' => r.x = true,
        'S' => r.sort = true,
        _ => unreachable!(),
    }
}

const SHORT_OPTS: &str = "HLPRafilNnprsvx";

/// BSD getopt behavior: option parsing stops at the first non-option
/// argument ("+" prefix in cp's getopt_long call).
fn parse_args() -> (RawFlags, Vec<OsString>) {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    let mut raw = RawFlags::default();
    let mut idx = 0;
    while idx < args.len() {
        let b = args[idx].as_bytes();
        if b == b"--" {
            idx += 1;
            break;
        }
        if b.len() > 2 && b.starts_with(b"--") {
            let body = &b[2..];
            let (name, has_eq) = match body.iter().position(|&c| c == b'=') {
                Some(p) => (&body[..p], true),
                None => (body, false),
            };
            let name_s = String::from_utf8_lossy(name).into_owned();
            let mut hits: Vec<char> = LONG_OPTS
                .iter()
                .filter(|(n, _)| n.starts_with(name_s.as_str()))
                .map(|(_, c)| *c)
                .collect();
            if let Some((_, c)) = LONG_OPTS.iter().find(|(n, _)| *n == name_s) {
                hits = vec![*c];
            }
            match hits.len() {
                0 => {
                    eprintln!(
                        "vcp: unrecognized option `--{}'",
                        String::from_utf8_lossy(body)
                    );
                    usage();
                }
                1 => {
                    if has_eq {
                        eprintln!("vcp: option `--{}' doesn't allow an argument", name_s);
                        usage();
                    }
                    apply_flag(hits[0], &mut raw);
                }
                _ => {
                    eprintln!("vcp: option `--{}' is ambiguous", name_s);
                    usage();
                }
            }
        } else if b.len() >= 2 && b[0] == b'-' {
            for &cb in &b[1..] {
                let c = cb as char;
                if SHORT_OPTS.contains(c) {
                    apply_flag(c, &mut raw);
                } else {
                    eprintln!("vcp: invalid option -- {}", c);
                    usage();
                }
            }
        } else {
            break;
        }
        idx += 1;
    }
    (raw, args[idx..].to_vec())
}

/// Equivalent of fts_name for a root path: last component with trailing
/// slashes stripped; "/" for the root directory itself.
fn last_component(s: &OsStr) -> OsString {
    let b = s.as_bytes();
    let mut end = b.len();
    while end > 0 && b[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return OsString::from("/");
    }
    let start = b[..end]
        .iter()
        .rposition(|&c| c == b'/')
        .map(|p| p + 1)
        .unwrap_or(0);
    OsString::from_vec(b[start..end].to_vec())
}

// Flag for opening the target directory as a "search-only" handle for
// *at() calls: O_SEARCH on the BSDs (FreeBSD, macOS), O_PATH on Linux.
#[cfg(any(target_os = "freebsd", target_os = "macos"))]
pub const O_SEARCH_FLAG: libc::c_int = libc::O_SEARCH;
#[cfg(target_os = "linux")]
pub const O_SEARCH_FLAG: libc::c_int = libc::O_PATH;
#[cfg(not(any(target_os = "freebsd", target_os = "macos", target_os = "linux")))]
pub const O_SEARCH_FLAG: libc::c_int = libc::O_RDONLY;

fn walk(
    src: &Path,
    dst: PathBuf,
    rel: PathBuf,
    level: usize,
    root_dev: Option<u64>,
    opts: &Options,
    anc: &mut Vec<(u64, u64)>,
    plan: &mut Plan,
) {
    let follow = if level == 0 {
        opts.follow_root
    } else {
        opts.follow_all
    };
    let meta = match xstat(src, follow) {
        Ok(m) => m,
        Err(e) => {
            if follow {
                // Dangling/inaccessible symlink while following: let the
                // copy stage report the open error, exactly like cp.
                if let Ok(lm) = xstat(src, false) {
                    if lm.mode & S_IFMT == S_IFLNK {
                        plan.actions.push(Action {
                            src: src.to_path_buf(),
                            dst,
                            rel,
                            kind: Kind::File,
                            meta: lm,
                            slash: false,
                        });
                        return;
                    }
                }
            }
            eprintln!("vcp: {}: {}", src.display(), err_str(&e));
            plan.rval = 1;
            return;
        }
    };
    let kind = match meta.mode & S_IFMT {
        S_IFDIR => {
            if !opts.recursive {
                eprintln!("vcp: {} is a directory (not copied).", src.display());
                plan.rval = 1;
                return;
            }
            // A directory that is its own ancestor (symlink loops with
            // -L, nullfs recursion, ...): warn and skip, like fts(3)'s
            // FTS_DC gives cp.  Copying into the target itself needs no
            // guard here: the plan is snapshotted before anything is
            // created, so mid-copy creations are never walked.
            if anc.contains(&(meta.dev, meta.ino)) {
                eprintln!("vcp: {}: directory causes a cycle", src.display());
                plan.rval = 1;
                return;
            }
            let rdev = root_dev.unwrap_or(meta.dev);
            let fixup = plan.fixups.len();
            plan.fixups.push(DirFixup {
                src: src.to_path_buf(),
                dst: dst.clone(),
                rel: rel.clone(),
                meta: meta.clone(),
                created: false,
                failed: false,
            });
            plan.actions.push(Action {
                src: src.to_path_buf(),
                dst: dst.clone(),
                rel: rel.clone(),
                kind: Kind::Dir { fixup },
                meta: meta.clone(),
                slash: false,
            });
            if level > 0 && opts.one_fs && meta.dev != rdev {
                return; // -x: do not traverse mount points
            }
            match fs::read_dir(src) {
                Err(e) => {
                    eprintln!("vcp: {}: {}", src.display(), err_str(&e));
                    plan.rval = 1;
                }
                Ok(rd) => {
                    let mut names: Vec<OsString> = Vec::new();
                    for ent in rd {
                        match ent {
                            Ok(d) => names.push(d.file_name()),
                            Err(e) => {
                                eprintln!("vcp: {}: {}", src.display(), err_str(&e));
                                plan.rval = 1;
                            }
                        }
                    }
                    if opts.sort {
                        names.sort();
                    }
                    anc.push((meta.dev, meta.ino));
                    for n in names {
                        walk(
                            &src.join(&n),
                            dst.join(&n),
                            rel.join(&n),
                            level + 1,
                            Some(rdev),
                            opts,
                            anc,
                            plan,
                        );
                    }
                    anc.pop();
                }
            }
            return;
        }
        S_IFLNK => Kind::Symlink,
        S_IFIFO => {
            if opts.recursive && opts.link != LinkMode::Sym {
                Kind::Fifo
            } else {
                Kind::File
            }
        }
        S_IFBLK | S_IFCHR => {
            if opts.recursive && opts.link != LinkMode::Sym {
                Kind::Special
            } else {
                Kind::File
            }
        }
        S_IFSOCK => Kind::Socket,
        _ => Kind::File,
    };
    plan.actions.push(Action {
        src: src.to_path_buf(),
        dst,
        rel,
        kind,
        meta,
        slash: false,
    });
}

enum TargetType {
    ToFile,   // cp file1 file2
    ToDir,    // cp file ... dir
    DirToDne, // cp -R dir newdir
}

fn main() {
    copy::install_siginfo();
    let (raw, mut operands) = parse_args();

    if operands.len() < 2 {
        usage();
    }
    if raw.big_r && raw.small_r {
        errx1("the -R and -r options may not be specified together");
    }
    if raw.l && raw.s {
        errx1("the -l and -s options may not be specified together");
    }
    let recursive = raw.big_r || raw.small_r;
    let om = unsafe {
        let o = libc::umask(0);
        libc::umask(o);
        o
    };
    let opts = Options {
        recursive,
        follow_root: if recursive {
            raw.h || raw.big_l
        } else {
            !raw.big_p
        },
        follow_all: if recursive { raw.big_l } else { !raw.big_p },
        clobber: if raw.f {
            Clobber::Force
        } else if raw.i {
            Clobber::Interactive
        } else if raw.n {
            Clobber::NoClobber
        } else {
            Clobber::Default
        },
        link: if raw.l {
            LinkMode::Hard
        } else if raw.s {
            LinkMode::Sym
        } else {
            LinkMode::Copy
        },
        preserve: raw.p,
        verbose: raw.v,
        one_fs: raw.x,
        no_flags: raw.big_n,
        sort: raw.sort,
        mask: !(om as u32) & 0o7777,
        umask: (om as u32) & 0o7777,
    };

    // Target processing (trailing slashes, empty target), like cp.
    let mut tb = operands.pop().unwrap().into_vec();
    if tb.is_empty() {
        tb = b".".to_vec();
    }
    let mut trailing = false;
    if tb.ends_with(b"/") {
        trailing = true;
        while tb.len() > 1 && tb[tb.len() - 1] == b'/' {
            tb.pop();
        }
    }
    let target = PathBuf::from(OsString::from_vec(tb));
    let tdisp = target.display().to_string();

    let tstat = xstat(&target, true);
    let missing = matches!(&tstat, Err(e) if e.raw_os_error() == Some(libc::ENOENT));
    if let Err(e) = &tstat {
        if !missing {
            eprintln!("vcp: {}: {}", tdisp, err_str(e));
            exit(1);
        }
    }
    let target_is_dir = tstat
        .as_ref()
        .map(|m| m.mode & S_IFMT == S_IFDIR)
        .unwrap_or(false);

    let mut to = To {
        dirfd: libc::AT_FDCWD,
        beneath: false,
    };
    let ttype = if target_is_dir {
        // cp opens the target directory early and errors out if that
        // fails; the fd is kept and all destination operations are
        // performed relative to it.
        let c = cstr(&target);
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_DIRECTORY | O_SEARCH_FLAG) };
        if fd < 0 {
            eprintln!(
                "vcp: {}: {}",
                tdisp,
                err_str(&io::Error::last_os_error())
            );
            exit(1);
        }
        to.dirfd = fd;
        to.beneath = recursive;
        TargetType::ToDir
    } else {
        if operands.len() > 1 {
            eprintln!("vcp: {}: {}", tdisp, strerror(libc::ENOTDIR));
            exit(1);
        }
        let mut t = TargetType::ToFile;
        if missing {
            // cp -R dir foo, where foo does not exist
            let follow0 = recursive && opts.follow_root;
            if let Ok(m0) = xstat(Path::new(&operands[0]), follow0) {
                if m0.mode & S_IFMT == S_IFDIR && recursive {
                    t = TargetType::DirToDne;
                    // the target dir fd is opened right after the root
                    // mkdir, in copy_dir(); dst ops are beneath it
                    to.dirfd = -1;
                    to.beneath = true;
                }
            }
        }
        if trailing && matches!(t, TargetType::ToFile) {
            eprintln!(
                "vcp: {}: {}",
                tdisp,
                strerror(if missing {
                    libc::ENOENT
                } else {
                    libc::ENOTDIR
                })
            );
            exit(1);
        }
        t
    };

    // Plan phase: walk the sources, snapshot what has to be done.
    let mut plan = Plan::default();
    let mut anc: Vec<(u64, u64)> = Vec::new();
    match ttype {
        TargetType::ToFile | TargetType::DirToDne => {
            let rel = if matches!(ttype, TargetType::ToFile) {
                target.clone() // ops via AT_FDCWD use the full path
            } else {
                PathBuf::new() // created root; children relative to it
            };
            walk(
                Path::new(&operands[0]),
                target.clone(),
                rel,
                0,
                None,
                &opts,
                &mut anc,
                &mut plan,
            );
            // cp prints the created root as "target/" in -v output
            if matches!(ttype, TargetType::DirToDne) {
                if let Some(a) = plan.actions.first_mut() {
                    if matches!(a.kind, Kind::Dir { .. }) && a.dst == target {
                        a.slash = true;
                    }
                }
            }
        }
        TargetType::ToDir => {
            // fts with a comparator sorts the root set as well
            if opts.sort {
                operands.sort_by(|a, b| {
                    last_component(a)
                        .as_bytes()
                        .cmp(last_component(b).as_bytes())
                });
            }
            for s in &operands {
                let name = last_component(s);
                let (dst, rel) = if name.as_bytes() == b"/" {
                    (target.clone(), PathBuf::new())
                } else {
                    (target.join(&name), PathBuf::from(&name))
                };
                walk(Path::new(s), dst, rel, 0, None, &opts, &mut anc, &mut plan);
            }
        }
    }

    let total_files = plan
        .actions
        .iter()
        .filter(|a| matches!(a.kind, Kind::File))
        .count() as u64;
    let total_bytes: u64 = if opts.link == LinkMode::Copy {
        plan.actions
            .iter()
            .filter(|a| matches!(a.kind, Kind::File))
            .map(|a| a.meta.size)
            .sum()
    } else {
        0
    };

    let mut rval = plan.rval;
    let mut fixups = plan.fixups;
    let mut prog = Progress::new(total_files, total_bytes);
    prog.start();

    // Execution phase.
    let mut skips: Vec<PathBuf> = Vec::new();
    for a in &plan.actions {
        if skips.iter().any(|s| a.dst.starts_with(s)) {
            match a.kind {
                Kind::File => {
                    // keep the overall bars accurate for skipped subtrees
                    prog.file_start(&a.src.display().to_string(), a.meta.size);
                    prog.file_end();
                }
                Kind::Dir { fixup } => fixups[fixup].failed = true,
                _ => {}
            }
            continue;
        }
        if copy::exec_action(a, &opts, &mut to, &mut prog, &mut fixups, &mut skips) {
            rval = 1;
        }
    }

    // Post-order directory attribute fixups (children first), like cp's
    // FTS_DP phase: with -p set everything, otherwise chmod created dirs
    // to the source mode modified by our umask.
    for f in fixups.iter().rev() {
        if f.failed || !(f.created || opts.preserve) {
            continue;
        }
        if opts.preserve {
            if copy::setfile(&f.meta, None, &to, &f.rel, &f.dst, false, &opts, &mut prog) {
                rval = 1;
            }
            if copy::preserve_dir_acls(&f.src, &to, &f.rel, &f.dst, &mut prog) {
                rval = 1;
            }
        } else {
            let c = if f.rel.as_os_str().is_empty() {
                cstr(Path::new("."))
            } else {
                cstr(&f.rel)
            };
            let r = unsafe {
                libc::fchmodat(
                    to.dirfd,
                    c.as_ptr(),
                    (f.meta.mode & opts.mask) as libc::mode_t,
                    0,
                )
            };
            if r != 0 {
                prog.warn(&format!(
                    "chmod: {}: {}",
                    f.dst.display(),
                    err_str(&io::Error::last_os_error())
                ));
                rval = 1;
            }
        }
    }

    prog.finish();
    exit(rval);
}
