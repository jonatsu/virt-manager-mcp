#!/bin/sh
# shellcheck shell=sh
# Fetch the ocrs project's ONNX text detection and recognition models for display.ocr_models.
#
# Upstream publishes no checksums, so the script pins the SHA-256 sums recorded in
# docs/research/2026-11-10-ocr-probe.md. It keeps a file already in place whose sum matches, and removes one whose
# sum does not. It then downloads to a temporary name in the target directory, checks the sum, and only then moves
# the file to its final name, so no file with another sum is left under that name.
#
# Needs curl and sha256sum. Exits 1 on a failed download or a sum mismatch, 2 on a usage error.

set -u

readonly BASE_URL=https://ocrs-models.s3-accelerate.amazonaws.com
readonly DETECTION_SHA256=a917b23dbd9524b465df7e922641b3ff2981623df4ded5a0234004ef2fee7cfe
readonly RECOGNITION_SHA256=86c145c2edb96c8caed5b1ebb8f44d706408922211451c309c625157dd6061c5

prog=${0##*/}
tmp=

usage() {
  cat << EOF
Usage: ${prog} DIR

Downloads text-detection.onnx and text-recognition.onnx into DIR, creating it if needed,
and checks each against its pinned SHA-256 sum. Point display.ocr_models at DIR.
EOF
}

log() {
  printf '%s: %s\n' "${prog}" "$*" >&2
}

die() {
  log "$*"
  exit 1
}

cleanup() {
  if [ -n "${tmp}" ]; then
    rm -f -- "${tmp}"
  fi
}

# Prints the SHA-256 sum of file $1. Hashing standard input keeps the file name, which sha256sum would escape when
# it holds a backslash or newline, out of the output.
sum_of() {
  sum=$(sha256sum < "$1") || return 1
  printf '%s\n' "${sum%% *}"
}

# Makes sure DIR/$1 exists and has sum $2.
fetch() {
  name=$1
  want=$2
  dest=${dir}/${name}
  if [ -f "${dest}" ]; then
    have=$(sum_of "${dest}") || die "cannot read ${dest}"
    if [ "${have}" = "${want}" ]; then
      log "${name}: present, sum matches"
      return 0
    fi
    log "${name}: sum ${have} does not match, removing it and downloading again"
    rm -f -- "${dest}" || die "cannot remove ${dest}"
  fi
  tmp=$(mktemp "${dir}/.${name}.XXXXXX") || die "cannot create a temporary file in ${dir}"
  log "${name}: downloading ${BASE_URL}/${name}"
  curl --fail --silent --show-error --location --proto '=https' --output "${tmp}" -- "${BASE_URL}/${name}" \
    || die "${name}: download failed"
  have=$(sum_of "${tmp}") || die "cannot read ${tmp}"
  if [ "${have}" != "${want}" ]; then
    die "${name}: downloaded file has sum ${have}, expected ${want}"
  fi
  chmod 0644 -- "${tmp}" || die "cannot set the mode of ${tmp}"
  mv -f -- "${tmp}" "${dest}" || die "cannot move ${tmp} to ${dest}"
  tmp=
  log "${name}: downloaded, sum matches"
}

case ${1-} in
  -h | --help)
    usage
    exit 0
    ;;
  *) ;;
esac
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
  usage >&2
  exit 2
fi
dir=$1
# A relative directory starting with "-" would read as an option to mktemp.
case ${dir} in
  /*) ;;
  *) dir=./${dir} ;;
esac

for tool in curl sha256sum mktemp; do
  command -v "${tool}" > /dev/null 2>&1 || die "${tool} is required"
done

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p -- "${dir}" || die "cannot create ${dir}"
fetch text-detection.onnx "${DETECTION_SHA256}"
fetch text-recognition.onnx "${RECOGNITION_SHA256}"
