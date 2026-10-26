# Source this to point lsync at the throwaway dev CouchDB (never the real one).
# Credentials stay in ~/.config/livesync-android/dev.env, outside the repo.
set -a
. "${LSYNC_DEV_ENV:-$HOME/.config/livesync-android/dev.env}"
set +a
case "$LSYNC_DEV_URL" in
  *:8410*) echo "refusing: dev.env points at port 8410 (the real CouchDB)" >&2; return 1 ;;
esac
export LSYNC_URL="$LSYNC_DEV_URL" LSYNC_DB="$LSYNC_DEV_DB" LSYNC_USER="$LSYNC_DEV_USER" LSYNC_PASSWORD="$LSYNC_DEV_PASSWORD"
export LSYNC_IGNORE_REGEX='\/\.git\/'
