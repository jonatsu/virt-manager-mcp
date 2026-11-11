#!/usr/bin/env bash
# Builds the t-win11 test guest: Windows 11 Pro installed unattended from tpl-win11-26h2.iso, with the virtio serial
# driver and the QEMU guest agent from the virtio-win ISO. Usage: tests/testbed/win11/build.sh VIRTIO_WIN_ISO
# VIRTIO_WIN_ISO is a local copy of virtio-win-0.1.302.iso; its SHA-256 must match the sum pinned below, because the
# virtio-win project publishes no checksum for the ISO itself. The install takes up to 90 minutes. Exits 0 once the
# guest agent answers and the VM is shut off with its ISOs ejected, 1 on a failure, 2 on a usage error. It touches only
# the VM t-win11 and the volumes t-win11.qcow2, tpl-win11-answer.iso and tpl-virtio-win-0.1.302.iso in pool agent-vms
# on qemu:///system. A failed build leaves the VM for inspection; remove it with:
#   virsh -c qemu:///system undefine t-win11 --nvram --tpm
#   virsh -c qemu:///system vol-delete --pool agent-vms t-win11.qcow2
set -uo pipefail

readonly URI=qemu:///system
readonly POOL=agent-vms
readonly VM=t-win11
readonly WIN_VOL=tpl-win11-26h2.iso
readonly ANSWER_VOL=tpl-win11-answer.iso
readonly VIRTIO_VOL=tpl-virtio-win-0.1.302.iso
readonly DISK_VOL=t-win11.qcow2
readonly VIRTIO_SHA256=303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d
readonly NEED_MEM_MIB=5120
readonly NEED_POOL_GIB=70
readonly AGENT_WAIT_SECS=$((90 * 60))
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd) || exit 1
readonly HERE
TMP=
# The volume an upload is filling, deleted on exit unless the upload finished.
UPLOADING=

vsh() { virsh -q -c "${URI}" "$@"; }
log() { printf '%s: %s\n' "${0##*/}" "$*" >&2; }
die() {
  log "$*"
  exit 1
}
cleanup() {
  [[ -n ${UPLOADING} ]] && vsh vol-delete --pool "${POOL}" "${UPLOADING}" > /dev/null 2>&1
  [[ -n ${TMP} ]] && rm -rf -- "${TMP:?}"
}
has_vol() { vsh vol-info --pool "${POOL}" "$1" > /dev/null 2>&1; }
is_state() { [[ $(vsh domstate "${VM}" 2> /dev/null) == "$1" ]]; }
agent_answers() { vsh qemu-agent-command "${VM}" '{"execute":"guest-ping"}' > /dev/null 2>&1; }
# Prints the SHA-256 of what is on standard input.
sum_of_stdin() {
  local sum
  sum=$(sha256sum) || return 1
  printf '%s\n' "${sum%% *}"
}

# Uploads local file $2 as volume $1 of the pool. A failed or interrupted upload leaves no volume behind.
upload() {
  local size
  size=$(stat -c %s -- "$2") || return 1
  vsh vol-create-as "${POOL}" "$1" "${size}" --format raw > /dev/null || return 1
  UPLOADING=$1
  vsh vol-upload --pool "${POOL}" "$1" "$2" || return 1
  UPLOADING=
}

