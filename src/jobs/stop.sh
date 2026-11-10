# shellcheck shell=sh
# Signals a job's process group: sh -c "${stop}" stop DIR SIGNAL, where SIGNAL is 15 (TERM) or 9 (KILL). Signals only
# when the recorded PID still has the recorded start time in the current boot, so a reused PID is never hit.
# The numeric signal and no "--" are the one form dash, bash and busybox all accept for a process group.
# Exit status: 0 signalled, 3 not started, 4 not running, 5 the signal failed, 6 no such job.

dir=$1
sig=$2
[ -f "${dir}/cmd" ] || {
  echo "no such job"
  exit 6
}
read -r pid start boot < "${dir}/pid" 2> /dev/null || {
  echo "not started"
  exit 3
}
now=$(sed 's/.*) //' "/proc/${pid}/stat" 2> /dev/null | cut -d ' ' -f 20)
if [ -z "${now}" ] || [ "${now}" != "${start}" ] || [ "${boot}" != "$(cat /proc/sys/kernel/random/boot_id)" ]; then
  echo "not running"
  exit 4
fi
kill "-${sig}" "-${pid}" || {
  echo "signal failed"
  exit 5
}
echo "signalled"
