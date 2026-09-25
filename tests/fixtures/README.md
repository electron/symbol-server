# Test TLS fixtures

`test-ca.pem` is a throwaway CA; `test-cert.pem` / `test-key.pem` are the fake
upstream's leaf certificate (SANs `localhost`, `127.0.0.1`, `::1`) signed by
it. The symbol server trusts the CA through `EXTRA_CA_CERTS` during tests.
(rustls, unlike Node, refuses a self-signed CA certificate as a server's leaf
certificate, so the old single self-signed certificate couldn't be reused.)

The CA key was discarded. To regenerate all three (valid for 100 years):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout ca-key.pem -out test-ca.pem -days 36500 -subj "/CN=symbol-server test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout test-key.pem -out leaf.csr -subj "/CN=localhost"
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1\n' > leaf.ext
openssl x509 -req -in leaf.csr -CA test-ca.pem -CAkey ca-key.pem -CAcreateserial \
  -out test-cert.pem -days 36500 -extfile leaf.ext
rm ca-key.pem leaf.csr leaf.ext test-ca.srl
```
