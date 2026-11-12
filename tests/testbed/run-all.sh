#!/usr/bin/env bash
# Runs the test-bed cases with mcp_call.py --check, in an order that keeps each case's preconditions, and prints a
# summary. Usage: tests/testbed/run-all.sh [case ...]; a case name may carry .json. With no arguments it runs every
# case except the manual root-only check. Exits 0 when every case passed or the test bed is absent, 1 when a case
# failed, 2 on bad arguments. It touches only trial-deb13, t-owrt, and the t-* VMs and t-*/tpl-scratch volumes it
# creates in pool agent-vms on qemu:///system. The OCR cases run only when VIRTMANAGER_OCR_MODELS names the directory
# scripts/fetch-ocr-models.sh fills, or, without it, when the directory a server with VIRTMANAGER_OCR_DOWNLOAD=1
# fills holds both models; they define t-inst and t-win from the XML files here. The Windows cases run on
# t-win11 when it exists, which tests/testbed/win11/build.sh makes, and leave it shut off. The transfer cases share and
# pull host files only under the run's work directory.
set -uo pipefail

readonly URI=qemu:///system
readonly POOL=agent-vms
readonly TEMPLATE=tpl-debian13.qcow2
REPO=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd) || exit 1
readonly REPO
readonly TESTBED=${REPO}/tests/testbed
readonly BIN=${REPO}/target/release/virtmanager-mcp

