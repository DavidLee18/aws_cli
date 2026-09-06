# awsc

A Rust port of the AWS CLI v2 — a drop-in `aws` with matching arguments, exit codes and
output.

The argument surface is complete: **19,452 of 19,452 operations across 427 services accept
exactly the arguments the official CLI does**, and every global argument it declares is
accepted. Output formats, `--query`, pagination, the credential chain, endpoint resolution
and the six wire protocols are verified against the reference and against live AWS.

## Install

```sh
cargo binstall awsc     # prebuilt binary from the GitHub release
cargo install awsc      # build from source
```

Or download an archive from [Releases](https://github.com/DavidLee18/aws_cli/releases) and
put `awsc` and `models.bin` in the same directory.

### The service catalogue

`awsc` reads a compiled catalogue of 432 service models — `models.bin`, 113 MB, memory
mapped rather than parsed, which is what keeps startup fast. The release archives contain
it; `cargo binstall` and `cargo install` cannot, because binstall installs binaries only
and crates.io caps a package at about 10 MB.

So an install of either kind downloads the catalogue once, on the first command that needs
it, from the GitHub release matching the binary's version, and caches it per user. Run it
ahead of time with:

```sh
awsc update-models
```

The download is verified against the `SHA256SUMS` published in the same release. That
guards against a truncated or corrupted transfer and pins the catalogue to the release the
binary was cut from; it is not protection against a compromised release, since the checksum
comes from the same place as the asset. To avoid the download entirely, install from a
release archive or point `AWSC_MODELS_DIR` at a copy you vetted.

| variable | effect |
|---|---|
| `AWSC_MODELS_DIR` | use this directory's `models.bin` |
| `AWSC_CACHE_DIR` | where a downloaded catalogue is kept |
| `AWSC_RELEASE_BASE` | download from a mirror or fork instead |

## Status

`aws configure`, `aws sso login`/`logout`, `aws configure sso` and the whole `aws s3`
transfer tree are implemented. Not implemented, and refused by name rather than
approximated: `aws login`/`logout` (AWS Sign-In), `ddb`, `history`, and several `configure`
subcommands. `docs/divergences.md` in the repository is the running ledger of every known
difference from the reference, with its cause.

## Licence

MIT. See `NOTICE` for the data extracted from the AWS CLI and botocore.
