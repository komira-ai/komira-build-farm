# An example kbf-mac-provision profile. Every value here is illustrative: the macOS
# build, the Xcode builds and every SHA-256 are placeholders. One KEY=VALUE per line;
# the keys are described in tools/mac-provision/README.md.
profile=mac-arm64-pool
macos_version=27.0.1
macos_build=26A000
hostname=kbf-mac-07
admin_user=farmadmin
role_user=_kbf
role_id=480
lease_uids=600-699
power_sleep=0
power_standby=0
power_powernap=0
power_autorestart=1
power_womp=1
updates_auto_download=0
updates_auto_install_macos=0
updates_auto_install_security=0
updates_config_data=1
appstore_auto_update=0
remote_login=1
ssh_password_auth=0
firewall=on
filevault=off
autologin=off
time_server=time.example.net
time_max_offset_ms=250
launchd_label=org.example.kbf-daemon
kbf_server=https://farm.example.net:8981
kbf_cas=https://farm.example.net:8980
kbf_ca_cert=/usr/local/kbf/etc/ca.pem
kbf_cert=/usr/local/kbf/etc/node.pem
kbf_key=/usr/local/kbf/etc/node.key
kbf_scratch=/Volumes/kbf/scratch
kbf_labels=pool=mac rack=r2
simulator_runtimes=none
xcode_xip_dir=/Library/kbf/xip
xcode=17A000:/Applications/Xcode-26.6.app/Contents/Developer:1111111111111111111111111111111111111111111111111111111111111111
xcode=18A000:/Applications/Xcode-27.0.app/Contents/Developer:2222222222222222222222222222222222222222222222222222222222222222
default_developer_dir=/Applications/Xcode-27.0.app/Contents/Developer
probe=host_identity:/usr/local/kbf/probes/host_identity.sh:3333333333333333333333333333333333333333333333333333333333333333
expect_probe=host_identity:17A000=26.6-0123456789abcdef
expect_probe=host_identity:18A000=
