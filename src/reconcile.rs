//! Reconcile mode (--copy-new / --remove-gone / --reconcile).
//!
//! vcp snapshots the source tree before copying.  With these options,
//! after a phase completes the source is walked again and compared with
//! what was seen so far (`State`): entries that are new or whose source
//! identity changed are copied in another phase (--copy-new), entries
//! that disappeared from the source are removed from the destination
//! (--remove-gone) -- but only entries this run itself created or
//! overwrote, and only while they still are the inode it made.  Root
//! operands are never removed.  Phases repeat until a re-scan finds no
//! more work (--passes=0) or the pass limit is hit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::{Action, Kind, Meta, Options, S_IFDIR, S_IFMT};

/// What is known about one planned destination entry, keyed by the
/// destination path relative to the target directory (`Action.rel`).
pub struct Entry {
    /// Source identity when the entry was last planned.
    pub src: Meta,
    /// (dev, ino) of the destination entry this run created/overwrote;
    /// None when the destination was not touched (skipped, failed,
    /// pre-existing and not overwritten, socket, ...).
    pub owned: Option<(u64, u64)>,
    /// The source was already gone when its turn came (deferred error).
    pub vanished: bool,
    /// Command-line operand: reported when missing, never removed.
    pub root: bool,
    pub dst: PathBuf,
}

pub type State = HashMap<PathBuf, Entry>;

/// Result of executing one action, as far as reconciliation cares.
#[derive(Default)]
pub struct Outcome {
    pub owned: Option<(u64, u64)>,
    pub vanished: bool,
}

/// Same source as seen before?  Type, inode, size and mtime for files
/// and links; directories change with their entries, so for them only
/// the type counts.  With -p the preserved attributes count as well.
pub fn same_source(old: &Meta, new: &Meta, preserve: bool) -> bool {
    if old.mode & S_IFMT != new.mode & S_IFMT {
        return false;
    }
    if new.mode & S_IFMT != S_IFDIR
        && (old.dev, old.ino, old.size, old.mtime) != (new.dev, new.ino, new.size, new.mtime)
    {
        return false;
    }
    if preserve
        && (old.mode & 0o7777, old.uid, old.gid, old.flags)
            != (new.mode & 0o7777, new.uid, new.gid, new.flags)
    {
        return false;
    }
    true
}

/// --settle: a file modified less than `settle` seconds ago is still
/// being written for all we know; hold it back to a later phase.
/// Returns the time until it settles, or None when it is settled.
pub fn unsettled_for(mtime: (i64, i64), now: i64, settle: u64) -> Option<Duration> {
    if settle == 0 {
        return None;
    }
    let age = now - mtime.0;
    if age < 0 || age as u64 >= settle {
        // mtimes in the future never settle; treat them as settled
        return None;
    }
    Some(Duration::from_secs(settle - age as u64))
}

/// Number of path components (depth) of a relative destination path.
fn depth(p: &Path) -> usize {
    p.components().count()
}

/// Deepest entries first, so children go before their directories.
pub fn deepest_first(v: &mut [PathBuf]) {
    v.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| b.cmp(a)));
}

fn add_ancestors(rel: &Path, need: &mut HashSet<PathBuf>) {
    let mut p = rel.parent();
    while let Some(q) = p {
        need.insert(q.to_path_buf());
        p = q.parent();
    }
}

pub struct Diff {
    /// Per new-plan action: execute it in this pass.
    pub keep: Vec<bool>,
    pub new: usize,
    pub changed: usize,
    /// Destination entries to remove (--remove-gone), deepest first:
    /// gone from the source, or still present with a different type
    /// (the old destination must go before the new one can be made).
    pub removed: Vec<PathBuf>,
    /// The type-changed subset of `removed`, for reporting.
    pub type_changed: Vec<PathBuf>,
    /// Files held back by --settle and the time until the first settles.
    pub unsettled: usize,
    pub wait: Option<Duration>,
}

