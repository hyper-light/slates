/* Static init for the crosvm virtio-fs DAX A/B (sibling main.rs).

   LEARNED-VALUE MEASUREMENT: writes a file four times the size of the guest's DAX cache, then mmaps it and reads
   every byte TWICE. The first pass maps more than the cache holds, so the guest kernel reclaims mapping ranges as
   it advances (each reclaim is a FUSE_REMOVEMAPPING the daemon sees as a revoke); the second pass must therefore
   ask for those ranges again (a FUSE_SETUPMAPPING the daemon sees as a re-map). The interval between the two is the
   distribution the DAX backing's learned re-seal hold is derived from — produced by the real guest kernel's reclaim
   policy, not by a pattern invented in a bench.

   Markers on the serial console: GUEST-BOOTED, GUEST-DAX-MODE/PLAIN, GUEST-MOUNTED, GUEST-WROTE, GUEST-PASS1,
   GUEST-PASS2, GUEST-MMAP-OK/-MISMATCH/-FAILED, SLATES-GUEST-OK/-FAIL. Statically linked: no loader surface in the
   bare initramfs. */
#include <fcntl.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>

/* 32 MiB: four times the 8 MiB DAX cache the harness advertises, so the kernel must reclaim and re-map. */
#define SIZE (32 * 1024 * 1024)
/* The pattern written, and the unit writes are issued in (no multi-MiB guest buffers: the VM has 512 MB). */
#define UNIT (64 * 1024)
static unsigned char unit[UNIT];

static void say(const char *s) { write(1, s, strlen(s)); }

static void say_ulong(const char *label, unsigned long v) {
  char b[32];
  int n = 0;
  if (v == 0) { b[n++] = '0'; }
  while (v > 0 && n < 20) { b[n++] = (char)('0' + (v % 10)); v /= 10; }
  say(label);
  while (n > 0) { char ch = b[--n]; write(1, &ch, 1); }
  say("\n");
}

/* Sums every byte of the mapping, touching each page so the kernel must have the range mapped. */
static unsigned long scan(const unsigned char *m, unsigned long len) {
  unsigned long sum = 0;
  for (unsigned long i = 0; i < len; i++) { sum += m[i]; }
  return sum;
}

int main(void) {
  mount("dev", "/dev", "devtmpfs", 0, 0);
  int c = open("/dev/console", O_RDWR);
  if (c >= 0) { dup2(c, 0); dup2(c, 1); dup2(c, 2); }
  mount("proc", "/proc", "proc", 0, 0);
  mount("sys", "/sys", "sysfs", 0, 0);
  say("GUEST-BOOTED\n");

  int dax = 0;
  int cf = open("/proc/cmdline", O_RDONLY);
  if (cf >= 0) {
    char cb[1024];
    int cn = read(cf, cb, (int)sizeof(cb) - 1);
    if (cn > 0) { cb[cn] = 0; if (strstr(cb, "slates_dax=1")) dax = 1; }
    close(cf);
  }
  say(dax ? "GUEST-DAX-MODE\n" : "GUEST-PLAIN-MODE\n");

  const char *opts = dax ? "dax=always" : 0;
  say(mount("slates", "/mnt", "virtiofs", 0, opts) == 0 ? "GUEST-MOUNTED\n" : "GUEST-MOUNT-FAILED\n");

  for (int i = 0; i < UNIT; i++) { unit[i] = (unsigned char)(i & 0xff); }

  int fd = open("/mnt/big.bin", O_CREAT | O_RDWR | O_TRUNC, 0644);
  int wok = fd >= 0;
  long off = 0;
  while (fd >= 0 && off < SIZE) {
    long n = (long)write(fd, unit, UNIT);
    if (n <= 0) { wok = 0; break; }
    off += n;
  }
  say(wok ? "GUEST-WROTE\n" : "GUEST-WRITE-FAILED\n");

  int ok = 0;
  if (fd >= 0 && wok) {
    unsigned char *m = mmap(0, SIZE, PROT_READ, MAP_SHARED, fd, 0);
    if (m != MAP_FAILED) {
      /* Pass 1: maps more than the cache holds, so the kernel reclaims ranges as it advances. */
      unsigned long first = scan(m, SIZE);
      say_ulong("GUEST-PASS1 sum=", first);
      /* Pass 2: the ranges reclaimed during pass 1 must be asked for again — the re-maps we are measuring. */
      unsigned long second = scan(m, SIZE);
      say_ulong("GUEST-PASS2 sum=", second);
      ok = (first == second) && (first != 0);
      munmap(m, SIZE);
      say(ok ? "GUEST-MMAP-OK\n" : "GUEST-MMAP-MISMATCH\n");
    } else {
      say("GUEST-MMAP-FAILED\n");
    }
  }
  if (fd >= 0) close(fd);

  say(ok ? "SLATES-GUEST-OK\n" : "SLATES-GUEST-FAIL\n");
  sleep(1);
  sync();
  reboot(RB_POWER_OFF);
  for (;;) pause();
  return 0;
}
