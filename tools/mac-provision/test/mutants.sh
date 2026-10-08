#!/bin/sh
# Plants each defect below in a copy of kbf-mac-provision and runs the tests that must
# catch it; every mutant must turn them red. A mutant whose edit no longer applies
# fails too, so this list cannot silently go stale.
#
#   sh tools/mac-provision/test/mutants.sh

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ORIG=$HERE/../kbf-mac-provision
WORK=$(mktemp -d "${TMPDIR:-/tmp}/kbf-mac-provision-mutants.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
FAILED=0
COUNT=0

# mutant NAME TESTS SED: the copy edited by SED must fail TESTS. A test t_drift:A,B runs
# only the drift cases of items A and B.
mutant() {
  COUNT=$((COUNT + 1))
  m=$WORK/m$COUNT/kbf-mac-provision
  mkdir -p "${m%/*}"
  sed -e "$3" "$ORIG" >"$m"
  if cmp -s "$ORIG" "$m"; then
    printf 'STALE  %s: the edit no longer applies\n' "$1"
    FAILED=$((FAILED + 1))
    return 0
  fi
  m_tests=
  m_drift=
  for m_w in $2; do
    case "$m_w" in
      t_drift:*)
        m_tests="$m_tests t_drift"
        m_drift="$m_drift $(printf '%s' "${m_w#t_drift:}" | tr ',' ' ')"
        ;;
      *) m_tests="$m_tests $m_w" ;;
    esac
  done
  if KBF_TESTS=$m_tests KBF_DRIFT=$m_drift sh "$HERE/run.sh" "$m" >"$WORK/m$COUNT/log" 2>&1; then
    printf 'LIVED  %s (%s)\n' "$1" "$2"
    FAILED=$((FAILED + 1))
  else
    printf 'killed %s (%s)\n' "$1" "$2"
  fi
}

mutant 'check skips a key (power_womp)' t_converge \
  '/^ITEMS=/s/ power_womp / /'
mutant 'check skips a key (updates_auto_download)' t_converge \
  '/^ITEMS=/s/ updates_auto_download / /'
mutant 'apply writes on every run (not idempotent)' t_converge \
  's/\[ "\$ST" = PASS \] || fix_item "\$ITEM"/fix_item "$ITEM"/'
mutant 'the profile is sourced' t_refused \
  's/^load_profile$/. "$PROFILE"; load_profile/'
mutant 'an unknown key is ignored' t_refused \
  's/\*) die "profile line \$lp_n: unknown key: \$lp_k" ;;/*) continue ;;/'
mutant 'a duplicate key is accepted' t_refused \
  's/\*" \$lp_k "\*) die "profile line \$lp_n: duplicate key: \$lp_k" ;;/*" $lp_k "*) : ;;/'
mutant 'a missing key is accepted' t_refused \
  's/\*) die "missing key: \$lp_k" ;;/*) : ;;/'
mutant 'any character in a value' t_refused \
  's/) die "profile line \$lp_n: \$lp_k: a value may hold only.*/) : ;;/'
mutant 'check exits 0 on drift' t_converge \
  's/run_check || exit 1/run_check/'
mutant 'apply exits 0 when something still fails' t_drift:filevault \
  's/run_check quiet || exit 1/run_check quiet/'
mutant 'apply turns FileVault off' t_drift:filevault \
  's/^ck_filevault() {/fix_filevault() { fdesetup disable; change "FileVault off"; }\nck_filevault() {/;s/ admin_user filevault probe / admin_user probe /'
mutant 'a lease user in range fails (apply clears it mid-lease)' t_lease_autologin \
  's/if in_lease_range "\$cu_uid"; then/if false; then/'
mutant 'any kbf-lease-* user passes, whatever its uid' t_drift:autologin \
  's/if in_lease_range "\$cu_uid"; then/if true; then/'
mutant 'any auto-login user in range passes, whatever its name' t_drift:autologin \
  's/^    kbf-lease-\*)$/    *)/'
mutant 'a stale /etc/kcpassword is ignored' t_drift:autologin \
  's/bad "off, but \/etc\/kcpassword is left over"/ok off/'
mutant 'the job runs as a Standard (throttled) process' t_converge \
  's/<string>Interactive<\/string>/<string>Standard<\/string>/'
mutant 'the job runs as root' t_converge \
  's/<key>UserName<\/key>/<key>Unused<\/key>/'
mutant 'the node label lacks the profile' t_converge \
  's/ --label "profile=\$P_profile@\$PROFILE_SHA12"//'
