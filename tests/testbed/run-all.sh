#!/usr/bin/env bash
# Runs the test-bed cases with mcp_call.py --check, in an order that keeps each case's preconditions, and prints a
# summary. Usage: tests/testbed/run-all.sh [case ...]; a case name may carry .json. With no arguments it runs every
# case except the manual root-only check. Exits 0 when every case passed or the test bed is absent, 1 when a case
# failed, 2 on bad arguments. It touches only trial-deb13, t-owrt, and the t-* VMs and t-*/tpl-scratch volumes it
# creates in pool agent-vms on qemu:///system. The OCR cases run only when VIRTMANAGER_OCR_MODELS names the directory
# scripts/fetch-ocr-models.sh fills, or, without it, when the directory a server with VIRTMANAGER_OCR_DOWNLOAD=1
# fills holds both models; they define t-inst and t-win from the XML files here. The Windows cases run on
# t-win11 when it exists, which tests/testbed/win11/build.sh makes, and leave it shut off; win-network and win-rdp run
# the same way on t-win11net, which create_vm makes (README, "The Windows guest"). The transfer cases share and pull
# host files only under the run's work directory.
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
  smoke guest-agent-and-snapshots lifecycle-guardrails write-file-mode exec-args transfer-file transfer-push
  transfer-pull transfer-background addresses console-reopen console-stall
  start-vm-waits-for-agent console-closed-on-power-off console-after-start screenshot-region screen-settled
  screen-no-models templates-create templates-reset templates-reset-uefi firmware-secure-boot extra-isos display hibernate uefi-vars tpm templates-expiry
  templates-expiry-command
  templates-outside-disk manage-lock memory-cap console-log console-generation console-refusals job-wait jobs wait-for owrt-boot owrt-wait owrt-pacing mouse-refusals
  diag-start-capture diag-crash-preserve diag-crash-destroy diag-attribution diag-wait-event diag-stats diag-identity
  diag-hidden
  screen-installer screen-windows win-exec win-screen win-network win-rdp
)
# Memory the host must have available before an OCR case starts its VM, in MiB: the VM's own plus 1 GiB.
declare -A SCREEN_VM_MIB=(["t-inst"]=2048 ["t-win"]=5120)
declare -A MEMBERS=(
  ["templates-reset"]="templates-reset templates-reset-missing"
  ["templates-reset-uefi"]="templates-reset-uefi templates-reset-saved"
  ["templates-outside-disk"]="templates-outside-disk-setup templates-outside-disk"
  ["display"]="display display-after"
  ["win-rdp"]="win-rdp win-rdp-after"
  ["hibernate"]="hibernate hibernate-after"
  ["uefi-vars"]="uefi-vars uefi-vars-start uefi-vars-guest uefi-vars-reset uefi-vars-virsh uefi-vars-virsh-after uefi-vars-delete uefi-vars-leftover uefi-vars-leftover-after"
  ["tpm"]="tpm-state tpm-state-delete tpm-leftover tpm-leftover-after tpm-refused"
  ["jobs"]="jobs jobs-reopen"
  ["diag-stats"]="diag-stats diag-stats-read-only diag-stats-after"
  ["transfer-pull"]="transfer-pull transfer-pull-caps"
  ["transfer-background"]="transfer-background transfer-background-after"
  ["win-exec"]="win-exec win-jobs win-jobs-reopen win-jobs-refusals win-wait win-transfer win-transfer-caps win-push-place win-push-volume"
)
# The pattern of the 128 MiB file the background-transfer case moves both ways, and its SHA-256.
readonly BIG_PATTERN='virtmanager-mcp test bed'
readonly BIG_BYTES=134217728
readonly BIG_SHA256=bfa16ddec123a5b3cfe4f858f57dfd0c1a24563a61948324e8f2a3b01d04ea24
# Memory the host must have available before t-win11 starts, in MiB: its 4 GiB plus 1 GiB.
readonly WIN_VM_MIB=5120
# The same for t-win11net: its 8 GiB plus 1 GiB.
readonly WIN_NET_VM_MIB=9216

declare -A RESULT=()
ORDER=()
WORK=
# The directory of extra-isos's ISOs; set while the case runs.
ISO_DIR=
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

# Zero-filled ISOs in a directory under /var/tmp, which is on disk, not tmpfs like /tmp, and which the qemu user can
# read, unlike the repository under the home directory: install, drivers and e1 are 1 MiB, and e2 to e4 are 2 to 4 MiB.
# extra-isos reads the directory as ${iso} from vars.json and runs under config-templates.toml with that directory as
# manage.iso_dirs and max_vms = 4, because the kept t-win11net counts toward max_vms. The case's guest check compares the
# sizes of /dev/sr0 and /dev/sr1 with those of e1 and e3.
unit_extra_isos() {
  local config=${WORK}/config-extra-isos.toml name avail
  local -A mib=([install]=1 [drivers]=1 [e1]=1 [e2]=2 [e3]=3 [e4]=4)
  if ! ISO_DIR=$(mktemp -d /var/tmp/vmm-testbed-isos.XXXXXX) || ! chmod 755 "${ISO_DIR}"; then
    record extra-isos failed "could not make a directory under /var/tmp"
    return
  fi
  for name in "${!mib[@]}"; do
    truncate -s "${mib[${name}]}M" "${ISO_DIR}/${name}.iso" || record extra-isos failed "could not write ${name}.iso"
  done
  if ! sed -e "s|^iso_dirs = \\[\\]\$|iso_dirs = [\"${ISO_DIR}\"]|" -e 's/^max_vms = 3$/max_vms = 4/' \
    "${TESTBED}/config-templates.toml" > "${config}" \
    || ! grep -qxF "iso_dirs = [\"${ISO_DIR}\"]" "${config}" || ! grep -qxF 'max_vms = 4' "${config}"; then
    record extra-isos failed "could not write ${config}"
    rm -rf -- "${ISO_DIR:?}"
    ISO_DIR=
    return
  fi
  printf '{"iso": "%s"}\n' "${ISO_DIR}" > "${WORK}/vars.json"
  avail=$(avail_mib)
  if ((avail >= 1024)); then
    run_case extra-isos "${config}"
  else
    record extra-isos failed "needs 1024 MiB of available memory; the host has ${avail} MiB"
  fi
  rm -rf -- "${ISO_DIR:?}" "${WORK:?}/vars.json"
  ISO_DIR=
}

