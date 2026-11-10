# shellcheck shell=sh
# Lists the jobs in a Linux guest, one per line: ID STARTED STATE [EXIT_CODE], where STARTED is the command file's
# modification time in seconds since the epoch and STATE is exited, starting, running or ended.

cd /var/lib/virtmanager-mcp/jobs 2> /dev/null || exit 0
boot=$(cat /proc/sys/kernel/random/boot_id)
for d in */; do
  id=${d%/}
  [ -f "${id}/cmd" ] || continue
  started=$(stat -c %Y "${id}/cmd")
  if [ -f "${id}/exit" ]; then
    printf '%s %s exited %s\n' "${id}" "${started}" "$(cat "${id}/exit")"
  elif ! read -r pid start pidboot < "${id}/pid" 2> /dev/null; then
    printf '%s %s starting\n' "${id}" "${started}"
  elif [ "${pidboot}" = "${boot}" ] \
    && [ "$(sed 's/.*) //' "/proc/${pid}/stat" 2> /dev/null | cut -d ' ' -f 20)" = "${start}" ]; then
    printf '%s %s running\n' "${id}" "${started}"
  elif [ -f "${id}/exit" ]; then
    # The job ended between the first check and now.
    printf '%s %s exited %s\n' "${id}" "${started}" "$(cat "${id}/exit")"
  else
    printf '%s %s ended\n' "${id}" "${started}"
  fi
done
