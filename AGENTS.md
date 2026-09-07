# Project workflow

- After changing Postbird, run the relevant checks, then always build and install
  the latest version with `./scripts/install.sh` before finishing the task.
  The user has explicitly requested this as the default; do not ask again for
  routine build/install confirmation. If installation is blocked, report it
  clearly rather than presenting a source-only change as installed.