# Prints the directory VIRTMANAGER_OCR_DOWNLOAD=1 makes the server fill, as src/ocrfetch.rs names it, when it holds
# both models.
downloaded_ocr_models() {
  local data=${XDG_DATA_HOME:-}
  [[ ${data} == /* ]] || data=${HOME:-}/.local/share
  local dir=${data}/virtmanager-mcp/ocr-models
  [[ ${dir} == /* && -f ${dir}/text-detection.onnx && -f ${dir}/text-recognition.onnx ]] || return 0
  printf '%s\n' "${dir}"
}

OCR_MODELS=${VIRTMANAGER_OCR_MODELS:-$(downloaded_ocr_models)}
readonly OCR_MODELS
readonly NO_OCR_MODELS="the server has no OCR models: set VIRTMANAGER_OCR_MODELS, or start the server once with \
VIRTMANAGER_OCR_DOWNLOAD=1"

# Units run in this order; each lists the case files it runs, in order.
readonly UNITS=(
  smoke guest-agent-and-snapshots lifecycle-guardrails write-file-mode transfer-file transfer-push transfer-pull
  transfer-background addresses console-reopen console-stall
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
  ["transfer-pull"]="transfer-pull transfer-pull-caps"
  ["transfer-background"]="transfer-background transfer-background-after"
  ["win-exec"]="win-exec win-jobs win-jobs-reopen win-jobs-refusals win-wait win-transfer win-transfer-caps"
)
# The pattern of the 128 MiB file the background-transfer case moves both ways, and its SHA-256.
readonly BIG_PATTERN='virtmanager-mcp test bed'
readonly BIG_BYTES=134217728
readonly BIG_SHA256=bfa16ddec123a5b3cfe4f858f57dfd0c1a24563a61948324e8f2a3b01d04ea24
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

# Builds $WORK/share, with a fixed tree to push, and the pull directory $WORK/pull, then writes the transfer configs:
# config-transfer.toml is config.toml with both directories, config-transfer-slow.toml lets a call wait 2 s, and
# config-transfer-caps.toml caps a pull at 64 KiB and 10 entries. The tree holds ignore rules in itself and in the
# share, ignored files, a script, a symlink inside and one leaving it, and a FIFO. Only the Windows cases push
# share/win-tree, whose names Windows cannot hold, beside ordinary ones. Does nothing the second time.
transfer_setup() {
  local share=${WORK}/share pull=${WORK}/pull tree=${WORK}/share/tree dirs name
  [[ -f ${WORK}/config-transfer.toml ]] && return 0
  mkdir -p -- "${tree}/sub" "${tree}/build" "${pull}" || return 1
  printf '*.tmp\nbuild/\n' > "${tree}/.gitignore" || return 1
  printf '*.secret\n' > "${share}/.gitignore"
  printf 'alpha\n' > "${tree}/a.txt"
  printf 'beta\n' > "${tree}/sub/b.txt"
  printf '#!/bin/sh\necho hi\n' > "${tree}/sub/run.sh"
  printf 'ignored\n' > "${tree}/skip.tmp"
  printf 'obj\n' > "${tree}/build/out.o"
  printf 'secret\n' > "${tree}/k.secret"
  printf 'outside\n' > "${share}/outside"
  printf 'windows transfer\n' > "${share}/win.txt"
  mkdir -p -- "${share}/win-tree/sub" "${share}/win-tree/aux" || return 1
  for name in CON nul.txt 'a?b' tail. Case.txt case.txt ok.txt sub/deep.txt été.txt aux/inner.txt; do
    printf '%s\n' "${name}" > "${share}/win-tree/${name}" || return 1
  done
  # Five levels of 50 characters: past Windows' 260-character limit once pushed to C:\Windows\Temp\vmm-deep.
  name=${share}/win-deep
  for _ in 1 2 3 4 5; do name+=/$(printf 'd%.0s' {1..50}); done
  mkdir -p -- "${name}" && printf 'deep\n' > "${name}/file.txt" || return 1
  printf 'x\n' > "${pull}/a-file"
  # Fixed modes, whatever the umask, so the cases can expect them.
  find "${share}" "${pull}" -type f -exec chmod 644 {} + && find "${share}" "${pull}" -type d -exec chmod 755 {} + \
    && chmod 755 "${tree}/sub/run.sh" || return 1
  ln -s a.txt "${tree}/link-in" && ln -s ../outside "${tree}/link-out" && mkfifo "${tree}/pipe" \
    && ln -s ok.txt "${share}/win-tree/link-in" || return 1
  dirs=$(printf 'host_share_dirs = ["%s"]\nhost_pull_dirs = ["%s"]' "${share}" "${pull}")
  # Top-level keys go before the first table.
  { printf '%s\n' "${dirs}" && cat "${TESTBED}/config.toml"; } > "${WORK}/config-transfer.toml.new" || return 1
  { cat "${WORK}/config-transfer.toml.new" && printf '\n[limits]\nmax_wait_secs = 2\n'; } \
    > "${WORK}/config-transfer-slow.toml" || return 1
  { cat "${WORK}/config-transfer.toml.new" && printf '\n[limits]\nmax_pull_bytes = 65536\nmax_pull_entries = 10\n'; } \
    > "${WORK}/config-transfer-caps.toml" || return 1
  mv -- "${WORK}/config-transfer.toml.new" "${WORK}/config-transfer.toml"
}

# Writes the share, pull and tree paths into vars.json, where the transfer cases read them as ${share}, ${pull} and
# ${tree}; mcp_call.py fills saved values into call arguments only.
transfer_vars() {
  printf '{"share": "%s", "pull": "%s", "tree": "%s"}\n' "${WORK}/share" "${WORK}/pull" "${WORK}/share/tree" \
    > "${WORK}/vars.json"
}

# Fails case $1 unless every pulled entry is a file with mode 644 or a directory with mode 755, and the staging
# directory is empty.
check_pull_dir() {
  local pull=${WORK}/pull odd
  odd=$(find "${pull}" -mindepth 1 -path "${pull}/.vmm-staging" -prune -o \
    \( \( -type f ! -perm 644 \) -o \( -type d ! -perm 755 \) -o \( ! -type f ! -type d \) \) -print)
  [[ -z ${odd} ]] || record "$1" failed "pulled entries with another mode or type: ${odd//$'\n'/ }"
  [[ -z $(ls -A -- "${pull}/.vmm-staging" 2> /dev/null) ]] || record "$1" failed "${pull}/.vmm-staging is not empty"
}

# Fails case $1 unless the pulled file $2, relative to the pull directory, holds exactly $3.
check_pulled() {
  cmp -s <(printf '%s' "$3") "${WORK}/pull/$2" || record "$1" failed "pull/$2 does not hold what the guest wrote"
}

# Fails case $1 when the work directory, which is on tmpfs here, holds a file over 1 MiB.
check_work_small() {
  local big
  big=$(find "${WORK}" -type f -size +1M -print)
  [[ -z ${big} ]] || record "$1" failed "large files left in ${WORK}: ${big//$'\n'/ }"
}

unit_transfer() { # case
  local config=${WORK}/config-transfer.toml big
  if ! transfer_setup; then
    record "$1" failed "could not build ${WORK}/share and the transfer configs"
    return
  fi
  if ! trial_ready; then
    record "$1" failed "trial-deb13 agent does not answer"
    return
  fi
  transfer_vars
  case $1 in
    transfer-file)
      run_case "$1" "${config}"
      check_pulled "$1" file/f.txt $'guest-file\n'
      ;;
    transfer-pull)
      run_case "$1" "${config}"
      # The last pull replaced the tree with the guest's sub directory, so only its file is left.
      check_pulled "$1" tree/b.txt $'beta\n'
      [[ ! -e ${WORK}/pull/tree/a.txt ]] || record "$1" failed "overwrite left pull/tree/a.txt behind"
      run_case transfer-pull-caps "${WORK}/config-transfer-caps.toml"
      [[ -z $(find "${WORK}/pull" -maxdepth 1 -name 'caps-*' -print) ]] \
        || record transfer-pull-caps failed "a refused pull left pull/caps-* behind"
      check_pull_dir transfer-pull-caps
      ;;
    transfer-background)
      big=${WORK}/share/big
      mkdir -p -- "${big}" && chmod 755 "${big}"
      # yes ends on SIGPIPE, so the file's size is the check.
      yes "${BIG_PATTERN}" | head -c "${BIG_BYTES}" > "${big}/big.bin"
      if [[ $(stat -c %s "${big}/big.bin") != "${BIG_BYTES}" ]]; then
        record "$1" failed "could not write ${big}/big.bin"
      else
        chmod 644 "${big}/big.bin"
        run_case "$1" "${WORK}/config-transfer-slow.toml"
        [[ $(sha256sum < "${WORK}/pull/bg/big.bin" 2> /dev/null) == "${BIG_SHA256}  -" ]] \
          || record "$1" failed "pull/bg/big.bin does not match the guest's file"
      fi
      check_pull_dir "$1"
      rm -rf -- "${big:?}" "${WORK:?}/pull/bg"
      check_work_small "$1"
      # The stop during a push leaves .vmm-* entries beside its target; the follow-up case deletes them.
      if trial_ready; then
        transfer_vars
        run_case transfer-background-after "${config}"
      else
        record transfer-background-after failed "trial-deb13 did not start again"
      fi
      ;;
    *) run_case "$1" "${config}" ;;
  esac
  check_pull_dir "$1"
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

# Writes $WORK/config-windows.toml: config-templates.toml with t-win11 allowed and powered, and the transfer
# directories that transfer_setup built; and config-windows-caps.toml, which also caps a pull at 64 MiB.
write_windows_config() {
  local file=${WORK}/config-windows.toml
  {
    printf 'host_share_dirs = ["%s"]\nhost_pull_dirs = ["%s"]\n' "${WORK}/share" "${WORK}/pull"
    sed -e 's/^allowed_vms = \[/allowed_vms = ["t-win11", /' -e 's/^power_vms = \[/power_vms = ["t-win11", /' \
      "${TESTBED}/config-templates.toml"
  } > "${file}" || return 1
  [[ $(grep -cF -e 'allowed_vms = ["t-win11", ' -e 'power_vms = ["t-win11", ' "${file}") == 2 ]] || return 1
  { cat "${file}" && printf '\n[limits]\nmax_pull_bytes = 67108864\n'; } > "${WORK}/config-windows-caps.toml"
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

# win-exec starts t-win11 and shuts it down; the job and wait cases then run on a fresh start, sharing vars.json, and
# the transfer cases last, with vars.json rewritten for the transfer directories and the caps case under its own
# config.
unit_windows() {
  local cases=(win-exec win-jobs win-jobs-reopen win-jobs-refusals win-wait win-transfer win-transfer-caps)
  local config=${WORK}/config-windows.toml reason case
  reason=$(win_blocker)
  if [[ -n ${reason} ]]; then
    for case in "${cases[@]}"; do record "${case}" skipped "${reason}"; done
    return
  fi
  if ! transfer_setup; then
    reason="could not build ${WORK}/share and the transfer configs"
  elif ! write_windows_config; then
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
    for case in "${cases[@]:1}"; do
      [[ ${case} == win-transfer ]] && transfer_vars
      if [[ ${case} == win-transfer-caps ]]; then
        run_case "${case}" "${WORK}/config-windows-caps.toml"
      else
        run_case "${case}" "${config}"
      fi
    done
    check_pulled win-transfer win/vmm-win.txt $'windows transfer\n'
    check_pulled win-transfer win-dir/a.txt $'alpha\n'
    check_pulled win-transfer win-dir/sub/b.txt $'beta\n'
    check_pull_dir win-transfer
    [[ -z $(find "${WORK}/pull" -maxdepth 1 \( -name win-big -o -name win-loop \) -print) ]] \
      || record win-transfer-caps failed "a refused Windows pull left pull/win-big or pull/win-loop behind"
    check_pull_dir win-transfer-caps
  else
    for case in "${cases[@]:1}"; do record "${case}" failed "the t-win11 agent did not answer"; done
  fi
  win_shut_off || record win-wait failed "could not shut t-win11 off"
}

unit_win_screen() {
  local reason
  if [[ -z ${OCR_MODELS} ]]; then
    record win-screen skipped "${NO_OCR_MODELS}"
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
    record "$1" skipped "${NO_OCR_MODELS}"
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
    transfer-* | addresses) unit_transfer "$1" ;;
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
    echo "the OCR cases are skipped: ${NO_OCR_MODELS}"
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
