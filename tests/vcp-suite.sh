#!/bin/sh
# vcp vs /bin/cp behavior comparison suite (run on the FreeBSD host):
#   cargo build --release && sh tests/vcp-suite.sh
# Every check runs vcp and the system cp side by side and compares
# messages, exit codes and resulting trees; pty checks (script(1))
# verify the rendered progress bars.
D=$(cd "$(dirname "$0")/.." && pwd)
V="$D/target/release/vcp"
C=/bin/cp
T=/tmp/vcptest
[ -x "$V" ] || { echo "build first: cargo build --release" >&2; exit 98; }
rm -rf "$T"; mkdir -p "$T"; cd "$T" || exit 99
pass=0; fail=0

ok()  { pass=$((pass+1)); }
bad() { fail=$((fail+1)); echo "FAIL: $1"; }
chk() { [ "$2" = "$3" ] && ok || bad "$1 (exp [$2] got [$3])"; }

# mtree-based tree comparison: type,size,link coverage
treecmp() { # name dir1 dir2
  mtree -c -p "$2" -k type,size,link > "$T/.spec" 2>/dev/null
  out=$(mtree -p "$3" < "$T/.spec" 2>&1); rc=$?
  [ $rc -eq 0 ] && [ -z "$out" ] && ok || bad "$1 (mtree rc=$rc: $out)"
}

### 1. single file copy
echo hello > a.txt
$V a.txt b.txt 2>e1; chk single-rc 0 $?
cmp -s a.txt b.txt && ok || bad single-content
[ -s e1 ] && bad "single-stderr ($(cat e1))" || ok   # not a tty -> no bars

### 2. copy into dir
mkdir d1
$V a.txt d1 2>/dev/null; chk intodir-rc 0 $?
cmp -s a.txt d1/a.txt && ok || bad intodir-content

### 3. multiple sources, non-dir target
m1=$($V a.txt b.txt notdir.txt 2>&1); r1=$?
m2=$($C a.txt b.txt notdir.txt 2>&1); r2=$?
chk multi-nondir-rc "$r2" "$r1"
chk multi-nondir-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"

### 4. -R tree copy (files, subdirs, symlinks, dangling link, fifo, empty dir)
mkdir -p tree/sub1/sub2 tree/empty
echo f1 > tree/f1; echo f2 > tree/sub1/f2
dd if=/dev/random of=tree/sub1/sub2/big bs=1m count=3 2>/dev/null
ln -s f1 tree/link1; ln -s /nonexistent tree/dangle; mkfifo tree/fifo
chmod 750 tree/sub1
$V -R tree tcopy 2>/dev/null; chk rtree-rc 0 $?
$C -R tree tcopy-cp 2>/dev/null
treecmp rtree-vs-cp tcopy-cp tcopy
[ "$(readlink tcopy/link1)" = "f1" ] && ok || bad rtree-symlink
[ -p tcopy/fifo ] && ok || bad rtree-fifo
chk rtree-submode "$(stat -f %p tcopy-cp/sub1)" "$(stat -f %p tcopy/sub1)"

### 5. -R into existing dir (target/tree created)
mkdir ex ex2
$V -R tree ex 2>/dev/null; chk rexist-rc 0 $?
$C -R tree ex2 2>/dev/null
treecmp rexist ex2/tree ex/tree

### 6. -R dir to non-existing (DIR_TO_DNE)
$V -R tree newtree 2>/dev/null; chk dne-rc 0 $?
$C -R tree newtree-cp 2>/dev/null
treecmp dne newtree-cp newtree

### 7. -p preservation (mode, mtime, atime, dir times)
chmod 4711 tree/f1 2>/dev/null
touch -t 202001020304.05 tree/f1
$C -pR tree ptree-cp 2>/dev/null
touch -t 202001020304.05 tree/f1   # reset atime mutated by cp's read
$V -pR tree ptree 2>/dev/null; chk p-rc 0 $?
chk p-file-meta "$(stat -f '%p %m %a' ptree-cp/f1)" "$(stat -f '%p %m %a' ptree/f1)"
chk p-dir-meta  "$(stat -f '%p %m' ptree-cp/sub1)" "$(stat -f '%p %m' ptree/sub1)"

