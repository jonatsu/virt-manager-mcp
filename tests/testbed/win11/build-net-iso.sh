#!/usr/bin/env bash
# Builds the driver ISO of the networked Windows guest t-win11net: a copy of virtio-win-0.1.302.iso with
# autounattend-net.xml added at its root. Windows Setup finds the answer file on that CD-ROM and loads the virtio
# storage and network drivers from the same disc. Usage: tests/testbed/win11/build-net-iso.sh VIRTIO_WIN_ISO OUTPUT_ISO
# VIRTIO_WIN_ISO is a local copy of virtio-win-0.1.302.iso; its SHA-256 must match the sum pinned below, because the
# virtio-win project publishes no checksum for the ISO itself. OUTPUT_ISO is written beside its final name and then
# moved into place, so a failed run leaves an existing OUTPUT_ISO alone. An ISO that libvirt has used belongs to
# libvirt-qemu, and the owner of its directory can still replace it, provided that no VM holds it in a CD-ROM.
# Exits 0 once the ISO holds the answer file byte for byte, 1 on a failure, 2 on a usage error. It touches only
# OUTPUT_ISO and a temporary file beside it. Then create t-win11net with create_vm, as tests/testbed/README.md says.
set -uo pipefail

readonly VIRTIO_SHA256=303f7ae40dad495d6ae474fdc571df58958a4dbc5c37a522d80f9a203867949d
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd) || exit 1
readonly HERE
readonly ANSWER=${HERE}/autounattend-net.xml
TMP=

log() { printf '%s: %s\n' "${0##*/}" "$*" >&2; }
die() {
  log "$*"
  exit 1
}
cleanup() {
  [[ -n ${TMP} ]] && rm -rf -- "${TMP:?}"
}

main() {
  local iso=${1-} out=${2-} sum dir staged
  if [[ $# -ne 2 || -z ${iso} || -z ${out} ]]; then
    echo "usage: ${0##*/} VIRTIO_WIN_ISO OUTPUT_ISO" >&2
    exit 2
  fi
  [[ -f ${iso} ]] || die "${iso} is not a file"
  [[ -f ${ANSWER} ]] || die "${ANSWER} is missing"
  [[ ${out} != "${iso}" ]] || die "OUTPUT_ISO must differ from VIRTIO_WIN_ISO"
  dir=$(dirname -- "${out}")
  [[ -d ${dir} && -w ${dir} ]] || die "${dir} is not a writable directory"
  for tool in xorriso sha256sum cmp; do
    command -v "${tool}" > /dev/null || die "${tool} is required"
  done

  log "checking ${iso}"
  sum=$(sha256sum -- "${iso}") || die "cannot read ${iso}"
  sum=${sum%% *}
  [[ ${sum} == "${VIRTIO_SHA256}" ]] || die "${iso} has SHA-256 ${sum}, expected ${VIRTIO_SHA256}"

  trap cleanup EXIT
  trap 'exit 130' INT TERM
  TMP=$(mktemp -d -p "${dir}" .build-net-iso.XXXXXX) || die "cannot create a temporary directory in ${dir}"
  staged=${TMP}/net.iso
  log "writing ${out}"
  xorriso -indev "${iso}" -outdev "${staged}" -joliet on -boot_image any keep \
    -map "${ANSWER}" /autounattend.xml -commit -end > "${TMP}/xorriso.log" 2>&1 \
    || {
      cat -- "${TMP}/xorriso.log" >&2
      die "cannot build the ISO"
    }
  xorriso -osirrox on -indev "${staged}" -extract /autounattend.xml "${TMP}/check.xml" > "${TMP}/check.log" 2>&1 \
    || {
      cat -- "${TMP}/check.log" >&2
      die "cannot read the answer file back from the new ISO"
    }
  cmp -s -- "${ANSWER}" "${TMP}/check.xml" || die "the answer file in the new ISO differs from ${ANSWER}"
  chmod 644 -- "${staged}" || die "cannot set the mode of the new ISO"
  mv -f -- "${staged}" "${out}" || die "cannot move the new ISO to ${out}"
  log "done: ${out} holds virtio-win 0.1.302 and the answer file"
}

main "$@"
