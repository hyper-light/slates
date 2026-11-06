#!/bin/sh
# DIAG (bug-sweep): why `slates exec`'s in-process user-namespace setup is refused on ubuntu-latest
# (docs/bugs/2026-10-20-the-launcher-writes-its-own-userns-maps.md). Each probe prints its own outcome.
set -u
echo "== kernel"; uname -a
for k in kernel.apparmor_restrict_unprivileged_userns kernel.unprivileged_userns_clone user.max_user_namespaces; do
  printf '%s = ' "$k"; sysctl -n "$k" 2>&1
done
echo "== id"; id
work=$(mktemp -d "$HOME/userns-probe.XXXXXX"); mkdir -p "$work/src" "$work/dst"; echo marker > "$work/src/m"
case "$(uname -m)" in x86_64) unshare_nr=272; mount_nr=165 ;; aarch64) unshare_nr=97; mount_nr=40 ;; *) unshare_nr=0; mount_nr=0 ;; esac
echo "== A: in-process unshare(NEWUSER|NEWNS), self-written maps, then private + bind (the launcher's sequence)"
perl -e '
  use strict; my ($src,$dst,$un,$mn)=@ARGV; my $uid=$<; my $gid=(split " ",$()[0];
  syscall($un, 0x10000000|0x00020000)==0 or do { print "unshare: $!\n"; exit 1 };
  print "unshare ok\n";
  for my $p (["/proc/self/setgroups","deny"],["/proc/self/gid_map","$gid $gid 1"],["/proc/self/uid_map","$uid $uid 1"]) {
    if (open(my $f,">",$p->[0])) { print $f $p->[1]; if (close($f)) { print "ok $p->[0]\n" } else { print "write $p->[0]: $!\n" } }
    else { print "open $p->[0]: $!\n" }
  }
  open(my $s,"<","/proc/self/status"); while (my $l = <$s>) { print $l if $l =~ /^(Cap(Eff|Prm)|Uid|NoNewPrivs)/ }
  my ($none,$root)=("none","/"); syscall($mn, $none, $root, 0, 0x4000|0x40000, 0)==0 ? print "make-rprivate ok\n" : print "make-rprivate: $!\n";
  syscall($mn, $src, $dst, 0, 0x1000, 0)==0 ? print "bind ok\n" : print "bind: $!\n";
  my $m; if (open($m,"<","$dst/m")) { print "read through bind: ", scalar(<$m>) } else { print "read through bind: $!\n" }
' "$work/src" "$work/dst" "$unshare_nr" "$mount_nr"
echo "== B: unshare(1) with the parent writing the maps (--map-user), then private + bind"
unshare -U -m --map-user="$(id -u)" --map-group="$(id -g)" sh -c "grep -E '^Cap(Eff|Prm)' /proc/self/status; mount --make-rprivate / && mount --bind '$work/src' '$work/dst' && cat '$work/dst/m' && echo B_BIND_OK" 2>&1
echo "== C: unshare -Urm (root-mapped, parent writes), then bind"
unshare -U -r -m sh -c "mount --make-rprivate / && mount --bind '$work/src' '$work/dst' && cat '$work/dst/m' && echo C_BIND_OK" 2>&1
echo "== D: the skip gate's own probe (unshare -U sh -c 'echo deny > /proc/self/setgroups')"
unshare -U sh -c 'echo deny > /proc/self/setgroups && echo D_OK' 2>&1
echo "== AppArmor"
sudo aa-status 2>/dev/null | grep -i -E 'userns|unprivileged' | head
sudo dmesg 2>/dev/null | grep -i -E 'apparmor.*(userns|unpriv)' | tail -5
rm -rf "$work"