### 8. -i interactive
echo old > i1.txt; echo new > i2.txt
printf 'n\n' | $V -i i2.txt i1.txt 2>im1; r1=$?
printf 'n\n' | $C -i i2.txt i1.txt 2>im2; r2=$?
chk i-decline-rc "$r2" "$r1"
chk i-decline-msg "$(sed s/^cp/vcp/ im2 | tr -d '\n')" "$(tr -d '\n' < im1)"
chk i-decline-content old "$(cat i1.txt)"
printf 'y\n' | $V -i i2.txt i1.txt 2>/dev/null; chk i-accept-rc 0 $?
chk i-accept-content new "$(cat i1.txt)"

### 9. -n no-clobber
echo old > n1.txt
$V -n i2.txt n1.txt 2>/dev/null; r1=$?
$C -n i2.txt n1.txt 2>/dev/null; r2=$?
chk n-rc "$r2" "$r1"
chk n-content old "$(cat n1.txt)"
m1=$($V -nv i2.txt n1.txt 2>/dev/null); m2=$($C -nv i2.txt n1.txt 2>/dev/null)
chk n-v-msg "$m2" "$m1"

### 10. -f force unwritable target
echo old > f1.txt; chmod 000 f1.txt
$V -f i2.txt f1.txt 2>/dev/null; chk f-rc 0 $?
chk f-content new "$(cat f1.txt)"

### 11. -l hardlink / -s symlink
$V -l a.txt hl.txt 2>/dev/null; chk l-rc 0 $?
chk l-inode "$(stat -f %i a.txt)" "$(stat -f %i hl.txt)"
$V -s "$T/a.txt" sl.txt 2>/dev/null; chk s-rc 0 $?
chk s-target "$T/a.txt" "$(readlink sl.txt)"

### 12. -v output format
rm -rf vv vv2
m1=$($V -Rv tree vv 2>/dev/null); m2=$($C -Rv tree vv2 2>/dev/null | sed s/vv2/vv/g)
chk v-output "$m2" "$m1"

### 13. identical file
m1=$($V a.txt a.txt 2>&1); r1=$?
m2=$($C a.txt a.txt 2>&1); r2=$?
chk ident-rc "$r2" "$r1"
chk ident-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"

### 14. directory without -R
m1=$($V tree nodirtarget 2>&1); r1=$?
m2=$($C tree nodirtarget2 2>&1); r2=$?
chk dirnor-rc "$r2" "$r1"
chk dirnor-msg "$(echo "$m2" | sed 's/^cp/vcp/;s/nodirtarget2/nodirtarget/')" "$m1"

### 15. trailing slash, missing target
m1=$($V a.txt missing/ 2>&1); r1=$?
m2=$($C a.txt missing/ 2>&1); r2=$?
chk slash-rc "$r2" "$r1"
chk slash-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"

### 16. symlink follow modes -R / -RH / -RL
mkdir s; echo x > s/f; ln -s s slink
$V -R slink c1 2>/dev/null; $C -R slink c1-cp 2>/dev/null
chk RP-type "$(stat -f %p c1-cp 2>/dev/null || echo GONE)" "$(stat -f %p c1 2>/dev/null || echo GONE)"
$V -RH slink c2 2>/dev/null; $C -RH slink c2-cp 2>/dev/null
treecmp RH c2-cp c2
$V -RL slink c3 2>/dev/null; $C -RL slink c3-cp 2>/dev/null
treecmp RL c3-cp c3
# default (no -R): follow
$V slink/f fl.txt 2>/dev/null; chk follow-rc 0 $?

### 17. src/. contents copy
$V -R tree/. dot 2>/dev/null; chk dot-rc 0 $?
$C -R tree/. dot-cp 2>/dev/null
treecmp dot dot-cp dot

### 18. usage / conflicting options
$V >/dev/null 2>&1; chk usage-rc 64 $?
m1=$($V 2>&1 | head -1); chk usage-msg "usage: vcp [-R [-H | -L | -P]] [-f | -i | -n] [-alpsvx] source_file target_file" "$m1"
$V -R -r a.txt x1 2>/dev/null; r1=$?
$C -R -r a.txt x1 2>/dev/null; r2=$?
chk Rr-rc "$r2" "$r1"
m1=$($V -R -r a.txt x1 2>&1); m2=$($C -R -r a.txt x1 2>&1)
chk Rr-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"
$V -l -s a.txt x1 2>/dev/null; chk ls-rc 1 $?

