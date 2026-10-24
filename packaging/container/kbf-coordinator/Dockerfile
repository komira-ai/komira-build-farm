# The image of the update coordinator (docs/design/updates.md): the `kbf-coordinator` binary the
# release pipeline built, and nothing else. No installer, no updater and no CA key: the
# coordinator holds only its own certificate and key, the farm CA certificate it checks updaters
# against, and the desired-state file it writes.
#
# The release pipeline builds it for each Linux arch (.github/workflows/artifacts.yml) as the
# asset `kbf-coordinator-image-<commit>-<platform>.oci.tar`, from the `kbf-coordinator` it built
# for the commit and checked names it. `cargo xtask pkg-image-asset --service coordinator` refuses
# the image unless /usr/bin/kbf-coordinator is byte for byte that file. A build by hand, for one
# arch, from a binary built for it:
#
#   cp target/release/kbf-coordinator ctx/
#   docker buildx build --platform linux/<amd64|arm64> -f packaging/container/kbf-coordinator/Dockerfile ctx
#
# The base is the frontend image's (packaging/container/frontend/Dockerfile): distroless `cc` on
# Debian 13, pinned by its image index digest, with the CA roots the coordinator's https client
# reads (the OIDC key fetch), no shell and no package manager. Its `nonroot` user, uid 65532, is
# the user below. The coordinator's DNS name is not in the image: it is the name in the
# certificate mounted below, and the agents that dial it are given it (`kbf-updater agent
# --coordinator`, required, no default).
#
# Files the orchestrator mounts (the default flags below name them; every flag may be replaced):
#   /etc/kbf-coordinator/tls.pem, tls.key   the coordinator's certificate (role `coordinator`)
#   /etc/kbf-coordinator/ca.pem             the farm CA certificate
#   /etc/kbf-coordinator/oidc.toml          the allow-list of CI's OIDC token
#   /etc/kbf-coordinator/operator.token     the operator token, owned by uid 65532, mode 0600 or
#                                           0400 (the coordinator refuses any other mode)
#   /var/lib/kbf-coordinator                a volume the coordinator may write: the desired state
#                                           (owned by uid 65532, mode 0750)
# The image holds none of them and no secret.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e
COPY --chmod=0755 kbf-coordinator /usr/bin/kbf-coordinator
USER 65532:65532
EXPOSE 8443 8444
ENTRYPOINT ["/usr/bin/kbf-coordinator"]
CMD ["--heartbeat-listen=0.0.0.0:8443", "--control-listen=0.0.0.0:8444", \
     "--tls-cert=/etc/kbf-coordinator/tls.pem", "--tls-key=/etc/kbf-coordinator/tls.key", \
     "--client-ca=/etc/kbf-coordinator/ca.pem", "--oidc-config=/etc/kbf-coordinator/oidc.toml", \
     "--operator-token-file=/etc/kbf-coordinator/operator.token", \
     "--state-file=/var/lib/kbf-coordinator/desired.json"]
