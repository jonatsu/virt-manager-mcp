# shellcheck shell=sh
# Runs one job: sh DIR/run DIR, where DIR/run holds this text. Runs DIR/cmd with stdout and stderr merged into
# DIR/out, records "PID STARTTIME BOOTID" in DIR/pid once the command is running, and writes the exit code to
# DIR/exit. On TERM it forwards the signal to the command and still records the code. The trap is a handler, not an
# ignore, because a child inherits ignored signals.

dir=$1
cd / || exit 1
child=
interrupted=
forward() {
  interrupted=1
  [ -n "${child}" ] && kill -TERM "${child}" 2> /dev/null
}
trap forward TERM

# A background command in a non-interactive shell starts with SIGINT and SIGQUIT ignored; reset them where env can.
set -- /bin/sh "${dir}/cmd"
if env --default-signal=INT,QUIT true 2> /dev/null; then
  set -- env --default-signal=INT,QUIT "$@"
fi
"$@" < /dev/null > "${dir}/out" 2>&1 &
child=$!

# Field 22 of /proc/PID/stat is the start time; the fields after the last ')' start at field 3. The boot ID tells a
# PID and start time from an earlier boot apart. Written after the fork, so a stop that finds it reaches the command.
start=$(sed 's/.*) //' "/proc/$$/stat" | cut -d ' ' -f 20)
boot=$(cat /proc/sys/kernel/random/boot_id)
printf '%s %s %s\n' "$$" "${start}" "${boot}" > "${dir}/pid.tmp" && mv "${dir}/pid.tmp" "${dir}/pid"

while :; do
  interrupted=
  wait "${child}"
  code=$?
  # A wait that the TERM handler interrupts returns early; wait again for the command's own status.
  [ -n "${interrupted}" ] || break
done
printf '%s\n' "${code}" > "${dir}/exit.tmp" && mv "${dir}/exit.tmp" "${dir}/exit"
