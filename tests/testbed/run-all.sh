#!/usr/bin/env bash
# Runs the test-bed cases with mcp_call.py --check, in an order that keeps each case's preconditions, and prints a
# summary. Usage: tests/testbed/run-all.sh [case ...]; a case name may carry .json. With no arguments it runs every
# case except the manual root-only check. Exits 0 when every case passed or the test bed is absent, 1 when a case
# failed, 2 on bad arguments. It touches only trial-deb13, t-owrt, and the t-* VMs and t-*/tpl-scratch volumes it
# creates in pool agent-vms on qemu:///system. The OCR cases run only when VIRTMANAGER_OCR_MODELS names the directory
# scripts/fetch-ocr-models.sh fills; they define t-inst and t-win from the XML files here. The Windows cases run on
# t-win11 when it exists, which tests/testbed/win11/build.sh makes, and leave it shut off.
set -uo pipefail

readonly URI=qemu:///system
readonly POOL=agent-vms
readonly TEMPLATE=tpl-debian13.qcow2
REPO=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd) || exit 1
readonly REPO
readonly TESTBED=${REPO}/tests/testbed
readonly BIN=${REPO}/target/release/virtmanager-mcp
readonly OCR_MODELS=${VIRTMANAGER_OCR_MODELS:-}

# Units run in this order; each lists the case files it runs, in order.
readonly UNITS=(
  smoke guest-agent-and-snapshots lifecycle-guardrails write-file-mode console-reopen console-stall
  start-vm-waits-for-agent console-closed-on-power-off console-after-start screenshot-region screen-settled
  screen-no-models templates-create templates-reset templates-reset-uefi templates-expiry templates-expiry-command
  templates-outside-disk manage-lock console-generation job-wait jobs wait-for owrt-boot owrt-wait owrt-pacing mouse-refusals
  screen-installer screen-windows win-exec win-screen
)
# Memory the host must have available before an OCR case starts its VM, in MiB: the VM's own plus 1 GiB.
declare -A SCREEN_VM_MIB=(["t-inst"]=2048 ["t-win"]=5120)
declare -A MEMBERS=(
  ["templates-reset"]="templates-reset templates-reset-missing"
  ["templates-reset-uefi"]="templates-reset-uefi templates-reset-saved"
  ["templates-outside-disk"]="templates-outside-disk-setup templates-outside-disk"
  ["jobs"]="jobs jobs-reopen"
  ["win-exec"]="win-exec win-jobs win-jobs-reopen win-jobs-refusals win-wait"
)
# Memory the host must have available before t-win11 starts, in MiB: its 4 GiB plus 1 GiB.
readonly WIN_VM_MIB=5120

declare -A RESULT=()
ORDER=()
WORK=
PRE_VMS=
PRE_VOLS=

vsh() { virsh -q -c "${URI}" "$@"; }

skip_all() {
  echo "skipped: $1"
  exit 0
}

unit_of() {
  local name=${1%.json} unit
  for unit in "${UNITS[@]}"; do
    [[ ${unit} == "${name}" ]] && {
      echo "${unit}"
      return 0
    }
    [[ " ${MEMBERS[${unit}]:-} " == *" ${name} "* ]] && {
      echo "${unit}"
      return 0
    }
  done
  return 1
}

record() { # case status [reason]
  [[ -v RESULT[$1] ]] || ORDER+=("$1")
  # A failure stays a failure when a later step of the same case passes.
  [[ ${RESULT[$1]:-} == failed* ]] && return 0
  RESULT[$1]=$2${3:+ ($3)}
  [[ $2 == failed ]] && echo "FAILED: $1${3:+: $3}"
  return 0
}