mutant 'the job file is not compared' t_drift:launchd_label \
  's/if \[ "\$(cat "\$cj_p" 2>\/dev\/null)" != "\$(render_plist)" \]; then/if false; then/'
mutant 'the job is not checked as loaded' t_drift:launchd_label \
  's/elif ! launchctl print "system\/\$P_launchd_label" >\/dev\/null 2>&1; then/elif false; then/'
mutant 'restart does not truncate the error log' t_restart \
  's/\[ -f "\$R\$ERR_LOG" \] && : >"\$R\$ERR_LOG"/:/'
mutant 'restart starts before it stops' t_restart \
  's/  launchctl bootout "system\/\$P_launchd_label" >\/dev\/null 2>&1/  :/'
mutant 'a simulator runtime image is ignored' t_drift:simulator_runtimes \
  's/\*\/Images\/\*.dmg | /*\/Images\/*.none | /'
mutant 'a mounted simulator runtime is ignored' t_drift:simulator_runtimes \
  's/ | \*\/Volumes\/\*) cs_found/) cs_found/'
mutant 'an Xcode of another build passes' t_drift:xcode \
  's/if \[ "\$cx_have" = "\$cx_build" \]; then/if true; then/'
mutant 'an unpinned Xcode is ignored' t_drift:xcode \
  's/cx_bad="\$cx_bad \$cx_e is not pinned;"/:/'
mutant 'a link to an Xcode counts as unpinned' t_quiet_entries \
  's/\[ -L "\$ex_d" \] && continue/:/'
mutant 'an xip is used without its SHA-256' t_xcode_sources \
  's/if \[ "\$(sha256 "\$fx_f")" = "\$1" \]; then/if true; then/'
mutant 'an expanded Xcode is not checked for its build' t_xcode_sources \
  's/if \[ "\$(xcode_build "\$ix_new")" != "\$ix_build" \]; then/if false; then/'
mutant 'the name scheme accepts upper case and spaces' t_refused \
  "s/want hostname '\\[a-z\\]\\[a-z0-9\\]\\*(-\\[a-z0-9\\]+)\\*-mac-\\[0-9\\]+'/want hostname '[A-Za-z0-9 -]+'/"
mutant 'one of the three names is not checked' t_drift:hostname \
  's/for ch_n in HostName LocalHostName ComputerName; do/for ch_n in HostName LocalHostName; do/'
mutant 'pmset keys match in any case (Sleep On Power Button)' t_converge \
  's/\$1 == k { print \$2; exit }/tolower($1) == k { print $2; exit }/'
mutant 'a role id of 500 or more is accepted' t_refused \
  's/\[ "\$P_role_id" -lt 500 \] || die/: || die/'
mutant 'the role id of another account is taken over' t_drift:role_user \
  's/\[ -z "\$(role_id_taken)" \] || return 0/:/'
mutant 'another administrator is ignored' t_drift:admin_user \
  's/\*) ca_others="\$ca_others \$ca_m" ;;/*) : ;;/'
mutant 'the sshd drop-in sorts after the system one' t_converge \
  's/000-kbf\.conf/200-kbf.conf/g'
mutant 'the sshd Include line is not required' t_drift:ssh_password_auth \
  's/if ! grep -qxF -- "\$SSHD_INCLUDE" "\$SSHD_CONFIG" 2>\/dev\/null; then/if false; then/'
mutant 'a large clock offset passes' t_drift:time_max_offset_ms \
  's/elif \[ "\$co_v" -le "\$P_time_max_offset_ms" \]; then/elif true; then/'
mutant 'a negative offset counts as in range' t_drift:time_max_offset_ms \
  's/if (o < 0) o = -o; //'
mutant 'an expectation may name an unpinned Xcode' t_refused \
  's/\*) die "expect_probe: \$ve_build names no pinned xcode" ;;/*) : ;;/'
mutant 'a probe with another SHA-256 passes' t_drift:probe \
  's/\[ "\$(sha256 "\$R\$cq_path")" = "\${cq##\*:}" \] || cq_bad/: || cq_bad/'
mutant 'apply runs without root' t_not_root \
  's/\[ "\$(id -u)" = 0 \] || die/: || die/'
mutant '--keys ignores an unknown item' t_keys \
  's/\*) die "--keys: unknown item: \$si_k (items: \$ITEMS)" ;;/*) : ;;/'

printf '%d mutants, %d not killed\n' "$COUNT" "$FAILED"
[ "$FAILED" = 0 ]
