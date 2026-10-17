# Host releases: the layout and the manual verbs

A host keeps each kbf release in its own directory and points two links at them.
`kbf-deploy host` reads and changes that layout. This layout is **Linux-only**: its lock
lives in `/run`, which macOS does not have, so the macOS install path will choose a
layout of its own.

```text
/usr/local/kbf/releases/<commit>/   one release: kbf, and units/ when it has units
/usr/local/kbf/current              link to releases/<commit>, the release that runs
/usr/local/kbf/previous             link to releases/<commit>, the one before it
/var/lib/kbf-deploy/state.json      the deploy state (0600)
/run/kbf-deploy.lock                the lock the state is changed under (0600)
```

`<commit>` is always the full 40-character lowercase commit id. The links are relative
(`releases/<commit>`). A release directory, its `units/` and its `kbf` are root's and
0755 (units 0644), so the user a service runs as cannot replace its own binary.

## The verbs

Every verb prints its plan and changes nothing unless `--apply` is given; `--apply`
needs root. The state file is root's alone, so run the dry runs with `sudo` too.

```sh
sudo kbf-deploy host status
sudo kbf-deploy host install --commit <commit> --from <dir>          # dry run
sudo kbf-deploy host install --commit <commit> --from <dir> --apply
sudo kbf-deploy host rollback [--apply]
sudo kbf-deploy host gc [--keep N] [--apply]
```

- `install` copies `<dir>/kbf` (and `<dir>/units/*` when present) into
  `releases/<commit>/`, refuses a release directory that is already there with other
  content, checks that the copied binary's sha256 is the source's, then points
  `previous` at the old current release and `current` at the new one and writes the
  state. Installing the current release again changes no link.
- `rollback` points `current` at the previous release and removes `previous` (the state
  records no release before it). It refuses when there is no previous release or its
  binary is gone.
- `gc` removes release directories. It never removes the current, previous or staged
  release, nor one the `current` or `previous` link points to; `--keep N` also keeps the
  N newest others (by the directory's modification time). Anything in `releases/` that
  is not a directory named by a commit id is left alone.

Each refuses while a commit-confirm deploy is pending in the state.

**No verb restarts a service.** After `install --apply` or `rollback --apply`, restart
the units that run `/usr/local/kbf/current/kbf` by hand (`sudo systemctl restart
<unit>`), then check `kbf-deploy host status`. Installing the units and restarting them
belongs to `kbf-deploy linux install-units`.