### 19. bad options
m1=$($V -Z a.txt z1 2>&1 | head -1); r1=$?
m2=$($C -Z a.txt z1 2>&1 | head -1)
chk badopt-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"
m1=$($V --bogus a.txt z1 2>&1 | head -1)
m2=$($C --bogus a.txt z1 2>&1 | head -1)
chk badlong-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"
# long option prefix + full long options work
$V --verb a.txt z2.txt >/dev/null 2>&1; chk longprefix-rc 0 $?
$V --recursive tree z3 2>/dev/null; chk longfull-rc 0 $?

### 20. missing source among good ones
rm -rf md; mkdir md
m1=$($V nosuchfile a.txt md 2>&1); r1=$?
m2=$($C nosuchfile a.txt md 2>&1); r2=$?
chk partial-rc "$r2" "$r1"
chk partial-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"
cmp -s a.txt md/a.txt && ok || bad partial-goodfile

### 21. names with spaces
echo sp > "sp ace.txt"
$V "sp ace.txt" "sp ace 2.txt" 2>/dev/null; chk space-rc 0 $?
cmp -s "sp ace.txt" "sp ace 2.txt" && ok || bad space-content

### 22. empty file
: > empty.txt
$V empty.txt ecopy.txt 2>/dev/null; chk empty-rc 0 $?
cmp -s empty.txt ecopy.txt && ok || bad empty-content

### 23. -a archive mode
$V -a tree atree 2>/dev/null; chk a-rc 0 $?
$C -a tree atree-cp 2>/dev/null
treecmp a-tree atree-cp atree
chk a-meta "$(stat -f '%p %m' atree-cp/f1)" "$(stat -f '%p %m' atree/f1)"

### 24. cp -R d d (copy into itself at root)
rm -rf selfd selfd-cp
mkdir selfd selfd-cp; echo s > selfd/s.txt; echo s > selfd-cp/s.txt
$V -R selfd selfd 2>v24; r1=$?
$C -R selfd-cp selfd-cp 2>c24; r2=$?
chk self-rc "$r2" "$r1"
[ -f selfd/selfd/s.txt ] && ok || bad self-created
[ -e selfd/selfd/selfd ] && bad self-recursed || ok

### 25. -R dir dir/sub (target nested in source)
rm -rf nest nest-cp
mkdir -p nest/sub; echo n > nest/n.txt
mkdir -p nest-cp/sub; echo n > nest-cp/n.txt
$V -R nest nest/sub 2>/dev/null; r1=$?
$C -R nest-cp nest-cp/sub 2>/dev/null; r2=$?
chk nest-rc "$r2" "$r1"
l1=$(cd nest && find . | sed 's/nest/X/g' | sort)
l2=$(cd nest-cp && find . | sed 's/nest-cp/X/g' | sort)
chk nest-structure "$l2" "$l1"

### 26. overwrite dir with file
m1=$($V a.txt d1 2>&1; echo rc=$?)   # d1 exists dir -> copies INTO it; use tree/f1 onto dir
rm -rf dird; mkdir dird; mkdir dird2
m1=$($V -R tree/f1 dird/empty 2>&1) # file onto missing path ok
# file over existing dir:
m1=$($V a.txt tree/empty/../empty 2>&1; echo ":$?")
m2=$($C a.txt tree/empty/../empty 2>&1; echo ":$?")
chk file-over-dir "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"

### 27. -RL symlink cycle
rm -rf cyc cyccopy cyccopy-cp
mkdir -p cyc/inner; ln -s .. cyc/inner/loop; echo c > cyc/c.txt
m1=$($V -RL cyc cyccopy 2>&1); r1=$?
m2=$($C -RL cyc cyccopy-cp 2>&1); r2=$?
chk cycle-rc "$r2" "$r1"
chk cycle-msg "$(echo "$m2" | sed 's/^cp/vcp/;s/cyccopy-cp/cyccopy/')" "$m1"

### 28. -s onto existing file: message names the DESTINATION
rm -rf sdir; mkdir sdir; echo x > sfile; echo y > sdir/sfile
m1=$($V -s "$T/sfile" sdir 2>&1); r1=$?
m2=$($C -s "$T/sfile" sdir 2>&1); r2=$?
chk s-exists-rc "$r2" "$r1"
chk s-exists-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"

