# Contributing to kbf

Thank you for helping. This file says what a pull request needs before it can
be merged.

## Developer Certificate of Origin (DCO)

Every commit must carry a `Signed-off-by` line. By adding it you certify the
[Developer Certificate of Origin](https://developercertificate.org): that you
wrote the change, or otherwise have the right to submit it under the project's
license ([Apache-2.0](LICENSE)).

Git adds the line for you with `-s`:

```sh
git commit -s -m "server: reject digests with a negative size"
```

which produces:

```
server: reject digests with a negative size

Signed-off-by: Your Name <you@example.com>
```

The name and email must match the commit author. A pull request with an
unsigned commit cannot be merged.

## The mutant rule

A test that cannot fail is not a test. Every test in kbf must have been seen
failing on a planted defect (a mutant) before it is merged:

1. Write the test and see it pass.
2. Plant a mutant: a small, deliberate bug in the code the test is meant to
   guard (flip a comparison, drop a check, return early).
3. Run the test and see it fail at the assertion you expect. A failure caused
   by a compile error proves nothing.
4. Remove the mutant.

Each test says, in a comment or its name, which defect it catches. The pull
request description lists every mutant you planted and the test that went red
on it.

A bug fix ships with a test that failed before the fix.

## Small pull requests

Keep pull requests small and about one thing. A reviewer should be able to
read the whole diff in one sitting. Split refactors from behavior changes, and
land interfaces before the code that uses them. Keep source files under 1000
lines.

## Building and testing

kbf is a Cargo workspace. All builds and tests use `cargo`:

```sh
cargo build --workspace
cargo test --workspace
```

There is no other build system in this repository.

## Continuous integration

CI runs only on GitHub-hosted runners. A pull request is merged only when CI is
green on its head commit. Changes under `.github/workflows/` need a review from
the code owners listed in [.github/CODEOWNERS](.github/CODEOWNERS).

## Reporting security issues

Do not open a public issue or pull request for a vulnerability. Follow
[SECURITY.md](SECURITY.md) instead.
