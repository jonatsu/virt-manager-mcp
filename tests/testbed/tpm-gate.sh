#!/usr/bin/env bash
# The TPM gate: whether a VM whose TPM state is a file volume in the pool agent-vms starts on this host. The stock
# AppArmor profile of swtpm allows no writes there, so the host needs the rule
# `owner <pool dir>/*.tpm rwk,` in /etc/apparmor.d/local/usr.bin.swtpm, and only a start shows that the profile is
# loaded. The script makes the state with swtpm_setup as the calling user, uploads it as a volume owned by the swtpm
# user, defines a diskless VM that names the volume, starts it and removes everything again. Usage:
# tests/testbed/tpm-gate.sh. Exits 0 when the VM starts, 1 when it does not or the host lacks a prerequisite; the
# reason goes to stderr. It touches only the VM t-tpmgate and the volume t-tpmgate.tpm in pool agent-vms on
# qemu:///system, and removes both first, in case an interrupted run left them.
set -uo pipefail

readonly URI=qemu:///system
readonly POOL=agent-vms
readonly VM=t-tpmgate
readonly VOL=${VM}.tpm
readonly MEMORY_MIB=64

vsh() { virsh -q -c "${URI}" "$@"; }

die() {
  echo "tpm-gate: $*" >&2
  exit 1
}

WORK=

# Removes the gate's own VM and volume, and its scratch directory.
cleanup() {
  local status=$?
  vsh domstate "${VM}" > /dev/null 2>&1 && {
    vsh destroy "${VM}" > /dev/null 2>&1
    # The default undefine removes a TPM file volume in the pool, and --tpm says so.
    vsh undefine "${VM}" --tpm --nvram > /dev/null 2>&1 || vsh undefine "${VM}" > /dev/null 2>&1
  }
  vsh pool-refresh "${POOL}" > /dev/null 2>&1
  vsh vol-info --pool "${POOL}" "${VOL}" > /dev/null 2>&1 && vsh vol-delete --pool "${POOL}" "${VOL}" > /dev/null 2>&1
  [[ -n ${WORK} && -d ${WORK} ]] && rm -rf -- "${WORK:?}"
  exit "${status}"
}

trap cleanup EXIT
trap 'exit 130' INT TERM

for tool in virsh getent swtpm_setup xmllint; do
  command -v "${tool}" > /dev/null || die "${tool} is not installed (swtpm_setup comes with swtpm-tools)"
done
vsh uri > /dev/null 2>&1 || die "virsh cannot connect to ${URI}"
vsh pool-info "${POOL}" > /dev/null 2>&1 || die "pool ${POOL} does not exist"

# The swtpm user and its primary group, looked up and not hard-coded.
passwd=$(getent passwd swtpm) || die "the host has no user swtpm; install swtpm-tools"
IFS=: read -r _ _ uid gid _ <<< "${passwd}"
[[ ${uid} =~ ^[0-9]+$ && ${gid} =~ ^[0-9]+$ ]] || die "cannot read the uid and gid of swtpm from: ${passwd}"

pool_dir=$(vsh pool-dumpxml "${POOL}" | xmllint --xpath 'string(//target/path)' -) || die "cannot read the path of pool ${POOL}"
[[ ${pool_dir} == /* ]] || die "pool ${POOL} has no absolute path"

# Clear what an interrupted run left, then work.
if vsh domstate "${VM}" > /dev/null 2>&1; then
  vsh destroy "${VM}" > /dev/null 2>&1
  vsh undefine "${VM}" --tpm --nvram > /dev/null 2>&1 || vsh undefine "${VM}" > /dev/null 2>&1 || die "cannot remove the old ${VM}"
fi
vsh pool-refresh "${POOL}" > /dev/null || die "cannot refresh pool ${POOL}"
if vsh vol-info --pool "${POOL}" "${VOL}" > /dev/null 2>&1; then
  vsh vol-delete --pool "${POOL}" "${VOL}" > /dev/null || die "cannot remove the old volume ${VOL}"
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/tpm-gate.XXXXXX") || die "cannot make a scratch directory"
: > "${WORK}/state.tpm"
swtpm_setup --tpm2 --tpmstate "file://${WORK}/state.tpm" --createek --lock-nvram --not-overwrite \
  --pcr-banks sha256 --config /etc/swtpm_setup.conf > "${WORK}/setup.log" 2>&1 \
  || die "swtpm_setup failed: $(tail -n 3 "${WORK}/setup.log")"
size=$(stat -c %s "${WORK}/state.tpm")
((size > 0)) || die "swtpm_setup left no state"

cat > "${WORK}/volume.xml" << EOF
<volume>
  <name>${VOL}</name>
  <capacity unit='bytes'>${size}</capacity>
  <target>
    <format type='raw'/>
    <permissions><owner>${uid}</owner><group>${gid}</group><mode>0600</mode></permissions>
  </target>
</volume>
EOF
vsh vol-create --pool "${POOL}" "${WORK}/volume.xml" > /dev/null || die "cannot create volume ${VOL}"
vsh vol-upload --pool "${POOL}" "${VOL}" "${WORK}/state.tpm" || die "cannot upload the state to ${VOL}"

cat > "${WORK}/domain.xml" << EOF
<domain type='kvm'>
  <name>${VM}</name>
  <memory unit='MiB'>${MEMORY_MIB}</memory>
  <vcpu>1</vcpu>
  <os><type arch='x86_64' machine='q35'>hvm</type></os>
  <devices>
    <tpm model='tpm-crb'>
      <backend type='emulator' version='2.0'><source type='file' path='${pool_dir}/${VOL}'/></backend>
    </tpm>
  </devices>
</domain>
EOF
vsh define "${WORK}/domain.xml" > /dev/null || die "cannot define ${VM}"
if ! start_log=$(vsh start "${VM}" 2>&1); then
  echo "tpm-gate: ${VM} does not start; check that /etc/apparmor.d/local/usr.bin.swtpm holds the line" >&2
  echo "tpm-gate:   owner ${pool_dir}/*.tpm rwk," >&2
  echo "tpm-gate: and that the profile is loaded. libvirt said: ${start_log}" >&2
  exit 1
fi
[[ $(vsh domstate "${VM}" 2> /dev/null) == running ]] || die "${VM} started but is not running"
echo "tpm-gate: ${VM} started with its TPM state in ${pool_dir}/${VOL}"