### 29. --sort orders the root operands too
rm -rf so vo; mkdir so vo; echo 1 > zz; echo 2 > aa; echo 3 > mm
m1=$($V --sort -v zz aa mm vo 2>/dev/null); r1=$?
m2=$($C --sort -v zz aa mm so 2>/dev/null); r2=$?
chk sort-roots-rc "$r2" "$r1"
chk sort-roots-out "$(echo "$m2" | sed 's/ so/ vo/')" "$m1"

### 30. -R beneath: relative symlink escaping the target is refused
rm -rf esc bsrc bdst bdst-cp; mkdir -p esc bsrc bdst/bsrc bdst-cp/bsrc
echo SAFE > esc/victim
echo EVIL > bsrc/victim
ln -s ../../esc/victim bdst/bsrc/victim
ln -s ../../esc/victim bdst-cp/bsrc/victim
m1=$($V -R bsrc bdst 2>&1); r1=$?
m2=$($C -R bsrc bdst-cp 2>&1); r2=$?
chk beneath-rel-rc "$r2" "$r1"
chk beneath-rel-msg "$(echo "$m2" | sed 's/^cp/vcp/;s/bdst-cp/bdst/')" "$m1"
chk beneath-rel-safe SAFE "$(cat esc/victim)"

### 31. -R beneath: absolute symlink escaping the target is refused
rm -rf esc2 bdst2 bdst2-cp; mkdir -p esc2 bdst2/bsrc bdst2-cp/bsrc
echo SAFE > esc2/victim
ln -s "$T/esc2/victim" bdst2/bsrc/victim
ln -s "$T/esc2/victim" bdst2-cp/bsrc/victim
m1=$($V -R bsrc bdst2 2>&1); r1=$?
m2=$($C -R bsrc bdst2-cp 2>&1); r2=$?
chk beneath-abs-rc "$r2" "$r1"
chk beneath-abs-msg "$(echo "$m2" | sed 's/^cp/vcp/;s/bdst2-cp/bdst2/')" "$m1"
chk beneath-abs-safe SAFE "$(cat esc2/victim)"

### 32. non-recursive into-dir: dst symlinks are followed (no beneath)
rm -rf ndir nout nout2; mkdir ndir; echo ORIG > nout; echo ORIG > nout2
ln -s ../nout ndir/a.txt
$V a.txt ndir 2>/dev/null; r1=$?
c1=$(cat nout)
rm ndir/a.txt; ln -s ../nout2 ndir/a.txt
$C a.txt ndir 2>/dev/null; r2=$?
c2=$(cat nout2)
chk nonrec-follow-rc "$r2" "$r1"
chk nonrec-follow-content "$c2" "$c1"
# ... but with -R even a single file goes beneath the target dir
rm ndir/a.txt; ln -s ../nout ndir/a.txt
m1=$($V -R a.txt ndir 2>&1); r1=$?
rm ndir/a.txt; ln -s ../nout ndir/a.txt
m2=$($C -R a.txt ndir 2>&1); r2=$?
chk beneath-single-rc "$r2" "$r1"
chk beneath-single-msg "$(echo "$m2" | sed s/^cp/vcp/)" "$m1"

### 33. DIR_TO_DNE root: -p fixes up root dir attrs via the kept fd
rm -rf pv pc
$V -Rp tree pv 2>/dev/null
$C -Rp tree pc 2>/dev/null
chk dne-root-p "$(stat -f '%Sp %m %u %g' pc)" "$(stat -f '%Sp %m %u %g' pv)"
treecmp dne-root-p-tree pc pv

### 34. COLUMNS forces bar width; single-file bar renders at 89 cols
ESC=$(printf '\033')
rm -f wcap wlines wbig.out
COLUMNS=90 script -q wcap $V tree/sub1/sub2/big wbig.out >/dev/null 2>&1
tr '\r' '\n' < wcap | sed -E "s/${ESC}\[[0-9]*[A-Za-z]//g" | grep '^|' > wlines
[ -s wlines ] && ok || bad w90-frames
awk 'length($0)!=89{exit 1}' wlines && ok || bad w90-linelen
grep -q '100\.00%' wlines && ok || bad w90-final
cmp -s tree/sub1/sub2/big wbig.out && ok || bad w90-content