# Runs one case file under --check, logging to $WORK/logs/<case>.{out,err}. A relative config is in tests/testbed.
run_case() { # case config
  local name=$1 config=$2
  [[ ${config} == /* ]] || config=${TESTBED}/${config}
  echo "== ${name}"
  if python3 -I "${TESTBED}/mcp_call.py" --check "${TESTBED}/cases/${name}.json" -- \
    "${BIN}" --config "${config}" > "${WORK}/logs/${name}.out" 2> "${WORK}/logs/${name}.err"; then
    record "${name}" passed
  else
    record "${name}" failed "$(grep -c '^CHECK FAILED: call' "${WORK}/logs/${name}.out") failed check(s), see ${WORK}/logs/${name}.out"
  fi
}

# Runs an unchecked helper call list, for set-up steps the server does best.
run_calls() { # config json
  local file=${WORK}/helper.json
  printf '%s\n' "$2" > "${file}"
  python3 -I "${TESTBED}/mcp_call.py" "${file}" -- "${BIN}" --config "${TESTBED}/$1" >> "${WORK}/logs/helpers.out" 2>&1
}

wait_until() { # timeout_secs command...
  local deadline=$((SECONDS + $1))
  shift
  until "$@"; do
    ((SECONDS < deadline)) || return 1
    sleep 2
  done
}

is_state() { [[ $(vsh domstate "$1" 2> /dev/null) == "$2" ]]; }
agent_answers() { vsh qemu-agent-command "$1" '{"execute":"guest-ping"}' > /dev/null 2>&1; }

# Whether systemd in VM $1 has finished booting. The guest agent answers before it has, while the getty and the
# shutdown path a case relies on may still be starting.
boot_finished() {
  local pid state
  pid=$(vsh qemu-agent-command "$1" \
    '{"execute":"guest-exec","arguments":{"path":"/bin/systemctl","arg":["is-system-running"],"capture-output":true}}' \
    2> /dev/null | jq -r '.return.pid') || return 1
  [[ ${pid} =~ ^[0-9]+$ ]] || return 1
  sleep 1
  state=$(vsh qemu-agent-command "$1" "{\"execute\":\"guest-exec-status\",\"arguments\":{\"pid\":${pid}}}" \
    2> /dev/null | jq -r '.return["out-data"] // ""' | base64 -d 2> /dev/null)
  [[ ${state} == running* || ${state} == degraded* ]]
}

trial_ready() {
  if ! is_state trial-deb13 running; then
    vsh start trial-deb13 > /dev/null || return 1
  fi
  wait_until 180 agent_answers trial-deb13 && wait_until 180 boot_finished trial-deb13
}

trial_shut_off() {
  is_state trial-deb13 "shut off" && return 0
  vsh shutdown trial-deb13 > /dev/null || return 1
  wait_until 120 is_state trial-deb13 "shut off"
}

owrt_shut_off() {
  is_state t-owrt running && vsh destroy t-owrt > /dev/null
  wait_until 30 is_state t-owrt "shut off"
}

# t-listed: an unowned VM, defined here and never started.
ensure_listed() {
  vsh dominfo t-listed > /dev/null 2>&1 && return 0
  printf '%s\n' "<domain type='kvm'><name>t-listed</name><memory unit='MiB'>256</memory><vcpu>1</vcpu>" \
    "<os><type arch='x86_64'>hvm</type></os></domain>" > "${WORK}/t-listed.xml"
  vsh define "${WORK}/t-listed.xml" > /dev/null
}

has_vol() { vsh vol-info --pool "${POOL}" "$1" > /dev/null 2>&1; }
vols_named() { vsh vol-list "${POOL}" | awk '{print $1}' | grep -E "$1"; }

# Waits until the VM a case created has expired: the case's reply says "'<vm>' ... expires at <unix time>".
wait_expired() { # case vm
  local at
  at=$(grep -o "VM '$2' .*expires at [0-9]*" "${WORK}/logs/$1.out" | grep -o '[0-9]*$' | head -n 1)
  [[ -n ${at} ]] || return 1
  while (($(date +%s) <= at + 1)); do sleep 2; done
}

unit_trial() { # case
  if trial_ready; then run_case "$1" config.toml; else record "$1" failed "trial-deb13 agent did not answer"; fi
}

unit_console_stall() {
  unit_trial console-stall
  run_calls config.toml '[{"tool":"console_open","args":{"vm":"trial-deb13","force":true}},
    {"tool":"console_write","args":{"vm":"trial-deb13","text":"exit"}},
    {"tool":"console_read","args":{"vm":"trial-deb13","wait_ms":3000}},
    {"tool":"console_close","args":{"vm":"trial-deb13"}}]' || record console-stall failed "could not log root out"
}

unit_console_after_start() {
  if trial_ready && trial_shut_off; then
    run_case console-after-start config.toml
  else
    record console-after-start failed "could not shut trial-deb13 off"
  fi
  trial_ready || record console-after-start failed "trial-deb13 agent did not answer afterwards"
}

unit_templates_reset() {
  ensure_listed || record templates-reset failed "could not define t-listed"
  has_vol tpl-scratch.qcow2 || vsh vol-clone --pool "${POOL}" "${TEMPLATE}" tpl-scratch.qcow2 > /dev/null
  run_case templates-reset config-templates.toml
  vsh vol-delete --pool "${POOL}" tpl-scratch.qcow2 > /dev/null || record templates-reset-missing failed "could not delete tpl-scratch.qcow2"
  run_case templates-reset-missing config-templates.toml
}

unit_templates_reset_uefi() {
  run_case templates-reset-uefi config-templates.toml
  if vsh managedsave t-u > /dev/null; then
    run_case templates-reset-saved config-templates.toml
  else
    record templates-reset-saved failed "virsh managedsave t-u failed"
  fi
}

unit_templates_expiry() {
  ensure_listed || record templates-expiry failed "could not define t-listed"
  run_case templates-expiry config-templates.toml
  if vols_named '^t-exp-' > /dev/null; then record templates-expiry failed "t-exp volumes remain"; fi
}

unit_templates_expiry_command() {
  local status
  run_case templates-expiry-command config-templates.toml
  wait_expired templates-expiry-command t-exp2 || {
    record templates-expiry-command failed "no expiry time"
    return
  }
  "${BIN}" --config "${TESTBED}/config-templates.toml" sweep > "${WORK}/logs/sweep.log" 2>&1
  status=$?
  ((status == 0)) || record templates-expiry-command failed "sweep exited ${status}, see ${WORK}/logs/sweep.log"
  if vsh dominfo t-exp2 > /dev/null 2>&1; then record templates-expiry-command failed "sweep kept t-exp2"; fi
  if vols_named '^t-exp2-' > /dev/null; then record templates-expiry-command failed "t-exp2 volumes remain"; fi
}

unit_templates_outside_disk() {
  local dir=${WORK}/outside file vm before owner
  file=${dir}/outside.img
  run_case templates-outside-disk-setup config-templates.toml
  if ! mkdir -p "${dir}" || ! truncate -s 1M "${file}"; then
    record templates-outside-disk failed "could not make ${file}"
    return
  fi
  before=$(sha256sum < "${file}")
  owner=$(stat -c %U "${file}")
  for vm in t-or t-oe; do
    vsh attach-disk "${vm}" "${file}" vdd --config --targetbus virtio --driver qemu --subdriver raw --mode readonly \
      > /dev/null || record templates-outside-disk failed "could not attach ${file} to ${vm}"
  done
  wait_expired templates-outside-disk-setup t-oe || record templates-outside-disk failed "no expiry time for t-oe"
  run_case templates-outside-disk config-templates.toml
  grep -q "expired VM 't-oe' .*outside.img is not a volume in manage.disk_pool" "${WORK}/logs/templates-outside-disk.err" \
    || record templates-outside-disk failed "the sweep did not log the outside disk as not deleted"
  [[ -f ${file} && $(sha256sum < "${file}") == "${before}" && $(stat -c %U "${file}") == "${owner}" ]] \
    || record templates-outside-disk failed "${file} changed"
  rm -f -- "${WORK:?}/outside/outside.img" && rmdir -- "${WORK:?}/outside"
}

# Another process, flock(1), holds the management lock for 40 s while the case runs. The server gets its own HOME, so
# its lock file is ${WORK}/home/.cache/virtmanager-mcp/manage.lock and servers of live sessions are not held up. The
# server's first create waits 30 s and fails; its second waits until the holder exits and succeeds.
unit_manage_lock() {
  local home=${WORK}/home holder
  if ! command -v flock > /dev/null; then
    record manage-lock skipped "flock is not installed"
    return
  fi
  if ! install -d -m 700 "${home}" "${home}/.cache" "${home}/.cache/virtmanager-mcp"; then
    record manage-lock failed "could not make ${home}"
    return
  fi
  flock -x "${home}/.cache/virtmanager-mcp/manage.lock" sleep 40 &
  holder=$!
  sleep 1
  HOME=${home} run_case manage-lock config-templates.toml
  wait "${holder}" || record manage-lock failed "the flock holder failed"
}

unit_jobs() {
  rm -f -- "${WORK:?}/vars.json"
  run_case jobs config-templates.toml
  run_case jobs-reopen config-templates.toml
}

unit_owrt() { # case
  if ! vsh dominfo t-owrt > /dev/null 2>&1; then
    record "$1" skipped "no t-owrt"
  elif owrt_shut_off; then
    run_case "$1" config-templates.toml
  else
    record "$1" failed "could not shut t-owrt off"
  fi
}

# Writes $WORK/config-screen.toml: config-templates.toml with t-inst, t-win and t-win11 allowed and powered, and the
# OCR models.
write_screen_config() {
  local file=${WORK}/config-screen.toml dir=${OCR_MODELS//\\/\\\\} vms='"t-inst", "t-win", "t-win11", '
  dir=${dir//\"/\\\"}
  sed -e "s/^allowed_vms = \[/allowed_vms = [${vms}/" -e "s/^power_vms = \[/power_vms = [${vms}/" \
    "${TESTBED}/config-templates.toml" > "${file}" || return 1
  [[ $(grep -cF -e "allowed_vms = [${vms}" -e "power_vms = [${vms}" "${file}") == 2 ]] || return 1
  printf '\n[display]\nocr_models = "%s"\n' "${dir}" >> "${file}"
}

# Writes $WORK/config-windows.toml: config-templates.toml with t-win11 allowed and powered.
write_windows_config() {
  local file=${WORK}/config-windows.toml
  sed -e 's/^allowed_vms = \[/allowed_vms = ["t-win11", /' -e 's/^power_vms = \[/power_vms = ["t-win11", /' \
    "${TESTBED}/config-templates.toml" > "${file}" || return 1
  [[ $(grep -cF -e 'allowed_vms = ["t-win11", ' -e 'power_vms = ["t-win11", ' "${file}") == 2 ]]
}

# Prints why the Windows cases cannot run, or nothing when they can. A running t-win11 already holds its memory.
win_blocker() {
  local avail
  if ! vsh dominfo t-win11 > /dev/null 2>&1; then
    echo "no t-win11; tests/testbed/win11/build.sh makes it"
    return
  fi
  is_state t-win11 running && return
  avail=$(avail_mib)
  ((avail >= WIN_VM_MIB)) || echo "t-win11 needs ${WIN_VM_MIB} MiB of available memory; the host has ${avail} MiB"
}

win_ready() {
  is_state t-win11 running || vsh start t-win11 > /dev/null || return 1
  wait_until 600 agent_answers t-win11
}

# Shuts t-win11 down through its agent, which Windows obeys while a user is logged on, unlike the ACPI button; powers
# it off if that fails.
win_shut_off() {
  is_state t-win11 "shut off" && return 0
  vsh shutdown t-win11 --mode agent > /dev/null 2>&1 || vsh shutdown t-win11 > /dev/null 2>&1
  wait_until 180 is_state t-win11 "shut off" && return 0
  vsh destroy t-win11 > /dev/null && wait_until 30 is_state t-win11 "shut off"
}

# win-exec starts t-win11 and shuts it down; the job and wait cases then run on a fresh start, sharing vars.json.
unit_windows() {
  local cases=(win-exec win-jobs win-jobs-reopen win-jobs-refusals win-wait) config=${WORK}/config-windows.toml
  local reason case
  reason=$(win_blocker)
  if [[ -n ${reason} ]]; then
    for case in "${cases[@]}"; do record "${case}" skipped "${reason}"; done
    return
  fi
  if ! write_windows_config; then
    reason="could not write ${config}"
  elif ! win_shut_off; then
    reason="could not shut t-win11 off"
  fi
  if [[ -n ${reason} ]]; then
    for case in "${cases[@]}"; do record "${case}" failed "${reason}"; done
    return
  fi
  run_case win-exec "${config}"
  rm -f -- "${WORK:?}/vars.json"
  if win_ready; then
    for case in "${cases[@]:1}"; do run_case "${case}" "${config}"; done
  else
    for case in "${cases[@]:1}"; do record "${case}" failed "the t-win11 agent did not answer"; done
  fi
  win_shut_off || record win-wait failed "could not shut t-win11 off"
}

unit_win_screen() {
  local reason
  if [[ -z ${OCR_MODELS} ]]; then
    record win-screen skipped "VIRTMANAGER_OCR_MODELS is not set, so the server has no OCR models"
    return
  fi
  reason=$(win_blocker)
  if [[ -n ${reason} ]]; then
    record win-screen skipped "${reason}"
    return
  fi
  if [[ ! -f ${WORK}/config-screen.toml ]]; then
    record win-screen failed "could not write ${WORK}/config-screen.toml"
  elif ! win_ready; then
    record win-screen failed "the t-win11 agent did not answer"
  else
    run_case win-screen "${WORK}/config-screen.toml"
  fi
  win_shut_off || record win-screen failed "could not shut t-win11 off"
}

avail_mib() { awk '/^MemAvailable:/ { print int($2 / 1024) }' /proc/meminfo; }

# Defines VM $1 from its XML here unless it exists, with t-inst's empty 8 GiB disk, and leaves it shut off.
screen_vm_ready() {
  if [[ $1 == t-inst ]] && ! has_vol t-inst.qcow2; then
    vsh vol-create-as "${POOL}" t-inst.qcow2 8G --format qcow2 > /dev/null || return 1
  fi
  vsh dominfo "$1" > /dev/null 2>&1 || vsh define "${TESTBED}/$1.xml" > /dev/null || return 1
  is_state "$1" running && vsh destroy "$1" > /dev/null
  wait_until 30 is_state "$1" "shut off"
}

unit_screen() { # case vm
  local need=${SCREEN_VM_MIB[$2]} avail
  if [[ -z ${OCR_MODELS} ]]; then
    record "$1" skipped "VIRTMANAGER_OCR_MODELS is not set, so the server has no OCR models"
    return
  fi
  [[ -f ${WORK}/config-screen.toml ]] || {
    record "$1" failed "could not write ${WORK}/config-screen.toml"
    return
  }
  screen_vm_ready "$2" || {
    record "$1" failed "could not define $2 or shut it off"
    return
  }
  avail=$(avail_mib)
  ((avail >= need)) || {
    record "$1" failed "$2 needs ${need} MiB of available memory; the host has ${avail} MiB"
    return
  }
  run_case "$1" "${WORK}/config-screen.toml"
  is_state "$2" "shut off" || vsh destroy "$2" > /dev/null || record "$1" failed "could not shut $2 off"
}

run_unit() {
  case $1 in
    console-stall) unit_console_stall ;;
    console-after-start) unit_console_after_start ;;
    templates-reset) unit_templates_reset ;;
    templates-reset-uefi) unit_templates_reset_uefi ;;
    templates-expiry) unit_templates_expiry ;;
    templates-expiry-command) unit_templates_expiry_command ;;
    templates-outside-disk) unit_templates_outside_disk ;;
    jobs) unit_jobs ;;
    manage-lock) unit_manage_lock ;;
    owrt-* | mouse-refusals) unit_owrt "$1" ;;
    screen-installer) unit_screen "$1" t-inst ;;
    screen-windows) unit_screen "$1" t-win ;;
    win-exec) unit_windows ;;
    win-screen) unit_win_screen ;;
    smoke | guest-agent-and-snapshots | lifecycle-guardrails | write-file-mode | console-reopen | \
      start-vm-waits-for-agent | console-closed-on-power-off | screenshot-region | screen-settled | \
      screen-no-models) unit_trial "$1" ;;
    *) run_case "$1" config-templates.toml ;;
  esac
}

# Shuts t-win11 off, and removes the t-* VMs and volumes this run created and tpl-scratch.qcow2; never t-owrt or
# anything listed before.
cleanup() {
  local vm vol
  [[ -n ${WORK} ]] || return 0
  # An interrupted Windows case leaves t-win11 running, and it holds 4 GiB.
  if is_state t-win11 running; then
    win_shut_off > /dev/null 2>&1 || echo "cleanup: could not shut t-win11 off"
  fi
  while read -r vm; do
    [[ ${vm} == t-* && ${vm} != t-owrt ]] || continue
    grep -qxF -- "${vm}" <<< "${PRE_VMS}" && continue
    is_state "${vm}" "shut off" || vsh destroy "${vm}" > /dev/null 2>&1
    vsh undefine "${vm}" --nvram --managed-save --snapshots-metadata > /dev/null 2>&1 \
      || vsh undefine "${vm}" --managed-save --snapshots-metadata > /dev/null || echo "cleanup: could not undefine ${vm}"
  done < <(vsh list --all --name)
  while read -r vol; do
    [[ (${vol} == t-* && ${vol} != t-owrt.img) || ${vol} == tpl-scratch.qcow2 ]] || continue
    grep -qxF -- "${vol}" <<< "${PRE_VOLS}" && continue
    vsh vol-delete --pool "${POOL}" "${vol}" > /dev/null || echo "cleanup: could not delete volume ${vol}"
  done < <(vsh vol-list "${POOL}" | awk '{print $1}')
  trial_ready > /dev/null 2>&1 || echo "cleanup: trial-deb13 agent does not answer"
}

main() {
  local arg unit units=() selected=() start=${SECONDS} name status
  for arg in "$@"; do
    unit=$(unit_of "${arg}") || {
      echo "unknown case: ${arg}" >&2
      exit 2
    }
    [[ " ${selected[*]} " == *" ${unit} "* ]] || selected+=("${unit}")
  done
  for unit in "${UNITS[@]}"; do
    (($# == 0)) || [[ " ${selected[*]} " == *" ${unit} "* ]] && units+=("${unit}")
  done

  command -v virsh > /dev/null || skip_all "virsh is not installed"
  command -v python3 > /dev/null || skip_all "python3 is not installed"
  vsh uri > /dev/null 2>&1 || skip_all "virsh cannot connect to ${URI}"
  vsh pool-info "${POOL}" > /dev/null 2>&1 || skip_all "pool ${POOL} does not exist"
  vsh dominfo trial-deb13 > /dev/null 2>&1 || skip_all "VM trial-deb13 does not exist"
  has_vol "${TEMPLATE}" || skip_all "volume ${TEMPLATE} does not exist in ${POOL}"

  if command -v cargo > /dev/null; then
    (cd -- "${REPO}" && cargo build --release --locked) || {
      echo "cargo build failed" >&2
      exit 1
    }
  fi
  [[ -x ${BIN} ]] || {
    echo "missing ${BIN}; run cargo build --release --locked" >&2
    exit 1
  }

  WORK=$(mktemp -d "${TMPDIR:-/tmp}/testbed-run.XXXXXX") || exit 1
  mkdir -p "${WORK}/logs"
  PRE_VMS=$(vsh list --all --name)
  PRE_VOLS=$(vsh vol-list "${POOL}" | awk '{print $1}')
  trap cleanup EXIT
  trap 'exit 130' INT TERM
  cd -- "${WORK}" || exit 1
  echo "logs: ${WORK}/logs"
  if [[ -n ${OCR_MODELS} ]]; then
    write_screen_config || rm -f -- "${WORK:?}/config-screen.toml"
  else
    echo "VIRTMANAGER_OCR_MODELS is not set: the OCR cases are skipped"
  fi

  for unit in "${units[@]}"; do
    run_unit "${unit}"
  done

  local passed=0 failed=0 skipped=0
  echo
  echo "Summary ($((SECONDS - start)) s):"
  for name in "${ORDER[@]}"; do
    status=${RESULT[${name}]}
    printf '  %-34s %s\n' "${name}" "${status}"
    case ${status} in
      passed*) ((passed++)) ;;
      skipped*) ((skipped++)) ;;
      *) ((failed++)) ;;
    esac
  done
  echo "${passed} passed, ${failed} failed, ${skipped} skipped"
  ((failed == 0))
}

main "$@"
