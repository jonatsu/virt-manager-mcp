# shellcheck shell=sh
# Starts a job in a Linux guest. Run through guest-exec as: sh -c "${launcher}" launcher ID WRAPPER
# ID is a UUID the server checked; WRAPPER is the fixed text of wrapper.sh; stdin carries the job's command, which this
# script stores as data and never evaluates. Prints one line saying how the job started or why it was refused.
# Exit status: 0 started, 3 refused, 4 failed to start.

id=$1
wrapper=$2
base=/var/lib/virtmanager-mcp
umask 077

refuse() {
  printf 'refused: %s\n' "$1"
  exit 3
}

# Each level must be a real directory owned by root with mode 700, so no other user can swap a path under it.
check_dir() {
  if [ ! -e "$1" ] && [ ! -L "$1" ]; then
    mkdir -m 700 "$1" || refuse "cannot create $1"
  fi
  [ -L "$1" ] && refuse "$1 is a symbolic link"
  [ -d "$1" ] || refuse "$1 is not a directory"
  [ "$(stat -c %u "$1")" = 0 ] || refuse "$1 is not owned by root"
  [ "$(stat -c %a "$1")" = 700 ] || refuse "$1 does not have mode 700"
}

check_dir "${base}"
check_dir "${base}/jobs"
dir=${base}/jobs/${id}
mkdir -m 700 "${dir}" || refuse "cannot create ${dir}"
# From here on, a job that does not start leaves no directory behind, so it never lists as starting.
discard() {
  rm -rf -- "${dir:?}"
}
cat > "${dir}/cmd" || {
  discard
  refuse "cannot write ${dir}/cmd"
}
# The wrapper runs from a file: systemd expands $VAR and %-specifiers in a unit's command line, so script text
# passed as an argument would be mangled. The paths below hold neither.
printf '%s\n' "${wrapper}" > "${dir}/run" || {
  discard
  refuse "cannot write ${dir}/run"
}

if [ -d /run/systemd/system ] && command -v systemd-run > /dev/null 2>&1; then
  if systemd-run --quiet --collect --unit="virtmanager-mcp-job-${id}" \
    /bin/sh "${dir}/run" "${dir}" < /dev/null > /dev/null 2>&1; then
    echo "started with systemd-run"
    exit 0
  fi
fi
if command -v setsid > /dev/null 2>&1; then
  setsid /bin/sh "${dir}/run" "${dir}" < /dev/null > /dev/null 2>&1 &
  echo "started with setsid"
  exit 0
fi
discard
echo "failed: neither systemd-run nor setsid could start the job"
exit 4
