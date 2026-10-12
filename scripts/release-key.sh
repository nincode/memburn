#!/usr/bin/env bash
# Creates the ed25519 release signing key (PEM, mode 600) if absent and prints its public key as hex.
# Usage: release-key.sh [KEY]; KEY defaults to ~/.config/memburn/release-ed25519.
set -euo pipefail
key=${1:-$HOME/.config/memburn/release-ed25519}
if [[ ! -e $key ]]; then
  mkdir -p "$(dirname "$key")"
  (umask 077 && openssl genpkey -algorithm ed25519 -out "$key")
  echo "created $key" >&2
fi
# The last 32 bytes of the DER SubjectPublicKeyInfo are the raw public key.
openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n'
echo
