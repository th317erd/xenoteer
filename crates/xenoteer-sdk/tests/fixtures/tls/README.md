<!-- SPDX-License-Identifier: Apache-2.0 -->

# TLS test fixtures

These are fixed, public test credentials. They make the SDK TLS integration
tests deterministic; they have no operational trust or secrecy value and must
never be used outside tests.

- The CA is self-signed as `CN=Xenoteer-Test-CA`.
- The server certificate is valid only for the IP subject alternative name
  `127.0.0.1`; the tests therefore bind directly to loopback.
- The client certificate is issued as `CN=Xenoteer-Test-Client`.
- All certificates become valid on 2026-07-30 UTC and expire on 2036-07-27
  UTC. Tests do not depend on public trust.
- Files are base64-encoded DER. Certificate files contain X.509 DER and key
  files contain unencrypted PKCS#8 DER.

To regenerate equivalent fixtures with OpenSSL, create a new RSA test CA,
create and sign server and client certificate requests, and give the server
certificate the extension `subjectAltName=IP:127.0.0.1`. Convert certificates
with:

```sh
openssl x509 -in INPUT.pem -outform DER | base64 -w 76
```

Convert private keys with:

```sh
openssl pkcs8 -topk8 -nocrypt -in INPUT.key.pem -outform DER | base64 -w 76
```

Regeneration intentionally produces a new public key set. Replace all five
certificate/key blobs together and rerun `tls_connection_options`; do not
change the loopback SAN or the mutual-TLS trust relationship.
