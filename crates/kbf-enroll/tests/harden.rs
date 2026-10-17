//! `harden_process` changes the whole process, so it has a test binary of its own.

#[test]
fn core_dumps_are_turned_off() {
    kbf_enroll::keys::harden_process().expect("hardened");
    let limit = rustix::process::getrlimit(rustix::process::Resource::Core);
    assert_eq!((limit.current, limit.maximum), (Some(0), Some(0)));
    #[cfg(target_os = "linux")]
    assert_eq!(
        rustix::process::dumpable_behavior().expect("dumpable"),
        rustix::process::DumpableBehavior::NotDumpable
    );
}