# The hibernate case leaves t-hb (BIOS) and t-hu (UEFI, Secure Boot off) defined, and this reads their inactive
# definitions with virsh: both have suspend to disk on and suspend to memory off, and t-hu, which libvirt first
# resolved by autoselection and the server then redefined, carries the explicit flash loader and variables template
# and neither the `firmware` attribute nor `<firmware>` features. The second case deletes them. The case runs under
# config-templates.toml with max_vms = 4, because the kept t-win11net counts toward max_vms.
unit_hibernate() {
  local config=${WORK}/config-hibernate.toml vm xml avail
  if ! sed 's/^max_vms = 3$/max_vms = 4/' "${TESTBED}/config-templates.toml" > "${config}" \
    || ! grep -qxF 'max_vms = 4' "${config}"; then
    record hibernate failed "could not write ${config}"
    return
  fi
  avail=$(avail_mib)
  if ((avail < 1024)); then
    record hibernate failed "needs 1024 MiB of available memory; the host has ${avail} MiB"
    return
  fi
  run_case hibernate "${config}"
  for vm in t-hb t-hu; do
    xml=${WORK}/${vm}.inactive.xml
    vsh dumpxml --inactive "${vm}" > "${xml}" || {
      record hibernate failed "could not read the definition of ${vm}"
      continue
    }
    [[ $(xmllint --xpath "count(//pm/suspend-to-disk[@enabled='yes'])" "${xml}") == 1 &&
    $(xmllint --xpath "count(//pm/suspend-to-mem[@enabled='no'])" "${xml}") == 1 ]] \
      || record hibernate failed "${vm} has no suspend to disk on and suspend to memory off"
  done
  [[ $(xmllint --xpath "count(//os/loader)" "${WORK}/t-hb.inactive.xml") == 0 ]] \
    || record hibernate failed "t-hb is BIOS and has a loader"
  xml=${WORK}/t-hu.inactive.xml
  [[ $(xmllint --xpath "count(//os/@firmware) + count(//os/firmware)" "${xml}") == 0 &&
  $(xmllint --xpath "string(//os/loader/@type)" "${xml}") == pflash &&
  $(xmllint --xpath "string(//os/loader/@secure)" "${xml}") == no &&
  $(xmllint --xpath "string(//os/loader)" "${xml}") == /*OVMF_CODE*.fd &&
  $(xmllint --xpath "string(//os/nvram/@template)" "${xml}") == /*OVMF_VARS*.fd ]] \
    || record hibernate failed "t-hu does not hold the explicit firmware form, see ${xml}"
  # The redefinition keeps the variables path the first definition named, which is the VM's own file in the pool.
  [[ $(xmllint --xpath "string(//os/nvram)" "${xml}") == "$(pool_path)/t-hu.vars" ]] \
    || record hibernate failed "t-hu's variables are not t-hu.vars in the pool, see ${xml}"
  # The first start recorded the pinned firmware, and the mode, which a loader named by path cannot tell from setup.
  check_firmware_record t-hu off hibernate
  run_case hibernate-after "${config}"
}

# The first twelve bytes a VNC server on 127.0.0.1 port $1 sends, which is its RFB protocol version.
rfb_banner() {
  # shellcheck disable=SC2016 # $0 is the subshell's own argument.
  timeout 5 bash -c 'exec 3<> "/dev/tcp/127.0.0.1/$0" && head -c 12 <&3' "$1" 2> /dev/null
}

# Prints the VNC port of VM $1 in the last describe_vm reply of the display case's log that names it.
logged_vnc_port() {
  awk -v call="### describe_vm {\"vm\": \"$1\"}" '
    /^### / { on = ($0 == call) }
    on && /"port":/ { port = $2; gsub(/[^0-9]/, "", port); last = port }
    END { print last }' "${WORK}/logs/display.out"
}

# display makes t-dv (VNC) and t-db (SPICE and VNC), starts them and leaves them running. This unit then reads each
# VM's VNC port from the case's describe_vm reply and expects the RFB version banner from it; display-after changes
# the displays of the shut-off VMs with set_display and deletes them. Both run under config-templates.toml with
# max_vms = 6, because the kept t-win11net and the other session's cachyos-gnome count toward max_vms.
unit_display() {
  local config=${WORK}/config-display.toml avail vm port
  if ! sed 's/^max_vms = 3$/max_vms = 6/' "${TESTBED}/config-templates.toml" > "${config}" \
    || ! grep -qxF 'max_vms = 6' "${config}"; then
    record display failed "could not write ${config}"
    return
  fi
  avail=$(avail_mib)
  if ((avail < 2048)); then
    record display failed "needs 2048 MiB of available memory; the host has ${avail} MiB"
    return
  fi
  run_case display "${config}"
  for vm in t-dv t-db; do
    port=$(logged_vnc_port "${vm}")
    if [[ ! ${port} =~ ^[0-9]+$ ]]; then
      record display failed "no VNC port of ${vm} in the describe_vm reply"
    elif [[ $(rfb_banner "${port}") != 'RFB 003.'* ]]; then
      record display failed "${vm} did not answer the RFB handshake on 127.0.0.1:${port}"
    fi
  done
  run_case display-after "${config}"
}

# The directory of the pool agent-vms.
pool_path() { vsh pool-dumpxml "${POOL}" | xmllint --xpath 'string(//target/path)' -; }

# Whether pool volume $1 is owned by the user QEMU runs as, which is who libvirt makes a VM's variables file for.
owned_by_qemu() {
  local uid
  uid=$(id -u libvirt-qemu 2> /dev/null || id -u qemu 2> /dev/null) || return 1
  [[ $(vsh vol-dumpxml --pool "${POOL}" "$1" | xmllint --xpath 'string(//target/permissions/owner)' -) == "${uid}" ]]
}

# Prints the attribute $2 of the owner element in the persistent definition of VM $1, empty when it has none. `--config`
# reads the persistent definition, which is where the firmware record goes: a redefinition of a running VM changes only
# that one.
owner_attr() {
  vsh metadata "$1" http://virtmanager-mcp/owner/1 --config 2> /dev/null \
    | xmllint --xpath "string(/*/@$2)" - 2> /dev/null
}

# Checks the firmware record of VM $1 against its persistent definition and the host: the code file path is the one the
# definition's <loader> names, the hash is the file's `sha256sum`, and the Secure Boot mode is $2. Records a failure
# under case $3.
check_firmware_record() { # vm mode case
  local vm=$1 mode=$2 name=$3 xml=${WORK}/$1.record.xml code sum
  vsh dumpxml --inactive "${vm}" > "${xml}" || {
    record "${name}" failed "could not read the definition of ${vm}"
    return
  }
  code=$(xmllint --xpath "string(//os/loader)" "${xml}")
  [[ $(owner_attr "${vm}" secure_boot) == "${mode}" ]] \
    || record "${name}" failed "${vm}'s owner element does not say secure_boot='${mode}'"
  [[ -n ${code} && $(owner_attr "${vm}" firmware_code) == "${code}" ]] \
    || record "${name}" failed "${vm}'s firmware_code is not the definition's loader '${code}'"
  sum=$(sha256sum -- "${code}" | cut -d' ' -f1)
  [[ -n ${sum} && $(owner_attr "${vm}" firmware_sha256) == "${sum}" ]] \
    || record "${name}" failed "${vm}'s firmware_sha256 is not the SHA-256 of ${code}"
  # The redefinition kept the rest of the definition: the variables are still the pool's file.
  [[ $(xmllint --xpath "string(//os/nvram)" "${xml}") == "$(pool_path)/${vm}.vars" ]] \
    || record "${name}" failed "${vm}'s variables moved after the record, see ${xml}"
}

# Checks that the owner element of VM $1 carries secure_boot='$2' and no firmware record. Records a failure under case $3.
check_no_firmware_record() { # vm mode case
  [[ $(owner_attr "$1" secure_boot) == "$2" ]] \
    || record "$3" failed "$1's owner element does not say secure_boot='$2'"
  [[ -z $(owner_attr "$1" firmware_code)$(owner_attr "$1" firmware_sha256) ]] \
    || record "$3" failed "$1 carries a firmware record"
}

# UEFI variables in the pool. The files appear in the pool listing without a refresh from here, because the server
# rescans the pool after a start; before a VM's first start there is no file, even after a refresh. t-uv is made by
# create_vm and only started and stopped, t-uvg comes from a template and keeps a variable the guest writes across a
# shutdown and start, and both files go with their VMs. A t-lv.vars made here with virsh makes create_vm and
# create_vm_from_template refuse t-lv for UEFI, names it and the command that removes it, and a create works once it
# is removed. The owner element of t-uv, made by create_vm, holds its Secure Boot mode and no firmware record before its
# first start; the server's start adds the record, whose hash equals `sha256sum` of the code file, and t-uvg, from a
# template, gets one too, and keeps it through a `reset_vm` of the running VM. t-uvv is started first with `virsh
# start`, so the server's later start of it records nothing.
# The case runs under config-templates.toml with max_vms = 4, because the kept t-win11net counts toward max_vms.
unit_uefi_vars() {
  local config=${WORK}/config-uefi-vars.toml xml=${WORK}/t-uv.inactive.xml vm avail left
  if ! sed 's/^max_vms = 3$/max_vms = 4/' "${TESTBED}/config-templates.toml" > "${config}" \
    || ! grep -qxF 'max_vms = 4' "${config}"; then
    record uefi-vars failed "could not write ${config}"
    return
  fi
  avail=$(avail_mib)
  if ((avail < 1024)); then
    record uefi-vars failed "needs 1024 MiB of available memory; the host has ${avail} MiB"
    return
  fi
  run_case uefi-vars "${config}"
  vsh pool-refresh "${POOL}" > /dev/null || record uefi-vars failed "could not refresh the pool"
  has_vol t-uv.vars && record uefi-vars failed "t-uv.vars exists before the VM's first start"
  if vsh dumpxml --inactive t-uv > "${xml}"; then
    [[ $(xmllint --xpath "string(//os/@firmware)" "${xml}") == efi &&
    $(xmllint --xpath "string(//os/nvram)" "${xml}") == "$(pool_path)/t-uv.vars" &&
    $(xmllint --xpath "string(//os/nvram/@format)" "${xml}") == raw ]] \
      || record uefi-vars failed "t-uv's variables are not t-uv.vars in the pool, see ${xml}"
  else
    record uefi-vars failed "could not read the definition of t-uv"
  fi
  check_no_firmware_record t-uv off uefi-vars
  run_case uefi-vars-start "${config}"
  check_firmware_record t-uv off uefi-vars-start
  has_vol t-uv.vars || record uefi-vars-start failed "t-uv.vars is not in the pool listing after the first start"
  owned_by_qemu t-uv.vars || record uefi-vars-start failed "t-uv.vars is not owned by the QEMU user"
  [[ ! -e /var/lib/libvirt/qemu/nvram/t-uv_VARS.fd ]] \
    || record uefi-vars-start failed "t-uv has variables in libvirt's own nvram directory"
  run_case uefi-vars-guest "${config}"
  has_vol t-uvg.vars || record uefi-vars-guest failed "t-uvg.vars is not in the pool listing after the first start"
  owned_by_qemu t-uvg.vars || record uefi-vars-guest failed "t-uvg.vars is not owned by the QEMU user"
  check_firmware_record t-uvg on uefi-vars-guest
  run_case uefi-vars-reset "${config}"
  # A reset of the running, recorded t-uvg rewrites its owner element from the persistent definition, so the record
  # survives and only the generation moves on.
  check_firmware_record t-uvg on uefi-vars-reset
  [[ $(owner_attr t-uvg generation) == 2 ]] \
    || record uefi-vars-reset failed "t-uvg's owner element does not say generation='2' after the reset"
  run_case uefi-vars-virsh "${config}"
  check_no_firmware_record t-uvv setup uefi-vars-virsh
  if vsh start t-uvv > /dev/null; then
    wait_until 30 is_state t-uvv running || record uefi-vars-virsh failed "t-uvv did not start with virsh"
    vsh destroy t-uvv > /dev/null || record uefi-vars-virsh failed "could not stop t-uvv"
  else
    record uefi-vars-virsh failed "virsh could not start t-uvv"
  fi
  vsh pool-refresh "${POOL}" > /dev/null
  has_vol t-uvv.vars || record uefi-vars-virsh failed "t-uvv.vars is not in the pool after virsh start"
  run_case uefi-vars-virsh-after "${config}"
  check_no_firmware_record t-uvv setup uefi-vars-virsh-after
  run_case uefi-vars-delete "${config}"
  vsh pool-refresh "${POOL}" > /dev/null || record uefi-vars-delete failed "could not refresh the pool"
  for vm in t-uv t-uvg t-uvv; do
    has_vol "${vm}.vars" && record uefi-vars-delete failed "${vm}.vars remains after delete_vm"
    [[ ! -e /var/lib/libvirt/qemu/nvram/${vm}_VARS.fd ]] \
      || record uefi-vars-delete failed "${vm}_VARS.fd remains in libvirt's nvram directory"
  done
  if ! vsh vol-create-as "${POOL}" t-lv.vars 1M --format raw > /dev/null; then
    record uefi-vars-leftover failed "could not make t-lv.vars"
    return
  fi
  run_case uefi-vars-leftover "${config}"
  left=$(vols_named '^t-lv')
  [[ ${left} == t-lv.vars ]] || record uefi-vars-leftover failed "volumes of t-lv remain besides t-lv.vars: ${left//$'\n'/ }"
  vsh dominfo t-lv > /dev/null 2>&1 && record uefi-vars-leftover failed "a VM named t-lv exists"
  vsh vol-delete --pool "${POOL}" t-lv.vars > /dev/null || record uefi-vars-leftover failed "could not delete t-lv.vars"
  run_case uefi-vars-leftover-after "${config}"
  has_vol t-lv.vars && record uefi-vars-leftover-after failed "t-lv.vars remains after delete_vm"
  vsh pool-refresh "${POOL}" > /dev/null
  has_vol t-lv.vars && record uefi-vars-leftover-after failed "t-lv.vars remains after delete_vm and a refresh"
}

# Whether pool volume $1 is owned by the user and group swtpm runs as, with mode 0600, which libvirt needs to start a VM
# whose TPM state is that file.
owned_by_swtpm() {
  local ids xml
  ids=$(getent passwd swtpm | cut -d: -f3,4) && [[ -n ${ids} ]] || return 1
  xml=$(vsh vol-dumpxml --pool "${POOL}" "$1") || return 1
  [[ $(xmllint --xpath 'string(//target/permissions/owner)' - <<< "${xml}") == "${ids%%:*}" &&
  $(xmllint --xpath 'string(//target/permissions/group)' - <<< "${xml}") == "${ids##*:}" &&
  $(xmllint --xpath 'string(//target/permissions/mode)' - <<< "${xml}") == 0600 ]]
}

# The result of tests/testbed/tpm-gate.sh, which runs once: empty before it ran, then passed or failed.
TPM_GATE=

# Runs the TPM gate the first time and records it, and succeeds when a VM with a TPM state file in the pool starts here.
tpm_gate_passes() {
  if [[ -z ${TPM_GATE} ]]; then
    echo "== tpm-gate"
    if "${TESTBED}/tpm-gate.sh" > "${WORK}/logs/tpm-gate.out" 2>&1; then
      TPM_GATE=passed
      record tpm-gate passed
    else
      TPM_GATE=failed
      record tpm-gate skipped "a VM with its TPM state in the pool does not start on this host, see ${WORK}/logs/tpm-gate.out"
    fi
  fi
  [[ ${TPM_GATE} == passed ]]
}

# TPM state in the pool. The gate decides which cases run. When it passes, create_vm makes the UEFI VM t-tp, whose
# definition names <pool>/t-tp.tpm, a volume owned by the swtpm user with mode 0600 that exists before the first start;
# the VM starts twice, and delete_vm leaves no volume. A t-tl.tpm made here with virsh makes create_vm refuse t-tl and
# name it, and a create works once it is removed. When the gate fails, the refusal case runs instead: create_vm
# succeeds, start_vm is refused with the AppArmor rule, and delete_vm removes the state. The case runs under
# config-templates.toml with max_vms = 4, because the kept t-win11net counts toward max_vms.
unit_tpm() {
  local config=${WORK}/config-tpm.toml xml=${WORK}/t-tp.inactive.xml avail left reason name
  if ! sed 's/^max_vms = 3$/max_vms = 4/' "${TESTBED}/config-templates.toml" > "${config}" \
    || ! grep -qxF 'max_vms = 4' "${config}"; then
    record tpm-state failed "could not write ${config}"
    return
  fi
  avail=$(avail_mib)
  if ((avail < 1024)); then
    record tpm-state failed "needs 1024 MiB of available memory; the host has ${avail} MiB"
    return
  fi
  if ! tpm_gate_passes; then
    reason="the TPM gate fails on this host"
    for name in tpm-state tpm-state-delete tpm-leftover tpm-leftover-after; do record "${name}" skipped "${reason}"; done
    run_case tpm-refused "${config}"
    vsh pool-refresh "${POOL}" > /dev/null || record tpm-refused failed "could not refresh the pool"
    has_vol t-tpr.tpm && record tpm-refused failed "t-tpr.tpm remains after delete_vm"
    return
  fi
  record tpm-refused skipped "the TPM gate passes, so there is no refusal to check"
  run_case tpm-state "${config}"
  has_vol t-tp.tpm || record tpm-state failed "t-tp.tpm is not in the pool"
  owned_by_swtpm t-tp.tpm || record tpm-state failed "t-tp.tpm is not owned by the swtpm user with mode 0600"
  if vsh dumpxml --inactive t-tp > "${xml}"; then
    [[ $(xmllint --xpath "string(//tpm/backend/source/@type)" "${xml}") == file &&
    $(xmllint --xpath "string(//tpm/backend/source/@path)" "${xml}") == "$(pool_path)/t-tp.tpm" ]] \
      || record tpm-state failed "t-tp's TPM state is not t-tp.tpm in the pool, see ${xml}"
  else
    record tpm-state failed "could not read the definition of t-tp"
  fi
  owned_by_swtpm t-tp.tpm || record tpm-state failed "t-tp.tpm is not owned by the swtpm user with mode 0600 after the starts"
  run_case tpm-state-delete "${config}"
  vsh pool-refresh "${POOL}" > /dev/null || record tpm-state-delete failed "could not refresh the pool"
  has_vol t-tp.tpm && record tpm-state-delete failed "t-tp.tpm remains after delete_vm"
  if ! vsh vol-create-as "${POOL}" t-tl.tpm 1M --format raw > /dev/null; then
    record tpm-leftover failed "could not make t-tl.tpm"
    return
  fi
  run_case tpm-leftover "${config}"
  left=$(vols_named '^t-tl')
  [[ ${left} == t-tl.tpm ]] || record tpm-leftover failed "volumes of t-tl remain besides t-tl.tpm: ${left//$'\n'/ }"
  vsh dominfo t-tl > /dev/null 2>&1 && record tpm-leftover failed "a VM named t-tl exists"
  vsh vol-delete --pool "${POOL}" t-tl.tpm > /dev/null || record tpm-leftover failed "could not delete t-tl.tpm"
  run_case tpm-leftover-after "${config}"
  vsh pool-refresh "${POOL}" > /dev/null
  has_vol t-tl.tpm && record tpm-leftover-after failed "t-tl.tpm remains after delete_vm"
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

# config-templates.toml with every VM the server created capped at 768 MiB of memory together, so one 512 MiB VM
# runs and a second waits.
unit_memory_cap() {
  sed -e 's/^max_memory_mib = 1024$/&\nmax_total_memory_mib = 768/' "${TESTBED}/config-templates.toml" \
    > "${WORK}/config-memory-cap.toml" || {
    record memory-cap failed "could not write config-memory-cap.toml"
    return
  }
  grep -qx 'max_total_memory_mib = 768' "${WORK}/config-memory-cap.toml" || {
    record memory-cap failed "config-templates.toml has no max_memory_mib = 1024 line"
    return
  }
  run_case memory-cap "${WORK}/config-memory-cap.toml"
}

# Runs console-log with a pull directory of its own, then checks that the boot's log outlived delete_vm.
unit_console_log() {
  local pull=${WORK}/console-pull logs
  if ! rm -rf -- "${pull:?}" || ! mkdir -p -- "${pull}"; then
    record console-log failed "could not make ${pull}"
    return
  fi
  {
    printf 'host_pull_dirs = ["%s"]\n' "${pull}"
    cat "${TESTBED}/config-templates.toml"
  } > "${WORK}/config-console-log.toml" || {
    record console-log failed "could not write config-console-log.toml"
    return
  }
  run_case console-log "${WORK}/config-console-log.toml"
  shopt -s nullglob
  logs=("${pull}"/console/t-cl/*-g*.log)
  shopt -u nullglob
  if ((${#logs[@]} != 1)); then
    record console-log failed "expected one log in ${pull}/console/t-cl, found ${#logs[@]}"
  elif ! grep -qF "Booting \`Debian GNU/Linux'" "${logs[0]}"; then
    record console-log failed "${logs[0]} lacks the boot menu line"
  fi
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
  mkdir -p -- "${share}/win-ete" && printf 'été\n' > "${share}/win-ete/été.txt" && printf 'a\n' > "${share}/win-ete/a.txt" \
    || return 1
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

win_ready() { # [vm]
  local vm=${1:-t-win11}
  is_state "${vm}" running || vsh start "${vm}" > /dev/null || return 1
  wait_until 600 agent_answers "${vm}"
}

# Shuts t-win11 down through its agent, which Windows obeys while a user is logged on, unlike the ACPI button; powers
# it off if that fails.
win_shut_off() { # [vm]
  local vm=${1:-t-win11}
  is_state "${vm}" "shut off" && return 0
  vsh shutdown "${vm}" --mode agent > /dev/null 2>&1 || vsh shutdown "${vm}" > /dev/null 2>&1
  wait_until 180 is_state "${vm}" "shut off" && return 0
  vsh destroy "${vm}" > /dev/null && wait_until 30 is_state "${vm}" "shut off"
}

# The blank disk that win-push-volume attaches to t-win11 as a second volume.
readonly WIN_EXTRA_VOL=t-push-vol.qcow2
readonly WIN_EXTRA_DEV=sde

# Shuts t-win11 off, then detaches and deletes WIN_EXTRA_VOL wherever it is still attached or present.
win_detach_volume() {
  local status=0
  win_shut_off || return 1
  if vsh dumpxml --inactive t-win11 | grep -qF -- "${WIN_EXTRA_VOL}"; then
    vsh detach-disk t-win11 "${WIN_EXTRA_DEV}" --config > /dev/null || status=1
  fi
  if vsh vol-info --pool "${POOL}" "${WIN_EXTRA_VOL}" > /dev/null 2>&1; then
    vsh vol-delete --pool "${POOL}" "${WIN_EXTRA_VOL}" > /dev/null || status=1
  fi
  return "${status}"
}

# Runs win-push-volume under config $1, with a blank 1 GiB disk attached to t-win11, then removes the disk.
win_push_volume() {
  local config=$1 path reason=
  if ! win_shut_off; then
    reason="could not shut t-win11 off"
  elif ! vsh vol-create-as "${POOL}" "${WIN_EXTRA_VOL}" 1G --format qcow2 > /dev/null; then
    reason="could not create ${WIN_EXTRA_VOL}"
  elif ! path=$(vsh vol-path --pool "${POOL}" "${WIN_EXTRA_VOL}") \
    || ! vsh attach-disk t-win11 "${path}" "${WIN_EXTRA_DEV}" --targetbus sata --driver qemu --subdriver qcow2 \
      --config > /dev/null; then
    reason="could not attach ${WIN_EXTRA_VOL} to t-win11"
  elif ! win_ready; then
    reason="the t-win11 agent did not answer"
  fi
  if [[ -n ${reason} ]]; then
    record win-push-volume failed "${reason}"
  else
    run_case win-push-volume "${config}"
  fi
  win_detach_volume || record win-push-volume failed "could not detach and delete ${WIN_EXTRA_VOL}"
}

# win-exec starts t-win11 and shuts it down; the job and wait cases then run on a fresh start, sharing vars.json, and
# the transfer cases last, with vars.json rewritten for the transfer directories and the caps case under its own
# config. win-push-volume runs after a restart with a second disk attached.
unit_windows() {
  local cases=(win-exec win-jobs win-jobs-reopen win-jobs-refusals win-wait win-transfer win-transfer-caps
    win-push-place win-push-volume)
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
      [[ ${case} == win-push-volume ]] && continue
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
    check_pulled win-transfer win-ete/a.txt $'a\n'
    check_pulled win-transfer win-ete/été.txt $'été\n'
    [[ ! -e ${WORK}/pull/win-jp ]] || record win-transfer failed "the refused pull of vmm-jp left pull/win-jp behind"
    check_pull_dir win-transfer
    [[ -z $(find "${WORK}/pull" -maxdepth 1 \( -name win-big -o -name win-loop \) -print) ]] \
      || record win-transfer-caps failed "a refused Windows pull left pull/win-big or pull/win-loop behind"
    check_pull_dir win-transfer-caps
    win_push_volume "${config}"
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

# Prints why win-network cannot run, or nothing when it can. A running t-win11net already holds its memory.
win_net_blocker() {
  local avail tool
  for tool in ssh ssh-keygen jq; do
    command -v "${tool}" > /dev/null || {
      echo "${tool} is not installed"
      return
    }
  done
  if ! vsh dominfo t-win11net > /dev/null 2>&1; then
    echo "no t-win11net; tests/testbed/README.md says how create_vm makes it"
    return
  fi
  is_state t-win11net running && return
  avail=$(avail_mib)
  ((avail >= WIN_NET_VM_MIB)) \
    || echo "t-win11net needs ${WIN_NET_VM_MIB} MiB of available memory; the host has ${avail} MiB"
}

# Whether t-win11net has a lease and its SSH port accepts a connection, which means that Windows has finished booting
# far enough for the guest agent to answer commands in time.
win_net_ssh_open() {
  local ip
  ip=$(vsh domifaddr t-win11net --source lease 2> /dev/null | awk '/ipv4/ { split($4, a, "/"); print a[1]; exit }')
  # shellcheck disable=SC2016 # $0 is the subshell's own argument.
  [[ -n ${ip} ]] && timeout 3 bash -c 'echo > "/dev/tcp/$0/22"' "${ip}" 2> /dev/null
}

# This unit starts t-win11net and waits for its SSH server. win-network then checks the address in vm_addresses and
# installs a test key for the SSH server; the unit logs in from the host with that key, at the address the case's
# vm_addresses call returned, and shuts the VM off.
unit_win_network() {
  local config=${WORK}/config-win-network.toml key=${WORK}/win-network-key reason ip out
  reason=$(win_net_blocker)
  if [[ -n ${reason} ]]; then
    record win-network skipped "${reason}"
    return
  fi
  rm -f -- "${key}" "${key}.pub"
  sed -e 's/^allowed_vms = \[/allowed_vms = ["t-win11net", /' -e 's/^power_vms = \[/power_vms = ["t-win11net", /' \
    "${TESTBED}/config-templates.toml" > "${config}" \
    && [[ $(grep -cF -e 'allowed_vms = ["t-win11net", ' -e 'power_vms = ["t-win11net", ' "${config}") == 2 ]] \
    || reason="could not write ${config}"
  if [[ -z ${reason} ]]; then
    ssh-keygen -q -t ed25519 -N '' -C vmm-testbed -f "${key}" > /dev/null || reason="could not make a test key"
  fi
  if [[ -z ${reason} ]]; then
    jq -n --rawfile pub "${key}.pub" '{pubkey: ($pub | rtrimstr("\n"))}' > "${WORK}/vars.json" \
      || reason="could not write ${WORK}/vars.json"
  fi
  if [[ -z ${reason} ]]; then
    win_ready t-win11net || reason="the t-win11net agent did not answer"
  fi
  if [[ -z ${reason} ]] && ! wait_until 300 win_net_ssh_open; then
    reason="the t-win11net SSH port did not open within 300 s"
  fi
  if [[ -n ${reason} ]]; then
    record win-network failed "${reason}"
    win_shut_off t-win11net || record win-network failed "could not shut t-win11net off"
    return
  fi
  run_case win-network "${config}"
  ip=$(sed -n '/^### vm_addresses/{n;p;q}' "${WORK}/logs/win-network.out" \
    | jq -r '[.interfaces[] | select(.source == "lease") | .addresses[].address][0] // empty' 2> /dev/null)
  if [[ ! ${ip} =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    record win-network failed "no lease address in the vm_addresses reply"
  else
    out=$(ssh -i "${key}" -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=20 "tester@${ip}" whoami 2>&1)
    [[ ${out//$'\r'/} == 't-win11net\tester' ]] \
      || record win-network failed "ssh to tester@${ip} with the test key printed: ${out}"
  fi
  rm -f -- "${key}" "${key}.pub" "${WORK:?}/vars.json"
  win_shut_off t-win11net || record win-network failed "could not shut t-win11net off"
}

# Whether TCP port $2 at $1 accepts a connection from the host.
tcp_open() { # ip port
  # shellcheck disable=SC2016 # $0 and $1 are the subshell's own arguments.
  timeout 5 bash -c 'exec 3<> "/dev/tcp/$0/$1"' "$1" "$2" 2> /dev/null
}

# win-rdp runs on t-win11net, as win-network does, and is a unit of its own so that it runs alone:
# `run-all.sh win-rdp`. It is in the default run, like win-network, because the Windows cases have no opt-in switch;
# a host without the VM or 9 GiB of memory skips it. The case expects the state of a fresh Windows 11 guest, with
# Remote Desktop off, enables it through `exec`, and the unit then connects to port 3389 at the address vm_addresses
# reported. win-rdp-after turns Remote Desktop off again, runs even when win-rdp failed, and the unit expects the
# port closed before it shuts the VM off.
unit_win_rdp() {
  local config=${WORK}/config-win-rdp.toml reason ip
  reason=$(win_net_blocker)
  if [[ -n ${reason} ]]; then
    record win-rdp skipped "${reason}"
    return
  fi
  sed -e 's/^allowed_vms = \[/allowed_vms = ["t-win11net", /' -e 's/^power_vms = \[/power_vms = ["t-win11net", /' \
    "${TESTBED}/config-templates.toml" > "${config}" \
    && [[ $(grep -cF -e 'allowed_vms = ["t-win11net", ' -e 'power_vms = ["t-win11net", ' "${config}") == 2 ]] \
    || reason="could not write ${config}"
  if [[ -z ${reason} ]]; then
    win_ready t-win11net || reason="the t-win11net agent did not answer"
  fi
  if [[ -z ${reason} ]] && ! wait_until 300 win_net_ssh_open; then
    reason="the t-win11net SSH port did not open within 300 s"
  fi
  if [[ -n ${reason} ]]; then
    record win-rdp failed "${reason}"
    win_shut_off t-win11net || record win-rdp failed "could not shut t-win11net off"
    return
  fi
  run_case win-rdp "${config}"
  ip=$(sed -n '/^### vm_addresses/{n;p;q}' "${WORK}/logs/win-rdp.out" \
    | jq -r '[.interfaces[] | select(.source == "lease") | .addresses[].address][0] // empty' 2> /dev/null)
  if [[ ! ${ip} =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    record win-rdp failed "no lease address in the vm_addresses reply"
  else
    tcp_open "${ip}" 3389 || record win-rdp failed "${ip} port 3389 did not accept a connection after RDP was enabled"
  fi
  run_case win-rdp-after "${config}"
  if [[ ${ip} =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] && tcp_open "${ip}" 3389; then
    record win-rdp-after failed "${ip} port 3389 still accepts connections after RDP was disabled"
  fi
  win_shut_off t-win11net || record win-rdp-after failed "could not shut t-win11net off"
}

# Writes $WORK/config-<name>.toml: config-templates.toml with one more line after the line that matches $2.
write_variant() { # name after-line line
  local file=${WORK}/config-$1.toml
  sed -e "/^$2\$/a $3" "${TESTBED}/config-templates.toml" > "${file}" || return 1
  grep -qxF -- "$3" "${file}"
}

# The crash under on_crash = "destroy": libvirt stops the VM, and the next start moves the panic to the previous run.
unit_diag_crash_destroy() {
  if write_variant crash-destroy '\[manage\]' 'on_crash = "destroy"'; then
    run_case diag-crash-destroy "${WORK}/config-crash-destroy.toml"
  else
    record diag-crash-destroy failed "could not write ${WORK}/config-crash-destroy.toml"
  fi
}

# diag-stats leaves t-dst running for the read-only server, and diag-stats-after deletes it.
unit_diag_stats() {
  run_case diag-stats config-templates.toml
  if sed -e 's/^read_only = false$/read_only = true/' "${TESTBED}/config-templates.toml" > "${WORK}/config-read-only.toml" \
    && grep -qxF 'read_only = true' "${WORK}/config-read-only.toml"; then
    run_case diag-stats-read-only "${WORK}/config-read-only.toml"
  else
    record diag-stats-read-only failed "could not write ${WORK}/config-read-only.toml"
  fi
  run_case diag-stats-after config-templates.toml
}

# trial-deb13 is outside config-templates.toml's allowed_vms. It starts from virsh while the case's server runs and
# creates t-dh, whose start is the positive control: the worker records events in that run, and no reply names
# trial-deb13.
unit_diag_hidden() {
  local starter
  if ! trial_shut_off; then
    record diag-hidden failed "could not shut trial-deb13 off"
    return
  fi
  (
    sleep 10
    vsh start trial-deb13 > /dev/null
  ) &
  starter=$!
  run_case diag-hidden config-templates.toml
  wait "${starter}" || record diag-hidden failed "virsh could not start trial-deb13 during the case"
  trial_ready || record diag-hidden failed "trial-deb13 agent did not answer afterwards"
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
    extra-isos) unit_extra_isos ;;
    display) unit_display ;;
    hibernate) unit_hibernate ;;
    uefi-vars) unit_uefi_vars ;;
    tpm) unit_tpm ;;
    templates-expiry) unit_templates_expiry ;;
    templates-expiry-command) unit_templates_expiry_command ;;
    templates-outside-disk) unit_templates_outside_disk ;;
    jobs) unit_jobs ;;
    diag-crash-destroy) unit_diag_crash_destroy ;;
    diag-stats) unit_diag_stats ;;
    diag-hidden) unit_diag_hidden ;;
    transfer-* | addresses) unit_transfer "$1" ;;
    manage-lock) unit_manage_lock ;;
    memory-cap) unit_memory_cap ;;
    console-log) unit_console_log ;;
    owrt-* | mouse-refusals) unit_owrt "$1" ;;
    screen-installer) unit_screen "$1" t-inst ;;
    screen-windows) unit_screen "$1" t-win ;;
    win-exec) unit_windows ;;
    win-screen) unit_win_screen ;;
    win-network) unit_win_network ;;
    win-rdp) unit_win_rdp ;;
    smoke | guest-agent-and-snapshots | lifecycle-guardrails | write-file-mode | exec-args | console-reopen | \
      start-vm-waits-for-agent | console-closed-on-power-off | screenshot-region | screen-settled | \
      screen-no-models) unit_trial "$1" ;;
    *) run_case "$1" config-templates.toml ;;
  esac
}

# Shuts t-win11 and t-win11net off, and removes the t-* VMs and volumes this run created and tpl-scratch.qcow2; never t-owrt or
# anything listed before.
cleanup() {
  local vm vol
  [[ -n ${WORK} ]] || return 0
  # An interrupted Windows case leaves t-win11 running, and it holds 4 GiB; t-win11net holds 8 GiB.
  if is_state t-win11 running; then
    win_shut_off > /dev/null 2>&1 || echo "cleanup: could not shut t-win11 off"
  fi
  if is_state t-win11net running; then
    win_shut_off t-win11net > /dev/null 2>&1 || echo "cleanup: could not shut t-win11net off"
  fi
  [[ -n ${ISO_DIR} ]] && rm -rf -- "${ISO_DIR:?}"
  while read -r vm; do
    [[ ${vm} == t-* && ${vm} != t-owrt ]] || continue
    grep -qxF -- "${vm}" <<< "${PRE_VMS}" && continue
    is_state "${vm}" "shut off" || vsh destroy "${vm}" > /dev/null 2>&1
    vsh undefine "${vm}" --nvram --managed-save --snapshots-metadata > /dev/null 2>&1 \
      || vsh undefine "${vm}" --managed-save --snapshots-metadata > /dev/null || echo "cleanup: could not undefine ${vm}"
  done < <(vsh list --all --name)
  # An interrupted win-push-volume leaves its disk attached to t-win11.
  if vsh dumpxml --inactive t-win11 2> /dev/null | grep -qF -- "${WIN_EXTRA_VOL}"; then
    win_detach_volume > /dev/null 2>&1 || echo "cleanup: could not detach ${WIN_EXTRA_VOL} from t-win11"
  fi
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
