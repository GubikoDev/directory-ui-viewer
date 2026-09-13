/* Test-only investigation of a deprecated kernel sandbox interface. Never linked
 * into the application. All passed paths belong to the parent test's fixture. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <sandbox.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
int main(int argc, char **argv) {
  if (argc != 3 || strchr(argv[1], '"') || strchr(argv[1], '\\')) return 2;
  char profile[8192];
  snprintf(profile, sizeof(profile), "(version 1)(deny default)(allow file-read* (subpath \"%s\"))", argv[1]);
  char *error = NULL;
  if (sandbox_init(profile, 0, &error)) {
    sandbox_free_error(error); puts("sandbox-unavailable"); return 3;
  }
  int root = open(argv[1], O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
  if (root < 0) { puts("root-open-failed"); return 4; }
  int child = openat(root, "child", O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
  if (child < 0) { puts("child-open-failed"); return 5; }
  struct stat metadata;
  int outside = lstat(argv[2], &metadata);
  printf("ready outside_lstat=%d\n", outside); fflush(stdout);
  if (getchar() != 'g') return 6;
  errno = 0;
  int stat_result = fstatat(child, "external-marker", &metadata, AT_SYMLINK_NOFOLLOW);
  int stat_errno = errno;
  errno = 0;
  DIR *directory = fdopendir(child);
  int found = 0;
  if (directory) {
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) if (!strcmp(entry->d_name, "external-marker")) found = 1;
  }
  printf("result metadata=%d errno=%d marker=%d\n", stat_result, stat_errno, found);
  if (directory) closedir(directory); else close(child);
  close(root);
  return 0;
}