main() {
  local iso=${1-} avail pool_free sum deadline i
  if [[ $# -ne 1 || -z ${iso} ]]; then
    echo "usage: ${0##*/} VIRTIO_WIN_ISO" >&2
    exit 2
  fi
  [[ -f ${iso} ]] || die "${iso} is not a file"
  for tool in virsh xorriso sha256sum; do
    command -v "${tool}" > /dev/null || die "${tool} is required"
  done

  vsh dominfo "${VM}" > /dev/null 2>&1 && die "${VM} already exists"
  has_vol "${DISK_VOL}" && die "volume ${DISK_VOL} already exists"
  has_vol "${WIN_VOL}" || die "volume ${WIN_VOL} is not in pool ${POOL}"
  avail=$(awk '/^MemAvailable:/ { print int($2 / 1024) }' /proc/meminfo)
  ((avail >= NEED_MEM_MIB)) || die "${VM} needs ${NEED_MEM_MIB} MiB of available memory; the host has ${avail} MiB"
  pool_free=$(vsh pool-info --bytes "${POOL}" | awk '/^Available:/ { print int($2 / 1073741824) }')
  ((pool_free >= NEED_POOL_GIB)) || die "pool ${POOL} has ${pool_free} GiB free; the build needs ${NEED_POOL_GIB}"

  trap cleanup EXIT
  trap 'exit 130' INT TERM
  if has_vol "${VIRTIO_VOL}"; then
    # A volume from an earlier run is reused only with the pinned content.
    log "checking the existing ${VIRTIO_VOL}"
    sum=$(vsh vol-download --pool "${POOL}" "${VIRTIO_VOL}" /dev/stdout | sum_of_stdin) || die "cannot read ${VIRTIO_VOL}"
    [[ ${sum} == "${VIRTIO_SHA256}" ]] \
      || die "${VIRTIO_VOL} has SHA-256 ${sum}, expected ${VIRTIO_SHA256}; delete it and run again"
  else
    log "checking ${iso}"
    sum=$(sum_of_stdin < "${iso}") || die "cannot read ${iso}"
    [[ ${sum} == "${VIRTIO_SHA256}" ]] || die "${iso} has SHA-256 ${sum}, expected ${VIRTIO_SHA256}"
    log "uploading ${VIRTIO_VOL}"
    upload "${VIRTIO_VOL}" "${iso}" || die "cannot upload ${VIRTIO_VOL}"
  fi

  TMP=$(mktemp -d) || die "cannot create a temporary directory"
  if ! mkdir -- "${TMP}/root" || ! cp -- "${HERE}/autounattend.xml" "${TMP}/root/"; then
    die "cannot stage the answer file"
  fi
  xorriso -as mkisofs -quiet -volid ANSWER -joliet -rational-rock -output "${TMP}/answer.iso" "${TMP}/root" \
    || die "cannot build the answer ISO"
  if has_vol "${ANSWER_VOL}"; then
    vsh vol-delete --pool "${POOL}" "${ANSWER_VOL}" > /dev/null || die "cannot replace ${ANSWER_VOL}"
  fi
  upload "${ANSWER_VOL}" "${TMP}/answer.iso" || die "cannot upload ${ANSWER_VOL}"

  vsh vol-create-as "${POOL}" "${DISK_VOL}" 64G --format qcow2 > /dev/null || die "cannot create ${DISK_VOL}"
  vsh define "${HERE}/../t-win11.xml" > /dev/null || die "cannot define ${VM}"
  vsh start "${VM}" > /dev/null || die "cannot start ${VM}"
  log "started ${VM}; pressing a key for the DVD's boot prompt"
  # The disk boots first, so the prompt shows only now, while the disk is empty. Keys stop after the first boot.
  for ((i = 0; i < 40; i++)); do
    sleep 0.5
    vsh send-key "${VM}" KEY_SPACE > /dev/null 2>&1
  done

  log "waiting up to $((AGENT_WAIT_SECS / 60)) minutes for the guest agent"
  deadline=$((SECONDS + AGENT_WAIT_SECS))
  until agent_answers; do
    is_state running || die "${VM} stopped during setup; see its screen with virt-viewer"
    ((SECONDS < deadline)) || die "the guest agent did not answer within $((AGENT_WAIT_SECS / 60)) minutes"
    (((SECONDS % 300) < 30)) && log "still waiting, $(((deadline - SECONDS) / 60)) minutes left"
    sleep 30
  done
  log "the guest agent answers after $((SECONDS / 60)) minutes; shutting ${VM} down"

  vsh shutdown "${VM}" > /dev/null || die "cannot shut ${VM} down"
  deadline=$((SECONDS + 600))
  until is_state "shut off"; do
    ((SECONDS < deadline)) || die "${VM} did not shut off within 10 minutes"
    sleep 5
  done
  for dev in sdb sdc sdd; do
    vsh change-media "${VM}" "${dev}" --eject --config > /dev/null || die "cannot eject ${dev}"
  done
  log "done: ${VM} is shut off with its ISOs ejected"
}

main "$@"
