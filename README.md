# kbf: the komira build farm

kbf is an open-source remote build and test farm for [Bazel](https://bazel.build)
and [Buck2](https://buck2.build). It speaks the
[Remote Execution API v2](https://github.com/bazelbuild/remote-apis) (REAPI v2):
a build tool sends actions to kbf, kbf runs them on worker machines, and the
results come back through a shared content-addressable cache.

kbf is written in Rust. Its programs are named `kbf-*`:

- `kbf-server`: the REAPI v2 service. It serves the action cache and the
  content-addressable storage, and schedules actions onto workers.
- `kbf-daemon`: the worker. It runs on each build machine, fetches inputs,
  runs actions in isolation, and uploads outputs. A Linux node runs each action
  in a rootless container (`--driver container`); a Mac runs it as plain
  processes (`--driver native`), with its process tree, memory and network
  watched by the daemon.

## Status

Early development. The project is design-first: interfaces and tests land
before features, and nothing here is ready for production use yet. Expect
breaking changes.

## Design

[ARCHITECTURE.md](ARCHITECTURE.md) describes how kbf is put together, and links the
design documents in [docs/design](docs/design): the scheduler, storage, the worker
protocol, the daemon and execution, and capabilities.

[docs/artifacts.md](docs/artifacts.md) says which binaries CI builds and attests, and
how a node verifies one before it runs it.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Every commit needs a DCO sign-off, and
every test must have been seen failing on a planted defect.

## Security

To report a vulnerability, see [SECURITY.md](SECURITY.md). Do not put details of
a security problem in a public issue.

## License

kbf is licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
