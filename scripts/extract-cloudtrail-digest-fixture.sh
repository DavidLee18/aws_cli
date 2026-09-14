#!/bin/sh
# Regenerates tests/golden/cloudtrail-digest-signature.json.
#
# The signature is produced by openssl rather than by this codebase, so the test that
# reads it is checking our verification against an independent signer.
set -eu
openssl genrsa -out key.pem 2048
openssl rsa -in key.pem -RSAPublicKey_out -outform DER -out pub.der
echo "Now sign the digest's string-to-sign with:"
echo "  openssl dgst -sha256 -sign key.pem < string_to_sign | xxd -p -c 256"
echo "and record the hex signature, the inflated digest and the base64 of pub.der."
