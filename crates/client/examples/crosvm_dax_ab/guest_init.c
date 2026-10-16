/* Static init for the crosvm virtio-fs DAX A/B (sibling main.rs). Writes a 32 KiB block-backed file (a byte
   ramp), then reads it back: under DAX (cmdline slates_dax=1, mount -o dax=always) via mmap (which forces
   FUSE_SETUPMAPPING through the DAX window), else via read(). Verifies the bytes. Markers printed to the serial
   console: GUEST-BOOTED, GUEST-DAX-MODE/PLAIN, GUEST-MOUNTED, GUEST-WROTE, GUEST-MMAP-OK/GUEST-READ-OK (or
   *-MISMATCH / *-FAILED), SLATES-GUEST-OK/-FAIL. Statically linked: no loader surface in the bare initramfs. */
#include <fcntl.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>

/* 32 KiB: above inline (~128 B) so it is block-backed, within one chunk (64 KiB on a 4 KiB page) so it is a
   single unsealed block (Body::Open) the DAX gate maps; freshly written and the fd kept open, so it never seals
   before the map. */
#define SIZE (32 * 1024)
static unsigned char buf[SIZE];
static unsigned char rb[SIZE];

static void say(const char *s) { write(1, s, strlen(s)); }

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

  for (int i = 0; i < SIZE; i++) buf[i] = (unsigned char)(i & 0xff);

  int fd = open("/mnt/big.bin", O_CREAT | O_RDWR | O_TRUNC, 0644);
  int wok = fd >= 0;
  int off = 0;
  while (fd >= 0 && off < SIZE) {
    int n = (int)write(fd, buf + off, (size_t)(SIZE - off));
    if (n <= 0) { wok = 0; break; }
    off += n;
  }
  say(wok ? "GUEST-WROTE\n" : "GUEST-WRITE-FAILED\n");

  int ok = 0;
  if (fd >= 0 && dax) {
    unsigned char *m = mmap(0, SIZE, PROT_READ, MAP_SHARED, fd, 0);
    if (m != MAP_FAILED) {
      ok = (memcmp(m, buf, SIZE) == 0);
      munmap(m, SIZE);
      say(ok ? "GUEST-MMAP-OK\n" : "GUEST-MMAP-MISMATCH\n");
    } else {
      say("GUEST-MMAP-FAILED\n");
    }
  } else if (fd >= 0) {
    lseek(fd, 0, SEEK_SET);
    int ro = 0, rok = 1;
    while (ro < SIZE) {
      int n = (int)read(fd, rb + ro, (size_t)(SIZE - ro));
      if (n <= 0) { rok = 0; break; }
      ro += n;
    }
    ok = rok && (memcmp(rb, buf, SIZE) == 0);
    say(ok ? "GUEST-READ-OK\n" : "GUEST-READ-MISMATCH\n");
  }
  if (fd >= 0) close(fd);

  say(ok ? "SLATES-GUEST-OK\n" : "SLATES-GUEST-FAIL\n");
  sleep(1);
  sync();
  reboot(RB_POWER_OFF);
  for (;;) pause();
  return 0;
}
