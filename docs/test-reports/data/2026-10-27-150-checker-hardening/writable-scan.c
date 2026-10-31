// Lists what a process of the container's user can write that could outlive it: mounts, every
// writable path outside /proc and /sys, and whether the kernel keyrings accept a key.
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int shown = 0;

static void walk(const char *path, int depth) {
  if (depth > 40 || shown > 400) return;
  if (!strcmp(path, "/proc") || !strcmp(path, "/sys")) return;
  struct stat st;
  if (lstat(path, &st) != 0) return;
  if (S_ISLNK(st.st_mode)) return;
  if (access(path, W_OK) == 0) {
    printf("writable %s %s mode %o uid %u\n", S_ISDIR(st.st_mode) ? "dir " : "file", path, st.st_mode & 07777, st.st_uid);
    shown++;
  }
  if (!S_ISDIR(st.st_mode)) return;
  DIR *dir = opendir(path);
  if (!dir) return;
  struct dirent *entry;
  while ((entry = readdir(dir))) {
    if (!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, "..")) continue;
    char child[4096];
    snprintf(child, sizeof child, "%s/%s", strcmp(path, "/") ? path : "", entry->d_name);
    walk(child, depth + 1);
  }
  closedir(dir);
}

int main(void) {
  printf("uid %u gid %u pid %d\n", getuid(), getgid(), getpid());
  FILE *mounts = fopen("/proc/self/mountinfo", "r");
  char line[4096];
  while (mounts && fgets(line, sizeof line, mounts)) {
    // mount point is field 5, options field 6; fs type after " - ".
    char type[64] = "?";
    char *dash = strstr(line, " - ");
    if (dash) sscanf(dash + 3, "%63s", type);
    char *fields[7] = {0};
    char *save = NULL, *tok = strtok_r(line, " ", &save);
    for (int i = 0; tok && i < 7; i++, tok = strtok_r(NULL, " ", &save)) fields[i] = tok;
    if (fields[4] && fields[5]) printf("mount %s %s %s\n", fields[4], type, fields[5]);
  }
  if (mounts) fclose(mounts);
  walk("/", 0);
  long key = syscall(SYS_add_key, "user", "tq-scan", "x", 1, -4 /* KEY_SPEC_USER_KEYRING */);
  if (key >= 0) printf("keyring: add_key to the user keyring succeeded (key %ld)\n", key);
  else printf("keyring: add_key refused: %s\n", strerror(errno));
  long session = syscall(SYS_keyctl, 0 /* KEYCTL_GET_KEYRING_ID */, -3 /* session */, 0);
  if (session >= 0) printf("keyring: keyctl succeeded (session keyring %ld)\n", session);
  else printf("keyring: keyctl refused: %s\n", strerror(errno));
  return 0;
}
