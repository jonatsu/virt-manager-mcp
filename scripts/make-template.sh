#!/usr/bin/env bash
# Build a virtmanager-mcp template volume from the official Debian 13 cloud image.
#
# The script downloads and verifies the image, uploads it as a new qcow2 volume, boots it once with a seed that
# installs the QEMU guest agent, cleans the guest's cloud-init state and machine ID, and removes the temporary domain.
# It never touches an existing volume. See docs/templates.md.

set -uo pipefail

readonly IMAGE_BASE=https://cloud.debian.org/images/cloud/trixie/latest
readonly IMAGE_NAME=debian-13-genericcloud-amd64.qcow2
readonly BUILD_VM=t-tpl-build
readonly AGENT_TIMEOUT_SECS=600
readonly SHUTDOWN_TIMEOUT_SECS=120

usage() {
  cat << EOF
Usage: ${0##*/} --pool POOL --volume NAME [--uri URI] [--cache DIR]

Builds template volume NAME (for example tpl-debian13.qcow2) in libvirt storage pool POOL.

  --pool POOL    libvirt storage pool, the server's manage.disk_pool
  --volume NAME  new volume name; the script refuses an existing one
  --uri URI      libvirt connection URI (default: qemu:///system)
  --cache DIR    download directory (default: \${XDG_CACHE_HOME:-~/.cache}/virtmanager-mcp/images)

Set VIRT_INSTALL to the virt-install command when the default one cannot run,
for example VIRT_INSTALL="/usr/bin/python3 /usr/bin/virt-install".
EOF
}

die() {
  printf '%s: %s\n' "${0##*/}" "$*" >&2
  exit 1
}

log() {
  printf '%s: %s\n' "${0##*/}" "$*" >&2
}

pool=
volume=
uri=qemu:///system
cache=${XDG_CACHE_HOME:-${HOME}/.cache}/virtmanager-mcp/images

while (($# > 0)); do
  case $1 in
    --pool | --volume | --uri | --cache)
      (($# >= 2)) || die "$1 needs a value"
      declare "${1#--}=$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
done

[[ -n ${pool} && -n ${volume} ]] || {
  usage >&2
  exit 2
}
[[ ${volume} =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] || die "invalid volume name: ${volume}"

for cmd in virsh curl sha512sum xorriso jq base64; do
  command -v "${cmd}" > /dev/null || die "missing command: ${cmd}"
done
if [[ -n ${VIRT_INSTALL-} ]]; then
  read -r -a virt_install <<< "${VIRT_INSTALL}"
else
  virt_install=(virt-install)
fi

virsh_() { virsh --connect "${uri}" "$@"; }

virsh_ pool-info "${pool}" > /dev/null 2>&1 || die "no storage pool ${pool}"
if virsh_ vol-info --pool "${pool}" "${volume}" > /dev/null 2>&1; then
  die "volume ${volume} already exists in ${pool}; templates are never rebuilt in place"
fi
virsh_ dominfo "${BUILD_VM}" > /dev/null 2>&1 && die "domain ${BUILD_VM} exists; remove it first"
seed_volume=${BUILD_VM}-seed.iso
virsh_ vol-info --pool "${pool}" "${seed_volume}" > /dev/null 2>&1 && die "volume ${seed_volume} exists; remove it first"

workdir=$(mktemp -d) || die "cannot create a temporary directory"
volume_created=0
seed_created=0
domain_defined=0
done_ok=0

cleanup() {
  local status=$?
  if ((domain_defined)); then
    virsh_ destroy "${BUILD_VM}" > /dev/null 2>&1
    virsh_ undefine "${BUILD_VM}" > /dev/null 2>&1
  fi
  ((seed_created)) && virsh_ vol-delete --pool "${pool}" "${seed_volume}" > /dev/null 2>&1
  if ((volume_created && ! done_ok)); then
    log "removing the unfinished volume ${volume}"
    virsh_ vol-delete --pool "${pool}" "${volume}" > /dev/null 2>&1
  fi
  rm -rf -- "${workdir:?}"
  exit "${status}"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Runs a shell command in the guest through the agent and prints its stdout; returns its exit code.
guest_sh() {
  local request pid reply exited code
  request=$(jq -cn --arg cmd "$1" \
    '{execute: "guest-exec", arguments: {path: "/bin/sh", arg: ["-c", $cmd], "capture-output": true}}')
  pid=$(virsh_ qemu-agent-command "${BUILD_VM}" "${request}" | jq -r '.return.pid') || return 1
  for _ in $(seq 1 "${AGENT_TIMEOUT_SECS}"); do
    reply=$(virsh_ qemu-agent-command "${BUILD_VM}" \
      "{\"execute\":\"guest-exec-status\",\"arguments\":{\"pid\":${pid}}}") || return 1
    exited=$(jq -r '.return.exited' <<< "${reply}")
    if [[ ${exited} == true ]]; then
      jq -r '.return."out-data" // empty' <<< "${reply}" | base64 -d
      code=$(jq -r '.return.exitcode // 1' <<< "${reply}")
      return "${code}"
    fi
    sleep 1
  done
  return 1
}

mkdir -p -- "${cache}" || die "cannot create ${cache}"
log "downloading ${IMAGE_NAME} and SHA512SUMS"
curl -fsSL -o "${workdir}/SHA512SUMS" "${IMAGE_BASE}/SHA512SUMS" || die "download of SHA512SUMS failed"
since=()
[[ -f ${cache}/${IMAGE_NAME} ]] && since=(-z "${cache}/${IMAGE_NAME}")
curl -fsSL "${since[@]}" -o "${cache}/${IMAGE_NAME}" "${IMAGE_BASE}/${IMAGE_NAME}" || die "download of the image failed"
expected=$(awk -v name="${IMAGE_NAME}" '$2 == name { print $1 }' "${workdir}/SHA512SUMS")
[[ -n ${expected} ]] || die "SHA512SUMS has no entry for ${IMAGE_NAME}"
actual=$(sha512sum -- "${cache}/${IMAGE_NAME}" | awk '{ print $1 }')
[[ ${actual} == "${expected}" ]] || die "checksum mismatch for ${cache}/${IMAGE_NAME}"

log "uploading the image as ${volume}"
size=$(stat -c %s -- "${cache}/${IMAGE_NAME}") || die "cannot stat the image"
virsh_ vol-create-as "${pool}" "${volume}" "${size}" --format qcow2 > /dev/null || die "cannot create ${volume}"
volume_created=1
virsh_ vol-upload --pool "${pool}" "${volume}" "${cache}/${IMAGE_NAME}" || die "upload failed"

mkdir -- "${workdir}/seed"
cat > "${workdir}/seed/user-data" << 'EOF'
#cloud-config
packages: [qemu-guest-agent]
runcmd:
  - [systemctl, enable, --now, qemu-guest-agent]
EOF
printf 'instance-id: %s\nlocal-hostname: %s\n' "${BUILD_VM}-$(date +%s)" "${BUILD_VM}" > "${workdir}/seed/meta-data"
xorriso -as mkisofs -quiet -V cidata -J -r -o "${workdir}/seed.iso" "${workdir}/seed/user-data" \
  "${workdir}/seed/meta-data" || die "cannot build the seed ISO"
virsh_ vol-create-as "${pool}" "${seed_volume}" "$(stat -c %s -- "${workdir}/seed.iso")" --format raw > /dev/null \
  || die "cannot create ${seed_volume}"
seed_created=1
virsh_ vol-upload --pool "${pool}" "${seed_volume}" "${workdir}/seed.iso" || die "seed upload failed"

# One vCPU: the fresh cloud image reset in a loop on its first boots with two on the development host
# (docs/research/2026-11-10-fatfs-seed-boot.md).
log "booting ${BUILD_VM} once to install the guest agent"
"${virt_install[@]}" --connect "${uri}" --name "${BUILD_VM}" --memory 1024 --vcpus 1 --osinfo debian13 --import \
  --disk "vol=${pool}/${volume},bus=virtio" --disk "vol=${pool}/${seed_volume},device=cdrom" \
  --network network=default,model=virtio \
  --channel unix,target.type=virtio,target.name=org.qemu.guest_agent.0 \
  --graphics spice --noautoconsole --print-xml > "${workdir}/domain.xml" || die "virt-install failed"
virsh_ define "${workdir}/domain.xml" > /dev/null || die "cannot define ${BUILD_VM}"
domain_defined=1
virsh_ start "${BUILD_VM}" > /dev/null || die "cannot start ${BUILD_VM}"

for _ in $(seq 1 "${AGENT_TIMEOUT_SECS}"); do
  virsh_ qemu-agent-command "${BUILD_VM}" '{"execute":"guest-ping"}' > /dev/null 2>&1 && break
  sleep 1
done
virsh_ qemu-agent-command "${BUILD_VM}" '{"execute":"guest-ping"}' > /dev/null 2>&1 \
  || die "the guest agent did not answer within ${AGENT_TIMEOUT_SECS}s"

guest_sh 'cloud-init status --wait >/dev/null; cloud-init status' || die "cloud-init did not finish cleanly"
log "cleaning cloud-init state and the machine ID"
guest_sh 'cloud-init clean --logs --machine-id && sync' || die "cloud-init clean failed"

virsh_ shutdown "${BUILD_VM}" --mode agent > /dev/null || die "cannot shut ${BUILD_VM} down"
for _ in $(seq 1 "${SHUTDOWN_TIMEOUT_SECS}"); do
  [[ $(virsh_ domstate "${BUILD_VM}") == "shut off" ]] && break
  sleep 1
done
[[ $(virsh_ domstate "${BUILD_VM}") == "shut off" ]] || die "${BUILD_VM} did not shut down"
virsh_ undefine "${BUILD_VM}" > /dev/null || die "cannot undefine ${BUILD_VM}"
domain_defined=0

done_ok=1
log "template volume ${volume} is ready in pool ${pool}"