### 35. three-bar mode: / and % never move within count/bytes bars
rm -rf w3dst; rm -f w3cap w3lines
COLUMNS=100 script -q w3cap $V -R tree w3dst >/dev/null 2>&1
tr '\r' '\n' < w3cap | sed -E "s/${ESC}\[[0-9]*[A-Za-z]//g" > w3lines
for lbl in count bytes; do
  grep "^$lbl: |" w3lines > w3one
  [ -s w3one ] && ok || bad "w3-$lbl-frames"
  awk 'length($0)!=99{exit 1}' w3one && ok || bad "w3-$lbl-linelen"
  awk -v c=/ 'BEGIN{p=-1} {i=index($0,c); if(p==-1)p=i; if(i!=p)exit 1}' w3one \
    && ok || bad "w3-$lbl-slash-pinned"
  awk -v c=% 'BEGIN{p=-1} {i=index($0,c); if(p==-1)p=i; if(i!=p)exit 1}' w3one \
    && ok || bad "w3-$lbl-pct-pinned"
done
grep -q '^file:  |' w3lines && ok || bad w3-file-frames
grep "^bytes: |" w3lines | head -1 | grep -q '   0\.00B/' \
  && ok || bad w3-bytes-initial-0.00B
# the fill never overwrites the (...) text: no = or > between ( and )
awk -F'[()]' 'NF>=2 && ($2 ~ /=/ || $2 ~ />/){exit 1}' w3lines \
  && ok || bad w3-text-untouched

### 36. below-minimum COLUMNS ignored + narrow tty clamped -> 80-col floor
rm -f wfcap wflines wbig2.out
COLUMNS=50 script -q wfcap sh -c \
  "stty columns 50 >/dev/null 2>&1; exec $V tree/sub1/sub2/big wbig2.out" \
  >/dev/null 2>&1
tr '\r' '\n' < wfcap | sed -E "s/${ESC}\[[0-9]*[A-Za-z]//g" | grep '^|' > wflines
awk 'length($0)!=79{exit 1}' wflines && [ -s wflines ] && ok || bad wmin80-linelen

### 37. speed line: always present right under the bars, boxed, pinned
# three-bar capture from 35 (w3lines, 99 cols)
grep '^speed: |' w3lines > wsp3
[ -s wsp3 ] && ok || bad speed3-present
awk 'length($0)!=99{exit 1}' wsp3 && ok || bad speed3-linelen
grep -Evq '^speed: \| current +[0-9]+\.[0-9][0-9][0-9] [BKMGT]/s    average +[0-9]+\.[0-9][0-9][0-9] [BKMGT]/s +\|$' wsp3 \
  && bad speed3-format || ok
# every file: line is followed by a speed: line, and frames end with it
awk '/^speed: \|/{f=0; next} /^file:  \|/{if(f) exit 1; f=1} END{if(f) exit 1}' w3lines \
  && ok || bad speed3-under-bars
# blank digits and units: every frame must then be identical (nothing moves)
[ "$(sed -E 's/[0-9]/#/g; s/ [BKMGT]\/s/ #\/s/g' wsp3 | sort -u | wc -l | tr -d ' ')" = 1 ] \
  && ok || bad speed3-pinned
tail -1 wsp3 | grep -q 'average    0.000 B/s' && bad speed3-final-avg || ok
# single-bar capture from 34 (wlines, 89 cols)
grep '^| speed:' wlines > wsp1
[ -s wsp1 ] && ok || bad speed1-present
awk 'length($0)!=89{exit 1}' wsp1 && ok || bad speed1-linelen
grep -Evq '^\| speed: current +[0-9]+\.[0-9][0-9][0-9] [BKMGT]/s    average +[0-9]+\.[0-9][0-9][0-9] [BKMGT]/s +\|$' wsp1 \
  && bad speed1-format || ok
awk '/^\| speed:/{f=0; next} /^\|/{if(f) exit 1; f=1} END{if(f) exit 1}' wlines \
  && ok || bad speed1-under-bar
[ "$(sed -E 's/[0-9]/#/g; s/ [BKMGT]\/s/ #\/s/g' wsp1 | sort -u | wc -l | tr -d ' ')" = 1 ] \
  && ok || bad speed1-pinned
tail -1 wsp1 | grep -q 'average    0.000 B/s' && bad speed1-final-avg || ok

echo
echo "passed=$pass failed=$fail"
[ $fail -eq 0 ]
