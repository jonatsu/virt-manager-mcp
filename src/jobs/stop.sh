# shellcheck shell=sh
# Signals a job's process group: sh -c "${stop}" stop DIR SIGNAL, where SIGNAL is TERM or KILL. Signals only when the
# recorded PID still has the recorded start time, so a reused PID is never hit.
# Exit status: 0 signalled, 3 not started, 4 not running, 5 the signal failed.

dir=$1
sig=$2
read -r pid start < "${dir}/pid" 2> /dev/null || {
  echo "not started"
  exit 3
}
now=$(sed 's/.*) //' "/proc/${pid}/stat" 2> /dev/null | cut -d ' ' -f 20)
if [ -z "${now}" ] || [ "${now}" != "${start}" ]; then
  echo "not running"
  exit 4
fi
kill -s "${sig}" -- "-${pid}" || {
  echo "signal failed"
  exit 5
}
echo "signalled"