/// Compare a fresh plan with the state accumulated so far.
pub fn diff(state: &State, actions: &[Action], opts: &Options, now: i64) -> Diff {
    let mut d = Diff {
        keep: vec![false; actions.len()],
        new: 0,
        changed: 0,
        removed: Vec::new(),
        type_changed: Vec::new(),
        unsettled: 0,
        wait: None,
    };
    let mut present: HashSet<&Path> = HashSet::new();
    let mut need: HashSet<PathBuf> = HashSet::new();
    for (i, a) in actions.iter().enumerate() {
        present.insert(a.rel.as_path());
        let prev = state.get(&a.rel);
        let is_new = prev.is_none();
        let is_changed = match prev {
            Some(e) => e.vanished || !same_source(&e.src, &a.meta, opts.preserve),
            None => false,
        };
        if let Some(e) = prev {
            if opts.remove_gone && e.src.mode & S_IFMT != a.meta.mode & S_IFMT {
                d.type_changed.push(a.rel.clone());
                d.removed.push(a.rel.clone());
                add_ancestors(&a.rel, &mut need);
            }
        }
        if !(is_new || is_changed) || !opts.copy_new {
            continue;
        }
        if matches!(a.kind, Kind::File) {
            if let Some(w) = unsettled_for(a.meta.mtime, now, opts.settle) {
                d.unsettled += 1;
                d.wait = Some(d.wait.map_or(w, |cur| cur.min(w)));
                continue;
            }
        }
        d.keep[i] = true;
        if is_new {
            d.new += 1;
        } else {
            d.changed += 1;
        }
        add_ancestors(&a.rel, &mut need);
    }
    if opts.remove_gone {
        for (rel, e) in state {
            if !e.root && !present.contains(rel.as_path()) {
                d.removed.push(rel.clone());
                add_ancestors(rel, &mut need);
            }
        }
    }
    // Directories on the way to anything done in this pass are kept so
    // that new ones get created in order and -p re-applies the times of
    // directories whose entries changed.
    for (i, a) in actions.iter().enumerate() {
        if !d.keep[i] && matches!(a.kind, Kind::Dir { .. }) && need.contains(&a.rel) {
            d.keep[i] = true;
        }
    }
    deepest_first(&mut d.removed);
    deepest_first(&mut d.type_changed);
    d
}

