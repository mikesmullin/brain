# build/certs — optional TLS trust anchors for the image build

Drop a `.crt` (PEM root or intermediate CA) in this directory and both
`Containerfile` and `Containerfile.prebuilt` copy it into the image's trust
store:

```
COPY build/certs/ /usr/local/share/ca-certificates/
… && update-ca-certificates
```

`update-ca-certificates` imports `*.crt` only, so this README is inert.

## Why this directory is empty in a clean clone

Nothing here is a build dependency. A clone with no `.crt` in it builds fine and
gets the stock Debian roots plus `ca-certificates`, which is correct for the
common case — public CAs, public registries, public model endpoints.

You add an anchor when **your** network is not the common case: a corporate
network that terminates TLS for `static.crates.io` / `static.rust-lang.org` with
a CA it issued itself. Without that anchor `rustup target add` and `cargo build`
fail inside the build stage with `invalid peer certificate: UnknownIssuer`, and
the failure lands a hundred steps away from its cause.

Two properties follow from that, and both are deliberate:

- **Anchors are never committed.** `*.crt` and `*.pem` are in `.gitignore`, and
  the negation that keeps this README tracked names only the README. A private
  or company-issued root belongs to the network that trusts it, not to a public
  repo. They are also frequently paired with a key — and a committed root CA is
  a disclosure whether or not it has a key next to it.
- **The public roots are kept, not replaced.** `github.com` (the pinned
  wasm-bindgen download) and any public model endpoint are not intercepted, and
  they must verify against their real issuer. So this adds to
  `/usr/local/share/ca-certificates/`; it never overwrites
  `/etc/ssl/certs/ca-certificates.crt`.

## Both stages, on purpose

The **build** stage needs the anchor for crate and tool downloads. The
**runtime** stage needs it for whatever private endpoint the brain's model or
embedding provider resolves to at run time — a pod should not discover a missing
anchor on its first request. Copying one directory into both stages is how that
stays symmetric; `tools/build-image.sh` warns (without failing) when it is empty.
