# shellcheck shell=sh
# Runs one job: sh DIR/run DIR, where DIR/run holds this text. Records "PID STARTTIME" in DIR/pid, runs DIR/cmd
# with stdout and stderr merged into DIR/out, and writes the exit code to DIR/exit. On TERM it forwards the signal to
# the command and still records the code. The trap is a handler, not an ignore, because a child inherits ignored signals.

dir=$1
cd / || exit 1
child=
forward() {
  [ -n "${child}" ] && kill -TERM "${child}" 2> /dev/null
}
trap forward TERM

# Field 22 of /proc/PID/stat is the start time; the fields after the last ')' start at field 3.
start=$(sed 's/.*) //' "/proc/$$/stat" | cut -d ' ' -f 20)
printf '%s %s\n' "$$" "${start}" > "${dir}/pid.tmp" && mv "${dir}/pid.tmp" "${dir}/pid"

/bin/sh "${dir}/cmd" < /dev/null > "${dir}/out" 2>&1 &
child=$!
while :; do
  wait "${child}"
  code=$?
  # A wait that a trapped signal interrupts returns early; wait again until the command has really ended.
  kill -0 "${child}" 2> /dev/null || break
done
printf '%s\n' "${code}" > "${dir}/exit.tmp" && mv "${dir}/exit.tmp" "${dir}/exit"