/// Record the result of an executed (or skipped) action.  Ownership of
/// a destination entry is never forgotten: a directory created in an
/// earlier phase stays ours when a later phase just passes through it.
pub fn record(state: &mut State, a: &Action, out: &Outcome) {
    let prev = state.remove(&a.rel);
    let owned = out.owned.or(prev.and_then(|e| e.owned));
    state.insert(
        a.rel.clone(),
        Entry {
            src: a.meta.clone(),
            owned,
            vanished: out.vanished,
            root: a.root,
            dst: a.dst.clone(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clobber, LinkMode, S_IFLNK, S_IFREG};

    fn meta(mode: u32, ino: u64, size: u64, mtime: i64) -> Meta {
        Meta {
            mode,
            uid: 1,
            gid: 1,
            size,
            dev: 7,
            ino,
            rdev: 0,
            atime: (0, 0),
            mtime: (mtime, 0),
            flags: 0,
        }
    }

    fn opts(copy_new: bool, remove_gone: bool, settle: u64, preserve: bool) -> Options {
        Options {
            recursive: true,
            follow_root: false,
            follow_all: false,
            clobber: Clobber::Default,
            link: LinkMode::Copy,
            preserve,
            verbose: false,
            one_fs: false,
            no_flags: false,
            sort: false,
            mask: 0o7777,
            umask: 0,
            copy_new,
            remove_gone,
            passes: 0,
            settle,
        }
    }

    fn act(rel: &str, meta: Meta, root: bool) -> Action {
        let kind = if meta.mode & S_IFMT == S_IFDIR {
            Kind::Dir { fixup: 0 }
        } else if meta.mode & S_IFMT == S_IFLNK {
            Kind::Symlink
        } else {
            Kind::File
        };
        Action {
            src: PathBuf::from("src").join(rel),
            dst: PathBuf::from("dst").join(rel),
            rel: PathBuf::from(rel),
            kind,
            meta,
            slash: false,
            root,
        }
    }

    fn entry(meta: Meta, owned: Option<(u64, u64)>, root: bool) -> Entry {
        Entry {
            src: meta,
            owned,
            vanished: false,
            root,
            dst: PathBuf::new(),
        }
    }

    #[test]
    fn source_identity_files() {
        let a = meta(S_IFREG | 0o644, 10, 100, 1000);
        assert!(same_source(&a, &a, false));
        assert!(!same_source(&a, &meta(S_IFREG | 0o644, 11, 100, 1000), false)); // ino
        assert!(!same_source(&a, &meta(S_IFREG | 0o644, 10, 101, 1000), false)); // size
        assert!(!same_source(&a, &meta(S_IFREG | 0o644, 10, 100, 1001), false)); // mtime
        assert!(!same_source(&a, &meta(S_IFDIR | 0o644, 10, 100, 1000), false)); // type
        // attribute-only changes count only with -p
        let chmod = meta(S_IFREG | 0o600, 10, 100, 1000);
        assert!(same_source(&a, &chmod, false));
        assert!(!same_source(&a, &chmod, true));
    }

    #[test]
    fn source_identity_dirs_ignore_contents_churn() {
        let a = meta(S_IFDIR | 0o755, 10, 100, 1000);
        let b = meta(S_IFDIR | 0o755, 99, 200, 2000); // new ino/size/mtime
        assert!(same_source(&a, &b, false));
        assert!(same_source(&a, &b, true));
        assert!(!same_source(&a, &meta(S_IFDIR | 0o700, 10, 100, 1000), true));
        assert!(!same_source(&a, &meta(S_IFREG | 0o755, 10, 100, 1000), false));
    }

    #[test]
    fn settle_window() {
        assert_eq!(unsettled_for((100, 0), 105, 0), None); // disabled
        assert_eq!(unsettled_for((100, 0), 105, 10), Some(Duration::from_secs(5)));
        assert_eq!(unsettled_for((100, 0), 110, 10), None); // exactly settled
        assert_eq!(unsettled_for((100, 0), 111, 10), None);
        assert_eq!(unsettled_for((200, 0), 105, 10), None); // future mtime
    }

    #[test]
    fn deepest_first_order() {
        let mut v = vec![
            PathBuf::from("a"),
            PathBuf::from("a/b/c"),
            PathBuf::from("a/b"),
            PathBuf::from("z"),
        ];
        deepest_first(&mut v);
        assert_eq!(
            v,
            vec![
                PathBuf::from("a/b/c"),
                PathBuf::from("a/b"),
                PathBuf::from("z"),
                PathBuf::from("a"),
            ]
        );
    }

    #[test]
    fn diff_new_changed_same_and_ancestors() {
        let o = opts(true, true, 0, false);
        let mut st = State::new();
        st.insert(PathBuf::from("d"), entry(meta(S_IFDIR | 0o755, 1, 0, 0), Some((7, 1)), true));
        st.insert(PathBuf::from("d/same"), entry(meta(S_IFREG | 0o644, 2, 10, 5), Some((7, 2)), false));
        st.insert(PathBuf::from("d/chg"), entry(meta(S_IFREG | 0o644, 3, 10, 5), Some((7, 3)), false));
        st.insert(PathBuf::from("d/sub"), entry(meta(S_IFDIR | 0o755, 4, 0, 0), Some((7, 4)), false));
        st.insert(PathBuf::from("d/sub/quiet"), entry(meta(S_IFREG | 0o644, 5, 1, 1), Some((7, 5)), false));
        let plan = vec![
            act("d", meta(S_IFDIR | 0o755, 1, 0, 0), true),
            act("d/same", meta(S_IFREG | 0o644, 2, 10, 5), false),
            act("d/chg", meta(S_IFREG | 0o644, 3, 11, 6), false),
            act("d/sub", meta(S_IFDIR | 0o755, 4, 0, 0), false),
            act("d/sub/quiet", meta(S_IFREG | 0o644, 5, 1, 1), false),
            act("d/new", meta(S_IFREG | 0o644, 6, 3, 9), false),
            act("d/ndir", meta(S_IFDIR | 0o755, 8, 0, 0), false),
            act("d/ndir/x", meta(S_IFREG | 0o644, 9, 3, 9), false),
        ];
        let d = diff(&st, &plan, &o, 1000);
        // root dir kept as ancestor, same/sub/quiet not, changed+new kept
        assert_eq!(d.keep, vec![true, false, true, false, false, true, true, true]);
        assert_eq!((d.new, d.changed), (3, 1));
        assert!(d.removed.is_empty());
        assert!(d.type_changed.is_empty());
        assert_eq!(d.unsettled, 0);
    }

    #[test]
    fn diff_removed_deepest_first_and_roots_never() {
        let o = opts(true, true, 0, false);
        let mut st = State::new();
        st.insert(PathBuf::from("r"), entry(meta(S_IFDIR | 0o755, 1, 0, 0), Some((7, 1)), true));
        st.insert(PathBuf::from("r/gone"), entry(meta(S_IFDIR | 0o755, 2, 0, 0), Some((7, 2)), false));
        st.insert(PathBuf::from("r/gone/f"), entry(meta(S_IFREG | 0o644, 3, 1, 1), Some((7, 3)), false));
        st.insert(PathBuf::from("r/keep"), entry(meta(S_IFREG | 0o644, 4, 1, 1), Some((7, 4)), false));
        st.insert(PathBuf::from("lost-root"), entry(meta(S_IFREG | 0o644, 5, 1, 1), Some((7, 5)), true));
        let plan = vec![
            act("r", meta(S_IFDIR | 0o755, 1, 0, 0), true),
            act("r/keep", meta(S_IFREG | 0o644, 4, 1, 1), false),
        ];
        let d = diff(&st, &plan, &o, 1000);
        assert_eq!(d.removed, vec![PathBuf::from("r/gone/f"), PathBuf::from("r/gone")]);
        // the root dir is kept so -p can re-apply its times after removals
        assert_eq!(d.keep, vec![true, false]);
        assert_eq!((d.new, d.changed), (0, 0));
    }

    #[test]
    fn diff_type_change_and_vanished() {
        let o = opts(true, true, 0, false);
        let mut st = State::new();
        st.insert(PathBuf::from("t"), entry(meta(S_IFDIR | 0o755, 1, 0, 0), Some((7, 1)), true));
        st.insert(PathBuf::from("t/x"), entry(meta(S_IFDIR | 0o755, 2, 0, 0), Some((7, 2)), false));
        st.insert(PathBuf::from("t/x/in"), entry(meta(S_IFREG | 0o644, 3, 1, 1), Some((7, 3)), false));
        let mut van = entry(meta(S_IFREG | 0o644, 4, 1, 1), None, false);
        van.vanished = true;
        st.insert(PathBuf::from("t/v"), van);
        let plan = vec![
            act("t", meta(S_IFDIR | 0o755, 1, 0, 0), true),
            act("t/x", meta(S_IFREG | 0o644, 9, 5, 5), false), // dir -> file
            act("t/v", meta(S_IFREG | 0o644, 4, 1, 1), false), // reappeared, same meta
        ];
        let d = diff(&st, &plan, &o, 1000);
        assert_eq!(d.type_changed, vec![PathBuf::from("t/x")]);
        // children first, then the directory that became a file
        assert_eq!(d.removed, vec![PathBuf::from("t/x/in"), PathBuf::from("t/x")]);
        assert_eq!(d.keep, vec![true, true, true]);
        assert_eq!((d.new, d.changed), (0, 2));
    }

    #[test]
    fn diff_respects_mode_flags_and_settle() {
        // --remove-gone only: nothing is copied, removals still found
        let o = opts(false, true, 0, false);
        let mut st = State::new();
        st.insert(PathBuf::from("r"), entry(meta(S_IFDIR | 0o755, 1, 0, 0), Some((7, 1)), true));
        st.insert(PathBuf::from("r/old"), entry(meta(S_IFREG | 0o644, 2, 1, 1), Some((7, 2)), false));
        let plan = vec![
            act("r", meta(S_IFDIR | 0o755, 1, 0, 0), true),
            act("r/new", meta(S_IFREG | 0o644, 3, 1, 990), false),
        ];
        let d = diff(&st, &plan, &o, 1000);
        assert_eq!((d.new, d.changed), (0, 0));
        assert_eq!(d.removed, vec![PathBuf::from("r/old")]);
        assert_eq!(d.keep, vec![true, false]);
        // --copy-new with --settle=30: the 10s old file is held back
        let o = opts(true, false, 30, false);
        let d = diff(&st, &plan, &o, 1000);
        assert_eq!(d.keep, vec![false, false]);
        assert_eq!(d.unsettled, 1);
        assert_eq!(d.wait, Some(Duration::from_secs(20)));
        assert!(d.removed.is_empty()); // not asked to remove
    }

    #[test]
    fn record_keeps_ownership() {
        let mut st = State::new();
        let a = act("d", meta(S_IFDIR | 0o755, 1, 0, 0), true);
        record(&mut st, &a, &Outcome { owned: Some((7, 1)), vanished: false });
        // later phase passes through the existing directory: no new id
        record(&mut st, &a, &Outcome::default());
        assert_eq!(st[&PathBuf::from("d")].owned, Some((7, 1)));
        let f = act("d/f", meta(S_IFREG | 0o644, 2, 1, 1), false);
        record(&mut st, &f, &Outcome { owned: Some((7, 2)), vanished: false });
        record(&mut st, &f, &Outcome { owned: None, vanished: true });
        let e = &st[&PathBuf::from("d/f")];
        assert!(e.vanished);
        assert_eq!(e.owned, Some((7, 2))); // our earlier copy is still ours
    }
}
